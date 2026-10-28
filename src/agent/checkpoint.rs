//! `execute checkpoint status DIR [--json]`: a read-only summary of a checkpoint.
//!
//! The command probes the checkpoint lock without blocking, never writes, and never runs a
//! program. It reads `journal.json` and the blobs directory and reports what a
//! resume would do. It does not verify the journal checksum, because that check
//! depends on the runtime's private serialization. The output says so.
//!
//! `execute checkpoint resolve ...` records one operator decision for an
//! uncertain node or element (issue #536). It refuses while the checkpoint is
//! locked, refuses unknown or already-settled targets, and appends exactly one
//! validated record; resume honors it.

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use crate::runtime::checkpoint::resolutions;

/// Protocol identifier of the `--json` object.
pub const PROTOCOL: &str = "execute.checkpoint_status/1";

const MAX_JOURNAL_BYTES: u64 = 16 * 1024 * 1024;
const USAGE: &str = "usage: execute checkpoint status DIR [--json]\n       execute checkpoint resolve DIR --node STEP_ID [--element N] --as committed|retry [--output FILE] [--binding NAME=VALUE ...] [--accept-empty-output] [--accept-empty-bindings] [--reason TEXT]";

/// Run the `checkpoint` subcommand and return the process exit status.
pub fn cli(arguments: &[String]) -> Result<i32> {
    match arguments.first().map(String::as_str) {
        Some("status") => status_cli(&arguments[1..]),
        Some("resolve") => resolve_cli(&arguments[1..]),
        _ => bail!("{USAGE}"),
    }
}

fn status_cli(arguments: &[String]) -> Result<i32> {
    let mut as_json = false;
    let mut directory = None;
    for argument in arguments {
        match argument.as_str() {
            "--json" => as_json = true,
            other if other.starts_with("--") => bail!("unknown option {other}; {USAGE}"),
            other => {
                if directory.replace(other).is_some() {
                    bail!("checkpoint status takes one directory; {USAGE}");
                }
            }
        }
    }
    let directory = directory.ok_or_else(|| anyhow!("{USAGE}"))?;
    let summary = summarize(Path::new(directory))?;
    if as_json {
        writeln!(io::stdout(), "{}", serde_json::to_string_pretty(&summary)?)?;
    } else {
        io::stdout().write_all(render_text(&summary).as_bytes())?;
    }
    Ok(0)
}

/// Record one operator decision for an uncertain node or element.
///
/// `committed` says the effect already happened: the operator supplies the
/// output (and the bindings later nodes read) that resume replays instead of
/// re-running. `retry` authorizes one uncertain item to run again without the
/// bulk `--retry-uncertain` flag. A committed element must have begun (its
/// begin fingerprint is captured so resume refuses a changed list); a retry
/// may also name a later element that has not begun yet, authorizing its run.
/// Element nodes take only per-element records; a node-level record authorizes
/// nothing there. Parallel (`parallel for each`) elements resolve as `retry`
/// only: `committed` is refused because the resolution cannot reproduce their
/// value, stderr and outcome. The checkpoint lock is held across validation
/// and append, so concurrent resolves fail closed instead of double-recording.
fn resolve_cli(arguments: &[String]) -> Result<i32> {
    let mut directory: Option<&str> = None;
    let mut node: Option<&str> = None;
    let mut element: Option<usize> = None;
    let mut decision: Option<&str> = None;
    let mut output: Option<&str> = None;
    let mut bindings: Vec<&str> = Vec::new();
    let mut accept_empty_output = false;
    let mut accept_empty_bindings = false;
    let mut reason = "";
    let mut index = 0;
    while index < arguments.len() {
        let argument = arguments[index].as_str();
        match argument {
            "--node" => {
                index += 1;
                node = Some(
                    arguments
                        .get(index)
                        .map(String::as_str)
                        .ok_or_else(|| anyhow!("--node requires a step id; {USAGE}"))?,
                );
            }
            "--element" => {
                index += 1;
                let text = arguments
                    .get(index)
                    .map(String::as_str)
                    .ok_or_else(|| anyhow!("--element requires an index; {USAGE}"))?;
                element =
                    Some(text.parse::<usize>().map_err(|_| {
                        anyhow!("--element requires a non-negative index; {USAGE}")
                    })?);
            }
            "--as" => {
                index += 1;
                decision = Some(
                    arguments
                        .get(index)
                        .map(String::as_str)
                        .ok_or_else(|| anyhow!("--as requires committed or retry; {USAGE}"))?,
                );
            }
            "--output" => {
                index += 1;
                output = Some(
                    arguments
                        .get(index)
                        .map(String::as_str)
                        .ok_or_else(|| anyhow!("--output requires a file; {USAGE}"))?,
                );
            }
            "--binding" => {
                index += 1;
                bindings.push(
                    arguments
                        .get(index)
                        .map(String::as_str)
                        .ok_or_else(|| anyhow!("--binding requires NAME=VALUE; {USAGE}"))?,
                );
            }
            "--accept-empty-output" => accept_empty_output = true,
            "--accept-empty-bindings" => accept_empty_bindings = true,
            "--reason" => {
                index += 1;
                reason = arguments
                    .get(index)
                    .map(String::as_str)
                    .ok_or_else(|| anyhow!("--reason requires text; {USAGE}"))?;
            }
            other if other.starts_with("--") => bail!("unknown option {other}; {USAGE}"),
            other => {
                if directory.replace(other).is_some() {
                    bail!("checkpoint resolve takes one directory; {USAGE}");
                }
            }
        }
        index += 1;
    }
    let directory = directory.ok_or_else(|| anyhow!("{USAGE}"))?;
    let directory = Path::new(directory);
    let node = node.ok_or_else(|| anyhow!("--node is required; {USAGE}"))?;
    if node.is_empty() {
        bail!("--node requires a step id; {USAGE}");
    }
    let decision = decision.ok_or_else(|| anyhow!("--as committed|retry is required; {USAGE}"))?;
    let committed = match decision {
        "committed" => true,
        "retry" => false,
        _ => bail!("--as requires committed or retry; {USAGE}"),
    };
    if !committed
        && (output.is_some()
            || !bindings.is_empty()
            || accept_empty_output
            || accept_empty_bindings)
    {
        bail!("a retry resolution carries no output or bindings; {USAGE}");
    }
    resolve_uncertain(
        directory,
        node,
        element,
        committed,
        output,
        &bindings,
        accept_empty_output,
        accept_empty_bindings,
        reason,
    )?;
    Ok(0)
}

