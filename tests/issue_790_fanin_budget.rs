//! A checkpointed node whose held outputs would pass the 16 MiB record limit fails with
//! the bounded checkpoint diagnostic while it runs, before its completion is recorded
//! (#790). The node is one records-delivery applet that emits 17 outputs of 1 MiB each.
#![cfg(feature = "graph")]

use flow::graph::{self, GraphCheckpointStore, GraphPlan, MemoryGraphStore};
use flow::manifest::Registry;
use flow::runtime::RunOptions;
use serde_json::json;
use std::fs;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

const CHUNK_BYTES: usize = 1024 * 1024;
const CHUNKS: usize = 17;

/// Writes the manifests and the 17 MiB JSON list the producer prints. The producer
/// prints the file, so its applet output is one JSON array of 1 MiB strings.
fn write_manifests(root: &Path) -> std::path::PathBuf {
    let manifests = root.join("manifests");
    fs::create_dir(&manifests).unwrap();
    let list = root.join("chunks.json");
    let chunks: Vec<String> = (0..CHUNKS).map(|_| "a".repeat(CHUNK_BYTES)).collect();
    fs::write(&list, serde_json::to_vec(&chunks).unwrap()).unwrap();
    let (binary, produce_args, echo_binary, echo_args) = if cfg!(windows) {
        (
            "cmd.exe",
            json!([
                {"literal": "/d"},
                {"literal": "/c"},
                {"literal": "type"},
                {"literal": list.to_string_lossy()}
            ]),
            "cmd.exe",
            json!([
                {"literal": "/d"},
                {"literal": "/c"},
                {"literal": "echo"},
                {"parameter": "text"}
            ]),
        )
    } else {
        (
            "/bin/cat",
            json!([{"literal": list.to_string_lossy()}]),
            "/bin/echo",
            json!([{"parameter": "text"}]),
        )
    };
    let producer = json!({
        "name": "ctrl.chunks",
        "binary": binary,
        "accepts_stdin": false,
        "cacheable": false,
        "arguments": produce_args,
        "contract": {
            "version": 1,
            "backend": "command",
            "lifecycle": {"kind": "oneshot"},
            "output": {"type": "list", "encoding": "json", "items": {"type": "text"}},
            "outcomes": [{"status": 0, "name": "success", "kind": "success"}],
            "capabilities": [],
            "effects": [],
            "ports": {
                "inputs": {},
                "outputs": {"value": {"schema": {"type": "text"}, "delivery": "records"}}
            }
        }
    });
    let consumer = json!({
        "name": "ctrl.echo",
        "binary": echo_binary,
        "accepts_stdin": false,
        "cacheable": false,
        "parameters": {"text": {"type": "string", "required": true}},
        "arguments": echo_args,
        "contract": {
            "version": 1,
            "backend": "command",
            "lifecycle": {"kind": "oneshot"},
            "output": {"type": "text", "encoding": "raw", "trim_final_lf": true},
            "outcomes": [{"status": 0, "name": "success", "kind": "success"}],
            "capabilities": [],
            "effects": [],
            "ports": {
                "inputs": {"text": {"schema": {"type": "text"}, "delivery": "records"}},
                "outputs": {"value": {"schema": {"type": "text"}, "delivery": "records"}}
            }
        }
    });
    fs::write(manifests.join("ctrl.chunks.json"), producer.to_string()).unwrap();
    fs::write(manifests.join("ctrl.echo.json"), consumer.to_string()).unwrap();
    manifests
}

#[test]
fn held_records_fail_at_the_checkpoint_record_limit_before_completion() {
    let directory = tempfile::tempdir().unwrap();
    let manifests = write_manifests(directory.path());
    let registry = Registry::load(&manifests).unwrap();
    let source = "graph oversized\n\
        \x20 run ctrl.chunks as producer\n\
        \x20 run ctrl.echo as sink\n\
        \x20 bind producer.value to sink.text with capacity 1 lossless\n\
        end graph\n";
    let plan: GraphPlan = graph::parse(source, &registry).unwrap();
    let store = Arc::new(MemoryGraphStore::default());
    let options = RunOptions {
        // The producer's whole output is 17 MiB, so the applet output cap must allow it.
        max_output_bytes: 64 * 1024 * 1024,
        command_timeout: Some(Duration::from_secs(60)),
        ..RunOptions::new(directory.path().to_path_buf())
    };
    let outcome = graph::run_checkpointed(
        plan,
        &registry,
        &options,
        Arc::new(AtomicBool::new(false)),
        store.clone(),
        false,
    );
    let message = match outcome {
        Err(error) => format!("{error:#}"),
        Ok(result) => panic!(
            "a node holding more than the record limit must not complete: {} terminal outputs",
            result.outputs.len()
        ),
    };
    assert!(
        message.contains("16 MiB checkpoint record limit"),
        "{message}"
    );
    let state = store.load().unwrap();
    assert!(
        state.completed.is_empty(),
        "no completion record may be written for the oversized node"
    );
}
