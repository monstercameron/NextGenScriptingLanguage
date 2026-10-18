//! `execute checkpoint status DIR [--json]`: a read-only summary of a checkpoint.
//!
//! The command never takes the checkpoint lock, never writes, and never runs a
//! program. It reads `journal.json` and the blobs directory and reports what a
//! resume would do. It does not verify the journal checksum, because that check
//! depends on the runtime's private serialization. The output says so.

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::fs;
use std::io::{self, Read, Write};
use std::path::Path;

/// Protocol identifier of the `--json` object.
pub const PROTOCOL: &str = "execute.checkpoint_status/1";

const MAX_JOURNAL_BYTES: u64 = 16 * 1024 * 1024;
const USAGE: &str = "usage: execute checkpoint status DIR [--json]";

/// Run the `checkpoint` subcommand and return the process exit status.
pub fn cli(arguments: &[String]) -> Result<i32> {
    match arguments.first().map(String::as_str) {
        Some("status") => status_cli(&arguments[1..]),
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

/// Build the summary object for one checkpoint directory.
pub fn summarize(directory: &Path) -> Result<Value> {
    let journal_path = directory.join("journal.json");
    let journal = read_journal_value(&journal_path)?;
    let data = journal
        .get("data")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow!("checkpoint journal has no data object"))?;
    if data.get("kind").and_then(Value::as_str) == Some("graph") {
        return summarize_graph(directory, &journal, data);
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
    // The element log belongs to the in-flight node. A pending `for each` element
    // whose body may have started needs the same override as an uncertain node.
    let resume_action = if !matches!(status, "completed" | "idle")
        && in_flight.is_some()
        && pending_element_is_uncertain(directory)?
    {
        "requires_retry_uncertain"
    } else {
        resume_action(status, in_flight.as_ref())
    };
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
        "resume_action": resume_action,
    }))
}

fn pending_element_is_uncertain(directory: &Path) -> Result<bool> {
    let path = directory.join("elements.jsonl");
    let file = match fs::File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error).context("reading checkpoint element log for status"),
    };
    // Status does not know the run's output limit, so it reads lines up to the bound
    // a run with the default 1 MiB limit can write, and refuses a longer line.
    let max_element_line = crate::runtime::checkpoint::element_line_limit(
        crate::runtime::RunOptions::new(std::path::PathBuf::new()).max_output_bytes,
    );
    let mut reader = std::io::BufReader::new(file);
    let mut line = Vec::new();
    let mut pending = std::collections::BTreeMap::<u64, bool>::new();
    loop {
        line.clear();
        let read =
            crate::runtime::checkpoint::read_bounded_line(&mut reader, &mut line, max_element_line)
                .context("reading checkpoint element log for status")?;
        if read == 0 {
            break;
        }
        if line.last() != Some(&b'\n') {
            break;
        }
        line.pop();
        let value: Value = serde_json::from_slice(&line)
            .context("checkpoint element log is malformed or corrupt")?;
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
                pending.insert(index, uncertain);
            }
            "commit" => {
                pending.remove(&index);
            }
            _ => bail!("checkpoint element log has an unknown record kind"),
        }
    }
    Ok(pending.values().any(|uncertain| *uncertain))
}

/// The status of a graph checkpoint (journal mode `graph`): committed nodes and the
/// nodes in flight. A graph journal has no program status or snapshot.
fn summarize_graph(
    directory: &Path,
    journal: &Value,
    data: &serde_json::Map<String, Value>,
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
    let action = if in_flight.is_empty() {
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
        "resume_action": action,
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
            b"{\"kind\":\"begin\",\"index\":2,\"uncertain\":true}\n",
        )
        .unwrap();
        assert!(pending_element_is_uncertain(directory.path()).unwrap());
        fs::OpenOptions::new()
            .append(true)
            .open(&log)
            .unwrap()
            .write_all(b"{\"kind\":\"commit\",\"index\":2}\n")
            .unwrap();
        assert!(!pending_element_is_uncertain(directory.path()).unwrap());
    }
}
