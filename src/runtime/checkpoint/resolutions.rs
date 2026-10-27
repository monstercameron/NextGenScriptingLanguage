//! Per-node uncertain-effect resolutions for issue #536.
//!
//! `execute checkpoint resolve DIR --node STEP_ID [--element N]
//! --as committed|retry [--output FILE] [--reason TEXT]` appends one record to
//! `resolutions.jsonl`. A `committed` record carries the operator-verified output
//! and bindings that later nodes read; a `retry` record explicitly allows one
//! uncertain node or element to run again without the bulk `--retry-uncertain`
//! flag. Element indices (`--element N`) are the 0-based indices used by the
//! element log and by existing diagnostics (`for each element 2` is the third
//! entry). Resume consults these records; `checkpoint status --json` lists them
//! together with the still-unresolved uncertain items.

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, Write};
use std::path::Path;

use crate::ast::Value;

/// Append-only log holding operator resolutions.
pub(super) const RESOLUTIONS_LOG: &str = "resolutions.jsonl";
/// Version of every resolution record.
pub(crate) const RESOLUTION_VERSION: u32 = 1;
/// Most resolutions one checkpoint keeps. A checkpoint holds few uncertain
/// nodes; the bound keeps status and resume reads finite.
const MAX_RESOLUTIONS: usize = 1024;
/// Longest reason text, in bytes.
const MAX_REASON_BYTES: usize = 4096;
/// Longest step id or actor name, in bytes.
const MAX_ID_BYTES: usize = 512;
/// Most bindings one committed resolution carries.
const MAX_RESOLUTION_BINDINGS: usize = 256;
/// Longest single binding name, in bytes.
const MAX_BINDING_NAME_BYTES: usize = 256;
/// Largest element index a resolution may name. Top-level loops are bounded by
/// `max_loop_iterations` (default one million); resolutions stay within an
/// order of magnitude above that so a corrupt log cannot name Absurd indices.
const MAX_ELEMENT_INDEX: usize = 10_000_000;
/// Longest resolution line read back. Writers stay below it: output is hex and
/// bounded by the run's output limit, and bindings are bounded per value.
const MAX_RESOLUTION_LINE: usize = 16 * 1024 * 1024;

/// Operator decision for one uncertain node or element.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ResolutionDecision {
    Committed,
    Retry,
}

impl ResolutionDecision {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Committed => "committed",
            Self::Retry => "retry",
        }
    }
}

/// One operator resolution, as stored in `resolutions.jsonl`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ResolutionRecord {
    pub(crate) version: u32,
    pub(crate) node: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) element: Option<usize>,
    pub(crate) decision: ResolutionDecision,
    #[serde(default)]
    pub(crate) reason: String,
    #[serde(default)]
    pub(crate) actor: String,
    pub(crate) at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) output: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) bindings: Option<BTreeMap<String, Value>>,
    /// Element fingerprint captured from the element log's begin record for a
    /// committed element. Resume recomputes it from the current list entry and
    /// refuses when the list changed. Absent for node-level records and retry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) fingerprint: Option<String>,
}

impl ResolutionRecord {
    /// Key used for duplicate detection: node plus optional element.
    pub(crate) fn key(&self) -> (String, Option<usize>) {
        (self.node.clone(), self.element)
    }

    /// Decoded output bytes. Committed records always carry output (possibly
    /// empty); retry records carry none.
    pub(crate) fn output_bytes(&self, max_output_bytes: usize) -> Result<Vec<u8>> {
        let hex = self.output.as_deref().unwrap_or("");
        let bytes = hex_decode(hex).context("checkpoint resolution output is malformed")?;
        if bytes.len() > max_output_bytes {
            bail!(
                "checkpoint resolution output exceeds maximum of {max_output_bytes} bytes"
            );
        }
        Ok(bytes)
    }

