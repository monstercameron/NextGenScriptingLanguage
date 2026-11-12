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
use crate::record::StoredRecord;
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
            // An unterminated final line has no ending to borrow; inserting
            // without one would join the two statements. Borrow the file's
            // line-ending style instead (see `insert_line_ending`). The
            // original line itself is untouched, so it stays unterminated.
            let ending = if ending.is_empty() {
                insert_line_ending(source)
            } else {
                ending
            };
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

/// The separator for a line inserted before an unterminated final line: the
/// file's first line ending when one exists, otherwise LF (a single
/// unterminated line gives no style to borrow).
fn insert_line_ending(source: &str) -> &'static str {
    match source.find('\n') {
        Some(index) if index > 0 && source.as_bytes()[index - 1] == b'\r' => "\r\n",
        _ => "\n",
    }
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

/// Largest longest-common-subsequence table `line_diff` will allocate, in
/// `u32` cells (about 4 MiB). Larger middles use a linear-memory algorithm.
const MAX_DIFF_TABLE_CELLS: u64 = 1_048_576;

/// Largest middle, in `(old + 1) * (new + 1)` cells, for which `line_diff`
/// still searches for a longest common subsequence. Beyond this the middle
/// is reported as removed and re-added wholesale, bounding work as well as
/// memory for pathological inputs. Repair operations edit one localized
/// line, so their middles stay far below either bound.
const MAX_DIFF_SEARCH_CELLS: u64 = 100_000_000;

/// A line diff: removed lines with `-`, added lines with `+`, each with its line
/// number in the side it belongs to. Computed from the longest common subsequence.
///
/// Memory stays linear in the program size: common prefix and suffix lines
/// are trimmed first (a repair changes one localized line, so the remaining
/// middle is normally tiny), the exact quadratic table is used only for a
/// bounded middle, and larger middles use Hirschberg's linear-memory
/// longest-common-subsequence search.
pub fn line_diff(before: &str, after: &str) -> String {
    let old: Vec<&str> = before.lines().collect();
    let new: Vec<&str> = after.lines().collect();
    let (old_mid, new_mid, old_offset, new_offset) = trim_common(&old, &new);
    let mut out = String::new();
    diff_middle(old_mid, new_mid, old_offset, new_offset, &mut out);
    out
}

/// Strip the common prefix and suffix, returning the differing middles and
/// each middle's starting index in its own side.
fn trim_common<'a>(
    old: &'a [&'a str],
    new: &'a [&'a str],
) -> (&'a [&'a str], &'a [&'a str], usize, usize) {
    let mut prefix = 0;
    while prefix < old.len() && prefix < new.len() && old[prefix] == new[prefix] {
        prefix += 1;
    }
    let mut suffix = 0;
    while suffix < old.len() - prefix
        && suffix < new.len() - prefix
        && old[old.len() - 1 - suffix] == new[new.len() - 1 - suffix]
    {
        suffix += 1;
    }
    (
        &old[prefix..old.len() - suffix],
        &new[prefix..new.len() - suffix],
        prefix,
        prefix,
    )
}

fn diff_middle(old: &[&str], new: &[&str], old_offset: usize, new_offset: usize, out: &mut String) {
    if old.is_empty() {
        push_added(new, new_offset, out);
        return;
    }
    if new.is_empty() {
        push_removed(old, old_offset, out);
        return;
    }
    let cells = (old.len() as u64 + 1) * (new.len() as u64 + 1);
    if cells <= MAX_DIFF_TABLE_CELLS {
        diff_table(old, new, old_offset, new_offset, out);
    } else if cells > MAX_DIFF_SEARCH_CELLS || !share_a_line(old, new) {
        // Reporting the middle as added then removed matches the table
        // walk's order when the sides share no line.
        push_added(new, new_offset, out);
        push_removed(old, old_offset, out);
    } else {
        diff_hirschberg(old, new, old_offset, new_offset, out);
    }
}

fn push_added(new: &[&str], new_offset: usize, out: &mut String) {
    for (index, line) in new.iter().enumerate() {
        out.push_str(&format!("+{:>5} | {}\n", new_offset + index + 1, line));
    }
}

fn push_removed(old: &[&str], old_offset: usize, out: &mut String) {
    for (index, line) in old.iter().enumerate() {
        out.push_str(&format!("-{:>5} | {}\n", old_offset + index + 1, line));
    }
}

fn share_a_line(old: &[&str], new: &[&str]) -> bool {
    let new_lines: std::collections::HashSet<&str> = new.iter().copied().collect();
    old.iter().any(|line| new_lines.contains(line))
}

