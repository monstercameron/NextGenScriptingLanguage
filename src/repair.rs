//! Structured repair of a checkpointed program: `execute repair CHECKPOINT --op ...`.
//!
//! A repair is one operation from a closed set, applied as a text edit to the program
//! file the checkpoint was started from. The edit may only touch the failed or
//! unexecuted suffix, which starts at the first top-level step no completed node
//! covers. Lines before that point are byte-identical after the edit, so the completed
//! nodes keep their fingerprints and `execute resume` accepts the repaired program. The
//! result must pass the same static checks as `execute check`. Each applied repair is
//! appended to `repairs.jsonl` beside the journal, which is never written.

use crate::manifest::Registry;
use crate::parser;
use anyhow::{anyhow, bail, Context, Result};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

pub const PROTOCOL: &str = "execute.repair/1";
pub const REPAIRS_FILE: &str = "repairs.jsonl";
pub const REPAIR_COPIES_DIR: &str = "repairs";

/// The closed set of repairs. Each names a 1-based physical line of the program.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Operation {
    /// Replace the value of an existing parameter on one statement line.
    SetParameter {
        line: usize,
        name: String,
        value: String,
    },
    /// Replace the statement on one line, keeping its indentation.
    ReplaceCommand { line: usize, statement: String },
    /// Insert a statement before a line, at that line's indentation.
    InsertBefore { line: usize, statement: String },
    /// Remove one single-line statement.
    Remove { line: usize },
    /// Ask for a retry around a statement. The grammar has no retry statement, so
    /// this operation is parsed and refused with that reason.
    AddRetry { line: usize, count: u32 },
}

impl Operation {
    /// Parse the operation text, for example `set-parameter 3 message "x"`.
    pub fn parse(text: &str) -> Result<Self> {
        let text = text.trim();
        let (verb, rest) = text
            .split_once(char::is_whitespace)
            .map(|(verb, rest)| (verb, rest.trim()))
            .unwrap_or((text, ""));
        match verb {
            "set-parameter" => {
                let (line, rest) = split_line(rest, verb)?;
                let (name, value) = rest
                    .split_once(char::is_whitespace)
                    .map(|(name, value)| (name, value.trim()))
                    .ok_or_else(|| anyhow!("set-parameter expects LINE NAME VALUE"))?;
                if name.is_empty() || value.is_empty() {
                    bail!("set-parameter expects LINE NAME VALUE");
                }
                Ok(Self::SetParameter {
                    line,
                    name: name.to_owned(),
                    value: value.to_owned(),
                })
            }
            "replace-command" => {
                let (line, statement) = split_line(rest, verb)?;
                if statement.is_empty() {
                    bail!("replace-command expects LINE STATEMENT");
                }
                Ok(Self::ReplaceCommand {
                    line,
                    statement: statement.to_owned(),
                })
            }
            "insert-before" => {
                let (line, statement) = split_line(rest, verb)?;
                if statement.is_empty() {
                    bail!("insert-before expects LINE STATEMENT");
                }
                Ok(Self::InsertBefore {
                    line,
                    statement: statement.to_owned(),
                })
            }
            "remove" => {
                let (line, extra) = split_line(rest, verb)?;
                if !extra.is_empty() {
                    bail!("remove expects only LINE");
                }
                Ok(Self::Remove { line })
            }
            "add-retry" => {
                let (line, count) = split_line(rest, verb)?;
                let count = count
                    .parse::<u32>()
                    .ok()
                    .filter(|count| *count >= 1)
                    .ok_or_else(|| anyhow!("add-retry expects LINE N with N at least 1"))?;
                Ok(Self::AddRetry { line, count })
            }
            "" => bail!(
                "a repair operation is required: set-parameter, replace-command, insert-before, remove or add-retry"
            ),
            other => bail!(
                "unknown repair operation {other:?}; expected set-parameter, replace-command, insert-before, remove or add-retry"
            ),
        }
    }

    fn line(&self) -> usize {
        match self {
            Self::SetParameter { line, .. }
            | Self::ReplaceCommand { line, .. }
            | Self::InsertBefore { line, .. }
            | Self::Remove { line }
            | Self::AddRetry { line, .. } => *line,
        }
    }

    fn kind(&self) -> &'static str {
        match self {
            Self::SetParameter { .. } => "set-parameter",
            Self::ReplaceCommand { .. } => "replace-command",
            Self::InsertBefore { .. } => "insert-before",
            Self::Remove { .. } => "remove",
            Self::AddRetry { .. } => "add-retry",
        }
    }
}