/// Validate the target against the journal and element log, then append the
/// resolution record. Every refusal below fails closed: a resolution is only
/// recorded for an item that is actually awaiting a decision.
#[allow(clippy::too_many_arguments)]
fn resolve_uncertain(
    directory: &Path,
    node: &str,
    element: Option<usize>,
    committed: bool,
    output: Option<&str>,
    bindings: &[&str],
    accept_empty_output: bool,
    accept_empty_bindings: bool,
    reason: &str,
) -> Result<()> {
    match fs::symlink_metadata(directory) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
        Ok(_) => bail!("checkpoint path must be a real directory"),
        Err(error) => return Err(error).context("reading checkpoint directory"),
    }
    // Lock end-to-end: the running program holds this same `lock` file from
    // `Journal::open` until the journal drops (flock EX|NB on Unix, LockFileEx
    // on Windows), so a resume in progress makes this acquire fail with
    // `locked` instead of racing validation. Status probes the same file
    // without creating it and reports `locked`; resolve creates it when
    // absent so two concurrent resolves serialize on one read-check-append.
    // The guard is held to the end of this function, across journal reads,
    // element-log reads, and the resolutions append.
    let _resolve_lock = crate::runtime::checkpoint::acquire_resolve_lock(directory)?;
    let journal = read_journal_value(&directory.join("journal.json"))?;
    let data = journal
        .get("data")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow!("checkpoint journal has no data object"))?;
    if data.get("kind").and_then(Value::as_str) == Some("graph") {
        bail!("resolutions are not supported for graph checkpoints");
    }
    let in_flight = data.get("in_flight").filter(|value| !value.is_null());
    let Some(in_flight) = in_flight else {
        bail!("checkpoint has no unfinished node; nothing awaits resolution");
    };
    let in_flight_step = in_flight
        .get("step_id")
        .and_then(Value::as_str)
        .unwrap_or("");
    if in_flight_step.is_empty() {
        bail!("checkpoint predates step ids; use a fresh checkpoint");
    }
    if in_flight_step != node {
        bail!(
            "checkpoint has no unfinished node {node:?}; the unfinished node is {in_flight_step:?}"
        );
    }
    let retry_safe = in_flight
        .get("retry_safe")
        .and_then(Value::as_bool)
        .ok_or_else(|| anyhow!("checkpoint node has no retry safety marking"))?;
    let pending = pending_elements(directory)?;
    let pending_uncertain = pending.iter().filter(|item| item.uncertain).count();
    // Identity is mandatory for every journal the runtime can resume; a
    // journal without one is too old or damaged to resolve against.
    let identity = data
        .get("identity")
        .ok_or_else(|| anyhow!("checkpoint journal has no identity"))?;
    let finite = identity.get("mode").and_then(Value::as_str) == Some("finite");
    // Version 6 marks the in-flight node kind (`for_each` or
    // `parallel_for_each`) so resolve can scope decisions without the program.
    // A journal without the marker predates it: a loop killed before its
    // first begin then looks exactly like a plain node (no marker, no element
    // log, no pending entries). Accepting a node-level record there is
    // unsafe: resume re-begins the node with the marker and the record would
    // authorize nothing (retry) or skip the whole loop (committed). Fail
    // closed: node-level records need the version 6 journal that proves the
    // node is plain; older journals resolve per element when the log proves
    // it, and otherwise use the bulk `--retry-uncertain` flag or a fresh
    // checkpoint. Resume enforces the same rule program-aware, so a
    // hand-written node-level record for a loop is refused instead of
    // silently skipping it.
    let journal_version = data.get("version").and_then(Value::as_u64).unwrap_or(0);
    let element_kind = in_flight
        .get("element_kind")
        .and_then(Value::as_str)
        .unwrap_or("");
    let is_parallel = element_kind == "parallel_for_each";
    let journal_marks_elements = !element_kind.is_empty()
        || in_flight
            .get("element_fingerprint")
            .is_some_and(|value| !value.is_null())
        || in_flight
            .get("element_start")
            .is_some_and(|value| !value.is_null());
    let has_progress = has_element_progress(directory)?;
    let is_element_node = journal_marks_elements || has_progress || !pending.is_empty();
    let pre_marking = journal_version < 6 && element_kind.is_empty();
    // Node-level records cover whole nodes, never `for each` elements. Each
    // uncertain element needs its own record; a node-level record authorizes
    // nothing on an element node however broad its wording.
    if element.is_none() && is_element_node {
        bail!(
            "node-level resolutions are not valid for for-each nodes; resolve each element instead"
        );
    }
    if element.is_none() && pre_marking && !has_progress && pending.is_empty() {
        bail!(
            "checkpoint predates element markers, so a plain node cannot be told from a for-each loop killed before its first begin; node-level resolutions are refused here, use --retry-uncertain or a fresh checkpoint"
        );
    }
    // A parallel result carries a value, stderr and outcome the resolution
    // cannot faithfully reproduce, so `committed` is refused for parallel
    // groups even per element. Resolve parallel elements as `retry`.
    if committed && is_parallel {
        bail!("parallel for each elements cannot resolve as committed; use retry");
    }
    if element.is_none() && retry_safe && pending_uncertain == 0 {
        bail!("node {node:?} is not uncertain and has no uncertain elements; nothing to resolve");
    }
    if element.is_none() && committed && !finite {
        bail!("node-level committed resolution requires a finite checkpoint");
    }
    // Per-element records name entries of a `for each` element log. Refuse them
    // for plain nodes, where a stray element index must not stand in for the
    // node-level decision resume enforces. On a pre-marking journal the same
    // refusal covers a loop killed before its first begin: only a future
    // retry is meaningful there, and without the marker the loop cannot be
    // told from a plain node, so the bulk `--retry-uncertain` flag is the
    // safe path until the element log proves the node.
    if element.is_some() && !is_element_node {
        if pre_marking {
            bail!("node {node:?} shows no element marker or element log, so it cannot be told from a plain node on this older checkpoint; use --retry-uncertain or a fresh checkpoint");
        }
        bail!("node {node:?} is not a for-each node; resolve it without --element");
    }
    // A retry carries no output, bindings, or fingerprint: it authorizes one
    // more run of the named element, and the element log's own begin record
    // remains the source of truth for what started. Only a committed element
    // pins its begin fingerprint, so resume can refuse a changed list entry
    // instead of replaying verified output against the wrong item. The match
    // below still validates the target (certain, already-committed, or
    // never-begun committed) before recording the retry with no fingerprint.
    let fingerprint = match element {
        Some(wanted) => {
            let began = pending
                .iter()
                .find(|item| usize::try_from(item.index).is_ok_and(|got| got == wanted));
            match began {
                Some(item) if !item.uncertain => {
                    bail!("for each element {wanted} is certain; only uncertain elements need resolution");
                }
                Some(_) if !committed => None,
                Some(item) => Some(item.fingerprint.clone()),
                None if committed => {
                    bail!("for each element {wanted} has not begun; only retry can authorize an element that has not begun");
                }
                None => {
                    if committed_element_index(directory, wanted)? {
                        bail!("for each element {wanted} already committed; nothing to resolve");
                    }
                    None
                }
            }
        }
        None => None,
    };
    let bound = crate::runtime::RunOptions::new(PathBuf::new()).max_output_bytes;
    let output_bytes = if committed {
        match output {
            Some(path) => Some(resolutions::read_output_file(Path::new(path), bound)?),
            None if accept_empty_output => Some(Vec::new()),
            None => {
                bail!("a committed resolution requires --output FILE or --accept-empty-output");
            }
        }
    } else {
        None
    };
    let mut binding_map = BTreeMap::new();
    for argument in bindings {
        let (name, value) = resolutions::parse_binding(argument)?;
        if binding_map.insert(name.clone(), value).is_some() {
            bail!("--binding {name:?} was given twice");
        }
    }
    if committed && binding_map.is_empty() && !accept_empty_bindings {
        bail!("a committed resolution requires --binding NAME=VALUE or --accept-empty-bindings");
    }
    let record = resolutions::ResolutionRecord {
        version: resolutions::RESOLUTION_VERSION,
        node: node.to_owned(),
        element,
        decision: if committed {
            resolutions::ResolutionDecision::Committed
        } else {
            resolutions::ResolutionDecision::Retry
        },
        reason: reason.to_owned(),
        actor: resolutions::current_actor(),
        at: resolutions::current_time_secs(),
        output: output_bytes.map(|bytes| resolutions::hex_encode(&bytes)),
        bindings: if committed {
            Some(binding_map.into_iter().collect())
        } else {
            None
        },
        fingerprint,
    };
    resolutions::append_resolution_locked(directory, &record, &_resolve_lock)?;
    let target = match element {
        Some(wanted) => format!("node {node:?} element {wanted}"),
        None => format!("node {node:?}"),
    };
    let outcome = if committed { "committed" } else { "retry" };
    if reason.is_empty() {
        writeln!(
            io::stdout(),
            "recorded {outcome} resolution for {target} by {}",
            record.actor
        )?;
    } else {
        writeln!(
            io::stdout(),
            "recorded {outcome} resolution for {target} by {}: {reason}",
            record.actor
        )?;
    }
    Ok(())
}

