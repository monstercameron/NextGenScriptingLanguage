//! Opt-in run records: `execute run --record DIR` keeps a bounded, redacted account
//! of one finite run. `execute show-run DIR` summarises it, and `execute replay DIR`
//! reruns the program against it (see `replay`).
//!
//! Layout of a record directory:
//!
//! - `run.json`: protocol `execute.record/1`. Program hash, plan (explain text) and
//!   its hash, redacted inputs, timing, the final result and output hash, and the
//!   limits that applied.
//! - `program.program`: a copy of the program as it ran.
//! - `trace.jsonl`: one JSON object per applet group, in execution order, with the
//!   resolved argv, status, declared outcome, wall time, and the stdout and stderr
//!   byte counts, SHA-256 hashes and blob names.
//! - `blobs/<sha256>`: the stdout and stderr bytes, content-addressed. Blobs are
//!   written after redaction, so the hash matches the stored bytes.
//!
//! Nothing is written unless a record directory is requested. The directory is
//! created private (mode 0700 on Unix, files 0600). Redaction covers every value
//! that the data-flow policy declares secret (see `security`). It is exact-value
//! redaction, not detection, so an undeclared secret is recorded as it was.

use crate::manifest::Registry;
use crate::runtime::observe::{Observer, StepRequest, StepResult};
use crate::security::{self, PersistenceKind, SecretRedactor, SensitivitySet};
use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value as Json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const PROTOCOL: &str = "execute.record/1";
pub const PROGRAM_FILE: &str = "program.program";
pub const RUN_FILE: &str = "run.json";
pub const TRACE_FILE: &str = "trace.jsonl";
pub const BLOB_DIR: &str = "blobs";
/// Total bytes of captured output kept across all blobs. Steps beyond this keep
/// their hashes and counts but no bytes, and the record is marked truncated.
pub const MAX_BLOB_BYTES: usize = 16 * 1024 * 1024;
/// Longest program copy a record will keep.
pub const MAX_PROGRAM_BYTES: usize = 1024 * 1024;
/// Longest explain text stored in the record.
pub const MAX_PLAN_BYTES: usize = 64 * 1024;
/// Stderr kept inline with each step, from its end.
pub const STDERR_TAIL_BYTES: usize = 2048;

pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// The final state of a recorded run, written to `run.json`.
#[derive(Debug, Clone)]
pub struct RunSummary {
    pub ok: bool,
    pub exit_status: Option<i32>,
    pub error: Option<String>,
    pub output: Vec<u8>,
}

/// What a record needs to know about a run before it starts.
pub struct RecordStart<'a> {
    pub directory: &'a Path,
    pub program_source: &'a [u8],
    pub plan: &'a str,
    /// Inputs as JSON; string values are redacted before they are stored.
    pub inputs: Json,
    pub registry: &'a Registry,
}

/// Writes one finite run's record. Install it with `runtime::observe::install`.
pub struct Recorder {
    directory: PathBuf,
    redactor: Arc<SecretRedactor>,
    /// Consulted for the data-flow persistence policy before any output is kept.
    registry: Registry,
    started: Instant,
    started_unix_ms: u128,
    plan_sha256: String,
    plan_text: String,
    inputs: Json,
    declared_secret_names: Vec<String>,
    state: Mutex<RecorderState>,
}

struct RecorderState {
    steps: usize,
    blob_bytes: usize,
    truncated: bool,
    trace: File,
    /// Raw streamed stdout, in order, bounded by `MAX_BLOB_BYTES`. It is redacted
    /// as a whole when the record is finished, so a secret split across two
    /// streamed writes is still found.
    terminal: Vec<u8>,
    /// Union of the sensitivity labels of every observed step.
    labels: SensitivitySet,
    /// Whether any observed applet received declared secret environment.
    secret_environment: bool,
}

