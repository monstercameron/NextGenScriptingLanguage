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
    /// Add a retry policy to a compatible invocation.
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
    apply_at(source, None, operation, boundary_line, registry)
}

/// Path-aware variant used for programs containing `run program file` calls.
pub fn apply_at(
    source: &str,
    path: Option<&Path>,
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
    let mut lines: Vec<String> = source.split_inclusive('\n').map(str::to_owned).collect();
    if !source.is_empty() && !source.ends_with('\n') {
        // `split_inclusive` already includes the unterminated final line.
    }
    let index = line - 1;
    match operation {
        Operation::AddRetry { count, .. } => {
            let statement = repair_statement(source, line)?;
            let layout = parser::repair_invocation_layout(&statement.text)
                .with_context(|| format!("line {line}"))?;
            if layout.has_retry {
                bail!("line {line} already has a retry policy");
            }
            let mut edited = source.to_owned();
            if let Some(raw) = statement
                .raw_token_spans
                .iter()
                .find(|span| span.end == layout.retry_insert)
            {
                // A raw closing delimiter must occupy its own line, so express the
                // identical value as a quoted literal before inserting the suffix.
                let start = statement.original_start(raw.start)?;
                let end = statement.original_end(raw.end)?;
                edited.replace_range(
                    start..end,
                    &format!(
                        "{} and retry up to {count} times",
                        &statement.text[raw.clone()]
                    ),
                );
            } else {
                let offset = statement.original_end(layout.retry_insert)?;
                edited.insert_str(offset, &format!(" and retry up to {count} times"));
            }
            lines = edited.split_inclusive('\n').map(str::to_owned).collect();
        }
        Operation::SetParameter { name, value, .. } => {
            let statement = repair_statement(source, line)?;
            let layout = parser::repair_invocation_layout(&statement.text)
                .with_context(|| format!("line {line}"))?;
            let span = layout
                .parameters
                .get(name)
                .ok_or_else(|| anyhow!("line {line}: no parameter {name:?} on this statement"))?;
            let start = statement.original_start(span.start)?;
            let end = statement.original_end(span.end)?;
            let mut edited = source.to_owned();
            edited.replace_range(start..end, value);
            lines = edited.split_inclusive('\n').map(str::to_owned).collect();
        }
        Operation::ReplaceCommand { statement, .. } => {
            let current = lines
                .get(index)
                .ok_or_else(|| anyhow!("line {line} is past the end of the program"))?
                .clone();
            let (body, ending) = line_body_and_ending(&current);
            lines[index] = format!("{}{statement}{ending}", indentation(body));
        }
        Operation::InsertBefore { statement, .. } => {
            let current = lines
                .get(index)
                .ok_or_else(|| anyhow!("line {line} is past the end of the program"))?
                .clone();
            let (body, ending) = line_body_and_ending(&current);
            lines.insert(index, format!("{}{statement}{ending}", indentation(body)));
        }
        Operation::Remove { .. } => {
            let current = lines
                .get(index)
                .ok_or_else(|| anyhow!("line {line} is past the end of the program"))?
                .clone();
            if let Some(next) = lines.get(index + 1) {
                let (next, _) = line_body_and_ending(next);
                if !next.trim().is_empty() && indentation(next) > indentation(&current) {
                    bail!("line {line} opens an indented body; remove the body's lines as their own repairs");
                }
            }
            lines.remove(index);
        }
    }
    let after = lines.concat();
    check(&after, path, registry)?;
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
fn check(source: &str, path: Option<&Path>, registry: &Registry) -> Result<()> {
    let errors = crate::diagnostics::analyze_source_at(source, path, registry)
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

fn line_body_and_ending(line: &str) -> (&str, &str) {
    if let Some(body) = line.strip_suffix("\r\n") {
        (body, "\r\n")
    } else if let Some(body) = line.strip_suffix('\n') {
        (body, "\n")
    } else {
        (line, "")
    }
}

/// The parser joins continued lines; retain a byte map back to the original source
/// so grammar-derived value spans can be edited without guessing at word positions.
struct RepairStatement {
    text: String,
    offsets: Vec<usize>,
    raw_token_spans: Vec<std::ops::Range<usize>>,
}

impl RepairStatement {
    fn original_start(&self, offset: usize) -> Result<usize> {
        self.offsets
            .get(offset)
            .copied()
            .context("invalid statement start span")
    }
    fn original_end(&self, offset: usize) -> Result<usize> {
        self.offsets
            .get(offset.checked_sub(1).context("empty statement span")?)
            .map(|offset| offset + 1)
            .context("invalid statement end span")
    }
}

fn repair_statement(source: &str, line: usize) -> Result<RepairStatement> {
    let statements = parser::logical_lines(source)?;
    let index = statements
        .iter()
        .position(|statement| statement.number == line)
        .ok_or_else(|| anyhow!("line {line} is not the start of a statement"))?;
    let statement = &statements[index];
    let end_line = statements
        .get(index + 1)
        .map_or(usize::MAX, |next| next.number);
    let mut text = String::new();
    let mut offsets = Vec::new();
    let mut raw_token_spans = Vec::new();
    let mut physical = Vec::new();
    let mut base = 0;
    for raw in source.split_inclusive('\n') {
        let (body, _) = line_body_and_ending(raw);
        physical.push((body, base));
        base += raw.len();
    }
    let mut index = line - 1;
    while index < physical.len() && index + 1 < end_line {
        let number = index + 1;
        let (body, base) = physical[index];
        index += 1;
        let code = parser::strip_comment(body);
        let code = if number == 1 {
            code.strip_prefix('\u{feff}').unwrap_or(code)
        } else {
            code
        };
        let piece = if number == line {
            code.trim_start_matches(' ')
        } else {
            code.trim()
        };
        if piece.is_empty() {
            continue;
        }
        let piece_start = piece.as_ptr() as usize - body.as_ptr() as usize;
        if number != line {
            text.push(' ');
            offsets.push(base + piece_start);
        }
        let trimmed = piece.trim_end();
        if trimmed.ends_with(" \"\"\"") {
            let prefix = &trimmed[..trimmed.len() - 3];
            text.push_str(prefix);
            offsets.extend((0..prefix.len()).map(|offset| base + piece_start + offset));
            let opener = base + piece_start + prefix.len();
            let mut raw_lines = Vec::new();
            let closer = loop {
                let (raw, raw_base) = *physical
                    .get(index)
                    .context("raw block has no closing delimiter")?;
                index += 1;
                if raw.trim() == "\"\"\"" {
                    let closing_start = raw.trim().as_ptr() as usize - raw.as_ptr() as usize;
                    break raw_base + closing_start + 2;
                }
                raw_lines.push(raw);
            };
            let quoted = serde_json::to_string(&raw_lines.join("\n"))?;
            let raw_start = text.len();
            text.push_str(&quoted);
            for byte in 0..quoted.len() {
                offsets.push(if byte + 1 == quoted.len() {
                    closer
                } else {
                    opener
                });
            }
            raw_token_spans.push(raw_start..text.len());
        } else {
            text.push_str(piece);
            offsets.extend((0..piece.len()).map(|offset| base + piece_start + offset));
        }
    }
    if text != statement.text {
        bail!("line {line}: structured edits cannot map this statement to source spans");
    }
    Ok(RepairStatement {
        text,
        offsets,
        raw_token_spans,
    })
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
    boundary_line_at(source, None, completed_steps)
}

/// Path-aware boundary lookup for programs with file-based reusable calls.
pub fn boundary_line_at(
    source: &str,
    path: Option<&Path>,
    completed_steps: usize,
) -> Result<usize> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let registry = Registry::default();
    let access = crate::program_files::Access {
        registry: &registry,
        cwd: &cwd,
    };
    let program = crate::program_files::parse_at(source, path, Some(access))
        .map_err(|error| anyhow!("{error:#}"))?;
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
    fn set_parameter_targets_the_parameter_not_an_earlier_value() {
        let registry =
            Registry::from_sources(&[
                include_str!("../manifests/core.text_replace.json").to_owned()
            ])
            .unwrap();
        let source = "accept old as text with default \"a\"\naccept text as text with default \"b\"\nrun \"printf\" with arguments [\"start\\n\"]\nrun \"false\"\nrun text.replace with text old and old text and new \"z\" as r\n";
        let plan = apply(
            source,
            &Operation::parse("set-parameter 5 old \"q\"").unwrap(),
            5,
            &registry,
        )
        .unwrap();
        assert!(plan
            .after
            .contains("with text old and old \"q\" and new \"z\" as r"));
        assert!(plan
            .after
            .starts_with(&source[..source.find("run text.replace").unwrap()]));
    }

    #[test]
    fn duplicate_parameter_is_rejected_as_ambiguous() {
        let source = "run fixture.count with count 1 and count 2\n";
        let error = apply(
            source,
            &Operation::parse("set-parameter 1 count 3").unwrap(),
            1,
            &registry(),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("repair.ambiguous"));
    }

    #[test]
    fn set_parameter_handles_names_in_strings_and_multiline_values() {
        let source = "run \"printf\" with arguments [\n  \"arguments\", # retained until replaced\n  \"old\"\n] # trailing comment\n";
        let plan = apply(
            source,
            &Operation::parse("set-parameter 1 arguments [\"new\"]").unwrap(),
            1,
            &Registry::default(),
        )
        .unwrap();
        assert_eq!(
            plan.after,
            "run \"printf\" with arguments [\"new\"] # trailing comment\n"
        );
        let labelled = "tag: run fixture.count with count 1 and message \"count is text\" # note\n";
        let plan = apply(
            labelled,
            &Operation::parse("set-parameter 1 count 2").unwrap(),
            1,
            &registry(),
        )
        .unwrap();
        assert_eq!(
            plan.after,
            "tag: run fixture.count with count 2 and message \"count is text\" # note\n"
        );
    }

    #[test]
    fn edits_around_raw_blocks_preserve_or_replace_the_whole_value() {
        let source = "run fixture.count with count 1 and message \"\"\"\nraw # text\n\"\"\"\n";
        let count = apply(
            source,
            &Operation::parse("set-parameter 1 count 2").unwrap(),
            1,
            &registry(),
        )
        .unwrap();
        assert_eq!(
            count.after,
            "run fixture.count with count 2 and message \"\"\"\nraw # text\n\"\"\"\n"
        );
        let message = apply(
            source,
            &Operation::parse("set-parameter 1 message \"new\"").unwrap(),
            1,
            &registry(),
        )
        .unwrap();
        assert_eq!(
            message.after,
            "run fixture.count with count 1 and message \"new\"\n"
        );
        let retry = apply(
            source,
            &Operation::parse("add-retry 1 2").unwrap(),
            1,
            &registry(),
        )
        .unwrap();
        assert_eq!(
            retry.after,
            "run fixture.count with count 1 and message \"raw # text\" and retry up to 2 times\n"
        );
    }

    #[test]
    fn retry_rewrites_multiline_raw_block_without_changing_its_value() {
        let value = "first \"quoted\"\n雪 and \\slashes";
        let source =
            format!("run fixture.count with count 1 and message \"\"\"\n{value}\n\"\"\"\n");
        let plan = apply(
            &source,
            &Operation::parse("add-retry 1 2").unwrap(),
            1,
            &registry(),
        )
        .unwrap();
        let quoted = serde_json::to_string(value).unwrap();
        assert_eq!(
            plan.after,
            format!(
                "run fixture.count with count 1 and message {quoted} and retry up to 2 times\n"
            )
        );
    }

    #[test]
    fn add_retry_uses_invocation_suffix_location() {
        for (source, expected) in [
            ("run \"printf\" with arguments [\"x\"] as result # comment\n", "run \"printf\" with arguments [\"x\"] as result and retry up to 2 times # comment\n"),
            ("run \"printf\" with arguments [\"x\"] and discard stderr\n", "run \"printf\" with arguments [\"x\"] and discard stderr and retry up to 2 times\n"),
            ("run \"printf\" with arguments [\"x\"] and environment {\"A\": \"b\"}\n", "run \"printf\" with arguments [\"x\"] and retry up to 2 times and environment {\"A\": \"b\"}\n"),
            ("run \"printf\" with arguments [\"x\"] and stderr to \"err\"\n", "run \"printf\" with arguments [\"x\"] and retry up to 2 times and stderr to \"err\"\n"),
        ] {
            let plan = apply(source, &Operation::parse("add-retry 1 2").unwrap(), 1, &Registry::default()).unwrap();
            assert_eq!(plan.after, expected);
        }
        for invalid in [
            "run \"printf\" and cache\n",
            "run \"printf\" and keep status as s\n",
            "run \"printf\" and retry up to 1 times\n",
        ] {
            assert!(
                apply(
                    invalid,
                    &Operation::parse("add-retry 1 2").unwrap(),
                    1,
                    &Registry::default()
                )
                .is_err(),
                "{invalid}"
            );
        }
    }

    #[test]
    fn parser_spans_cover_program_interactive_and_handoff_calls() {
        for source in [
            "run program file \"lib.program\" with value \"old\"",
            "run \"printf\" with arguments [\"old\"] interactively",
            "hand over to \"printf\" with arguments [\"old\"]",
            "launch detached \"printf\" with arguments [\"old\"] and output to \"out\"",
        ] {
            let layout = parser::repair_invocation_layout(source)
                .unwrap_or_else(|error| panic!("{source}: {error:#}"));
            let name = if source.starts_with("run program") {
                "value"
            } else {
                "arguments"
            };
            let value = &source[layout.parameters[name].clone()];
            assert!(value.contains("old"), "{source}: {value}");
        }
    }

    #[test]
    fn repairs_preserve_initial_bom() {
        let source = "\u{feff}run fixture.count with count 1\n";
        let plan = apply(
            source,
            &Operation::parse("set-parameter 1 count 2").unwrap(),
            1,
            &registry(),
        )
        .unwrap();
        assert_eq!(plan.after, "\u{feff}run fixture.count with count 2\n");
    }

    #[test]
    fn add_retry_preserves_each_compatible_external_suffix() {
        for suffix in [
            "and stdin from \"in\"",
            "and stderr to \"err\"",
            "and stderr appended to \"err\"",
            "and stderr merged",
            "and stdout to stderr",
            "and environment {\"A\": \"v\"}",
            "and without environment [\"A\"]",
            "and environment {\"A\": \"v\"} and without environment [\"B\"] and stderr to \"err\"",
        ] {
            let source = format!("run \"printf\" with arguments [\"x\"] {suffix}\n");
            let plan = apply(
                &source,
                &Operation::parse("add-retry 1 2").unwrap(),
                1,
                &Registry::default(),
            )
            .unwrap_or_else(|error| panic!("{source}: {error:#}"));
            assert_eq!(
                plan.after,
                format!("run \"printf\" with arguments [\"x\"] and retry up to 2 times {suffix}\n")
            );
        }
        for suffix in [
            "and cache",
            "and keep status as status",
            "and keep result as result",
            "interactively",
        ] {
            let source = format!("run \"printf\" with arguments [\"x\"] {suffix}\n");
            assert!(
                apply(
                    &source,
                    &Operation::parse("add-retry 1 2").unwrap(),
                    1,
                    &Registry::default()
                )
                .is_err(),
                "{source}"
            );
        }
    }

    #[test]
    fn retry_insertion_checks_compatible_suffix_combinations() {
        let mut checked = 0;
        for binding in ["", " as value"] {
            for discard in ["", " and discard stderr"] {
                for environment in [
                    "",
                    " and environment {\"A\": \"v\"}",
                    " and without environment [\"B\"]",
                    " and environment {\"A\": \"v\"} and without environment [\"B\"]",
                    " and without environment [\"B\"] and environment {\"A\": \"v\"}",
                ] {
                    for input in ["", " and stdin from \"in\""] {
                        for output in [
                            "",
                            " and stderr to \"err\"",
                            " and stderr appended to \"err\"",
                            " and stderr merged",
                            " and stdout to stderr",
                        ] {
                            for order in [
                                [0, 1, 2],
                                [0, 2, 1],
                                [1, 0, 2],
                                [1, 2, 0],
                                [2, 0, 1],
                                [2, 1, 0],
                            ] {
                                for external_position in 0..3 {
                                    let parts = [environment, input, output];
                                    let external =
                                        order.iter().map(|index| parts[*index]).collect::<String>();
                                    let mut source =
                                        String::from("run \"printf\" with arguments [\"x\"]");
                                    for (index, part) in [binding, discard].iter().enumerate() {
                                        if index == external_position {
                                            source.push_str(&external);
                                        }
                                        source.push_str(part);
                                    }
                                    if external_position == 2 {
                                        source.push_str(&external);
                                    }
                                    if parser::validate_statement(
                                        &parser::tokenize(&source).unwrap(),
                                    )
                                    .is_err()
                                    {
                                        continue;
                                    }
                                    let layout = parser::repair_invocation_layout(&source).unwrap();
                                    let mut edited = source.clone();
                                    edited.insert_str(
                                        layout.retry_insert,
                                        " and retry up to 2 times",
                                    );
                                    parser::validate_statement(&parser::tokenize(&edited).unwrap())
                                        .unwrap_or_else(|error| {
                                            panic!("{source} -> {edited}: {error:#}")
                                        });
                                    let original = parser::parse(&source).unwrap();
                                    let mut repaired = parser::parse(&edited).unwrap();
                                    let mut found = false;
                                    for step in &mut repaired.steps {
                                        if let crate::ast::StepKind::Invoke { call, .. } =
                                            &mut step.kind
                                        {
                                            assert_eq!(call.retry.take().unwrap().retries, 2);
                                            found = true;
                                        }
                                    }
                                    assert!(found, "{edited}");
                                    assert_eq!(
                                        original.steps, repaired.steps,
                                        "{source} -> {edited}"
                                    );
                                    checked += 1;
                                }
                            }
                        }
                    }
                }
            }
        }
        eprintln!("retry suffix combinations checked: {checked}");
        assert!(checked >= 200, "only {checked} combinations were exercised");
    }

    #[test]
    fn retry_rejects_existing_retry_options() {
        for suffix in [
            "and retry up to 1 times",
            "and retry up to 2 times even if uncertain",
            "and retry up to 2 times with backoff 1 second",
            "and retry up to 2 times even if uncertain with backoff 1 second doubling",
        ] {
            let source = format!("run \"printf\" with arguments [] {suffix}\n");
            let error = apply(
                &source,
                &Operation::parse("add-retry 1 2").unwrap(),
                1,
                &Registry::default(),
            )
            .unwrap_err();
            assert!(
                format!("{error:#}").contains("already has a retry policy"),
                "{source}: {error:#}"
            );
        }
    }

    #[test]
    fn retry_insertion_handles_interleaved_suffixes_and_named_applets() {
        let source = "run \"printf\" with arguments [\"x\"] and environment {\"A\": \"v\"} as value and stdin from \"in\" and without environment [\"B\"] and stderr to \"err\"\n";
        let plan = apply(
            source,
            &Operation::parse("add-retry 1 2").unwrap(),
            1,
            &Registry::default(),
        )
        .unwrap();
        assert_eq!(
            plan.after,
            source.replace(
                "as value and stdin",
                "as value and retry up to 2 times and stdin"
            )
        );
        let named = "run fixture.count with count 1 as value and environment {\"A\": \"v\"}\n";
        let plan = apply(
            named,
            &Operation::parse("add-retry 1 2").unwrap(),
            1,
            &registry(),
        )
        .unwrap();
        assert_eq!(
            plan.after,
            named.replace(
                "as value and environment",
                "as value and retry up to 2 times and environment"
            )
        );
    }

    #[test]
    fn retry_words_in_literals_or_comments_are_not_a_retry_policy() {
        let source = "run \"printf\" with arguments [\"and retry up to 3 times\"] # and retry up to 4 times\n";
        let plan = apply(
            source,
            &Operation::parse("add-retry 1 2").unwrap(),
            1,
            &Registry::default(),
        )
        .unwrap();
        assert_eq!(plan.after, "run \"printf\" with arguments [\"and retry up to 3 times\"] and retry up to 2 times # and retry up to 4 times\n");
    }

    #[test]
    fn retry_insertion_preserves_multiline_comments_and_suffixes() {
        let source = "label: run \"printf\" with arguments [\r\n  \"x\", # comment inside list\r\n  \"y\"\r\n] and environment {\"A\": \"v\"} # final comment\r\n";
        let plan = apply(
            source,
            &Operation::parse("add-retry 1 2").unwrap(),
            1,
            &Registry::default(),
        )
        .unwrap();
        assert_eq!(
            plan.after,
            source.replace(
                "] and environment",
                "] and retry up to 2 times and environment"
            )
        );
    }

    #[test]
    fn retry_rejects_incompatible_suffixes_and_timeout_is_not_a_statement_suffix() {
        let mut exercised = [0; 4];
        for binding in ["", " as value"] {
            for discard in ["", " and discard stderr"] {
                for (suffix_index, suffix) in [
                    "and cache",
                    "and keep status as code",
                    "and keep result as result",
                    "and keep status as code and keep result as result",
                ]
                .iter()
                .enumerate()
                {
                    let source =
                        format!("run \"printf\" with arguments []{binding}{discard} {suffix}");
                    if parser::validate_statement(&parser::tokenize(&source).unwrap()).is_err() {
                        continue;
                    }
                    exercised[suffix_index] += 1;
                    let layout = parser::repair_invocation_layout(&source).unwrap();
                    let mut edited = source.clone();
                    edited.insert_str(layout.retry_insert, " and retry up to 2 times");
                    assert!(
                        parser::validate_statement(&parser::tokenize(&edited).unwrap()).is_err(),
                        "incompatible suffix accepted: {edited}"
                    );
                }
            }
        }
        assert!(exercised.iter().all(|count| *count > 0), "{exercised:?}");
        for source in [
            "then \"printf\" with arguments []",
            "pipe to \"printf\" with arguments []",
            "run \"printf\" with arguments [] interactively",
            "hand over to \"printf\" with arguments []",
            "launch detached \"printf\" with arguments [] and output to \"out\"",
            "run program file \"lib.program\" with name \"x\"",
        ] {
            parser::validate_statement(&parser::tokenize(source).unwrap()).unwrap();
            let layout = parser::repair_invocation_layout(source).unwrap();
            let mut edited = source.to_owned();
            edited.insert_str(layout.retry_insert, " and retry up to 2 times");
            assert!(
                parser::validate_statement(&parser::tokenize(&edited).unwrap()).is_err(),
                "{edited}"
            );
        }
        assert!(parser::validate_statement(
            &parser::tokenize("run \"printf\" and timeout 1 second").unwrap()
        )
        .is_err());
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
    fn repairs_that_do_not_check_are_refused_and_add_retry_is_applied() {
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
        )
        .unwrap();
        assert_eq!(
            retry.after,
            "run fixture.count with count 1 and retry up to 2 times\n"
        );
    }

    #[test]
    fn repair_preserves_crlf_and_missing_final_newline() {
        let source = "run fixture.count with count 1\r\nrun fixture.count with count 2\r\n";
        let plan = apply(
            source,
            &Operation::parse("replace-command 2 run fixture.count with count 3").unwrap(),
            2,
            &registry(),
        )
        .unwrap();
        assert_eq!(
            plan.after,
            "run fixture.count with count 1\r\nrun fixture.count with count 3\r\n"
        );
        let unterminated = "run fixture.count with count 1\r\nrun fixture.count with count 2";
        let plan = apply(
            unterminated,
            &Operation::parse("replace-command 2 run fixture.count with count 3").unwrap(),
            2,
            &registry(),
        )
        .unwrap();
        assert_eq!(
            plan.after,
            "run fixture.count with count 1\r\nrun fixture.count with count 3"
        );
    }

    #[test]
    fn repair_accepts_logical_continuations_and_path_aware_file_calls() {
        let directory = tempfile::tempdir().unwrap();
        let library = directory.path().join("lib.program");
        std::fs::write(&library, "run \"printf\" with arguments [\"lib\"]\n").unwrap();
        let path = directory.path().join("main.program");
        let source = "run program file \"lib.program\"\nrun \"false\"\n";
        std::fs::write(&path, source).unwrap();
        let boundary = boundary_line_at(source, Some(&path), 1).unwrap();
        assert_eq!(boundary, 1);
        let plan = apply_at(
            source,
            Some(&path),
            &Operation::parse("replace-command 2 run \"true\"").unwrap(),
            boundary,
            &Registry::default(),
        )
        .unwrap();
        assert_eq!(
            plan.after,
            "run program file \"lib.program\"\nrun \"true\"\n"
        );

        let multiline = "run \"printf\" with arguments [\n  \"a\",\n  \"b\"\n]\nrun \"false\"\n";
        let boundary = boundary_line(multiline, 1).unwrap();
        assert_eq!(boundary, 5);
        let plan = apply(
            multiline,
            &Operation::parse("replace-command 5 run \"true\"").unwrap(),
            boundary,
            &Registry::default(),
        )
        .unwrap();
        assert!(plan.after.ends_with("run \"true\"\n"));
    }

    #[test]
    fn the_diff_marks_removed_and_added_lines() {
        let diff = line_diff("a\nb\nc\n", "a\nx\nc\n");
        assert!(diff.contains("-    2 | b"), "{diff}");
        assert!(diff.contains("+    2 | x"), "{diff}");
        assert!(!diff.contains("| a"), "{diff}");
    }
}
