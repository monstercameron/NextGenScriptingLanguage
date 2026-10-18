//! `execute run --checkpoint`, `execute resume --checkpoint`, and `execute checkpoint
//! status` on a finite applet graph (#71, journal v5, mode `graph`). A run is killed
//! while its second node is in flight, and the checkpoint is resumed through the CLI.

use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// `ctrl.count` appends a line to a log on every run and echoes its text.
/// `ctrl.slow` blocks on its first attempt, marked by a file, and echoes on the retry.
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
            serde_json::json!([
                {"literal": "/d"},
                {"literal": "/c"},
                {"literal": script.to_string_lossy()},
                {"parameter": "text"}
            ])
        } else {
            serde_json::json!([{"literal": script.to_string_lossy()}, {"parameter": "text"}])
        }
    };
    for (name, script) in [("ctrl.count", &count_script), ("ctrl.slow", &slow_script)] {
        let manifest = serde_json::json!({
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

#[test]
fn graph_checkpoint_refuses_secret_prompt_inputs() {
    let work = tempfile::tempdir().unwrap();
    let program = work.path().join("secret.program");
    let checkpoint = work.path().join("checkpoint");
    fs::write(
        &program,
        "accept token as text from secret prompt \"Token?\"\ngraph g\n  run text.concat with parts [token, \"x\"] and separator \" \" as joined\nend graph\n",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_execute"))
        .arg("run")
        .arg(&program)
        .arg("--input")
        .arg("token=SuperSecret123")
        .arg("--checkpoint")
        .arg(&checkpoint)
        .current_dir(work.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains(
        "--checkpoint cannot be used with a secret prompt input: a journal would keep the answer or data computed from it"
    ));
    assert!(!checkpoint.join("graph.jsonl").exists());
}

/// `first` and `third` are counted runs, and `second` is the slow node in between.
const CHAIN: &str = "graph chain\n\
    \x20 run ctrl.count with text \"a\" as first\n\
    \x20 run ctrl.slow as second\n\
    \x20 bind first.value to second.text with capacity 1 lossless\n\
    \x20 run ctrl.count as third\n\
    \x20 bind second.value to third.text with capacity 1 lossless\n\
    end graph\n";

fn execute(directory: &Path, arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(directory)
        .args(arguments)
        .env_remove("EXECUTE_MODELS_OFFLINE")
        .stdin(Stdio::null())
        .output()
        .expect("start execute")
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

/// Start `run --checkpoint`, wait until the slow node is in flight, and kill the process.
fn interrupt_run(fixture: &Fixture, workflow: &Path, checkpoint: &Path) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(&fixture.root)
        .arg("run")
        .arg(workflow)
        .arg("--manifest-dir")
        .arg(&fixture.manifests)
        .arg("--checkpoint")
        .arg(checkpoint)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    assert!(
        wait_for(&fixture.marker, Duration::from_secs(20)),
        "the slow node never started"
    );
    child.kill().unwrap();
    child.wait().unwrap();
}

/// The `value` output of a graph run's JSON result, for one node.
fn value_of(output: &Output, node: &str) -> String {
    let result: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "expected graph JSON ({error}): {}",
            String::from_utf8_lossy(&output.stdout)
        )
    });
    result["outputs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|event| event["port"]["node"] == node && event["port"]["port"] == "value")
        .map(|event| {
            event["payload"]["value"]["String"]
                .as_str()
                .unwrap_or_default()
                .trim()
                .to_owned()
        })
        .unwrap_or_else(|| panic!("no value output for {node}"))
}

#[test]
fn interrupted_graph_resumes_through_the_cli_and_refuses_without_retry_uncertain() {
    let fixture = fixture();
    let workflow = fixture.root.join("chain.program");
    fs::write(&workflow, CHAIN).unwrap();
    let checkpoint = fixture.root.join("checkpoint");
    let workflow_text = workflow.to_string_lossy().into_owned();
    let checkpoint_text = checkpoint.to_string_lossy().into_owned();
    let manifests_text = fixture.manifests.to_string_lossy().into_owned();

    interrupt_run(&fixture, &workflow, &checkpoint);
    assert_eq!(
        log_lines(&fixture.count_log),
        1,
        "`first` ran once before the kill"
    );

    // A fresh run refuses the existing checkpoint, rather than replacing it.
    let fresh = execute(
        &fixture.root,
        &[
            "run",
            &workflow_text,
            "--manifest-dir",
            &manifests_text,
            "--checkpoint",
            &checkpoint_text,
        ],
    );
    assert!(!fresh.status.success());
    assert!(
        String::from_utf8_lossy(&fresh.stderr).contains("use resume explicitly"),
        "{}",
        String::from_utf8_lossy(&fresh.stderr)
    );

    // The status command reads the graph journal: one node committed, one in flight.
    let status = execute(
        &fixture.root,
        &["checkpoint", "status", "--json", &checkpoint_text],
    );
    assert!(
        status.status.success(),
        "{}",
        String::from_utf8_lossy(&status.stderr)
    );
    let summary: Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(summary["journal"]["kind"], "graph", "{summary}");
    assert_eq!(summary["journal"]["completed_nodes"], 1, "{summary}");
    assert_eq!(
        summary["journal"]["in_flight"][0]["node"], "second",
        "{summary}"
    );
    assert_eq!(
        summary["resume_action"], "requires_retry_uncertain",
        "{summary}"
    );

    // Resume without the flag is refused before any node runs.
    let refused = execute(
        &fixture.root,
        &[
            "resume",
            &workflow_text,
            "--manifest-dir",
            &manifests_text,
            "--checkpoint",
            &checkpoint_text,
        ],
    );
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("retry-uncertain"),
        "{}",
        String::from_utf8_lossy(&refused.stderr)
    );
    assert_eq!(
        log_lines(&fixture.count_log),
        1,
        "a refused resume ran a node"
    );

    // With the flag, `first` is replayed and only `second` and `third` run.
    let resumed = execute(
        &fixture.root,
        &[
            "resume",
            &workflow_text,
            "--manifest-dir",
            &manifests_text,
            "--checkpoint",
            &checkpoint_text,
            "--retry-uncertain",
        ],
    );
    assert!(
        resumed.status.success(),
        "{}",
        String::from_utf8_lossy(&resumed.stderr)
    );
    assert_eq!(value_of(&resumed, "third"), "a");
    assert_eq!(
        log_lines(&fixture.count_log),
        2,
        "only `third` should have run"
    );

    // The completed checkpoint replays without running anything.
    let replayed = execute(
        &fixture.root,
        &[
            "resume",
            &workflow_text,
            "--manifest-dir",
            &manifests_text,
            "--checkpoint",
            &checkpoint_text,
        ],
    );
    assert!(
        replayed.status.success(),
        "{}",
        String::from_utf8_lossy(&replayed.stderr)
    );
    assert_eq!(value_of(&replayed, "third"), "a");
    assert_eq!(
        log_lines(&fixture.count_log),
        2,
        "a completed replay ran a node"
    );
}

#[test]
fn a_tampered_graph_log_refuses_resume() {
    let fixture = fixture();
    let workflow = fixture.root.join("chain.program");
    fs::write(&workflow, CHAIN).unwrap();
    let checkpoint = fixture.root.join("checkpoint");
    let workflow_text = workflow.to_string_lossy().into_owned();
    let checkpoint_text = checkpoint.to_string_lossy().into_owned();
    let manifests_text = fixture.manifests.to_string_lossy().into_owned();

    interrupt_run(&fixture, &workflow, &checkpoint);
    let log = checkpoint.join("graph.jsonl");
    let committed = fs::read_to_string(&log).unwrap();
    assert!(committed.contains("\"node\":\"first\""), "{committed}");
    fs::write(
        &log,
        committed.replacen("\"node\":\"first\"", "\"node\":\"firsx\"", 1),
    )
    .unwrap();

    let refused = execute(
        &fixture.root,
        &[
            "resume",
            &workflow_text,
            "--manifest-dir",
            &manifests_text,
            "--checkpoint",
            &checkpoint_text,
            "--retry-uncertain",
        ],
    );
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("checksum mismatch"),
        "{}",
        String::from_utf8_lossy(&refused.stderr)
    );
    assert_eq!(
        log_lines(&fixture.count_log),
        1,
        "a refused resume ran a node"
    );
}

