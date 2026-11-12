//! Syntax and validation for a persistent native pipeline with a line sink.
use crate::ast::{Connection, Program, StepKind};
use crate::manifest::Registry;
use crate::{parser, runtime};
use anyhow::{bail, Context, Result};
use parser::Token;
use std::num::IntErrorKind;
use std::ops::Range;

/// A native pipeline that writes each final output record to stdout.
///
/// `line_limit`, when present, counts records after the last native transform.
/// Bytes are preserved exactly; newline is the delimiter and is included in a
/// terminated record. An unterminated nonempty suffix at EOF is one record.
#[derive(Clone, Debug)]
pub struct StreamProgram {
    pub pipeline: Program,
    pub line_limit: Option<usize>,
}

/// A stream syntax error with its position and a stable code from `diagnostics::CODES`.
///
/// `message` is the text `parse` returns, in `{:#}` form, so a message that names a
/// line starts with `line N: `. `line` is 1-based and `column` is the 1-based character
/// column of the failing token, set only where the parser knows that token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DetailedError {
    pub code: &'static str,
    pub line: usize,
    pub column: Option<usize>,
    pub message: String,
    /// The outermost context layer when `message` is `<layer>: <cause>`. `parse` shows
    /// only this layer with `{}`, as it did before this type existed.
    outer: Option<String>,
}

impl DetailedError {
    fn new(code: &'static str, line: usize, column: Option<usize>, message: String) -> Self {
        Self {
            code,
            line,
            column,
            message,
            outer: None,
        }
    }

    /// Add a statement's line as the outermost layer, as `with_context` did. An error
    /// created without a line takes this one.
    fn within(mut self, line: usize) -> Self {
        if self.line == 0 {
            self.line = line;
        }
        let layer = format!("line {line}");
        self.message = format!("{layer}: {}", self.message);
        self.outer = Some(layer);
        self
    }

    /// The `anyhow` error `parse` returns, with the same message and the same outer layer.
    fn into_anyhow(self) -> anyhow::Error {
        match self.outer {
            Some(layer) => {
                let cause = self
                    .message
                    .strip_prefix(layer.as_str())
                    .and_then(|rest| rest.strip_prefix(": "))
                    .unwrap_or(&self.message)
                    .to_owned();
                anyhow::Error::msg(cause).context(layer)
            }
            None => anyhow::Error::msg(self.message),
        }
    }
}

impl std::fmt::Display for DetailedError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for DetailedError {}

/// Identify stream syntax from the first nonempty, uncommented line.
pub fn is_stream(source: &str) -> bool {
    source
        .lines()
        .map(parser::strip_comment)
        .map(str::trim)
        .find(|line| !line.is_empty() && !parser::is_input_header(line))
        .is_some_and(|line| stream_header_body(line).is_some())
}

/// Parse the stream header and directives, delegating invocation syntax to the
/// ordinary finite-program parser so applet parameters stay identical.
pub fn parse(source: &str) -> Result<StreamProgram> {
    parse_detailed(source).map_err(DetailedError::into_anyhow)
}