fn split_line<'a>(rest: &'a str, verb: &str) -> Result<(usize, &'a str)> {
    let (line, tail) = rest
        .split_once(char::is_whitespace)
        .map(|(line, tail)| (line, tail.trim()))
        .unwrap_or((rest, ""));
    let line = line
        .parse::<usize>()
        .ok()
        .filter(|line| *line >= 1)
        .ok_or_else(|| anyhow!("{verb} expects a line number from 1"))?;
    Ok((line, tail))
}

/// A repair that has been applied to text and checked, but not yet written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub operation: Operation,
    pub before: String,
    pub after: String,
    /// The first unexecuted top-level statement's line. Earlier lines are untouched.
    pub boundary_line: usize,
    pub diff: String,
}

/// Apply one operation to program text, restricted to lines from `boundary_line`.
pub fn apply(
    source: &str,
    operation: &Operation,
    boundary_line: usize,
    registry: &Registry,
) -> Result<Plan> {
    let line = operation.line();
    if line < boundary_line {
        bail!(
            "line {line} is in completed steps; a repair may change only line {boundary_line} onward"
        );
    }
    let mut lines: Vec<String> = source.lines().map(str::to_owned).collect();
    let index = line - 1;
    match operation {
        Operation::AddRetry { .. } => bail!(
            "add-retry is not supported: the grammar has no retry statement. Use --retry-uncertain to retry a checkpoint's in-flight node."
        ),
        Operation::SetParameter { name, value, .. } => {
            let current = lines
                .get(index)
                .ok_or_else(|| anyhow!("line {line} is past the end of the program"))?
                .clone();
            lines[index] = set_parameter_value(&current, name, value)
                .with_context(|| format!("line {line}"))?;
        }
        Operation::ReplaceCommand { statement, .. } => {
            let current = lines
                .get(index)
                .ok_or_else(|| anyhow!("line {line} is past the end of the program"))?
                .clone();
            lines[index] = format!("{}{statement}", indentation(&current));
        }
        Operation::InsertBefore { statement, .. } => {
            let current = lines
                .get(index)
                .ok_or_else(|| anyhow!("line {line} is past the end of the program"))?
                .clone();
            lines.insert(index, format!("{}{statement}", indentation(&current)));
        }
        Operation::Remove { .. } => {
            let current = lines
                .get(index)
                .ok_or_else(|| anyhow!("line {line} is past the end of the program"))?
                .clone();
            if let Some(next) = lines.get(index + 1) {
                if !next.trim().is_empty() && indentation(next) > indentation(&current) {
                    bail!("line {line} opens an indented body; remove the body's lines as their own repairs");
                }
            }
            lines.remove(index);
        }
    }
    let mut after = lines.join("\n");
    if source.ends_with('\n') || !after.is_empty() {
        after.push('\n');
    }
    check(&after, registry)?;
    let diff = line_diff(source, &after);
    Ok(Plan {
        operation: operation.clone(),
        before: source.to_owned(),
        after,
        boundary_line,
        diff,
    })
}

/// The repaired program must parse and pass the static checks `execute check` runs.
fn check(source: &str, registry: &Registry) -> Result<()> {
    parser::parse(source)
        .map_err(|error| anyhow!("the repaired program does not parse: {error:#}"))?;
    let errors = crate::diagnostics::analyze_source(source, registry)
        .into_iter()
        .filter(|diagnostic| diagnostic.severity == crate::diagnostics::DiagnosticSeverity::Error)
        .map(|diagnostic| format!("line {}: {}", diagnostic.span.line, diagnostic.message))
        .collect::<Vec<_>>();
    if !errors.is_empty() {
        bail!("the repaired program does not check: {}", errors.join("; "));
    }
    Ok(())
}

fn indentation(line: &str) -> &str {
    &line[..line.len() - line.trim_start_matches(' ').len()]
}

/// Replace the value token after `name` on one statement line. Only code before a
/// comment is considered, and quoted strings and bracketed lists count as one token.
fn set_parameter_value(line: &str, name: &str, value: &str) -> Result<String> {
    let code_end = parser::strip_comment(line).len();
    let spans = word_spans(&line[..code_end]);
    let position = spans
        .windows(2)
        .position(|pair| &line[pair[0].0..pair[0].1] == name)
        .ok_or_else(|| anyhow!("no parameter {name:?} on this statement"))?;
    let (start, end) = spans[position + 1];
    let mut text = String::with_capacity(line.len() + value.len());
    text.push_str(&line[..start]);
    text.push_str(value);
    text.push_str(&line[end..]);
    Ok(text)
}