#[test]
fn resume_refuses_a_different_graph_and_a_program_checkpoint() {
    let fixture = fixture();
    let workflow = fixture.root.join("chain.program");
    fs::write(&workflow, CHAIN).unwrap();
    let checkpoint = fixture.root.join("checkpoint");
    let checkpoint_text = checkpoint.to_string_lossy().into_owned();
    let manifests_text = fixture.manifests.to_string_lossy().into_owned();

    interrupt_run(&fixture, &workflow, &checkpoint);
    let changed = fixture.root.join("changed.program");
    fs::write(&changed, CHAIN.replace("text \"a\"", "text \"z\"")).unwrap();
    let different = execute(
        &fixture.root,
        &[
            "resume",
            &changed.to_string_lossy(),
            "--manifest-dir",
            &manifests_text,
            "--checkpoint",
            &checkpoint_text,
            "--retry-uncertain",
        ],
    );
    assert!(!different.status.success());
    assert!(
        String::from_utf8_lossy(&different.stderr).contains("different graph"),
        "{}",
        String::from_utf8_lossy(&different.stderr)
    );

    // A program run must not open a graph checkpoint.
    let program = fixture.root.join("program.program");
    fs::write(&program, "run ctrl.count with text \"p\"\n").unwrap();
    let as_program = execute(
        &fixture.root,
        &[
            "resume",
            &program.to_string_lossy(),
            "--manifest-dir",
            &manifests_text,
            "--checkpoint",
            &checkpoint_text,
            "--retry-uncertain",
        ],
    );
    assert!(!as_program.status.success());
    assert!(
        String::from_utf8_lossy(&as_program.stderr).contains("graph run"),
        "{}",
        String::from_utf8_lossy(&as_program.stderr)
    );
}