/// The plan as it is stored: redacted in full, then truncated, and hashed after
/// redaction. Redacting before truncation keeps a secret that straddles the limit
/// from surviving as a fragment, and hashing the redacted plan means the hash cannot
/// confirm a guess at a secret.
fn prepare_plan(redactor: &SecretRedactor, plan: &str) -> (String, String) {
    let redacted = redactor.redact_text(plan);
    let sha256 = sha256_hex(redacted.as_bytes());
    (truncate_text(&redacted, MAX_PLAN_BYTES), sha256)
}

/// Whether the data-flow persistence policy lets output with these labels be kept
/// in a record. Output of an applet that receives declared secret environment is
/// never kept, whatever the policy says.
fn persistence_allowed(
    registry: &Registry,
    labels: &SensitivitySet,
    secret_environment: bool,
) -> bool {
    !secret_environment
        && security::authorize_persistence_set(
            registry,
            "workflow.record",
            PersistenceKind::Record,
            labels,
        )
        .is_ok()
}

impl Recorder {
    /// Create the record directory and store the program copy. Fails if the
    /// directory exists and is not empty, so a record is never mixed with another.
    pub fn start(start: RecordStart<'_>) -> Result<Self> {
        if start.program_source.len() > MAX_PROGRAM_BYTES {
            bail!("program is larger than the {MAX_PROGRAM_BYTES}-byte record limit");
        }
        create_private_directory(start.directory)?;
        write_private(&start.directory.join(PROGRAM_FILE), start.program_source)?;
        let trace = create_private(&start.directory.join(TRACE_FILE))?;
        // Declared secret values are registered before any step runs, so they are
        // redacted from the first byte written.
        let environment = std::env::vars_os().collect::<Vec<_>>();
        security::register_declared_environment_secrets(start.registry, &environment)
            .map_err(|error| anyhow!("registering declared secrets: {error}"))?;
        let redactor = security::secret_redactor(start.registry);
        let (plan_text, plan_sha256) = prepare_plan(&redactor, start.plan);
        Ok(Self {
            directory: start.directory.to_path_buf(),
            registry: start.registry.clone(),
            started: Instant::now(),
            started_unix_ms: unix_ms(),
            plan_sha256,
            plan_text,
            inputs: redact_json(&redactor, &start.inputs),
            declared_secret_names: security::declared_secret_environment_names(start.registry),
            redactor,
            state: Mutex::new(RecorderState {
                steps: 0,
                blob_bytes: 0,
                truncated: false,
                trace,
                terminal: Vec::new(),
                labels: SensitivitySet::empty(),
                secret_environment: false,
            }),
        })
    }

