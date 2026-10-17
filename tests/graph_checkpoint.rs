//! Checkpoints for finite applet graphs (#71, phase 1), against the in-memory store.
//! A completed node is recorded and replayed by a later run, an interrupted node
//! needs `--retry-uncertain`, and a changed or unsupported graph is refused before
//! any effect runs.

use flow::graph::{self, GraphCheckpointStore, GraphPlan, MemoryGraphStore};
use flow::manifest::Registry;
use flow::runtime::RunOptions;
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

/// `count` appends one line to a log on every run and echoes its text. `slow` blocks
/// on its first attempt, marked by a file, and echoes its text on the retry.
struct Fixture {
    _directory: tempfile::TempDir,
    root: PathBuf,
    manifests: PathBuf,
    count_log: PathBuf,
    marker: PathBuf,
}

fn fixture() -> Fixture {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().to_path_buf();
    let manifests = root.join("manifests");
    fs::create_dir(&manifests).unwrap();
    let count_log = root.join("count.log");
    let marker = root.join("slow.marker");
    let (count_script, slow_script, binary) = if cfg!(windows) {
        let count = root.join("count.bat");
        fs::write(
            &count,
            format!(
                "@echo off\r\necho run>> \"{log}\"\r\necho %1\r\n",
                log = count_log.display()
            ),
        )
        .unwrap();
        let slow = root.join("slow.bat");
        fs::write(
            &slow,
            format!(
                "@echo off\r\nif exist \"{m}\" goto go\r\ntype nul > \"{m}\"\r\nping -n 30 127.0.0.1 > nul\r\nexit /b 0\r\n:go\r\necho %1\r\n",
                m = marker.display()
            ),
        )
        .unwrap();
        (count, slow, "cmd.exe")
    } else {
        let count = root.join("count.sh");
        fs::write(
            &count,
            format!(
                "#!/bin/sh\necho run >> '{log}'\nprintf '%s\\n' \"$1\"\n",
                log = count_log.display()
            ),
        )
        .unwrap();
        let slow = root.join("slow.sh");
        fs::write(
            &slow,
            format!(
                "#!/bin/sh\nif [ ! -f '{m}' ]; then\n  : > '{m}'\n  exec /bin/sleep 30\nfi\nprintf '%s\\n' \"$1\"\n",
                m = marker.display()
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for script in [&count, &slow] {
                fs::set_permissions(script, fs::Permissions::from_mode(0o755)).unwrap();
            }
        }
        (count, slow, "/bin/sh")
    };
    let arguments = |script: &Path| {
        if cfg!(windows) {
            json!([
                {"literal": "/d"},
                {"literal": "/c"},
                {"literal": script.to_string_lossy()},
                {"parameter": "text"}
            ])
        } else {
            json!([{"literal": script.to_string_lossy()}, {"parameter": "text"}])
        }
    };
    for (name, script) in [("ctrl.count", &count_script), ("ctrl.slow", &slow_script)] {
        let manifest = json!({
            "name": name,
            "binary": binary,
            "accepts_stdin": false,
            "cacheable": false,
            "parameters": {"text": {"type": "string", "required": true}},
            "arguments": arguments(script),
            "contract": {
                "version": 1,
                "backend": "command",
                "lifecycle": {"kind": "oneshot"},
                "output": {"type": "text", "encoding": "raw", "trim_final_lf": true},
                "outcomes": [{"status": 0, "name": "success", "kind": "success"}],
                "capabilities": [],
                "effects": [],
                "ports": {
                    "inputs": {"text": {"schema": {"type": "text"}, "delivery": "single"}},
                    "outputs": {"value": {"schema": {"type": "text"}, "delivery": "single"}}
                }
            }
        });
        fs::write(manifests.join(format!("{name}.json")), manifest.to_string()).unwrap();
    }
    Fixture {
        _directory: directory,
        root,
        manifests,
        count_log,
        marker,
    }
}

/// `first` and `third` are counted runs, and `second` is the slow node in between.
const CHAIN: &str = "graph chain\n\
    \x20 run ctrl.count with text \"a\" as first\n\
    \x20 run ctrl.slow as second\n\
    \x20 bind first.value to second.text with capacity 1 lossless\n\
    \x20 run ctrl.count as third\n\
    \x20 bind second.value to third.text with capacity 1 lossless\n\
    end graph\n";

fn plan(source: &str, registry: &Registry) -> GraphPlan {
    graph::parse(source, registry).unwrap()
}

fn options(cwd: &Path) -> RunOptions {
    RunOptions {
        inputs: Default::default(),
        working_directory: cwd.to_path_buf(),
        cache_directory: None,
        trace: false,
        max_output_bytes: 1024 * 1024,
        command_timeout: Some(Duration::from_secs(60)),
        total_timeout: None,
    }
}

fn log_lines(path: &Path) -> usize {
    fs::read_to_string(path)
        .map(|text| text.lines().count())
        .unwrap_or(0)
}

fn wait_for(path: &Path, limit: Duration) -> bool {
    let deadline = Instant::now() + limit;
    while !path.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    path.exists()
}

/// Terminal outputs as `(node, port, payload)`, in the order the run returned them.
fn terminal(result: &graph::GraphRunResult) -> Vec<(String, String, Value)> {
    result
        .outputs
        .iter()
        .map(|event| {
            (
                event.port.node.clone(),
                event.port.port.clone(),
                serde_json::to_value(&event.payload).unwrap(),
            )
        })
        .collect()
}

fn trimmed_value(payload: &Value) -> String {
    payload["value"]["String"]
        .as_str()
        .unwrap_or_default()
        .trim()
        .to_owned()
}

#[test]
fn interrupted_node_is_uncertain_and_completed_nodes_are_not_rerun() {
    let fixture = fixture();
    let registry = Registry::load(&fixture.manifests).unwrap();
    let store = Arc::new(MemoryGraphStore::default());
    let cancelled = Arc::new(AtomicBool::new(false));

    let run = {
        let plan = plan(CHAIN, &registry);
        let registry = registry.clone();
        let options = options(&fixture.root);
        let store = Arc::clone(&store);
        let cancelled = Arc::clone(&cancelled);
        thread::spawn(move || {
            graph::run_checkpointed(plan, &registry, &options, cancelled, store, false)
        })
    };
    assert!(
        wait_for(&fixture.marker, Duration::from_secs(20)),
        "the slow node never started"
    );
    cancelled.store(true, std::sync::atomic::Ordering::SeqCst);
    assert!(
        run.join().unwrap().is_err(),
        "the interrupted run should fail"
    );

    let state = store.load().unwrap();
    let completed: Vec<&str> = state
        .completed
        .iter()
        .map(|node| node.node.as_str())
        .collect();
    assert_eq!(completed, ["first"]);
    assert_eq!(state.interrupted.len(), 1, "{state:?}");
    assert_eq!(state.interrupted[0].node, "second");
    assert!(!state.interrupted[0].retry_safe);
    assert_eq!(log_lines(&fixture.count_log), 1);

    // Without the explicit flag, resume refuses the uncertain node.
    let refused = graph::run_checkpointed(
        plan(CHAIN, &registry),
        &registry,
        &options(&fixture.root),
        Arc::new(AtomicBool::new(false)),
        store.clone(),
        false,
    );
    let message = format!("{:#}", refused.unwrap_err());
    assert!(message.contains("retry-uncertain"), "{message}");
    assert_eq!(
        log_lines(&fixture.count_log),
        1,
        "a refused resume ran a node"
    );

    // With it, `first` is replayed and `second` runs again.
    let resumed = graph::run_checkpointed(
        plan(CHAIN, &registry),
        &registry,
        &options(&fixture.root),
        Arc::new(AtomicBool::new(false)),
        store.clone(),
        true,
    )
    .unwrap();
    assert_eq!(
        log_lines(&fixture.count_log),
        2,
        "only `third` should have run"
    );
    let values: Vec<String> = terminal(&resumed)
        .iter()
        .filter(|(node, port, _)| node == "third" && port == "value")
        .map(|(_, _, payload)| trimmed_value(payload))
        .collect();
    assert_eq!(values, ["a"]);
}

#[test]
fn clean_checkpointed_run_matches_plain_run_and_replays_without_rerunning() {
    let fixture = fixture();
    let registry = Registry::load(&fixture.manifests).unwrap();
    // The slow node finds its marker and succeeds on the first attempt.
    fs::write(&fixture.marker, b"").unwrap();

    let plain = graph::run(
        plan(CHAIN, &registry),
        &registry,
        &options(&fixture.root),
        Arc::new(AtomicBool::new(false)),
    )
    .unwrap();
    let plain_outputs = terminal(&plain);
    assert_eq!(log_lines(&fixture.count_log), 2);
    fs::remove_file(&fixture.count_log).unwrap();

    let store = Arc::new(MemoryGraphStore::default());
    let checkpointed = graph::run_checkpointed(
        plan(CHAIN, &registry),
        &registry,
        &options(&fixture.root),
        Arc::new(AtomicBool::new(false)),
        store.clone(),
        false,
    )
    .unwrap();
    let mut expected = plain_outputs.clone();
    let mut actual = terminal(&checkpointed);
    let key = |(node, port, payload): &(String, String, Value)| {
        (node.clone(), port.clone(), payload.to_string())
    };
    expected.sort_by_key(key);
    actual.sort_by_key(key);
    assert_eq!(actual, expected, "same terminal outputs as a plain run");

    // Terminal outputs come in node completion order under checkpoints.
    let state = store.load().unwrap();
    let order: Vec<&str> = state
        .completed
        .iter()
        .map(|node| node.node.as_str())
        .collect();
    assert_eq!(order, ["first", "second", "third"]);
    let produced: Vec<&str> = checkpointed
        .outputs
        .iter()
        .map(|event| event.port.node.as_str())
        .collect();
    let mut sorted = produced.clone();
    sorted.sort_by_key(|node| order.iter().position(|name| name == node));
    assert_eq!(produced, sorted, "outputs follow completion order");

    // A second run replays everything: no node runs, and the outputs match exactly.
    let replayed = graph::run_checkpointed(
        plan(CHAIN, &registry),
        &registry,
        &options(&fixture.root),
        Arc::new(AtomicBool::new(false)),
        store.clone(),
        false,
    )
    .unwrap();
    assert_eq!(log_lines(&fixture.count_log), 2, "replay reran a node");
    assert_eq!(terminal(&replayed).len(), terminal(&checkpointed).len());
}

#[test]
fn a_changed_completed_node_is_refused_before_any_effect() {
    let fixture = fixture();
    let registry = Registry::load(&fixture.manifests).unwrap();
    fs::write(&fixture.marker, b"").unwrap();
    let store = Arc::new(MemoryGraphStore::default());
    graph::run_checkpointed(
        plan(CHAIN, &registry),
        &registry,
        &options(&fixture.root),
        Arc::new(AtomicBool::new(false)),
        store.clone(),
        false,
    )
    .unwrap();
    let lines = log_lines(&fixture.count_log);

    let changed = CHAIN.replace("text \"a\"", "text \"z\"");
    let refused = graph::run_checkpointed(
        plan(&changed, &registry),
        &registry,
        &options(&fixture.root),
        Arc::new(AtomicBool::new(false)),
        store.clone(),
        false,
    );
    let message = format!("{:#}", refused.unwrap_err());
    assert!(
        message.contains("changed since the checkpoint"),
        "{message}"
    );
    assert_eq!(
        log_lines(&fixture.count_log),
        lines,
        "a refused run ran a node"
    );
}

#[test]
fn persistent_sources_are_refused_under_checkpoints() {
    let fixture = fixture();
    let registry = Registry::load(&fixture.manifests).unwrap();
    let source = "graph ticking\n\
        \x20 every 1 second as ticker\n\
        \x20 run ctrl.count with text \"x\" as counted\n\
        end graph\n";
    let store = Arc::new(MemoryGraphStore::default());
    let refused = graph::run_checkpointed(
        plan(source, &registry),
        &registry,
        &options(&fixture.root),
        Arc::new(AtomicBool::new(false)),
        store.clone(),
        false,
    );
    let message = format!("{:#}", refused.unwrap_err());
    assert!(
        message.contains("\"ticker\"") && message.contains("timer"),
        "{message}"
    );
    assert_eq!(
        log_lines(&fixture.count_log),
        0,
        "a refused graph ran a node"
    );
}

#[test]
fn a_node_with_two_stream_inputs_is_refused_naming_the_node() {
    let fixture = fixture();
    let manifest = json!({
        "name": "ctrl.merge",
        "binary": if cfg!(windows) { "cmd.exe" } else { "/bin/sh" },
        "accepts_stdin": false,
        "cacheable": false,
        "parameters": {"left": {"type": "string"}, "right": {"type": "string"}},
        "arguments": [],
        "contract": {
            "version": 1,
            "backend": "command",
            "lifecycle": {"kind": "oneshot"},
            "output": {"type": "text", "encoding": "raw"},
            "outcomes": [{"status": 0, "name": "success", "kind": "success"}],
            "capabilities": [],
            "effects": [],
            "ports": {
                "inputs": {
                    "left": {"schema": {"type": "text"}, "delivery": "records"},
                    "right": {"schema": {"type": "text"}, "delivery": "records"}
                },
                "outputs": {"value": {"schema": {"type": "text"}, "delivery": "single"}}
            }
        }
    });
    fs::write(
        fixture.manifests.join("ctrl.merge.json"),
        manifest.to_string(),
    )
    .unwrap();
    let registry = Registry::load(&fixture.manifests).unwrap();
    let source = "graph merging\n\
        \x20 run ctrl.merge as merged\n\
        end graph\n";
    let store = Arc::new(MemoryGraphStore::default());
    let refused = graph::run_checkpointed(
        plan(source, &registry),
        &registry,
        &options(&fixture.root),
        Arc::new(AtomicBool::new(false)),
        store,
        false,
    );
    let message = format!("{:#}", refused.unwrap_err());
    assert!(
        message.contains("\"merged\"") && message.contains("stream inputs"),
        "{message}"
    );
}