/// The exact suffix table and walk, for middles within `MAX_DIFF_TABLE_CELLS`.
fn diff_table(old: &[&str], new: &[&str], old_offset: usize, new_offset: usize, out: &mut String) {
    let (rows, cols) = (old.len() + 1, new.len() + 1);
    let mut table = vec![0u32; rows * cols];
    let cell = |i: usize, j: usize| i * cols + j;
    for i in (0..old.len()).rev() {
        for j in (0..new.len()).rev() {
            table[cell(i, j)] = if old[i] == new[j] {
                table[cell(i + 1, j + 1)] + 1
            } else {
                table[cell(i + 1, j)].max(table[cell(i, j + 1)])
            };
        }
    }
    let (mut i, mut j) = (0, 0);
    while i < old.len() || j < new.len() {
        if i < old.len() && j < new.len() && old[i] == new[j] {
            i += 1;
            j += 1;
        } else if j < new.len()
            && (i == old.len() || table[cell(i, j + 1)] >= table[cell(i + 1, j)])
        {
            out.push_str(&format!("+{:>5} | {}\n", new_offset + j + 1, new[j]));
            j += 1;
        } else {
            out.push_str(&format!("-{:>5} | {}\n", old_offset + i + 1, old[i]));
            i += 1;
        }
    }
}

/// Hirschberg's algorithm: a longest common subsequence diff using memory
/// linear in `new.len()` per level instead of the quadratic table. The
/// emitted diff is minimal, like the table walk's, though equally minimal
/// choices among repeated lines may differ from it.
fn diff_hirschberg(
    old: &[&str],
    new: &[&str],
    old_offset: usize,
    new_offset: usize,
    out: &mut String,
) {
    let (old, new, trimmed_old, trimmed_new) = trim_common(old, new);
    let (old_offset, new_offset) = (old_offset + trimmed_old, new_offset + trimmed_new);
    if old.is_empty() {
        push_added(new, new_offset, out);
        return;
    }
    if new.is_empty() {
        push_removed(old, old_offset, out);
        return;
    }
    if (old.len() as u64 + 1) * (new.len() as u64 + 1) <= MAX_DIFF_TABLE_CELLS {
        diff_table(old, new, old_offset, new_offset, out);
        return;
    }
    if old.len() == 1 {
        match new.iter().position(|line| *line == old[0]) {
            Some(position) => {
                push_added(&new[..position], new_offset, out);
                push_added(&new[position + 1..], new_offset + position + 1, out);
            }
            None => {
                push_added(new, new_offset, out);
                push_removed(old, old_offset, out);
            }
        }
        return;
    }
    let mid = old.len() / 2;
    let forward = lcs_row(&old[..mid], new);
    let backward = lcs_suffix_row(&old[mid..], new);
    // The split of `new` that keeps the two halves' common subsequences
    // longest overall. Later splits win ties, mirroring the table walk's
    // preference for emitting an addition before a removal.
    let mut split = 0;
    for candidate in 0..=new.len() {
        if forward[candidate] + backward[candidate] >= forward[split] + backward[split] {
            split = candidate;
        }
    }
    diff_hirschberg(&old[..mid], &new[..split], old_offset, new_offset, out);
    diff_hirschberg(
        &old[mid..],
        &new[split..],
        old_offset + mid,
        new_offset + split,
        out,
    );
}

/// `row[j]` is the longest-common-subsequence length of `old` and `new[..j]`.
fn lcs_row(old: &[&str], new: &[&str]) -> Vec<u32> {
    let mut row = vec![0u32; new.len() + 1];
    for old_line in old {
        let mut diagonal = 0u32;
        for (j, new_line) in new.iter().enumerate() {
            let above = row[j + 1];
            row[j + 1] = if old_line == new_line {
                diagonal + 1
            } else {
                above.max(row[j])
            };
            diagonal = above;
        }
    }
    row
}