    /// Write `run.json` for a finished run. Call after the run, whatever its result.
    pub fn finish(&self, summary: &RunSummary) -> Result<()> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.trace.flush()?;
        // The terminal stream is the streamed output followed by the final value,
        // which the CLI prints after the run. Both are redacted as whole buffers.
        let mut raw_terminal = state.terminal.clone();
        raw_terminal.extend_from_slice(&summary.output);
        let terminal = self.redactor.redact_bytes(&raw_terminal);
        let output_bytes = self.redactor.redact_bytes(&summary.output);
        let persist = persistence_allowed(&self.registry, &state.labels, state.secret_environment);
        let output = self.blob(&mut state, &output_bytes, persist)?;
        let terminal = self.blob(&mut state, &terminal, persist)?;
        let error = summary
            .error
            .as_ref()
            .map(|error| self.redactor.redact_text(error));
        let document = json!({
            "protocol": PROTOCOL,
            "program": {
                "file": PROGRAM_FILE,
                "sha256": sha256_hex(&fs::read(self.directory.join(PROGRAM_FILE))?),
            },
            "plan": {
                "sha256": self.plan_sha256,
                "text": self.plan_text,
            },
            "inputs": self.inputs,
            "started_unix_ms": self.started_unix_ms,
            "wall_ms": self.started.elapsed().as_millis() as u64,
            "steps": state.steps,
            "result": {
                "ok": summary.ok,
                "exit_status": summary.exit_status,
                "error": error,
                "output": output,
            },
            "terminal": terminal,
            "truncated": state.truncated,
            "limits": {
                "blob_bytes_max": MAX_BLOB_BYTES,
                "blob_bytes_used": state.blob_bytes,
                "stderr_tail_bytes": STDERR_TAIL_BYTES,
            },
            "redaction": {
                "declared_secret_names": self.declared_secret_names,
            },
        });
        let text = serde_json::to_string_pretty(&document)?;
        write_private(&self.directory.join(RUN_FILE), text.as_bytes())
    }

    /// Store bytes as a content-addressed blob and describe them. `bytes` must
    /// already be redacted, so the hash is of the stored content. When persistence is
    /// not allowed, only the count is kept: no bytes and no hash, since a hash of
    /// withheld content would confirm guesses about it. Past the budget, the
    /// description keeps the hash and count but names no blob.
    fn blob(&self, state: &mut RecorderState, bytes: &[u8], persist: bool) -> Result<Json> {
        if !persist {
            return Ok(json!({
                "bytes": bytes.len(),
                "blob": null,
                "withheld": "policy",
                "truncated": false,
            }));
        }
        let hash = sha256_hex(bytes);
        let within_budget = state.blob_bytes.saturating_add(bytes.len()) <= MAX_BLOB_BYTES;
        if !within_budget {
            state.truncated = true;
            return Ok(json!({
                "bytes": bytes.len(),
                "sha256": hash,
                "blob": null,
                "truncated": true,
            }));
        }
        let directory = self.directory.join(BLOB_DIR);
        if !directory.is_dir() {
            create_private_directory(&directory)?;
        }
        let path = directory.join(&hash);
        if !path.exists() {
            write_private(&path, bytes)?;
            state.blob_bytes += bytes.len();
        }
        Ok(json!({
            "bytes": bytes.len(),
            "sha256": hash,
            "blob": format!("{BLOB_DIR}/{hash}"),
            "truncated": false,
        }))
    }
}

impl Observer for Recorder {
    fn replay(&self, _request: &StepRequest) -> Result<Option<StepResult>> {
        Ok(None)
    }