/// `parse` with each error's code, line and column. The message is the one `parse` returns.
pub fn parse_detailed(source: &str) -> Result<StreamProgram, DetailedError> {
    let mut normalized = String::new();
    let mut saw_header = false;
    let mut saw_sink = false;
    let mut saw_take = false;
    let mut line_limit = None;
    // The header's line number, the byte offset of its applet phrase in that line, and the line.
    let mut header: Option<(usize, usize, String)> = None;
    let mut first_statement = None;
    let mut last_statement = None;

    for (index, raw_line) in source.lines().enumerate() {
        let line_number = index + 1;
        let uncommented = parser::strip_comment(raw_line);
        let line = uncommented.trim();
        if line.is_empty() {
            normalized.push('\n');
            continue;
        }
        if first_statement.is_none() {
            first_statement = Some(line_number);
        }
        last_statement = Some(line_number);
        let at = |code: &'static str, column: usize, text: &str| {
            DetailedError::new(
                code,
                line_number,
                Some(column),
                format!("line {line_number}: {text}"),
            )
        };
        if uncommented.trim_start().len() != uncommented.len() {
            return Err(at(
                "parse.indentation",
                1,
                "stream statements must not be indented",
            ));
        }

        if !saw_header {
            if parser::is_input_header(line) {
                normalized.push_str(line);
                normalized.push('\n');
                continue;
            }
            let Some(body) = stream_header_body(line) else {
                return Err(at("parse.syntax", 1, "expected `stream from <applet>`"));
            };
            if body.is_empty() {
                return Err(at(
                    "parse.syntax",
                    column_of(line, line.len()),
                    "stream source requires an applet phrase",
                ));
            }
            header = Some((line_number, line.len() - body.len(), line.to_owned()));
            normalized.push_str("run ");
            normalized.push_str(body);
            normalized.push('\n');
            saw_header = true;
            continue;
        }

        let (tokens, ranges) = parser::tokenize_with_ranges(line)
            .map_err(|error| lexical_error(&error, line_number, line))?;
        if begins_with_words(&tokens, &["pipe", "to"]) {
            if saw_take || saw_sink {
                return Err(at(
                    "parse.syntax",
                    1,
                    "native pipeline stages must precede `take` and the final sink",
                ));
            }
            normalized.push_str(line);
            normalized.push('\n');
            continue;
        }
        if begins_with_words(&tokens, &["take"]) {
            if saw_take {
                return Err(at("parse.syntax", 1, "`take` may appear only once"));
            }
            if saw_sink {
                return Err(at(
                    "parse.syntax",
                    1,
                    "statements cannot follow the final sink",
                ));
            }
            line_limit = Some(
                parse_line_limit(line, &tokens, &ranges)
                    .map_err(|error| error.within(line_number))?,
            );
            saw_take = true;
            normalized.push('\n');
            continue;
        }
        if begins_with_words(&tokens, &["show"]) {
            if !is_show_each_line(&tokens) {
                return Err(at(
                    "parse.syntax",
                    1,
                    "expected final sink `show each line`",
                ));
            }
            if saw_sink {
                return Err(at("parse.syntax", 1, "the final sink may appear only once"));
            }
            saw_sink = true;
            normalized.push('\n');
            continue;
        }
        if saw_sink {
            return Err(at(
                "parse.syntax",
                1,
                "statements cannot follow the final sink",
            ));
        }
        return Err(at(
            "parse.syntax",
            1,
            "expected `pipe to <applet>`, `take the first <count> lines`, or final sink `show each line`",
        ));
    }

    let Some((header_line, body_offset, header_text)) = header else {
        return Err(DetailedError::new(
            "parse.syntax",
            first_statement.unwrap_or(1),
            None,
            "stream program requires `stream from <applet>`".to_owned(),
        ));
    };
    if !saw_sink {
        return Err(DetailedError::new(
            "parse.syntax",
            last_statement.unwrap_or(header_line),
            None,
            "stream program requires final sink `show each line`".to_owned(),
        ));
    }
    let pipeline = parser::parse(&normalized).map_err(|error| {
        native_pipeline_error(
            &error,
            &normalized,
            (header_line, body_offset, header_text.as_str()),
        )
    })?;
    Ok(StreamProgram {
        pipeline,
        line_limit,
    })
}

/// Validate the underlying finite native pipeline and stream-only restrictions.
/// Validation performs no applet launches or output writes.
pub fn validate(stream: &StreamProgram, registry: &Registry) -> Result<()> {
    validate_with_inputs(stream, registry, Default::default())
}

