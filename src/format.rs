//! The canonical program layout behind `execute fmt` (#345).
//!
//! Formatting is line-level. Statements are re-indented from the parser's own block rule
//! (a body is the run of following statements indented deeper than its opener), trailing
//! whitespace is removed, blank runs collapse to one line, and line endings become LF.
//! The text inside a statement is never rewritten, so string literals, raw block strings
//! and comments keep their bytes. Every result is checked before it is returned: the input
//! and the output must both parse, and they must describe the same program with source
//! line numbers ignored. A layout bug therefore yields an error, never a changed program.

use crate::ast::{InputDeclaration, Program, Step, StepKind, Trigger};
use crate::parser::{logical_lines, parse, strip_comment};
use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

/// Why a program was not formatted. Text is returned only when the output keeps the
/// program's meaning, so every error means no text at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FormatError {
    /// The input does not parse. An invalid program is never reshaped.
    Input(String),
    /// The formatted text does not parse. A layout bug.
    Output(String),
    /// The formatted text parses to a different program. A layout bug.
    MeaningChanged,
}

impl fmt::Display for FormatError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Input(message) => write!(formatter, "program does not parse: {message}"),
            Self::Output(message) => {
                write!(formatter, "formatted program does not parse: {message}")
            }
            Self::MeaningChanged => {
                formatter.write_str("formatted program would change the program's meaning")
            }
        }
    }
}

impl std::error::Error for FormatError {}

/// Formats a program into the canonical layout, after proving the result has the same
/// meaning as the input.
pub fn format_program(source: &str) -> Result<String, FormatError> {
    format_with(source, canonical_layout)
}

/// Runs a layout function and returns its text only if it passes the meaning check.
fn format_with(
    source: &str,
    layout: impl Fn(&str) -> Result<String, FormatError>,
) -> Result<String, FormatError> {
    let original = parse(source).map_err(|error| FormatError::Input(format!("{error:#}")))?;
    let output = layout(source)?;
    let reparsed = parse(&output).map_err(|error| FormatError::Output(format!("{error:#}")))?;
    if without_spans(original) != without_spans(reparsed) {
        return Err(FormatError::MeaningChanged);
    }
    Ok(output)
}

/// Spaces per block level.
const INDENT_WIDTH: usize = 2;

