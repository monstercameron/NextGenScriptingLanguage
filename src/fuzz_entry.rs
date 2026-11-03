//! Entry points for the fuzz targets (`fuzz/`) and the regression replay test. Compiled
//! only with the `fuzzing` feature; it is not part of the product interface.
//!
//! `run` feeds one input to one named target. A target passes when the input is
//! rejected with an error or accepted; a panic, an abort, or a hang is a finding. The
//! libFuzzer targets and `tests/fuzz_regressions.rs` both call `run`, so a crash
//! saved under `fuzz/regressions/<target>/` replays through the exact same code.

use crate::ast::{InputDeclaration, InputType};
use std::collections::BTreeMap;
use std::io::{BufReader, Cursor, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// The fuzz targets, in the order the nightly job runs them.
pub const TARGETS: &[&str] = &[
    "parse_program",
    "manifests",
    "env_load",
    "input_values",
    "dap_frame",
    "live_frame",
    "checkpoint_journal",
    "graph_journal",
    "record_run",
    "element_log",
    "completed_log",
    "capability_policy",
    "data_flow_policy",
    "sandbox_profile",
    "model_response",
    "remote_response",
];

/// Feed one input to a named target. Malformed input returns an error, which is the
/// expected result; only a panic, abort, or hang counts as a finding.
pub fn run(target: &str, data: &[u8]) {
    match target {
        "parse_program" => {
            if let Ok(text) = std::str::from_utf8(data) {
                let _ = crate::parser::parse(text);
            }
        }
        "manifests" => {
            if let Ok(text) = std::str::from_utf8(data) {
                let _ = crate::manifest::Registry::from_sources(&[text]);
            }
        }
        "env_load" => {
            let _ = crate::applets::execute("env.load", &serde_json::Map::new(), data);
        }
        "input_values" => input_values(data),
        "dap_frame" => {
            let mut reader = BufReader::new(Cursor::new(data));
            let _ = crate::agent::dap::read_message(&mut reader);
        }
        "live_frame" => live_frame(data),
        "checkpoint_journal" => checkpoint_journal(data),
        "graph_journal" => graph_journal(data),
        "record_run" => record_run(data),
        "element_log" => element_log(data),
        "completed_log" => completed_log(data),
        "capability_policy" => {
            let _ = crate::capabilities::fuzz_parse_capability_policy(data);
        }
        "data_flow_policy" => {
            let _ = crate::security::fuzz_parse_data_flow_policy(data);
        }
        "sandbox_profile" => sandbox_profile(data),
        "model_response" => model_response(data),
        "remote_response" => remote_response(data),
        other => panic!("unknown fuzz target {other:?}"),
    }
}

/// Lines of `name=value` supplied for a fixed set of declared inputs, one per type.
fn input_values(data: &[u8]) {
    let declarations: Vec<InputDeclaration> = [
        ("text", InputType::Text),
        ("count", InputType::Integer),
        ("flag", InputType::Boolean),
        ("items", InputType::TextList),
        ("payload", InputType::Json),
    ]
    .into_iter()
    .enumerate()
    .map(|(index, (name, kind))| InputDeclaration {
        name: name.to_owned(),
        kind,
        default: None,
        source: None,
        public: false,
        line: index + 1,
        description: None,
        choices: None,
    })
    .collect();
    let mut supplied = BTreeMap::new();
    for line in String::from_utf8_lossy(data).lines().take(32) {
        if let Some((name, value)) = line.split_once('=') {
            supplied.insert(name.to_owned(), value.to_owned());
        }
    }
    let _ = crate::inputs::parse_cli(&declarations, &supplied);
}

/// Live request framing and the JSON decode of the framed request, over a socket pair.
#[cfg(unix)]
fn live_frame(data: &[u8]) {
    use std::os::unix::net::UnixStream;
    use std::time::Duration;
    // Largest frame sent, to stay inside one socket buffer.
    const LIVE_FRAME_LIMIT: usize = 64 * 1024;
    let data = &data[..data.len().min(LIVE_FRAME_LIMIT)];
    let Ok((mut writer, mut reader)) = UnixStream::pair() else {
        return;
    };
    let _ = writer.write_all(data);
    drop(writer);
    if let Ok(bytes) =
        crate::ipc::transport::read_message(&mut reader, LIVE_FRAME_LIMIT, Duration::from_secs(1))
    {
        let _ = serde_json::from_slice::<serde_json::Value>(&bytes);
    }
}

#[cfg(not(unix))]
fn live_frame(_data: &[u8]) {}

fn checkpoint_journal(data: &[u8]) {
    let Some(scratch) = Scratch::new("checkpoint") else {
        return;
    };
    let path = scratch.path().join("journal.json");
    if write_private(&path, data).is_ok() {
        let _ = crate::runtime::checkpoint::fuzz_read_journal(&path);
    }
}

fn graph_journal(data: &[u8]) {
    let Some(scratch) = Scratch::new("graph") else {
        return;
    };
    let path = scratch.path().join("journal.json");
    if write_private(&path, data).is_ok() {
        let _ = crate::runtime::checkpoint::graph::fuzz_read_disk(&path);
    }
}

/// The input is two parts split at the first NUL byte: `run.json`, then `trace.jsonl`.
fn record_run(data: &[u8]) {
    let split = data
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(data.len());
    let run = &data[..split];
    let trace = data.get(split + 1..).unwrap_or(&[]);
    let Some(scratch) = Scratch::new("record") else {
        return;
    };
    let run_path = scratch.path().join(crate::record::RUN_FILE);
    let trace_path = scratch.path().join(crate::record::TRACE_FILE);
    if write_private(&run_path, run).is_ok() && write_private(&trace_path, trace).is_ok() {
        let _ = crate::record::StoredRecord::open(scratch.path());
    }
}

/// The per-element checkpoint log of an in-flight `for each` node.
fn element_log(data: &[u8]) {
    let Some(scratch) = Scratch::new("elements") else {
        return;
    };
    let _ = crate::runtime::checkpoint::fuzz_replay_element_log(scratch.path(), data);
}

/// The append-only completed-node log of a checkpoint.
fn completed_log(data: &[u8]) {
    let Some(scratch) = Scratch::new("completed") else {
        return;
    };
    let _ = crate::runtime::checkpoint::fuzz_read_completed_log(scratch.path(), data);
}

/// Build sandbox argv/profiles for every grant in a fuzzed capability
/// policy, plus one synthetic grant over the scratch directory whose
/// network/write flags come from the input, so builders that succeed are
/// checked against the no-widening invariant on every run.
fn sandbox_profile(data: &[u8]) {
    let Some(scratch) = Scratch::new("sandbox") else {
        return;
    };
    let binary = scratch.path().join("applet");
    if std::fs::write(&binary, b"fuzz").is_err() {
        return;
    }
    if let Ok(policy) = crate::capabilities::fuzz_parse_capability_policy(data) {
        for grant in policy.applets.values() {
            crate::platform::sandbox::fuzz_sandbox_profile(grant, &binary, scratch.path());
        }
    }
    let network = data.first().is_some_and(|byte| byte % 2 == 1);
    let writable = data.get(1).is_some_and(|byte| byte % 2 == 1);
    let mut capabilities = std::collections::BTreeSet::from(["filesystem.read".to_owned()]);
    if writable {
        capabilities.insert("filesystem.write".to_owned());
    }
    if network {
        capabilities.insert("network.connect".to_owned());
    }
    let synthetic = crate::capabilities::AppletGrant {
        capabilities,
        binary_sha256: None,
        environment: std::collections::BTreeSet::new(),
        filesystem: crate::capabilities::FilesystemGrant {
            read: vec![scratch.path().to_path_buf()],
            write: if writable {
                vec![scratch.path().to_path_buf()]
            } else {
                Vec::new()
            },
        },
        network,
        network_origins: std::collections::BTreeSet::new(),
        sandbox_required: true,
        resource_limits: None,
    };
    crate::platform::sandbox::fuzz_sandbox_profile(&synthetic, &binary, scratch.path());
}

/// The model normalizer lives behind the `model` feature; the fuzz crate
/// enables it, plain `fuzzing` replays skip the body.
#[cfg(feature = "model")]
fn model_response(data: &[u8]) {
    crate::models::fuzz_normalize_model_response(data);
}

#[cfg(not(feature = "model"))]
fn model_response(_data: &[u8]) {}

/// The remote response parser lives behind the `remote` feature; the fuzz
/// crate enables it, plain `fuzzing` replays skip the body.
#[cfg(feature = "remote")]
fn remote_response(data: &[u8]) {
    let _ = crate::remote::fuzz_parse_remote_response(data);
}

#[cfg(not(feature = "remote"))]
fn remote_response(_data: &[u8]) {}

/// A directory for one fuzz iteration, removed when dropped.
struct Scratch(PathBuf);

static NEXT_SCRATCH: AtomicU64 = AtomicU64::new(0);

impl Scratch {
    fn new(label: &str) -> Option<Self> {
        let number = NEXT_SCRATCH.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "execute-fuzz-{}-{label}-{number}",
            std::process::id()
        ));
        std::fs::create_dir(&path).ok()?;
        Some(Self(path))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Write a new file that only the owner can read, as the journal readers expect.
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)?.write_all(bytes)
}