/// Byte spans of the words in a statement. Quoted strings (with `\` escapes) and
/// bracketed lists are single words.
fn word_spans(code: &str) -> Vec<(usize, usize)> {
    let bytes = code.as_bytes();
    let mut spans = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index].is_ascii_whitespace() {
            index += 1;
            continue;
        }
        let start = index;
        let mut depth = 0usize;
        let mut quoted = false;
        while index < bytes.len() {
            let byte = bytes[index];
            if quoted {
                match byte {
                    b'\\' => index += 1,
                    b'"' => quoted = false,
                    _ => {}
                }
            } else {
                match byte {
                    b'"' => quoted = true,
                    b'[' => depth += 1,
                    b']' => depth = depth.saturating_sub(1),
                    byte if byte.is_ascii_whitespace() && depth == 0 => break,
                    _ => {}
                }
            }
            index += 1;
        }
        spans.push((start, index.min(bytes.len())));
    }
    spans
}

/// A line diff: removed lines with `-`, added lines with `+`, each with its line
/// number in the side it belongs to. Computed from the longest common subsequence.
pub fn line_diff(before: &str, after: &str) -> String {
    let old: Vec<&str> = before.lines().collect();
    let new: Vec<&str> = after.lines().collect();
    let (rows, cols) = (old.len() + 1, new.len() + 1);
    let mut table = vec![vec![0u32; cols]; rows];
    for i in (0..old.len()).rev() {
        for j in (0..new.len()).rev() {
            table[i][j] = if old[i] == new[j] {
                table[i + 1][j + 1] + 1
            } else {
                table[i + 1][j].max(table[i][j + 1])
            };
        }
    }
    let (mut i, mut j) = (0, 0);
    let mut out = String::new();
    while i < old.len() || j < new.len() {
        if i < old.len() && j < new.len() && old[i] == new[j] {
            i += 1;
            j += 1;
        } else if j < new.len() && (i == old.len() || table[i][j + 1] >= table[i + 1][j]) {
            out.push_str(&format!("+{:>5} | {}\n", j + 1, new[j]));
            j += 1;
        } else {
            out.push_str(&format!("-{:>5} | {}\n", i + 1, old[i]));
            i += 1;
        }
    }
    out
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[derive(Serialize)]
struct RepairRecord<'a> {
    protocol: &'static str,
    sequence: usize,
    at_unix_ms: u128,
    operation: &'a str,
    kind: &'static str,
    line: usize,
    boundary_line: usize,
    program_path: String,
    before_sha256: String,
    after_sha256: String,
    before_copy: String,
    diff: &'a str,
}

/// Write an applied plan: keep a copy of the program as it was, replace the program
/// file atomically, and append the record to `repairs.jsonl`. Returns the record's
/// sequence number.
pub fn commit(
    checkpoint: &Path,
    program_path: &Path,
    plan: &Plan,
    operation_text: &str,
) -> Result<usize> {
    let before_sha = sha256_hex(plan.before.as_bytes());
    let copies = checkpoint.join(REPAIR_COPIES_DIR);
    fs::create_dir_all(&copies).with_context(|| format!("creating {}", copies.display()))?;
    let copy = copies.join(format!("{before_sha}.program"));
    if !copy.exists() {
        write_private(&copy, plan.before.as_bytes())?;
    }
    let temporary = program_path.with_extension("repair-tmp");
    fs::write(&temporary, plan.after.as_bytes())
        .with_context(|| format!("writing {}", temporary.display()))?;
    fs::rename(&temporary, program_path)
        .with_context(|| format!("replacing {}", program_path.display()))?;

    let log = checkpoint.join(REPAIRS_FILE);
    let sequence = match fs::read_to_string(&log) {
        Ok(text) => text.lines().count() + 1,
        Err(_) => 1,
    };
    let record = RepairRecord {
        protocol: PROTOCOL,
        sequence,
        at_unix_ms: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_millis())
            .unwrap_or(0),
        operation: operation_text,
        kind: plan.operation.kind(),
        line: plan.operation.line(),
        boundary_line: plan.boundary_line,
        program_path: program_path.display().to_string(),
        before_sha256: before_sha,
        after_sha256: sha256_hex(plan.after.as_bytes()),
        before_copy: format!(
            "{REPAIR_COPIES_DIR}/{}.program",
            sha256_hex(plan.before.as_bytes())
        ),
        diff: &plan.diff,
    };
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)
        .with_context(|| format!("opening {}", log.display()))?;
    writeln!(file, "{}", serde_json::to_string(&record)?)?;
    file.sync_all()?;
    Ok(sequence)
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("creating {}", path.display()))?;
    file.write_all(bytes)?;
    Ok(())
}

/// The program path a checkpoint was started from, or an error if it cannot be
/// repaired: only failed or interrupted checkpoints have a suffix to change.
pub fn repairable_program(checkpoint: &Path) -> Result<(PathBuf, usize)> {
    let state = crate::runtime::read_repair_state(checkpoint)?;
    match state.status {
        "failed" | "interrupted" => Ok((state.program_path, state.completed_steps)),
        "completed" => {
            bail!("the checkpoint completed; there is no failed or unexecuted suffix to repair")
        }
        other => bail!(
            "the checkpoint is {other}; repairs apply only to a failed or interrupted checkpoint"
        ),
    }
}