/// Whether the checkpoint holds any element-log progress. Mirrors the
/// runtime's guard so resolve refuses node-level commits the runtime would
/// refuse to honor.
fn has_element_progress(directory: &Path) -> Result<bool> {
    let path = directory.join("elements.jsonl");
    match fs::symlink_metadata(&path) {
        Ok(metadata) => {
            if !metadata.is_file() || metadata.file_type().is_symlink() {
                bail!("checkpoint element log must be a regular file");
            }
            Ok(metadata.len() > 0)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).context("inspecting checkpoint element log"),
    }
}

/// Whether the element log already holds a commit for `wanted`: a retry (or
/// any second decision) for a settled element is a typo, not a decision.
fn committed_element_index(directory: &Path, wanted: usize) -> Result<bool> {
    let path = directory.join("elements.jsonl");
    let file = match fs::File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error).context("reading checkpoint element log"),
    };
    if !file.metadata()?.is_file() {
        bail!("checkpoint element log must be a regular file");
    }
    let bound = crate::runtime::RunOptions::new(PathBuf::new()).max_output_bytes;
    let max_element_line = crate::runtime::checkpoint::element_line_limit(bound);
    let mut reader = std::io::BufReader::new(file);
    let mut line = Vec::new();
    let mut committed = std::collections::BTreeSet::<u64>::new();
    let mut line_number = 0usize;
    loop {
        line.clear();
        let read =
            crate::runtime::checkpoint::read_bounded_line(&mut reader, &mut line, max_element_line)
                .context("reading checkpoint element log")?;
        if read == 0 {
            break;
        }
        line_number += 1;
        if line.last() != Some(&b'\n') {
            bail!("checkpoint element log has an incomplete line {line_number}; remove only the torn trailing bytes");
        }
        line.pop();
        if line.is_empty() {
            continue;
        }
        let value: Value = serde_json::from_slice(&line).with_context(|| {
            format!("checkpoint element log line {line_number} is malformed or corrupt; remove only that line")
        })?;
        if value.get("kind").and_then(Value::as_str) == Some("commit") {
            if let Some(index) = value.get("index").and_then(Value::as_u64) {
                committed.insert(index);
            }
        }
    }
    Ok(u64::try_from(wanted)
        .ok()
        .is_some_and(|index| committed.contains(&index)))
}