    /// Bindings carried by a committed record (possibly empty).
    pub(crate) fn bindings_map(&self, max_output_bytes: usize) -> Result<BTreeMap<String, Value>> {
        let bindings = self.bindings.clone().unwrap_or_default();
        if bindings.len() > MAX_RESOLUTION_BINDINGS {
            bail!("checkpoint resolution carries too many bindings");
        }
        let encoded_limit = max_output_bytes.saturating_mul(6).saturating_add(16);
        for (name, value) in &bindings {
            if name.len() > MAX_BINDING_NAME_BYTES {
                bail!("checkpoint resolution binding name is too long");
            }
            let encoded =
                serde_json::to_vec(value).context("encoding checkpoint resolution binding")?;
            if encoded.len() > encoded_limit {
                bail!(
                    "checkpoint resolution binding {name:?} exceeds maximum of {max_output_bytes} bytes"
                );
            }
        }
        Ok(bindings)
    }

    /// The `checkpoint status --json` view of this record: who decided what,
    /// when and why. Output, bindings and fingerprints stay in the checkpoint;
    /// they are evidence for resume, not status display.
    pub(crate) fn status_value(&self) -> serde_json::Value {
        serde_json::json!({
            "node": self.node,
            "element": self.element,
            "decision": self.decision.as_str(),
            "reason": self.reason,
            "actor": self.actor,
            "at": self.at,
        })
    }
}

/// Read and validate every resolution in a checkpoint directory. A missing log
/// means no resolutions. Each line is bounded; a torn final line from an
/// interrupted append is refused rather than silently dropped.
pub(crate) fn read_resolutions(
    directory: &Path,
    max_output_bytes: usize,
) -> Result<Vec<ResolutionRecord>> {
    let path = directory.join(RESOLUTIONS_LOG);
    let file = match fs::File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).context("reading checkpoint resolutions"),
    };
    if !file.metadata()?.is_file() {
        bail!("checkpoint resolutions must be a regular file");
    }
    let mut reader = std::io::BufReader::new(file);
    let mut line = Vec::new();
    let mut records = Vec::new();
    loop {
        line.clear();
        let read = crate::runtime::checkpoint::read_bounded_line(
            &mut reader,
            &mut line,
            MAX_RESOLUTION_LINE,
        )
        .context("reading checkpoint resolutions")?;
        if read == 0 {
            break;
        }
        if line.last() != Some(&b'\n') {
            bail!("checkpoint resolutions has an incomplete line");
        }
        line.pop();
        if line.is_empty() {
            continue;
        }
        let record: ResolutionRecord = serde_json::from_slice(&line)
            .context("checkpoint resolutions is malformed or corrupt")?;
        validate_record(&record, max_output_bytes)?;
        records.push(record);
        if records.len() > MAX_RESOLUTIONS {
            bail!("checkpoint has too many resolutions");
        }
    }
    let mut seen = std::collections::BTreeSet::new();
    for record in &records {
        if !seen.insert(record.key()) {
            bail!("checkpoint has a duplicate resolution for its node");
        }
    }
    Ok(records)
}

fn validate_record(record: &ResolutionRecord, max_output_bytes: usize) -> Result<()> {
    if record.version != RESOLUTION_VERSION {
        bail!("unsupported checkpoint resolution version");
    }
    if record.node.is_empty() || record.node.len() > MAX_ID_BYTES {
        bail!("checkpoint resolution node id is invalid");
    }
    if let Some(index) = record.element {
        if index > MAX_ELEMENT_INDEX {
            bail!("checkpoint resolution element index is too large");
        }
    }
    if record.reason.len() > MAX_REASON_BYTES {
        bail!("checkpoint resolution reason is too long");
    }
    if record.actor.len() > MAX_ID_BYTES {
        bail!("checkpoint resolution actor is too long");
    }
    if let Some(fingerprint) = &record.fingerprint {
        if fingerprint.is_empty() || fingerprint.len() > 128 {
            bail!("checkpoint resolution fingerprint is invalid");
        }
        if record.element.is_none() {
            bail!("only element resolutions carry a fingerprint");
        }
    }
    match record.decision {
        ResolutionDecision::Retry => {
            if record.output.is_some() || record.bindings.is_some() || record.fingerprint.is_some()
            {
                bail!("a retry resolution carries no output, bindings, or fingerprint");
            }
        }
        ResolutionDecision::Committed => {
            if record.element.is_some() && record.fingerprint.is_none() {
                bail!("a committed element resolution requires the element fingerprint");
            }
            let hex = record.output.as_deref().ok_or_else(|| {
                anyhow!("a committed resolution requires output (or explicitly empty output)")
            })?;
            if hex.len() > max_output_bytes.saturating_mul(2) {
                bail!("checkpoint resolution output exceeds the output limit");
            }
            // Hex validity and binding bounds are enforced by the accessors.
            let _ = record.output_bytes(max_output_bytes)?;
            let _ = record.bindings_map(max_output_bytes)?;
        }
    }
    Ok(())
}