/// `row[j]` is the longest-common-subsequence length of `old` and `new[j..]`.
fn lcs_suffix_row(old: &[&str], new: &[&str]) -> Vec<u32> {
    let old_reversed: Vec<&str> = old.iter().rev().copied().collect();
    let new_reversed: Vec<&str> = new.iter().rev().copied().collect();
    let mut row = lcs_row(&old_reversed, &new_reversed);
    row.reverse();
    row
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

/// The checkpoint's exclusive lock, held for one whole repair. This is the
/// same `lock` file resume holds through its journal and resolve holds
/// through its read-check-append, so holding it from before
/// [`repairable_program`] until after [`commit`] serializes the repair's
/// read, check and commit against live checkpoint use and other repairs.
/// Dropping the guard releases the lock.
pub struct RepairLock {
    _lock: crate::runtime::checkpoint::ResolveLock,
}

/// Acquire the checkpoint's exclusive lock for one repair. Fails with the
/// busy `locked` error when a resume, a resolve or another repair already
/// holds it, instead of letting this repair read, plan or commit
/// concurrently.
pub fn acquire_repair_lock(checkpoint: &Path) -> Result<RepairLock> {
    Ok(RepairLock {
        _lock: crate::runtime::checkpoint::acquire_resolve_lock(checkpoint)?,
    })
}

/// Refuse a plan whose program has changed since it was planned: the program
/// on disk must still hold `plan.before`, the text the plan and its audit
/// record's before hash were computed from. Callers check this under the
/// [`RepairLock`], right before [`commit`], so a stale plan is never applied.
pub fn ensure_program_unchanged(program_path: &Path, plan: &Plan) -> Result<()> {
    let current =
        fs::read(program_path).with_context(|| format!("reading {}", program_path.display()))?;
    if current != plan.before.as_bytes() {
        bail!(
            "the program {} changed while this repair was planned; nothing was written, run repair again",
            program_path.display()
        );
    }
    Ok(())
}

/// Write an applied plan: keep a copy of the program as it was, append the record
/// to `repairs.jsonl` and sync it, and only then replace the program file
/// atomically. The audit record is durable before the program changes, so a
/// crash or a failed replace can leave a record for a repair that was not
/// applied, but never an applied repair with no record. Returns the record's
/// sequence number. The caller must hold the [`RepairLock`] from
/// [`acquire_repair_lock`] across reading the checkpoint and program,
/// planning, and this commit, so no other repair or live checkpoint use can
/// interleave with the sequence choice, audit append and program replace.
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
    drop(file);

    let temporary = program_path.with_extension("repair-tmp");
    replace_with_temporary(program_path, &temporary, plan.after.as_bytes()).with_context(|| {
        format!(
            "repair record {sequence} is already durable in {}; replacing {} failed",
            log.display(),
            program_path.display()
        )
    })?;
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

fn is_link_or_reparse(metadata: &fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        crate::platform::windows::is_reparse(metadata)
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// Remove a temporary this process created. A planted link is left in place:
/// only a regular, non-link file at the path can be the one just written.
fn remove_temporary_if_regular(path: &Path) {
    if let Ok(metadata) = fs::symlink_metadata(path) {
        if metadata.is_file() && !is_link_or_reparse(&metadata) {
            let _ = fs::remove_file(path);
        }
    }
}

/// Publish repaired bytes over `program_path` through its sibling temporary.
/// The temporary name is predictable, so it is created exclusively with
/// no-follow semantics: a pre-existing entry there, planted as a symbolic
/// link/reparse point or otherwise, is refused rather than written through,
/// and its target is never modified.
fn replace_with_temporary(program_path: &Path, temporary: &Path, bytes: &[u8]) -> Result<()> {
    let permissions = match fs::symlink_metadata(program_path) {
        Ok(metadata) if is_link_or_reparse(&metadata) => {
            bail!(
                "refusing to replace a symbolic link or reparse point: {}",
                program_path.display()
            );
        }
        Ok(metadata) if !metadata.is_file() => {
            bail!(
                "refusing to replace a non-regular file: {}",
                program_path.display()
            );
        }
        Ok(metadata) => Some(metadata.permissions()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(error).with_context(|| format!("checking {}", program_path.display()));
        }
    };
    if let Ok(metadata) = fs::symlink_metadata(temporary) {
        if is_link_or_reparse(&metadata) {
            bail!(
                "refusing to write the repair temporary file through a symbolic link or reparse point: {}",
                temporary.display()
            );
        }
    }
    let mut options = crate::platform::private_options();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let mut file = options
        .open(temporary)
        .with_context(|| format!("creating {}", temporary.display()))?;
    let written = (|| -> Result<()> {
        file.write_all(bytes)?;
        if let Some(permissions) = &permissions {
            file.set_permissions(permissions.clone())?;
        }
        file.sync_all()?;
        Ok(())
    })();
    // Close before publishing or unlinking so both also work on Windows.
    drop(file);
    if let Err(error) = written {
        remove_temporary_if_regular(temporary);
        return Err(error).with_context(|| format!("writing {}", temporary.display()));
    }
    // Validate the file being published and its destination once more, so a
    // link swapped in after the create is refused instead of renamed over
    // the program or followed onto an external target.
    let published = (|| -> Result<()> {
        let metadata = fs::symlink_metadata(temporary)
            .with_context(|| format!("checking {}", temporary.display()))?;
        if is_link_or_reparse(&metadata) || !metadata.is_file() {
            bail!(
                "refusing to publish a repair temporary file that is a symbolic link or reparse point: {}",
                temporary.display()
            );
        }
        if let Ok(metadata) = fs::symlink_metadata(program_path) {
            if is_link_or_reparse(&metadata) {
                bail!(
                    "refusing to replace a symbolic link or reparse point: {}",
                    program_path.display()
                );
            }
        }
        crate::platform::replace_file(temporary, program_path)
            .with_context(|| format!("replacing {}", program_path.display()))
    })();
    if published.is_err() {
        remove_temporary_if_regular(temporary);
    }
    published
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

/// The bytes of `source` before the 1-based `boundary` line: the completed
/// prefix a repair must leave untouched. A boundary past the last newline
/// covers the whole file.
pub fn prefix_before_line(source: &str, boundary: usize) -> &str {
    if boundary <= 1 {
        return "";
    }
    let mut line = 1;
    for (offset, byte) in source.bytes().enumerate() {
        if byte == b'\n' {
            line += 1;
            if line == boundary {
                return &source[..offset + 1];
            }
        }
    }
    source
}

/// Check that the resume program keeps the record's completed prefix byte for
/// byte. Line numbers in a repair refer to the recorded program, so a resume
/// program whose earlier lines differ would silently shift the boundary over
/// completed steps and re-run their effects. Repairs after the boundary stay
/// free: only the prefix is compared, never the suffix.
pub fn check_resume_prefix(recorded: &str, current: &str, boundary: usize) -> Result<()> {
    let before_recorded = prefix_before_line(recorded, boundary);
    let before_current = prefix_before_line(current, boundary);
    if before_recorded != before_current {
        bail!(
            "the resume program differs from the recorded program before line {boundary}; the completed prefix must still match, so repair the recorded program instead of rewriting its earlier lines"
        );
    }
    Ok(())
}

/// The repair boundary for a failed recorded run: the first line of the
/// top-level statement that holds the record's first failed step, with the
/// failed step's own line alongside it. Repairs may change that line and
/// everything after it; earlier lines are the completed prefix that
/// `resume --from-record` replays instead of re-running. Completed nodes are
/// atomic, exactly as with checkpoint resume: when the failure is inside a
/// block, the whole block re-runs, so the whole block is repairable.
///
/// A record with no failed step (an interrupted run, or an error outside any
/// applet group) treats every recorded group as completed: the boundary is the
/// first top-level statement after the last recorded line, or one past the end
/// of the program when nothing follows.
pub fn from_record_boundary(
    source: &str,
    path: Option<&Path>,
    record: &StoredRecord,
) -> Result<(usize, Option<usize>)> {
    let failed_line = record
        .steps
        .iter()
        .find(|step| crate::record::step_failed(step))
        .map(|step| step.line);
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let registry = Registry::default();
    let access = crate::program_files::Access {
        registry: &registry,
        cwd: &cwd,
    };
    let program = crate::program_files::parse_at(source, path, Some(access))
        .map_err(|error| anyhow!("{error:#}"))?;
    let mut starts = Vec::new();
    let mut at = 0;
    while at < program.steps.len() {
        starts.push(at);
        at = crate::runtime::node_end(&program, at).max(at + 1);
    }
    let line_of = |index: usize| program.steps[index].line;
    let boundary = match failed_line {
        Some(failed) => {
            let mut boundary = failed;
            for (position, start) in starts.iter().enumerate() {
                let start_line = line_of(*start);
                let next_line = starts.get(position + 1).map(|next| line_of(*next));
                if start_line <= failed && next_line.is_none_or(|next| failed < next) {
                    boundary = start_line;
                    break;
                }
            }
            boundary
        }
        None => {
            let last = record.steps.iter().map(|step| step.line).max().unwrap_or(0);
            starts
                .iter()
                .map(|start| line_of(*start))
                .find(|line| *line > last)
                .unwrap_or_else(|| source.lines().count() + 1)
        }
    };
    Ok((boundary, failed_line))
}

/// The closed set of strategies `repair --suggest` may choose among. Choosing one
/// is left to a classifier; this type only names the set and validates a label.
/// There is no variant for `remove`, `replace-command` or `insert-before`, so a
/// suggestion can never name them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strategy {
    /// Run the failed step again, as `add-retry`.
    RetryTransient,
    /// Change one parameter to a value from a bounded candidate list, as `set-parameter`.
    AdjustParameter,
    /// Report the input problem only; no operation is built.
    FixInput,
    /// Stop and hand the failure to a person or an agent.
    Escalate,
}

/// Every strategy, in the order a suggestion lists them.
pub const STRATEGIES: [Strategy; 4] = [
    Strategy::RetryTransient,
    Strategy::AdjustParameter,
    Strategy::FixInput,
    Strategy::Escalate,
];

/// The most retries a suggestion may ask for.
pub const SUGGESTED_RETRY_LIMIT: u32 = 3;

impl Strategy {
    pub fn label(self) -> &'static str {
        match self {
            Self::RetryTransient => "retry-transient",
            Self::AdjustParameter => "adjust-parameter",
            Self::FixInput => "fix-input",
            Self::Escalate => "escalate",
        }
    }

    /// Read a label returned by a classifier. A label outside the closed set is
    /// invalid output: it is rejected here, never turned into an operation.
    pub fn from_label(label: &str) -> Result<Self> {
        STRATEGIES
            .iter()
            .copied()
            .find(|strategy| strategy.label() == label)
            .ok_or_else(|| {
                anyhow!(
                    "invalid repair strategy label; expected one of retry-transient, adjust-parameter, fix-input, escalate"
                )
            })
    }
}

/// Deterministic facts about a failed step that decide which strategies apply.
/// The caller derives them from the checkpoint, the record and the manifest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FailureFacts {
    /// The failure code is one the program declares as possibly transient.
    pub retryable_code: bool,
    /// The step's effects are declared idempotent, so running it again repeats nothing.
    pub idempotent: bool,
    /// The failure names a bad input, such as a missing or unwritable path.
    pub input_problem: bool,
    /// A parameter of the step has a manifest schema with a bounded candidate set.
    pub bounded_parameter: bool,
}