    fn finished(&self, request: &StepRequest, wall: Duration, result: &StepResult) -> Result<()> {
        // Redact each buffer in full before it is hashed, tailed or stored.
        let stdout = self.redactor.redact_bytes(&result.stdout);
        let stderr = self.redactor.redact_bytes(&result.stderr);
        let argv = request
            .argv
            .iter()
            .map(|argv| {
                argv.iter()
                    .map(|argument| self.redactor.redact_text(argument))
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let index = state.steps;
        state.steps += 1;
        state.labels.extend(&result.sensitivity);
        state.secret_environment |= result.secret_environment;
        let persist = persistence_allowed(
            &self.registry,
            &result.sensitivity,
            result.secret_environment,
        );
        let stdout = self.blob(&mut state, &stdout, persist)?;
        let stderr_tail = if persist {
            String::from_utf8_lossy(&tail(&stderr, STDERR_TAIL_BYTES)).into_owned()
        } else {
            String::new()
        };
        let stderr = self.blob(&mut state, &stderr, persist)?;
        let outcome = match &result.outcome {
            Some(outcome) => json!({
                "name": outcome.name,
                "kind": serde_json::to_value(outcome.kind)?,
                "certainty": serde_json::to_value(outcome.effect_certainty)?,
            }),
            None => Json::Null,
        };
        let entry = json!({
            "event": "step",
            "id": crate::runtime::current_step_id().map(|id| id.to_string()),
            "index": index,
            "t_ms": self.started.elapsed().as_millis() as u64,
            "line": request.line,
            "applets": request.applets,
            "binaries": request.binaries,
            "argv": argv,
            "status": result.status,
            "outcome": outcome,
            "failed": result.failed,
            "wall_us": wall.as_micros() as u64,
            "stdout": stdout,
            "stderr": {
                "bytes": stderr["bytes"],
                "sha256": stderr["sha256"],
                "blob": stderr["blob"],
                "truncated": stderr["truncated"],
                "withheld": stderr.get("withheld").cloned().unwrap_or(Json::Null),
                "tail": stderr_tail,
            },
        });
        writeln!(state.trace, "{entry}")?;
        Ok(())
    }

    fn suppresses_writes(&self, _line: usize) -> bool {
        false
    }

    fn terminal_output(&self, bytes: &[u8]) {
        // Kept raw here and redacted as a whole in `finish`.
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state.terminal.len().saturating_add(bytes.len()) <= MAX_BLOB_BYTES {
            state.terminal.extend_from_slice(bytes);
        } else {
            state.truncated = true;
        }
    }
}

/// A recorded applet group, as read back from `trace.jsonl`.
#[derive(Debug, Clone)]
pub struct StoredStep {
    pub index: usize,
    pub line: usize,
    pub applets: Vec<String>,
    pub binaries: Vec<String>,
    pub argv: Vec<Vec<String>>,
    pub status: Option<i32>,
    pub outcome: Option<Json>,
    /// The group ended as a typed failure, which stops the program unless caught.
    pub failed: bool,
    pub wall_us: u64,
    pub stdout: Json,
    pub stderr: Json,
}

/// A record directory opened for reading.
pub struct StoredRecord {
    pub directory: PathBuf,
    pub run: Json,
    pub steps: Vec<StoredStep>,
}

impl StoredRecord {
    pub fn open(directory: &Path) -> Result<Self> {
        let run: Json = serde_json::from_slice(
            &read_regular_no_follow(&directory.join(RUN_FILE))
                .with_context(|| format!("{} is not a run record", directory.display()))?,
        )
        .context("run.json is not valid JSON")?;
        if run["protocol"] != PROTOCOL {
            bail!("unsupported record protocol {}", run["protocol"]);
        }
        let trace = String::from_utf8(
            read_regular_no_follow(&directory.join(TRACE_FILE)).context("reading trace.jsonl")?,
        )
        .context("reading trace.jsonl")?;
        let mut steps = Vec::new();
        for (number, line) in trace.lines().enumerate() {
            let value: Json = serde_json::from_str(line)
                .with_context(|| format!("trace.jsonl line {}", number + 1))?;
            if value["event"] != "step" {
                continue;
            }
            steps.push(StoredStep {
                index: value["index"].as_u64().unwrap_or(0) as usize,
                line: value["line"].as_u64().unwrap_or(0) as usize,
                applets: strings(&value["applets"]),
                binaries: strings(&value["binaries"]),
                argv: value["argv"]
                    .as_array()
                    .map(|items| items.iter().map(strings).collect())
                    .unwrap_or_default(),
                status: value["status"].as_i64().map(|code| code as i32),
                outcome: (!value["outcome"].is_null()).then(|| value["outcome"].clone()),
                wall_us: value["wall_us"].as_u64().unwrap_or(0),
                stdout: value["stdout"].clone(),
                stderr: value["stderr"].clone(),
                failed: value["failed"].as_bool().unwrap_or(false),
            });
        }
        Ok(Self {
            directory: directory.to_path_buf(),
            run,
            steps,
        })
    }

    /// Read a stored blob and check its hash. A missing or truncated blob is an error.
    pub fn blob(&self, description: &Json) -> Result<Vec<u8>> {
        let Some(name) = description["blob"].as_str() else {
            if description["withheld"] == "policy" {
                bail!(
                    "this output was withheld from the record by the data-flow persistence policy"
                );
            }
            bail!("the record kept no bytes for this output (the record was truncated)");
        };
        let expected = description["sha256"].as_str().unwrap_or_default();
        // The name comes from record JSON, which may have been edited. Only the writer's
        // own `blobs/<sha256>` names are read, so no path can leave the record.
        if !valid_blob_name(name) {
            bail!("recorded blob name {name:?} is not a blob of this record");
        }
        let bytes = read_regular_no_follow(&self.directory.join(name))
            .with_context(|| format!("reading recorded blob {name}"))?;
        if sha256_hex(&bytes) != expected {
            bail!("recorded blob {name} does not match its hash");
        }
        Ok(bytes)
    }
}

/// Whether a stored blob name is exactly the writer's `blobs/<64 lowercase hex>` form.
fn valid_blob_name(name: &str) -> bool {
    name.strip_prefix(BLOB_DIR)
        .and_then(|rest| rest.strip_prefix('/'))
        .is_some_and(|digest| {
            digest.len() == 64
                && digest
                    .bytes()
                    .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        })
}

/// Read a record file without following a link in its final component, and require a
/// regular file. A record is read from its own directory, so a link planted there must
/// not redirect the read elsewhere.
fn read_regular_no_follow(path: &Path) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // O_NONBLOCK keeps a FIFO from stalling the open; the regular-file check rejects it.
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        if fs::symlink_metadata(path)?.file_type().is_symlink() {
            return Err(std::io::Error::other(format!(
                "{} is a link, not a record file",
                path.display()
            )));
        }
    }
    let mut file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::other(format!(
            "{} is not a regular file",
            path.display()
        )));
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn strings(value: &Json) -> Vec<String> {
    value
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(Json::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// A short summary of a record for `show-run`, as text or as JSON.
pub fn show_run(directory: &Path, as_json: bool) -> Result<String> {
    let record = StoredRecord::open(directory)?;
    let failed = record
        .steps
        .iter()
        .find(|step| step_failed(step))
        .map(step_summary);
    let mut slowest = record.steps.iter().collect::<Vec<_>>();
    slowest.sort_by(|left, right| {
        right
            .wall_us
            .cmp(&left.wall_us)
            .then(left.index.cmp(&right.index))
    });
    let slowest = slowest
        .into_iter()
        .take(5)
        .map(step_summary)
        .collect::<Vec<_>>();
    let ok = record.run["result"]["ok"].as_bool().unwrap_or(false);
    if as_json {
        let document = json!({
            "protocol": "execute.show-run/1",
            "ok": ok,
            "exit_status": record.run["result"]["exit_status"],
            "error": record.run["result"]["error"],
            "program_sha256": record.run["program"]["sha256"],
            "plan_sha256": record.run["plan"]["sha256"],
            "steps": record.steps.len(),
            "truncated": record.run["truncated"],
            "failed_step": failed,
            "slowest_steps": slowest,
        });
        return Ok(serde_json::to_string_pretty(&document)? + "\n");
    }
    let mut text = String::new();
    text.push_str(&format!(
        "record {}\nresult: {}\n",
        directory.display(),
        if ok { "ok" } else { "failed" }
    ));
    if let Some(error) = record.run["result"]["error"].as_str() {
        text.push_str(&format!("error: {error}\n"));
    }
    text.push_str(&format!("steps: {}\n", record.steps.len()));
    match &failed {
        Some(step) => text.push_str(&format!(
            "failed step: #{} line {} {} status {}\n  outcome: {}\n  stderr tail: {}\n",
            step["index"],
            step["line"],
            step["applets"],
            step["status"],
            step["outcome"],
            step["stderr_tail"].as_str().unwrap_or("")
        )),
        None => text.push_str("failed step: none\n"),
    }
    text.push_str("slowest steps:\n");
    for step in &slowest {
        text.push_str(&format!(
            "  #{} line {} {} {} us\n",
            step["index"], step["line"], step["applets"], step["wall_us"]
        ));
    }
    if record.run["truncated"].as_bool().unwrap_or(false) {
        text.push_str("note: the record reached its output limit; some outputs were not kept\n");
    }
    Ok(text)
}

fn step_failed(step: &StoredStep) -> bool {
    step.failed
        || match &step.outcome {
            Some(outcome) => outcome["kind"] != "success",
            None => step.status != Some(0),
        }
}

fn step_summary(step: &StoredStep) -> Json {
    json!({
        "index": step.index,
        "line": step.line,
        "applets": step.applets,
        "status": step.status,
        "outcome": step.outcome.as_ref().map(|outcome| outcome["name"].clone()),
        "kind": step.outcome.as_ref().map(|outcome| outcome["kind"].clone()),
        "certainty": step.outcome.as_ref().map(|outcome| outcome["certainty"].clone()),
        "wall_us": step.wall_us,
        "stderr_tail": step.stderr["tail"].clone(),
    })
}

fn redact_json(redactor: &SecretRedactor, value: &Json) -> Json {
    match value {
        Json::String(text) => Json::String(redactor.redact_text(text)),
        Json::Array(items) => Json::Array(
            items
                .iter()
                .map(|item| redact_json(redactor, item))
                .collect(),
        ),
        Json::Object(fields) => Json::Object(
            fields
                .iter()
                .map(|(key, item)| (redactor.redact_text(key), redact_json(redactor, item)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// Input values for a record. A sensitive input (an environment value or a secret
/// prompt answer) is withheld: the record keeps a marker in its place, so the value is
/// never written to disk.
pub fn inputs_json_for(
    inputs: &BTreeMap<String, crate::ast::Value>,
    declarations: &[crate::ast::InputDeclaration],
) -> Json {
    let secret: std::collections::BTreeSet<&str> = declarations
        .iter()
        .filter(|d| crate::ast::is_sensitive(d))
        .map(|d| d.name.as_str())
        .collect();
    let mut json = inputs_json(inputs);
    if let Json::Object(fields) = &mut json {
        for name in secret {
            if fields.contains_key(name) {
                fields.insert(name.to_owned(), serde_json::json!({ "withheld": "secret" }));
            }
        }
    }
    json
}

/// Input values as JSON for a record, from the run's named inputs.
pub fn inputs_json(inputs: &BTreeMap<String, crate::ast::Value>) -> Json {
    Json::Object(
        inputs
            .iter()
            .map(|(name, value)| {
                (
                    name.clone(),
                    serde_json::to_value(value).unwrap_or(Json::Null),
                )
            })
            .collect(),
    )
}

fn tail(bytes: &[u8], limit: usize) -> Vec<u8> {
    let start = bytes.len().saturating_sub(limit);
    bytes[start..].to_vec()
}

fn truncate_text(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

fn unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0)
}

/// Create a directory that only the owner can read, or confirm an empty one.
fn create_private_directory(path: &Path) -> Result<()> {
    if path.exists() {
        if !path.is_dir() {
            bail!("{} exists and is not a directory", path.display());
        }
        if fs::read_dir(path)?.next().is_some() {
            bail!(
                "record directory {} is not empty; choose a new directory",
                path.display()
            );
        }
        return Ok(());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)
            .with_context(|| format!("creating record directory {}", path.display()))?;
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(path)
            .with_context(|| format!("creating record directory {}", path.display()))?;
    }
    Ok(())
}

fn create_private(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(path)
        .with_context(|| format!("creating {}", path.display()))
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = create_private(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_plans_are_cut_on_a_character_boundary() {
        let text = "é".repeat(10);
        let cut = truncate_text(&text, 5);
        assert_eq!(cut, "éé");
    }

    #[test]
    fn tails_keep_the_end_of_stderr() {
        assert_eq!(tail(b"abcdef", 3), b"def".to_vec());
        assert_eq!(tail(b"ab", 3), b"ab".to_vec());
    }

    #[test]
    fn redaction_applies_to_keys_and_values() {
        let redactor = SecretRedactor::default();
        redactor.register(b"hunter2").unwrap();
        let value = json!({"hunter2": ["x hunter2 y"]});
        let redacted = redact_json(&redactor, &value);
        let text = redacted.to_string();
        assert!(!text.contains("hunter2"), "{text}");
    }

    #[test]
    fn a_secret_straddling_the_plan_limit_never_survives_truncation() {
        let redactor = SecretRedactor::default();
        let secret = "SECRET-straddle-value-9731";
        redactor.register(secret.as_bytes()).unwrap();
        // Truncating first would keep "SECRET-str" as an unredactable fragment.
        let plan = format!("{}{secret}tail", "x".repeat(MAX_PLAN_BYTES - 10));
        let (stored, _) = prepare_plan(&redactor, &plan);
        assert!(stored.len() <= MAX_PLAN_BYTES);
        assert!(
            !stored.contains("SECRET"),
            "a fragment of the secret survived"
        );
        assert!(
            !stored.contains("straddle"),
            "a fragment of the secret survived"
        );
    }

    #[test]
    fn the_plan_hash_is_over_the_redacted_plan_not_the_raw_plan() {
        let redactor = SecretRedactor::default();
        let secret = "SECRET-confirmation-value-5521";
        redactor.register(secret.as_bytes()).unwrap();
        let plan = format!("run printf with message \"{secret}\"\n");
        let (stored, sha256) = prepare_plan(&redactor, &plan);
        assert_ne!(
            sha256,
            sha256_hex(plan.as_bytes()),
            "the raw plan's hash leaks a guess"
        );
        assert_eq!(sha256, sha256_hex(stored.as_bytes()));
    }

    #[test]
    fn persistence_policy_withholds_unknown_labels_and_secret_environment() {
        let registry = Registry::default();
        let _ = registry
            .set_host_state(security::DataFlowPolicy::from_json(br#"{"version":1}"#).unwrap());
        let public = SensitivitySet::from_label(crate::contracts::Sensitivity::Public);
        let unknown = SensitivitySet::from_label(crate::contracts::Sensitivity::Unknown);
        assert!(persistence_allowed(&registry, &public, false));
        assert!(!persistence_allowed(&registry, &unknown, false));
        assert!(
            !persistence_allowed(&registry, &public, true),
            "declared secret environment is never persisted"
        );
    }
}

#[cfg(test)]
mod blob_name_tests {
    use super::valid_blob_name;

    #[test]
    fn only_the_writers_blob_names_are_accepted() {
        let digest = "a".repeat(64);
        assert!(valid_blob_name(&format!("blobs/{digest}")));
        assert!(valid_blob_name(&format!(
            "blobs/{}",
            "0123456789abcdef".repeat(4)
        )));
        for name in [
            "",
            "blobs/",
            "blobs",
            "../blobs/x",
            "/etc/passwd",
            "blobs/../run.json",
            "blobs/../../etc/passwd",
            "blobs/A",
            "blobs/ABCDEF",
            "run.json",
        ] {
            assert!(!valid_blob_name(name), "{name:?} was accepted");
        }
        assert!(!valid_blob_name(&format!("blobs/{}", "a".repeat(63))));
        assert!(!valid_blob_name(&format!("blobs/{}", "a".repeat(65))));
        assert!(!valid_blob_name(&format!(
            "blobs/{}/{}",
            "a".repeat(32),
            "b".repeat(31)
        )));
    }
}

#[cfg(all(test, unix))]
mod no_follow_tests {
    #[test]
    fn record_reads_refuse_a_link_in_the_final_component() {
        use std::os::unix::fs::symlink;
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("target.json"), b"secret").unwrap();
        symlink(
            directory.path().join("target.json"),
            directory.path().join("run.json"),
        )
        .unwrap();
        assert!(super::read_regular_no_follow(&directory.path().join("run.json")).is_err());
        std::fs::write(directory.path().join("plain.json"), b"ok").unwrap();
        assert_eq!(
            super::read_regular_no_follow(&directory.path().join("plain.json")).unwrap(),
            b"ok"
        );
    }
}