/// Build the summary object for one checkpoint directory.
pub fn summarize(directory: &Path) -> Result<Value> {
    let lock_held = crate::runtime::checkpoint::directory_lock_held(directory)?;
    let journal_path = directory.join("journal.json");
    let journal = read_journal_value(&journal_path)?;
    let data = journal
        .get("data")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow!("checkpoint journal has no data object"))?;
    if data.get("kind").and_then(Value::as_str) == Some("graph") {
        return summarize_graph(directory, &journal, data, lock_held);
    }
    let status = data
        .get("status")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("checkpoint journal has no status"))?;
    // Version 3 journals count completed nodes; older journals list them inline.
    let completed_nodes = match data.get("completed").and_then(Value::as_array) {
        Some(nodes) => nodes.len(),
        None => data
            .get("completed_count")
            .and_then(Value::as_u64)
            .and_then(|count| usize::try_from(count).ok())
            .ok_or_else(|| anyhow!("checkpoint journal has no completed count"))?,
    };
    let in_flight = data
        .get("in_flight")
        .filter(|value| !value.is_null())
        .map(|node| {
            json!({
                "line": node.get("line").cloned().unwrap_or(Value::Null),
                "start": node.get("start").cloned().unwrap_or(Value::Null),
                "end": node.get("end").cloned().unwrap_or(Value::Null),
                "retry_safe": node.get("retry_safe").cloned().unwrap_or(Value::Null),
                "step_id": node.get("step_id").cloned().unwrap_or(Value::Null),
            })
        });
    let error = data.get("error").and_then(Value::as_str).map(str::to_owned);
    let program_path = data
        .get("identity")
        .and_then(|identity| identity.get("program_path"))
        .and_then(Value::as_array)
        .map(|bytes| {
            let raw: Vec<u8> = bytes
                .iter()
                .filter_map(|byte| byte.as_u64().and_then(|value| u8::try_from(value).ok()))
                .collect();
            decode_path(&raw)
        });
    let (blob_count, blob_bytes) = blob_totals(&directory.join("blobs"))?;
    // Issue #536: per-node resolutions recorded with `checkpoint resolve`.
    // Output and bindings stay in the checkpoint; status shows who, when and
    // why. Unresolved uncertain items keep their existing refusal. A torn or
    // duplicate resolutions log still fails closed, but the error names the
    // journal it blocks (status, in-flight step, lock) so recovery is not
    // blind; remove only the damaged line, keep the file 0600, re-run
    // resolve one item at a time. There is no revoke tool.
    let in_flight_step_for_error = in_flight
        .as_ref()
        .and_then(|node| node.get("step_id"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let resolutions = read_status_resolutions(directory).map_err(|error| {
        anyhow!(
            "checkpoint {status} with in-flight step {in_flight_step_for_error:?} (locked={lock_held}): {error:#}"
        )
    })?;
    let journal_marks_elements = data
        .get("in_flight")
        .filter(|value| !value.is_null())
        .is_some_and(|node| {
            node.get("element_kind").is_some_and(|value| {
                value.as_str().is_some_and(|kind| !kind.is_empty()) && !value.is_null()
            }) || node
                .get("element_fingerprint")
                .is_some_and(|value| !value.is_null())
                || node
                    .get("element_start")
                    .is_some_and(|value| !value.is_null())
        });
    let is_element_node = journal_marks_elements || directory.join("elements.jsonl").is_file();
    let in_flight_step = in_flight
        .as_ref()
        .and_then(|node| node.get("step_id"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let pending_uncertain: Vec<u64> = pending_elements(directory)
        .map_err(|error| {
            anyhow!(
                "checkpoint {status} with in-flight step {in_flight_step_for_error:?} (locked={lock_held}): {error:#}"
            )
        })?
        .into_iter()
        .filter(|element| element.uncertain)
        .map(|element| element.index)
        .collect();
    let node_has_resolution = !in_flight_step.is_empty()
        && resolutions.iter().any(|record| {
            record.get("node").and_then(Value::as_str) == Some(in_flight_step)
                && record.get("element").is_none_or(Value::is_null)
        });
    // A node-level record never covers an element node: each uncertain
    // element needs its own record, so unresolved elements are never hidden
    // by a node-level retry.
    let unresolved_elements: Vec<Value> = pending_uncertain
        .iter()
        .filter(|index| {
            !resolutions.iter().any(|record| {
                record.get("node").and_then(Value::as_str) == Some(in_flight_step)
                    && record.get("element").and_then(Value::as_u64) == Some(**index)
            })
        })
        .map(|index| json!({ "node": in_flight_step, "element": index }))
        .collect();
    // The element log belongs to the in-flight node. A pending `for each` element
    // whose body may have started needs the same override as an uncertain node,
    // unless every such element carries its own resolution. This mirrors the
    // runtime's resume gate: per-element records let resume reach the
    // per-element checks, which still refuse every uncovered element by name.
    // `unresolved_uncertain` therefore names begun-but-uncommitted elements; a
    // later element the loop has not reached is gated when its begin runs.
    let element_allows_resume = is_element_node
        && !in_flight_step.is_empty()
        && resolutions.iter().any(|record| {
            record.get("node").and_then(Value::as_str) == Some(in_flight_step)
                && record.get("element").and_then(Value::as_u64).is_some()
        });
    let element_blocks = !matches!(status, "completed" | "idle")
        && in_flight.is_some()
        && !unresolved_elements.is_empty();
    let node_blocks = !matches!(status, "completed" | "idle")
        && in_flight
            .as_ref()
            .is_some_and(|node| node.get("retry_safe").and_then(Value::as_bool) == Some(false))
        && !node_has_resolution
        && (!is_element_node || !element_allows_resume);
    let resume_action = if lock_held {
        "locked"
    } else if node_blocks || element_blocks {
        "requires_retry_uncertain"
    } else {
        resume_action(status, in_flight.as_ref())
    };
    let mut unresolved_uncertain: Vec<Value> = Vec::new();
    // A `for each` node never takes a node-level decision for its elements, so
    // once its begun elements are named the node entry would only duplicate
    // them. The node entry is kept when no begun element names the work: a
    // plain node, or a loop killed before its first begin.
    if node_blocks && (!is_element_node || pending_uncertain.is_empty()) {
        unresolved_uncertain.push(json!({ "node": in_flight_step, "element": Value::Null }));
    }
    unresolved_uncertain.extend(unresolved_elements);
    let checksum = if journal.get("checksum").is_some() {
        "present"
    } else {
        "missing"
    };
    Ok(json!({
        "protocol": PROTOCOL,
        "directory": directory.to_string_lossy(),
        "journal": {
            "version": data.get("version").cloned().unwrap_or(Value::Null),
            "status": status,
            "program_path": program_path,
            "completed_nodes": completed_nodes,
            "in_flight": in_flight,
            "error": error,
            "checksum": checksum,
            "checksum_verified": false,
        },
        "blobs": { "count": blob_count, "bytes": blob_bytes },
        "lock_file_present": directory.join("lock").is_file(),
        "lock_held": lock_held,
        "resume_action": resume_action,
        "resolutions": resolutions,
        "unresolved_uncertain": unresolved_uncertain,
    }))
}

/// One element that began without committing, from the element log.
struct PendingElement {
    index: u64,
    fingerprint: String,
    uncertain: bool,
}

/// Elements that began without committing. A torn final line is refused rather
/// than silently dropped: both status and resolve must fail closed on a
/// damaged log instead of reporting or recording against a partial view.
/// Status does not know the run's output limit, so it reads lines up to the
/// bound a run with the default 1 MiB limit can write, and refuses a longer
/// line.
fn pending_elements(directory: &Path) -> Result<Vec<PendingElement>> {
    let path = directory.join("elements.jsonl");
    let file = match fs::File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).context("reading checkpoint element log"),
    };
    if !file.metadata()?.is_file() {
        bail!("checkpoint element log must be a regular file");
    }
    let max_element_line = crate::runtime::checkpoint::element_line_limit(
        crate::runtime::RunOptions::new(std::path::PathBuf::new()).max_output_bytes,
    );
    let mut reader = std::io::BufReader::new(file);
    let mut line = Vec::new();
    let mut pending = std::collections::BTreeMap::<u64, (String, bool)>::new();
    let mut line_number = 0usize;
    loop {
        line.clear();
        let read =
            crate::runtime::checkpoint::read_bounded_line(&mut reader, &mut line, max_element_line)
                .with_context(|| {
                    format!(
                        "reading checkpoint element log after {} pending entr(y/ies)",
                        pending.len()
                    )
                })?;
        if read == 0 {
            break;
        }
        line_number += 1;
        if line.last() != Some(&b'\n') {
            bail!("checkpoint element log has an incomplete line {line_number} after {} pending entr(y/ies); remove only the torn trailing bytes", pending.len());
        }
        line.pop();
        if line.is_empty() {
            continue;
        }
        let value: Value = serde_json::from_slice(&line).with_context(|| {
            format!(
                "checkpoint element log line {line_number} is malformed or corrupt; remove only that line"
            )
        })?;
        let kind = value.get("kind").and_then(Value::as_str).unwrap_or("");
        let index = value
            .get("index")
            .and_then(Value::as_u64)
            .ok_or_else(|| anyhow!("checkpoint element log has an invalid element index"))?;
        match kind {
            "begin" => {
                let uncertain = value
                    .get("uncertain")
                    .and_then(Value::as_bool)
                    .ok_or_else(|| anyhow!("checkpoint element log has an invalid begin record"))?;
                let fingerprint = value
                    .get("fingerprint")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow!("checkpoint element log has an invalid begin record"))?;
                pending.insert(index, (fingerprint.to_owned(), uncertain));
            }
            "commit" => {
                pending.remove(&index);
            }
            _ => bail!("checkpoint element log has an unknown record kind"),
        }
    }
    Ok(pending
        .into_iter()
        .map(|(index, (fingerprint, uncertain))| PendingElement {
            index,
            fingerprint,
            uncertain,
        })
        .collect())
}

