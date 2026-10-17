//! `execute checkpoint status DIR [--json]`: a read-only summary of a checkpoint.
//!
//! The command never takes the checkpoint lock, never writes, and never runs a
//! program. It reads `journal.json` and the blobs directory and reports what a
//! resume would do. It does not verify the journal checksum, because that check
//! depends on the runtime's private serialization. The output says so.

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::fs;
use std::io::Read;
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
        println!("{}", serde_json::to_string_pretty(&summary)?);
    } else {
        print!("{}", render_text(&summary));
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
    let status = data
        .get("status")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("checkpoint journal has no status"))?;
    let completed_nodes = data
        .get("completed")
        .and_then(Value::as_array)
        .map(Vec::len)
        .ok_or_else(|| anyhow!("checkpoint journal has no completed list"))?;
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
    let resume_action = resume_action(status, in_flight.as_ref());
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

/// What `execute resume` would do with this journal, from its status alone.
fn resume_action(status: &str, in_flight: Option<&Value>) -> &'static str {
    let retry_safe = in_flight
        .and_then(|node| node.get("retry_safe"))
        .and_then(Value::as_bool);
    match (status, in_flight, retry_safe) {
        ("completed", _, _) => "restores_completed_result",
        ("idle", _, _) => "idle_no_action",
        (_, Some(_), Some(false)) => "requires_retry_uncertain",
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