/// The strategies a failure is eligible for, in closed-set order. `Escalate` is
/// always eligible, so the result is never empty. A non-idempotent step is never
/// eligible for `RetryTransient`, whatever its code.
pub fn eligible_strategies(facts: FailureFacts) -> Vec<Strategy> {
    let mut eligible = Vec::new();
    if facts.retryable_code && facts.idempotent {
        eligible.push(Strategy::RetryTransient);
    }
    if facts.bounded_parameter {
        eligible.push(Strategy::AdjustParameter);
    }
    if facts.input_problem {
        eligible.push(Strategy::FixInput);
    }
    eligible.push(Strategy::Escalate);
    eligible
}

/// Build the `add-retry` operation for a `RetryTransient` suggestion. The count is
/// clamped to 1 through `SUGGESTED_RETRY_LIMIT`, and the text goes through the same
/// parser as an operation typed by a person. Refused unless the facts allow a retry.
pub fn retry_operation(facts: FailureFacts, line: usize, count: u32) -> Result<Operation> {
    if !(facts.retryable_code && facts.idempotent) {
        bail!(
            "a step is retried only when its failure is retryable and its effects are idempotent"
        );
    }
    let count = count.clamp(1, SUGGESTED_RETRY_LIMIT);
    Operation::parse(&format!("add-retry {line} {count}"))
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
    fn retry_rejects_incompatible_suffixes_and_timeout_is_a_statement_suffix() {
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
        // `and timeout N UNIT` is a statement suffix since the per-command timeout (#310).
        parser::validate_statement(
            &parser::tokenize("run \"printf\" and timeout 1 second").unwrap(),
        )
        .unwrap();
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
    fn insert_before_unterminated_final_line_keeps_statements_separate() {
        // LF and CRLF: the inserted line borrows the file's line-ending style,
        // and the original final line keeps its lack of a trailing newline.
        for (source, expected) in [
            (
                "run fixture.count with count 1\nrun fixture.count with count 2",
                "run fixture.count with count 1\nrun fixture.count with count 3\nrun fixture.count with count 2",
            ),
            (
                "run fixture.count with count 1\r\nrun fixture.count with count 2",
                "run fixture.count with count 1\r\nrun fixture.count with count 3\r\nrun fixture.count with count 2",
            ),
        ] {
            let plan = apply(
                source,
                &Operation::parse("insert-before 2 run fixture.count with count 3").unwrap(),
                2,
                &registry(),
            )
            .unwrap();
            assert_eq!(plan.after, expected);
            assert!(!plan.after.ends_with('\n'), "{source:?}");
            assert_eq!(parser::parse(&plan.after).unwrap().steps.len(), 3);
        }
        // A single unterminated line has no ending to borrow; fall back to LF.
        let plan = apply(
            "run \"false\"",
            &Operation::parse("insert-before 1 run \"true\"").unwrap(),
            1,
            &Registry::default(),
        )
        .unwrap();
        assert_eq!(plan.after, "run \"true\"\nrun \"false\"");
        assert_eq!(parser::parse(&plan.after).unwrap().steps.len(), 2);
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

    #[test]
    fn the_diff_output_for_small_programs_is_unchanged() {
        assert_eq!(
            line_diff("a\nb\nc\n", "a\nx\nc\n"),
            "+    2 | x\n-    2 | b\n"
        );
        assert_eq!(line_diff("a\nc\n", "a\nb\nc\n"), "+    2 | b\n");
        assert_eq!(line_diff("a\nb\nc\n", "a\nc\n"), "-    2 | b\n");
        assert_eq!(line_diff("a\nb\n", "a\nb\n"), "");
        assert_eq!(line_diff("", ""), "");
        assert_eq!(line_diff("", "héllo\n"), "+    1 | héllo\n");
        assert_eq!(line_diff("héllo\n", ""), "-    1 | héllo\n");
    }

    #[test]
    fn the_diff_for_a_large_program_with_a_one_line_edit_stays_bounded() {
        // Regression for #755: the suffix table for this pair would hold
        // about 100M cells. Trimming the common lines leaves a one-line
        // middle, so the diff is exactly the edited line.
        let lines: Vec<String> = (0..10_000)
            .map(|index| format!("step {index:05}"))
            .collect();
        let before = format!("{}\n", lines.join("\n"));
        let mut edited = lines.clone();
        edited[5_000] = "step replaced".to_owned();
        let after = format!("{}\n", edited.join("\n"));

        let diff = line_diff(&before, &after);

        assert_eq!(diff, "+ 5001 | step replaced\n- 5001 | step 05000\n");
    }

    #[test]
    fn the_diff_for_a_large_program_with_scattered_edits_stays_minimal() {
        // The middle after trimming exceeds the bounded table, so this
        // exercises the linear-memory search. Unchanged lines, including
        // the Unicode one, must not appear in the diff.
        let lines: Vec<String> = (0..1_500).map(|index| format!("step {index:04}")).collect();
        let before = format!("{}\n", lines.join("\n"));
        let mut edited = lines.clone();
        edited[750] = "step héllo".to_owned();
        for index in (100..1_500).step_by(100) {
            edited[index] = format!("step edited {index:04}");
        }
        let after = format!("{}\n", edited.join("\n"));

        let diff = line_diff(&before, &after);

        assert!(diff.contains("-  101 | step 0100\n"), "{diff}");
        assert!(diff.contains("+  101 | step edited 0100\n"), "{diff}");
        assert!(diff.contains("-  751 | step 0750\n"), "{diff}");
        assert!(diff.contains("+  751 | step héllo\n"), "{diff}");
        assert!(!diff.contains("| step 0000\n"), "{diff}");
        assert!(!diff.contains("| step 0751\n"), "{diff}");
        assert!(!diff.contains("| step 1499\n"), "{diff}");
        assert_eq!(diff.lines().count(), 30, "{diff}");
    }

    #[test]
    fn the_large_diff_with_repeated_lines_matches_the_table_diff_size() {
        // Repeated lines make the common subsequence ambiguous. The
        // linear-memory search must still emit a minimal diff that
        // transforms the old program into the new one.
        let mut state = 0x2545F4914F6CDD1Du64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let alphabet = [
            "alpha", "beta", "gamma", "delta", "", "end", "run x", "alpha",
        ];
        let old_lines: Vec<&str> = (0..1200)
            .map(|_| alphabet[(next() % alphabet.len() as u64) as usize])
            .collect();
        let mut new_lines: Vec<&str> = old_lines.clone();
        for _ in 0..60 {
            let at = (next() % new_lines.len() as u64) as usize;
            match next() % 3 {
                0 => new_lines[at] = alphabet[(next() % alphabet.len() as u64) as usize],
                1 => {
                    new_lines.remove(at);
                }
                _ => new_lines.insert(at, alphabet[(next() % alphabet.len() as u64) as usize]),
            }
        }
        let before = format!("{}\n", old_lines.join("\n"));
        let after = format!("{}\n", new_lines.join("\n"));

        let diff = line_diff(&before, &after);
        let mut table_diff = String::new();
        diff_table(&old_lines, &new_lines, 0, 0, &mut table_diff);
        assert_eq!(diff.lines().count(), table_diff.lines().count(), "{diff}");

        let mut removed: Vec<usize> = Vec::new();
        let mut added: Vec<usize> = Vec::new();
        for entry in diff.lines() {
            let number: usize = entry[1..6].trim().parse().unwrap();
            let content = &entry[9..];
            if entry.starts_with('-') {
                assert_eq!(old_lines[number - 1], content, "{diff}");
                removed.push(number - 1);
            } else {
                assert_eq!(new_lines[number - 1], content, "{diff}");
                added.push(number - 1);
            }
        }
        let kept_old: Vec<&str> = old_lines
            .iter()
            .enumerate()
            .filter(|(index, _)| !removed.contains(index))
            .map(|(_, line)| *line)
            .collect();
        let kept_new: Vec<&str> = new_lines
            .iter()
            .enumerate()
            .filter(|(index, _)| !added.contains(index))
            .map(|(_, line)| *line)
            .collect();
        assert_eq!(kept_old, kept_new, "{diff}");
    }

    fn commit_fixture() -> (tempfile::TempDir, PathBuf, PathBuf, Plan) {
        let directory = tempfile::tempdir().unwrap();
        let checkpoint = directory.path().join("checkpoint");
        std::fs::create_dir(&checkpoint).unwrap();
        let program = directory.path().join("main.program");
        let source = "run fixture.count with count 1\n";
        std::fs::write(&program, source).unwrap();
        let plan = apply(
            source,
            &Operation::parse("set-parameter 1 count 2").unwrap(),
            1,
            &registry(),
        )
        .unwrap();
        (directory, checkpoint, program, plan)
    }

    #[test]
    fn commit_replaces_the_program_and_removes_the_temporary() {
        let (_directory, checkpoint, program, plan) = commit_fixture();
        commit(&checkpoint, &program, &plan, "set-parameter 1 count 2").unwrap();
        assert_eq!(std::fs::read_to_string(&program).unwrap(), plan.after);
        assert!(std::fs::symlink_metadata(program.with_extension("repair-tmp")).is_err());
        assert!(checkpoint.join(REPAIRS_FILE).is_file());
    }

    #[test]
    fn commit_records_the_repair_before_replacing_the_program() {
        let (_directory, checkpoint, program, plan) = commit_fixture();
        // Force the replace step to fail: its temporary already exists.
        let temporary = program.with_extension("repair-tmp");
        std::fs::write(&temporary, "planted").unwrap();
        let before = std::fs::read_to_string(&program).unwrap();

        assert!(commit(&checkpoint, &program, &plan, "set-parameter 1 count 2").is_err());

        // The program is untouched, but its audit record is already durable:
        // the record is written and synced before the replace is attempted.
        assert_eq!(std::fs::read_to_string(&program).unwrap(), before);
        let log = std::fs::read_to_string(checkpoint.join(REPAIRS_FILE)).unwrap();
        assert_eq!(log.lines().count(), 1, "{log}");
        assert!(log.contains("\"sequence\":1"), "{log}");
        assert!(log.contains(&sha256_hex(plan.after.as_bytes())), "{log}");
    }

    #[test]
    fn commit_refuses_a_pre_existing_repair_temporary_file() {
        let (_directory, checkpoint, program, plan) = commit_fixture();
        let temporary = program.with_extension("repair-tmp");
        std::fs::write(&temporary, "planted").unwrap();
        let before = std::fs::read_to_string(&program).unwrap();

        assert!(commit(&checkpoint, &program, &plan, "set-parameter 1 count 2").is_err());

        assert_eq!(std::fs::read_to_string(&program).unwrap(), before);
        assert_eq!(std::fs::read_to_string(&temporary).unwrap(), "planted");
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn commit_refuses_a_planted_symlink_at_the_repair_temporary_path() {
        let (directory, checkpoint, program, plan) = commit_fixture();
        let external = directory.path().join("external.txt");
        std::fs::write(&external, "old").unwrap();
        let temporary = program.with_extension("repair-tmp");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&external, &temporary).unwrap();
        #[cfg(windows)]
        if std::os::windows::fs::symlink_file(&external, &temporary).is_err() {
            // Creating a file symlink needs a privilege this host may not grant.
            return;
        }
        let before = std::fs::read_to_string(&program).unwrap();

        let error = commit(&checkpoint, &program, &plan, "set-parameter 1 count 2").unwrap_err();

        assert!(format!("{error:#}").contains("symbolic link"), "{error:#}");
        assert_eq!(std::fs::read_to_string(&external).unwrap(), "old");
        assert_eq!(std::fs::read_to_string(&program).unwrap(), before);
        assert!(std::fs::symlink_metadata(&temporary)
            .unwrap()
            .file_type()
            .is_symlink());
    }

    /// While one repair holds the checkpoint lock, a second repair on the
    /// same checkpoint is refused with the busy error instead of racing the
    /// first repair's read-plan-commit. The same lock also excludes resolve,
    /// which shares the checkpoint `lock` file with resume. Same-process
    /// separate handles conflict on both `flock` (Unix) and `LockFileEx`
    /// (Windows), so this is deterministic in-process.
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn repair_lock_refuses_a_second_repair_while_held() {
        let (_directory, checkpoint, _program, _plan) = commit_fixture();
        let held = acquire_repair_lock(&checkpoint).unwrap();

        let error = acquire_repair_lock(&checkpoint)
            .err()
            .expect("a second repair must be rejected while the first holds the lock");
        assert!(
            error.to_string().contains("locked"),
            "unexpected concurrent error: {error:#}"
        );
        let error = crate::runtime::checkpoint::acquire_resolve_lock(&checkpoint)
            .err()
            .expect("resolve must be rejected while a repair holds the lock");
        assert!(
            error.to_string().contains("locked"),
            "unexpected concurrent error: {error:#}"
        );

        drop(held);
        let _reacquired = acquire_repair_lock(&checkpoint).unwrap();
    }

    #[test]
    fn stale_plan_is_refused_when_the_program_changed_after_planning() {
        let (_directory, checkpoint, program, plan) = commit_fixture();
        let concurrent = "run fixture.count with count 9\n";
        std::fs::write(&program, concurrent).unwrap();

        let error = ensure_program_unchanged(&program, &plan)
            .expect_err("a stale plan must be refused before commit");
        assert!(error.to_string().contains("changed"), "{error:#}");
        assert_eq!(std::fs::read_to_string(&program).unwrap(), concurrent);
        assert!(std::fs::symlink_metadata(checkpoint.join(REPAIRS_FILE)).is_err());
        ensure_program_unchanged(&program, &plan).expect_err("still stale");

        std::fs::write(&program, &plan.before).unwrap();
        ensure_program_unchanged(&program, &plan).unwrap();
    }

    fn facts(retryable_code: bool, idempotent: bool, input_problem: bool) -> FailureFacts {
        FailureFacts {
            retryable_code,
            idempotent,
            input_problem,
            bounded_parameter: false,
        }
    }

    #[test]
    fn suggestion_strategies_come_only_from_the_closed_set() {
        let labels: Vec<&str> = STRATEGIES.iter().map(|s| s.label()).collect();
        assert_eq!(
            labels,
            [
                "retry-transient",
                "adjust-parameter",
                "fix-input",
                "escalate"
            ]
        );
        for label in labels {
            assert_eq!(Strategy::from_label(label).unwrap().label(), label);
        }
        for invalid in [
            "remove",
            "replace-command",
            "insert-before",
            "",
            "RETRY-TRANSIENT",
            " escalate",
        ] {
            assert!(
                Strategy::from_label(invalid).is_err(),
                "{invalid:?} must be refused"
            );
        }
    }

    #[test]
    fn transient_exit_on_an_idempotent_step_is_eligible_for_retry() {
        let eligible = eligible_strategies(facts(true, true, false));
        assert_eq!(eligible, [Strategy::RetryTransient, Strategy::Escalate]);
    }

    #[test]
    fn non_idempotent_steps_never_get_a_retry_suggestion() {
        for input_problem in [false, true] {
            let eligible = eligible_strategies(facts(true, false, input_problem));
            assert!(
                !eligible.contains(&Strategy::RetryTransient),
                "{eligible:?}"
            );
            assert!(retry_operation(facts(true, false, input_problem), 4, 2).is_err());
        }
    }

    #[test]
    fn unwritable_parameter_path_is_an_input_problem_not_a_retry() {
        let eligible = eligible_strategies(facts(false, true, true));
        assert_eq!(eligible, [Strategy::FixInput, Strategy::Escalate]);
    }

    #[test]
    fn deterministic_failure_only_escalates() {
        let eligible = eligible_strategies(facts(false, true, false));
        assert_eq!(eligible, [Strategy::Escalate]);
    }

    #[test]
    fn bounded_parameter_makes_adjust_parameter_eligible() {
        let mut bounded = facts(false, true, true);
        bounded.bounded_parameter = true;
        assert_eq!(
            eligible_strategies(bounded),
            [
                Strategy::AdjustParameter,
                Strategy::FixInput,
                Strategy::Escalate
            ]
        );
    }

    #[test]
    fn retry_operation_is_built_by_the_existing_parser_with_a_clamped_count() {
        let retry = facts(true, true, false);
        assert_eq!(
            retry_operation(retry, 4, 2).unwrap(),
            Operation::AddRetry { line: 4, count: 2 }
        );
        assert_eq!(
            retry_operation(retry, 4, 0).unwrap(),
            Operation::AddRetry { line: 4, count: 1 }
        );
        assert_eq!(
            retry_operation(retry, 4, 99).unwrap(),
            Operation::AddRetry {
                line: 4,
                count: SUGGESTED_RETRY_LIMIT
            }
        );
        assert!(retry_operation(facts(false, true, false), 4, 2).is_err());
    }
}
