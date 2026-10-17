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
        crate::live::transport::read_message(&mut reader, LIVE_FRAME_LIMIT, Duration::from_secs(1))
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