pub(crate) fn validate_with_inputs(
    stream: &StreamProgram,
    registry: &Registry,
    names: std::collections::BTreeSet<String>,
) -> Result<()> {
    if stream.line_limit == Some(0) {
        bail!("line count must be greater than zero");
    }
    runtime::validate_with_inputs(&stream.pipeline, registry, names, false)?;
    if stream.pipeline.trigger.is_some() || stream.pipeline.steps.is_empty() {
        bail!("stream requires a nonempty finite native pipeline");
    }

    for (index, step) in stream.pipeline.steps.iter().enumerate() {
        let StepKind::Invoke { call, connection } = &step.kind else {
            bail!(
                "line {}: stream pipelines contain only native applet invocations",
                step.line
            );
        };
        let expected = if index == 0 {
            Connection::Start
        } else {
            Connection::Pipe
        };
        if *connection != expected {
            bail!(
                "line {}: stream uses `run` for its source and `pipe to` for every later stage",
                step.line
            );
        }
        if call.binding.is_some() || call.status_binding.is_some() || call.cache {
            bail!(
                "line {}: stream applets cannot bind output/status or cache finite results",
                step.line
            );
        }
        let manifest = registry
            .resolve_call(call)
            .with_context(|| format!("line {}", step.line))?;
        if manifest.builtin.is_some() {
            bail!(
                "line {}: stream pipelines require external native applets",
                step.line
            );
        }
        if index > 0 && !manifest.accepts_stdin {
            bail!(
                "line {}: applet {:?} does not accept piped input",
                step.line,
                manifest.name
            );
        }
    }
    Ok(())
}

/// A tokenizer failure on the statement at `line_number`. The line is the outer layer,
/// as the tokenizer's `with_context` made it.
fn lexical_error(error: &anyhow::Error, line_number: usize, text: &str) -> DetailedError {
    let column = error
        .downcast_ref::<parser::LocatedParseError>()
        .and_then(parser::LocatedParseError::source_range)
        .map(|range| column_of(text, range.start));
    let layer = format!("line {line_number}");
    DetailedError {
        code: "parse.lexical",
        line: line_number,
        column,
        message: format!("{layer}: {error:#}"),
        outer: Some(layer),
    }
}

/// A finite-parser failure on the normalized pipeline. Normalization keeps one output
/// line per source line, so the parser's `line N:` is the source line. The header line
/// reads `run <applet>` there, so its columns are mapped back to the source text.
fn native_pipeline_error(
    error: &anyhow::Error,
    normalized: &str,
    (header_line, body_offset, header_text): (usize, usize, &str),
) -> DetailedError {
    let text = format!("{error:#}");
    let line = message_line(&text).unwrap_or(header_line);
    let normalized_line = normalized.lines().nth(line.saturating_sub(1)).unwrap_or("");
    let lexical = error
        .downcast_ref::<parser::LocatedParseError>()
        .and_then(parser::LocatedParseError::source_range);
    let column = lexical.and_then(|range| {
        if line == header_line {
            range
                .start
                .checked_sub("run ".len())
                .map(|offset| column_of(header_text, offset + body_offset))
        } else {
            Some(column_of(normalized_line, range.start))
        }
    });
    let code = if lexical.is_some() {
        "parse.lexical"
    } else {
        code_for(&text)
    };
    let layer = "parsing stream native pipeline";
    DetailedError {
        code,
        line,
        column,
        message: format!("{layer}: {text}"),
        outer: Some(layer.to_owned()),
    }
}

/// The stable code for a finite-parser message that is not a located token error.
fn code_for(message: &str) -> &'static str {
    if message.contains("tabs are not allowed for indentation")
        || message.contains("inconsistent indentation")
    {
        "parse.indentation"
    } else if message.contains("unsupported control") || message.contains("control nesting") {
        "parse.control"
    } else {
        "parse.syntax"
    }
}

/// The 1-based character column of byte `offset` in `text`.
fn column_of(text: &str, offset: usize) -> usize {
    text.get(..offset)
        .map_or(offset + 1, |prefix| prefix.chars().count() + 1)
}

/// The number after the first `line N:` in a message, as the finite parser writes it.
fn message_line(message: &str) -> Option<usize> {
    message.match_indices("line ").find_map(|(index, _)| {
        let rest = &message[index + "line ".len()..];
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        (digits > 0 && rest[digits..].starts_with(':'))
            .then(|| rest[..digits].parse().ok())
            .flatten()
    })
}

fn stream_header_body(line: &str) -> Option<&str> {
    let after_stream = line.strip_prefix("stream")?;
    let after_stream = after_stream.trim_start();
    if after_stream.len() == line.len() - "stream".len() {
        return None;
    }
    let after_from = after_stream.strip_prefix("from")?;
    if after_from
        .chars()
        .next()
        .is_some_and(|character| !character.is_whitespace())
    {
        return None;
    }
    Some(after_from.trim_start())
}