/// Append one validated resolution. The caller must hold no run lock; this
/// function refuses when the checkpoint is locked by a running process.
/// Concurrent resolves are serialized by this check only in the fail-closed
/// sense: two resolves for the same node both pass validation, both append,
/// and the log then refuses every resume with a duplicate record until the
/// operator removes one. Resolve one item at a time.
pub(crate) fn append_resolution(directory: &Path, record: &ResolutionRecord) -> Result<()> {
    if crate::runtime::checkpoint::directory_lock_held(directory)? {
        bail!("checkpoint is locked by a running process; stop it before resolving");
    }
    let existing = read_resolutions(directory, usize::MAX)?;
    if existing.len() >= MAX_RESOLUTIONS {
        bail!("checkpoint has too many resolutions");
    }
    if existing.iter().any(|known| known.key() == record.key()) {
        bail!("a resolution is already recorded for this node; it is not overwritten");
    }
    // Validate against a finite bound even when the resuming run uses a larger
    // output limit; resume re-validates against its own limit.
    validate_record(record, 1024 * 1024)?;
    let mut line = serde_json::to_vec(record).context("encoding checkpoint resolution")?;
    if line.len() + 1 > MAX_RESOLUTION_LINE {
        bail!("checkpoint resolution is too large");
    }
    line.push(b'\n');
    let path = directory.join(RESOLUTIONS_LOG);
    let mut options = fs::OpenOptions::new();
    options.create(true).append(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&path)
        .with_context(|| format!("appending {}", path.display()))?;
    crate::runtime::checkpoint::restrict_file(&file)?;
    file.write_all(&line)
        .context("appending checkpoint resolution")?;
    file.sync_all()
        .context("syncing checkpoint resolution")?;
    Ok(())
}

/// Find the resolution for one node-level uncertain item.
pub(crate) fn node_resolution<'a>(
    resolutions: &'a [ResolutionRecord],
    step_id: &str,
) -> Option<&'a ResolutionRecord> {
    resolutions
        .iter()
        .find(|record| record.element.is_none() && record.node == step_id)
}

/// Find the resolution for one element of a node.
pub(crate) fn element_resolution<'a>(
    resolutions: &'a [ResolutionRecord],
    step_id: &str,
    index: usize,
) -> Option<&'a ResolutionRecord> {
    resolutions.iter().find(|record| {
        record.node == step_id && record.element.is_some_and(|element| element == index)
    })
}

/// Current operator name for a new resolution record.
pub(crate) fn current_actor() -> String {
    for key in ["USER", "USERNAME", "LOGNAME"] {
        if let Ok(name) = std::env::var(key) {
            let name = name.trim().to_owned();
            if !name.is_empty() {
                return name.chars().take(128).collect();
            }
        }
    }
    "operator".to_owned()
}

/// Current Unix time in seconds for a new resolution record.
pub(crate) fn current_time_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

/// Hex-encode bytes for a committed resolution's output.
pub(crate) fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut text, byte| {
            let _ = write!(text, "{byte:02x}");
            text
        })
}

/// Decode a resolution's hex output.
fn hex_decode(text: &str) -> Result<Vec<u8>> {
    if !text.is_ascii() || !text.len().is_multiple_of(2) {
        bail!("checkpoint resolution output is malformed");
    }
    (0..text.len())
        .step_by(2)
        .map(|index| {
            u8::from_str_radix(&text[index..index + 2], 16)
                .context("checkpoint resolution output is malformed")
        })
        .collect()
}

