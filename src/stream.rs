//! Syntax and validation for a persistent native pipeline with a line sink.
use crate::ast::{Connection, Program, StepKind};
use crate::manifest::Registry;
use crate::{parser, runtime};
use anyhow::{anyhow, bail, Context, Result};
use parser::Token;

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
    let mut normalized = String::new();
    let mut saw_header = false;
    let mut saw_sink = false;
    let mut saw_take = false;
    let mut line_limit = None;

    for (index, raw_line) in source.lines().enumerate() {
        let line_number = index + 1;
        let uncommented = parser::strip_comment(raw_line);
        let line = uncommented.trim();
        if line.is_empty() {
            normalized.push('\n');
            continue;
        }
        if uncommented.trim_start().len() != uncommented.len() {
            bail!("line {line_number}: stream statements must not be indented");
        }

        if !saw_header {
            if parser::is_input_header(line) {
                normalized.push_str(line);
                normalized.push('\n');
                continue;
            }
            let body = stream_header_body(line)
                .ok_or_else(|| anyhow!("line {line_number}: expected `stream from <applet>`"))?;
            if body.is_empty() {
                bail!("line {line_number}: stream source requires an applet phrase");
            }
            normalized.push_str("run ");
            normalized.push_str(body);
            normalized.push('\n');
            saw_header = true;
            continue;
        }

        let tokens = parser::tokenize(line).with_context(|| format!("line {line_number}"))?;
        if begins_with_words(&tokens, &["pipe", "to"]) {
            if saw_take || saw_sink {
                bail!("line {line_number}: native pipeline stages must precede `take` and the final sink");
            }
            normalized.push_str(line);
            normalized.push('\n');
            continue;
        }
        if begins_with_words(&tokens, &["take"]) {
            if saw_take {
                bail!("line {line_number}: `take` may appear only once");
            }
            if saw_sink {
                bail!("line {line_number}: statements cannot follow the final sink");
            }
            line_limit =
                Some(parse_line_limit(&tokens).with_context(|| format!("line {line_number}"))?);
            saw_take = true;
            normalized.push('\n');
            continue;
        }
        if begins_with_words(&tokens, &["show"]) {
            if !is_show_each_line(&tokens) {
                bail!("line {line_number}: expected final sink `show each line`");
            }
            if saw_sink {
                bail!("line {line_number}: the final sink may appear only once");
            }
            saw_sink = true;
            normalized.push('\n');
            continue;
        }
        if saw_sink {
            bail!("line {line_number}: statements cannot follow the final sink");
        }
        bail!("line {line_number}: expected `pipe to <applet>`, `take the first <count> lines`, or final sink `show each line`");
    }

    if !saw_header {
        bail!("stream program requires `stream from <applet>`");
    }
    if !saw_sink {
        bail!("stream program requires final sink `show each line`");
    }
    let pipeline = parser::parse(&normalized).context("parsing stream native pipeline")?;
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

fn parse_line_limit(tokens: &[Token<'_>]) -> Result<usize> {
    if tokens.len() != 5 || !begins_with_words(tokens, &["take", "the", "first"]) {
        bail!("expected `take the first <positive integer> lines`");
    }
    if !matches!(tokens.get(4), Some(Token::Word("line" | "lines"))) {
        bail!("expected `take the first <positive integer> lines`");
    }
    let Some(Token::Word(count)) = tokens.get(3) else {
        bail!("line count must be a positive integer");
    };
    let count = count
        .parse::<usize>()
        .map_err(|_| anyhow!("line count must be a positive integer in range"))?;
    if count == 0 {
        bail!("line count must be greater than zero");
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