/// The first line of the first top-level step after the completed prefix. Repairs
/// may change this line and everything after it.
pub fn boundary_line(source: &str, completed_steps: usize) -> Result<usize> {
    let program = parser::parse(source).map_err(|error| anyhow!("{error:#}"))?;
    Ok(program
        .steps
        .get(completed_steps)
        .map(|step| step.line)
        .unwrap_or_else(|| source.lines().count() + 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> Registry {
        Registry::from_sources(&[serde_json::json!({
            "name": "fixture.count",
            "binary": "fixture-count",
            "accepts_stdin": false,
            "cacheable": false,
            "parameters": {
                "count": { "type": "integer", "required": true },
                "message": { "type": "string" }
            },
            "arguments": [{ "parameter": "count" }]
        })
        .to_string()])
        .unwrap()
    }

    #[test]
    fn operations_parse_from_the_closed_set() {
        assert_eq!(
            Operation::parse("set-parameter 3 count 4").unwrap(),
            Operation::SetParameter {
                line: 3,
                name: "count".into(),
                value: "4".into()
            }
        );
        assert_eq!(
            Operation::parse("replace-command 2 run \"true\"").unwrap(),
            Operation::ReplaceCommand {
                line: 2,
                statement: "run \"true\"".into()
            }
        );
        assert_eq!(
            Operation::parse("remove 5").unwrap(),
            Operation::Remove { line: 5 }
        );
        assert!(Operation::parse("delete 5").is_err());
        assert!(Operation::parse("remove 0").is_err());
        assert!(Operation::parse("add-retry 2 0").is_err());
        assert!(Operation::parse("").is_err());
    }

    #[test]
    fn set_parameter_replaces_only_the_value_token() {
        let source = "run fixture.count with count 1 and message \"a b\"\n";
        let plan = apply(
            source,
            &Operation::parse("set-parameter 1 message \"new\"").unwrap(),
            1,
            &registry(),
        )
        .unwrap();
        assert_eq!(
            plan.after,
            "run fixture.count with count 1 and message \"new\"\n"
        );
        assert!(plan.diff.contains("+    1 |"));
    }

    #[test]
    fn set_parameter_handles_bracketed_and_quoted_values_and_comments() {
        let source = "run fixture.count with count 1 # note count 9\n";
        let plan = apply(
            source,
            &Operation::parse("set-parameter 1 count 2").unwrap(),
            1,
            &registry(),
        )
        .unwrap();
        assert_eq!(
            plan.after,
            "run fixture.count with count 2 # note count 9\n"
        );
    }

    #[test]
    fn insert_and_remove_keep_indentation_and_respect_the_boundary() {
        let source = "run fixture.count with count 1\nrun fixture.count with count 2\n";
        let inserted = apply(
            source,
            &Operation::parse("insert-before 2 run fixture.count with count 3").unwrap(),
            2,
            &registry(),
        )
        .unwrap();
        assert_eq!(
            inserted.after,
            "run fixture.count with count 1\nrun fixture.count with count 3\nrun fixture.count with count 2\n"
        );
        let removed = apply(
            source,
            &Operation::parse("remove 2").unwrap(),
            2,
            &registry(),
        )
        .unwrap();
        assert_eq!(removed.after, "run fixture.count with count 1\n");
        let refused = apply(
            source,
            &Operation::parse("remove 1").unwrap(),
            2,
            &registry(),
        );
        assert!(refused.unwrap_err().to_string().contains("completed steps"));
    }

    #[test]
    fn repairs_that_do_not_check_are_refused_and_add_retry_says_why() {
        let source = "run fixture.count with count 1\n";
        let invalid = apply(
            source,
            &Operation::parse("replace-command 1 run fixture.count with bogus 1").unwrap(),
            1,
            &registry(),
        );
        assert!(invalid.unwrap_err().to_string().contains("does not check"));
        let retry = apply(
            source,
            &Operation::parse("add-retry 1 2").unwrap(),
            1,
            &registry(),
        );
        assert!(retry
            .unwrap_err()
            .to_string()
            .contains("no retry statement"));
    }

    #[test]
    fn the_diff_marks_removed_and_added_lines() {
        let diff = line_diff("a\nb\nc\n", "a\nx\nc\n");
        assert!(diff.contains("-    2 | b"), "{diff}");
        assert!(diff.contains("+    2 | x"), "{diff}");
        assert!(!diff.contains("| a"), "{diff}");
    }
}