/// Parse one `--binding NAME=VALUE` argument. VALUE is JSON when it parses as
/// JSON, otherwise plain text.
pub(crate) fn parse_binding(argument: &str) -> Result<(String, Value)> {
    let (name, value) = argument
        .split_once('=')
        .context("--binding requires NAME=VALUE")?;
    if name.is_empty() || name.len() > MAX_BINDING_NAME_BYTES {
        bail!("--binding name is invalid");
    }
    let value = serde_json::from_str::<serde_json::Value>(value)
        .map(|json| json_to_value(&json))
        .unwrap_or_else(|_| Value::String(value.to_owned()));
    Ok((name.to_owned(), value))
}

fn json_to_value(json: &serde_json::Value) -> Value {
    match json {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(value) => Value::Bool(*value),
        serde_json::Value::Number(value) => {
            if let Some(int) = value.as_i64() {
                Value::Integer(int)
            } else {
                Value::Number(value.to_string())
            }
        }
        serde_json::Value::String(value) => Value::String(value.clone()),
        serde_json::Value::Array(items) => Value::List(items.iter().map(json_to_value).collect()),
        serde_json::Value::Object(fields) => Value::Record(
            fields
                .iter()
                .map(|(key, item)| (key.clone(), json_to_value(item)))
                .collect(),
        ),
    }
}

/// Read output bytes for `--output FILE`, bounded by the output limit.
pub(crate) fn read_output_file(path: &Path, max_output_bytes: usize) -> Result<Vec<u8>> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("reading --output {}", path.display()))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        bail!("--output must be a regular file");
    }
    if metadata.len() > max_output_bytes as u64 {
        bail!("--output exceeds maximum of {max_output_bytes} bytes");
    }
    let bytes = fs::read(path).with_context(|| format!("reading --output {}", path.display()))?;
    if bytes.len() > max_output_bytes {
        bail!("--output exceeds maximum of {max_output_bytes} bytes");
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(node: &str, element: Option<usize>, decision: ResolutionDecision) -> ResolutionRecord {
        ResolutionRecord {
            version: RESOLUTION_VERSION,
            node: node.to_owned(),
            element,
            decision,
            reason: "checked".to_owned(),
            actor: "tester".to_owned(),
            at: 1,
            output: match decision {
                ResolutionDecision::Committed => Some(String::new()),
                ResolutionDecision::Retry => None,
            },
            bindings: match decision {
                ResolutionDecision::Committed => Some(BTreeMap::new()),
                ResolutionDecision::Retry => None,
            },
            fingerprint: None,
        }
    }

    #[test]
    fn retry_record_carries_no_output_or_bindings() {
        let mut bad = record("node:1", None, ResolutionDecision::Retry);
        bad.output = Some(String::new());
        assert!(validate_record(&bad, 1024).is_err());
    }

    #[test]
    fn committed_record_requires_output() {
        let mut bad = record("node:1", None, ResolutionDecision::Committed);
        bad.output = None;
        assert!(validate_record(&bad, 1024).is_err());
    }

    #[test]
    fn duplicate_resolutions_are_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let first = record("step:abc#1", Some(0), ResolutionDecision::Retry);
        append_resolution(directory.path(), &first).unwrap();
        let error = append_resolution(directory.path(), &first).unwrap_err();
        assert!(error.to_string().contains("already recorded"));
        let kept = read_resolutions(directory.path(), 1024 * 1024).unwrap();
        assert_eq!(kept.len(), 1);
    }

    #[test]
    fn oversized_reason_is_rejected() {
        let mut bad = record("node:1", None, ResolutionDecision::Retry);
        bad.reason = "x".repeat(MAX_REASON_BYTES + 1);
        assert!(validate_record(&bad, 1024).is_err());
    }

    #[test]
    fn binding_text_falls_back_to_plain_string() {
        let (name, value) = parse_binding("greeting=hello").unwrap();
        assert_eq!(name, "greeting");
        assert_eq!(value, Value::String("hello".to_owned()));
        let (_, value) = parse_binding("count=3").unwrap();
        assert_eq!(value, Value::Integer(3));
    }
}