/// One physical line after classification.
enum Row<'source> {
    Blank,
    /// A comment-only line, trimmed. Its indent follows the next code line.
    Comment(&'source str),
    /// A statement's first line, with its indent already chosen.
    Code {
        indent: usize,
        text: &'source str,
    },
    /// Raw block string lines: the body and its closing `"""` line, kept byte for byte.
    Verbatim(&'source str),
}

/// Lays out the text without checking its meaning. The caller checks the result.
fn canonical_layout(source: &str) -> Result<String, FormatError> {
    let source = source.strip_prefix('\u{feff}').unwrap_or(source);
    let statements =
        logical_lines(source).map_err(|error| FormatError::Input(format!("{error:#}")))?;
    let depths = block_depths(&statements);

    let physical: Vec<&str> = source.lines().collect();
    let mut rows = Vec::with_capacity(physical.len());
    let mut owner_depth = 0;
    let mut index = 0;
    while index < physical.len() {
        let line = physical[index];
        let number = index + 1;
        index += 1;
        if line.trim().is_empty() {
            rows.push(Row::Blank);
            continue;
        }
        let code = strip_comment(line);
        if code.trim().is_empty() {
            // A comment line keeps its bytes; only the leading layout whitespace changes.
            rows.push(Row::Comment(line.trim_start()));
            continue;
        }
        // A line the statement splitter started is a statement at its block depth. Any
        // other code line continues the bracketed statement above it.
        let indent = match depths.get(&number) {
            Some(&depth) => {
                owner_depth = depth;
                depth * INDENT_WIDTH
            }
            None => (owner_depth + 1) * INDENT_WIDTH,
        };
        // A line with a trailing comment keeps its bytes after the indent, so the comment's
        // own whitespace survives. Only a line without a comment loses trailing whitespace.
        let body = line.trim_start_matches(' ');
        let text = if code.len() < line.len() {
            body
        } else {
            body.trim_end()
        };
        rows.push(Row::Code { indent, text });
        // A line ending in ` """` opens a raw block that runs to a line holding only `"""`.
        // The parser reads that block verbatim, so its lines must not change at all.
        if code.trim_end().ends_with(" \"\"\"") {
            while index < physical.len() {
                let body = physical[index];
                index += 1;
                rows.push(Row::Verbatim(body));
                if body.trim() == "\"\"\"" {
                    break;
                }
            }
        }
    }

    // A comment takes the indent of the next code line, so resolve those from the end.
    let mut comment_indents = vec![0; rows.len()];
    let mut next_indent = 0;
    for (position, row) in rows.iter().enumerate().rev() {
        match row {
            Row::Code { indent, .. } => next_indent = *indent,
            Row::Comment(_) => comment_indents[position] = next_indent,
            Row::Blank | Row::Verbatim(_) => {}
        }
    }

    let mut output = String::new();
    let mut started = false;
    let mut pending_blank = false;
    for (position, row) in rows.iter().enumerate() {
        let (indent, text) = match row {
            Row::Blank => {
                pending_blank = started;
                continue;
            }
            Row::Comment(text) => (comment_indents[position], *text),
            Row::Code { indent, text } => (*indent, *text),
            Row::Verbatim(text) => {
                if pending_blank {
                    output.push('\n');
                    pending_blank = false;
                }
                started = true;
                output.push_str(text);
                output.push('\n');
                continue;
            }
        };
        if pending_blank {
            output.push('\n');
            pending_blank = false;
        }
        started = true;
        output.push_str(&" ".repeat(indent));
        output.push_str(text);
        output.push('\n');
    }
    Ok(output)
}

/// The block depth of each statement, keyed by its first physical line number. The
/// parser opens a body for each following statement indented deeper than its opener, so
/// a stack of the indents still open gives the depth.
fn block_depths(statements: &[crate::parser::LogicalLine]) -> HashMap<usize, usize> {
    let mut depths = HashMap::with_capacity(statements.len());
    let mut open: Vec<usize> = Vec::new();
    for statement in statements {
        while open
            .last()
            .is_some_and(|&indent| indent >= statement.indent)
        {
            open.pop();
        }
        depths.insert(statement.number, open.len());
        open.push(statement.indent);
    }
    depths
}

/// Diagnostic for the corpus test: the first line where the span-free renderings of the
/// input and of its unchecked layout differ, or `None` when they agree or either side
/// does not parse.
#[doc(hidden)]
pub fn first_meaning_difference(source: &str) -> Option<String> {
    let original = parse(source).ok()?;
    let layout = canonical_layout(source).ok()?;
    let formatted = parse(&layout).ok()?;
    let before = format!("{:#?}", without_spans(original));
    let after = format!("{:#?}", without_spans(formatted));
    if before == after {
        return None;
    }
    let mut left = before.lines();
    let mut right = after.lines();
    let mut number = 1;
    loop {
        match (left.next(), right.next()) {
            (Some(a), Some(b)) if a == b => number += 1,
            (a, b) => {
                return Some(format!(
                    "first difference at span-free line {number}: input {:?}, formatted {:?}",
                    a.unwrap_or("<end>"),
                    b.unwrap_or("<end>")
                ))
            }
        }
    }
}

/// The program with every source line number cleared. Two programs that differ only in
/// where their statements sit compare equal.
fn without_spans(mut program: Program) -> Program {
    clear_inputs(&mut program.inputs);
    clear_steps(&mut program.steps);
    if let Some(trigger) = program.trigger.as_mut() {
        match trigger {
            Trigger::Every { line, .. }
            | Trigger::Cron { line, .. }
            | Trigger::Listen { line, .. } => *line = 0,
        }
    }
    if !program.definitions.is_empty() {
        let mut definitions = (*program.definitions).clone();
        for definition in definitions.values_mut() {
            definition.line = 0;
            clear_inputs(&mut definition.inputs);
            clear_steps(&mut definition.steps);
        }
        program.definitions = Arc::new(definitions);
    }
    program
}

fn clear_inputs(inputs: &mut [InputDeclaration]) {
    for input in inputs {
        input.line = 0;
    }
}

/// The parser names an unlabelled same-file program call `CallProgram@<line>` (see
/// `parser/reusable.rs`) and prefixes its expanded body's labels with that name. The line
/// is a position that formatting may move, so it is dropped here. User labels cannot
/// contain `@` (the parser accepts only letters, digits, `_` and `-`), so this touches
/// generated names only.
fn without_generated_lines(label: &str) -> String {
    const MARK: &str = "CallProgram@";
    let mut normalised = String::with_capacity(label.len());
    let mut rest = label;
    while let Some(position) = rest.find(MARK) {
        let (before, after) = rest.split_at(position + MARK.len());
        normalised.push_str(before);
        let digits = after.bytes().take_while(u8::is_ascii_digit).count();
        rest = &after[digits..];
    }
    normalised.push_str(rest);
    normalised
}

fn clear_steps(steps: &mut [Step]) {
    for step in steps {
        step.line = 0;
        if let Some(label) = step.label.as_mut() {
            let normalised = without_generated_lines(label);
            *label = normalised;
        }
        match &mut step.kind {
            StepKind::CallProgram { inputs, .. } => clear_inputs(inputs),
            StepKind::SpawnJob { stages, .. } => {
                for stage in stages {
                    stage.line = 0;
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn formatted(source: &str) -> String {
        match format_program(source) {
            Ok(text) => text,
            Err(error) => panic!("expected formatting to succeed: {error}"),
        }
    }

    #[test]
    fn indents_block_bodies_from_the_parsed_structure() {
        let source = "accept live as boolean with default false\n\
                      if live is true\n\
                      \x20\x20\x20\x20run printf with message \"yes\"\n\
                      otherwise\n\
                      \x20\x20run printf with message \"no\"\n";
        assert_eq!(
            formatted(source),
            "accept live as boolean with default false\n\
             if live is true\n\
             \x20\x20run printf with message \"yes\"\n\
             otherwise\n\
             \x20\x20run printf with message \"no\"\n"
        );
    }

    #[test]
    fn strips_trailing_whitespace_collapses_blank_runs_and_normalises_line_endings() {
        let source = "run printf with message \"a\"   \r\n\r\n\r\n\r\nrun printf with message \"b\"\t \r\n\r\n\r\n";
        assert_eq!(
            formatted(source),
            "run printf with message \"a\"\n\nrun printf with message \"b\"\n"
        );
    }

    #[test]
    fn drops_leading_blank_lines_and_adds_one_final_newline() {
        assert_eq!(
            formatted("\n\n  \nrun printf with message \"a\""),
            "run printf with message \"a\"\n"
        );
    }

    #[test]
    fn preserves_comments_and_string_contents() {
        let source = "    # keep   this comment  \n\
                      run printf with message \"x # not a comment   \"   # trailing note  \n";
        assert_eq!(
            formatted(source),
            "# keep   this comment  \n\
             run printf with message \"x # not a comment   \"   # trailing note  \n"
        );
    }

    #[test]
    fn same_file_call_labels_do_not_count_as_meaning() {
        // The unlabelled call is named `CallProgram@<line>` by the parser. Collapsing the
        // blank lines moves the call, which must not be reported as a meaning change.
        let source = "define a program named lines\n\
                      \x20\x20accept path as text\n\
                      \x20\x20run printf with message \"hi\"\n\
                      \n\
                      \n\
                      accept first as text\n\
                      \n\
                      \n\
                      run program lines with path first as first_count\n";
        assert_eq!(
            formatted(source),
            "define a program named lines\n\
             \x20\x20accept path as text\n\
             \x20\x20run printf with message \"hi\"\n\
             \n\
             accept first as text\n\
             \n\
             run program lines with path first as first_count\n"
        );
    }

    #[test]
    fn generated_call_labels_drop_only_their_line() {
        assert_eq!(without_generated_lines("CallProgram@12"), "CallProgram@");
        assert_eq!(
            without_generated_lines("CallProgram@7::CallProgram@19::fetch"),
            "CallProgram@::CallProgram@::fetch"
        );
        assert_eq!(without_generated_lines("fetch-data"), "fetch-data");
    }

    #[test]
    fn raw_block_lines_are_kept_byte_for_byte() {
        let source = "accept live as boolean with default false\n\
                      if live is true\n\
                      \x20\x20\x20\x20run printf with message \"\"\"\n\
                      \x20\x20\x20keep   \n\
                      \n\
                      \x20\x20trailing   \n\
                      \"\"\"\n";
        assert_eq!(
            formatted(source),
            "accept live as boolean with default false\n\
             if live is true\n\
             \x20\x20run printf with message \"\"\"\n\
             \x20\x20\x20keep   \n\
             \n\
             \x20\x20trailing   \n\
             \"\"\"\n"
        );
    }

    #[test]
    fn formatting_is_idempotent() {
        let source = "# head\n\n\nif live is true\n        run printf with message \"a\"   \n      \n\notherwise\n\n\n  run printf with message \"b\"";
        let once = formatted(source);
        assert_eq!(formatted(&once), once);
    }

    #[test]
    fn line_numbers_do_not_count_as_meaning() {
        let spaced = parse("run printf with message \"a\"\n\n\nrun printf with message \"b\"\n");
        let tight = parse("run printf with message \"a\"\nrun printf with message \"b\"\n");
        let (Ok(spaced), Ok(tight)) = (spaced, tight) else {
            panic!("both programs should parse");
        };
        assert_ne!(spaced.steps[1].line, tight.steps[1].line);
        assert_eq!(without_spans(spaced), without_spans(tight));
    }

    #[test]
    fn a_layout_that_changes_meaning_returns_no_text() {
        let result = format_with("run printf with message \"a\"\n", |source| {
            Ok(source.replace("printf", "cat"))
        });
        assert_eq!(result, Err(FormatError::MeaningChanged));
    }

    #[test]
    fn a_layout_that_breaks_parsing_returns_no_text() {
        let result = format_with("run printf with message \"a\"\n", |_| {
            Ok("run printf with message \"unterminated\n".to_owned())
        });
        assert!(matches!(result, Err(FormatError::Output(_))), "{result:?}");
    }

    #[test]
    fn an_invalid_input_is_refused_not_reshaped() {
        let result = format_program("run printf with values [\"a\"\n");
        assert!(matches!(result, Err(FormatError::Input(_))), "{result:?}");
    }
}