fn begins_with_words(tokens: &[Token<'_>], expected: &[&str]) -> bool {
    tokens.len() >= expected.len()
        && expected.iter().enumerate().all(|(index, word)| {
            matches!(tokens.get(index), Some(Token::Word(actual)) if *actual == *word)
        })
}

fn is_show_each_line(tokens: &[Token<'_>]) -> bool {
    tokens.len() == 3 && begins_with_words(tokens, &["show", "each", "line"])
}

/// `take the first <count> lines`. Errors name the token that failed and carry no line.
fn parse_line_limit(
    line: &str,
    tokens: &[Token<'_>],
    ranges: &[Range<usize>],
) -> Result<usize, DetailedError> {
    let column = |index: usize| ranges.get(index).map(|range| column_of(line, range.start));
    let shape = |index: usize| {
        DetailedError::new(
            "parse.syntax",
            0,
            column(index),
            "expected `take the first <positive integer> lines`".to_owned(),
        )
    };
    if tokens.len() != 5 || !begins_with_words(tokens, &["take", "the", "first"]) {
        return Err(shape(0));
    }
    if !matches!(tokens.get(4), Some(Token::Word("line" | "lines"))) {
        return Err(shape(4));
    }
    let Some(Token::Word(count)) = tokens.get(3) else {
        return Err(DetailedError::new(
            "parse.syntax",
            0,
            column(3),
            "line count must be a positive integer".to_owned(),
        ));
    };
    let count = count.parse::<usize>().map_err(|error| {
        let code = if *error.kind() == IntErrorKind::PosOverflow {
            "check.range"
        } else {
            "parse.syntax"
        };
        DetailedError::new(
            code,
            0,
            column(3),
            "line count must be a positive integer in range".to_owned(),
        )
    })?;
    if count == 0 {
        return Err(DetailedError::new(
            "check.range",
            0,
            column(3),
            "line count must be greater than zero".to_owned(),
        ));
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::StepKind;

    #[test]
    fn recognizes_a_stream_after_comments_and_blank_lines() {
        assert!(is_stream(
            "# a comment\n  \nstream from tail with path \"events.txt\""
        ));
        assert!(!is_stream("# a comment\nrun cat"));
        assert!(!is_stream("streaming from tail"));
    }

    #[test]
    fn parses_source_pipeline_limit_and_terminal_sink_without_changing_invocations() {
        let stream = parse(
            "# persistent pipeline\n\nstream from tail with flags [\"-f\"] and path \"events.txt\"\npipe to tr with from \"[:lower:]\" and to \"[:upper:]\"\ntake the first 3 lines\nshow each line\n",
        )
        .unwrap();
        assert_eq!(stream.line_limit, Some(3));
        assert_eq!(stream.pipeline.steps.len(), 2);
        assert_eq!(stream.pipeline.steps[0].line, 3);
        assert_eq!(stream.pipeline.steps[1].line, 4);
        let StepKind::Invoke { call, connection } = &stream.pipeline.steps[0].kind else {
            panic!("stream source was not parsed as an invocation");
        };
        assert_eq!(call.operation, "tail");
        assert_eq!(*connection, Connection::Start);
        assert_eq!(
            call.parameters.get("flags"),
            Some(&crate::ast::Value::List(vec![crate::ast::Value::String(
                "-f".into()
            ),]))
        );
        let StepKind::Invoke { call, connection } = &stream.pipeline.steps[1].kind else {
            panic!("stream transform was not parsed as an invocation");
        };
        assert_eq!(call.operation, "tr");
        assert_eq!(*connection, Connection::Pipe);
    }

    #[test]
    fn requires_one_final_sink_and_a_positive_optional_limit() {
        for source in [
            "stream from tail\n",
            "stream from tail\nshow each line\ntake the first 2 lines\n",
            "stream from tail\ntake the first 0 lines\nshow each line\n",
            "stream from tail\ntake the first nope lines\nshow each line\n",
            "stream from tail\nshow each line\nshow each line\n",
            "stream from tail\ntake the first 2 lines\npipe to tr\nshow each line\n",
        ] {
            assert!(parse(source).is_err(), "accepted invalid stream:\n{source}");
        }
    }
}