/// The `checkpoint status --json` view of every recorded resolution: who
/// decided what, when and why. Output, bindings and fingerprints stay in the
/// checkpoint; they are evidence for resume, not status display.
fn read_status_resolutions(directory: &Path) -> Result<Vec<Value>> {
    let bound = crate::runtime::RunOptions::new(std::path::PathBuf::new()).max_output_bytes;
    let records = resolutions::read_resolutions(directory, bound)?;
    Ok(records.iter().map(|record| record.status_value()).collect())
}

/// The status of a graph checkpoint (journal mode `graph`): committed nodes and the
/// nodes in flight. A graph journal has no program status or snapshot.
fn summarize_graph(
    directory: &Path,
    journal: &Value,
    data: &serde_json::Map<String, Value>,
    lock_held: bool,
) -> Result<Value> {
    let completed_nodes = data
        .get("completed_count")
        .and_then(Value::as_u64)
        .ok_or_else(|| anyhow!("graph checkpoint has no completed count"))?;
    let in_flight: Vec<Value> = data
        .get("in_flight")
        .and_then(Value::as_array)
        .map(|nodes| {
            nodes
                .iter()
                .map(|node| {
                    json!({
                        "node": node.get("node").cloned().unwrap_or(Value::Null),
                        "retry_safe": node.get("retry_safe").cloned().unwrap_or(Value::Null),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let (blob_count, blob_bytes) = blob_totals(&directory.join("blobs"))?;
    let action = if lock_held {
        "locked"
    } else if in_flight.is_empty() {
        "continues_from_last_completed_node"
    } else if in_flight
        .iter()
        .any(|node| node["retry_safe"] != json!(true))
    {
        "requires_retry_uncertain"
    } else {
        "retries_in_flight_node"
    };
    Ok(json!({
        "protocol": PROTOCOL,
        "directory": directory.to_string_lossy(),
        "journal": {
            "kind": "graph",
            "version": data.get("version").cloned().unwrap_or(Value::Null),
            "completed_nodes": completed_nodes,
            "in_flight": in_flight,
            "checksum": if journal.get("checksum").is_some() { "present" } else { "missing" },
            "checksum_verified": false,
        },
        "blobs": { "count": blob_count, "bytes": blob_bytes },
        "lock_file_present": directory.join("lock").is_file(),
        "lock_held": lock_held,
        "resume_action": action,
        // Graph checkpoints refuse resolutions; the fields stay present so the
        // status shape is the same for both journal kinds.
        "resolutions": [],
        "unresolved_uncertain": [],
    }))
}

/// What `execute resume` would do with this journal, from its status alone.
fn resume_action(status: &str, in_flight: Option<&Value>) -> &'static str {
    let retry_safe = in_flight
        .and_then(|node| node.get("retry_safe"))
        .and_then(Value::as_bool);
    match (status, in_flight, retry_safe) {
        ("completed", _, _) => "restores_completed_result",
        ("idle", _, _) => "idle_no_action",
        ("failed" | "interrupted" | "running", Some(_), Some(false)) => "requires_retry_uncertain",
        (_, Some(_), _) => "retries_in_flight_node",
        ("running", None, _) => "may_be_active_or_crashed",
        _ => "continues_from_last_completed_node",
    }
}

fn read_journal_value(path: &Path) -> Result<Value> {
    let metadata = fs::metadata(path)
        .with_context(|| format!("no checkpoint journal at {}", path.display()))?;
    if !metadata.is_file() {
        bail!(
            "checkpoint journal is not a regular file: {}",
            path.display()
        );
    }
    if metadata.len() > MAX_JOURNAL_BYTES {
        bail!("checkpoint journal is too large");
    }
    let mut bytes = Vec::new();
    fs::File::open(path)
        .with_context(|| format!("reading checkpoint journal {}", path.display()))?
        .take(MAX_JOURNAL_BYTES + 1)
        .read_to_end(&mut bytes)?;
    serde_json::from_slice(&bytes).context("checkpoint journal is malformed or corrupt")
}

/// Decode a stored path for display. The runtime stores Windows paths as
/// UTF-16LE and Unix paths as raw bytes, so the decoding follows the platform
/// running this command. A journal from another platform may show replacement
/// characters. The `\\?\` verbatim prefix is removed for display only.
fn decode_path(bytes: &[u8]) -> String {
    let decoded = if cfg!(windows) {
        let units: Vec<u16> = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u16::from_le_bytes(*pair))
            .collect();
        String::from_utf16_lossy(&units)
    } else {
        String::from_utf8_lossy(bytes).into_owned()
    };
    decoded
        .strip_prefix(r"\\?\")
        .map(str::to_owned)
        .unwrap_or(decoded)
}

fn blob_totals(directory: &Path) -> Result<(usize, u64)> {
    if !directory.is_dir() {
        return Ok((0, 0));
    }
    let mut count = 0;
    let mut bytes = 0;
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let metadata = entry.metadata()?;
        if metadata.is_file() {
            count += 1;
            bytes += metadata.len();
        }
    }
    Ok((count, bytes))
}

fn render_text(summary: &Value) -> String {
    let journal = &summary["journal"];
    let mut text = String::new();
    text.push_str(&format!(
        "directory: {}\n",
        summary["directory"].as_str().unwrap_or("")
    ));
    text.push_str(&format!(
        "status: {}\n",
        journal["status"].as_str().unwrap_or("")
    ));
    text.push_str(&format!(
        "completed nodes: {}\n",
        journal["completed_nodes"].as_u64().unwrap_or(0)
    ));
    match journal.get("in_flight").filter(|value| !value.is_null()) {
        Some(node) => text.push_str(&format!(
            "in flight: lines {} steps {}..{} retry_safe={}\n",
            node["line"], node["start"], node["end"], node["retry_safe"]
        )),
        None => text.push_str("in flight: none\n"),
    }
    if let Some(error) = journal["error"].as_str() {
        text.push_str(&format!("error: {error}\n"));
    }
    text.push_str(&format!(
        "blobs: {} files, {} bytes\n",
        summary["blobs"]["count"], summary["blobs"]["bytes"]
    ));
    text.push_str("checksum: present, not verified by this command\n");
    text.push_str(&format!(
        "resume action: {}\n",
        summary["resume_action"].as_str().unwrap_or("")
    ));
    let resolutions = summary["resolutions"].as_array();
    text.push_str(&format!(
        "resolutions: {}\n",
        resolutions.map_or(0, Vec::len)
    ));
    for record in resolutions.into_iter().flatten() {
        let element = record
            .get("element")
            .and_then(Value::as_u64)
            .map_or(String::new(), |index| format!(" element={index}"));
        text.push_str(&format!(
            "  {} node={}{} actor={} at={} reason={}\n",
            record["decision"].as_str().unwrap_or("?"),
            record["node"].as_str().unwrap_or("?"),
            element,
            record["actor"].as_str().unwrap_or("?"),
            record["at"].as_u64().unwrap_or(0),
            record["reason"].as_str().unwrap_or("")
        ));
    }
    let unresolved = summary["unresolved_uncertain"].as_array();
    text.push_str(&format!(
        "unresolved uncertain: {}\n",
        unresolved.map_or(0, Vec::len)
    ));
    for item in unresolved.into_iter().flatten() {
        match item.get("element").and_then(Value::as_u64) {
            Some(index) => text.push_str(&format!(
                "  node={} element={index}\n",
                item["node"].as_str().unwrap_or("?")
            )),
            None => text.push_str(&format!(
                "  node={}\n",
                item["node"].as_str().unwrap_or("?")
            )),
        }
    }
    text
}

#[cfg(test)]
mod resume_action_tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn failed_uncertain_node_is_reported_as_requiring_override() {
        let in_flight = json!({"retry_safe": false});
        assert_eq!(
            resume_action("failed", Some(&in_flight)),
            "requires_retry_uncertain"
        );
    }

    #[test]
    fn status_reads_pending_element_uncertainty() {
        let directory = tempfile::tempdir().unwrap();
        let log = directory.path().join("elements.jsonl");
        fs::write(
            &log,
            b"{\"kind\":\"begin\",\"index\":2,\"fingerprint\":\"fp\",\"uncertain\":true}\n",
        )
        .unwrap();
        let pending = pending_elements(directory.path()).unwrap();
        assert_eq!(pending.len(), 1);
        assert!(pending[0].uncertain);
        fs::OpenOptions::new()
            .append(true)
            .open(&log)
            .unwrap()
            .write_all(b"{\"kind\":\"commit\",\"index\":2}\n")
            .unwrap();
        assert!(pending_elements(directory.path()).unwrap().is_empty());
    }
}
