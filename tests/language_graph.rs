//! End-to-end coverage for graph blocks and graph checkpoints.

use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Fixture {
    _directory: tempfile::TempDir,
    root: PathBuf,
    program: PathBuf,
    checkpoint: PathBuf,
}

impl Fixture {
    fn new(source: &str) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().to_path_buf();
        let program = root.join("graph.program");
        fs::write(&program, source).unwrap();
        let checkpoint = root.join("checkpoint");
        Self {
            _directory: directory,
            root,
            program,
            checkpoint,
        }
    }

    fn command(&self, action: &str) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_execute"));
        command
            .current_dir(&self.root)
            .arg(action)
            .arg(&self.program);
        command
    }

    fn path_text(path: &Path) -> String {
        path.to_string_lossy().into_owned()
    }
}

const GRAPH: &str = "graph greeting\n  run text.concat with parts [\"hello\", \"graph\"] and separator \" \" as joined\nend graph\n";

fn assert_graph_result(output: &Output) {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "expected graph result JSON ({error}): {}",
            String::from_utf8_lossy(&output.stdout)
        )
    });
    assert_eq!(result["protocol"], "execute.graph-run/1", "{result}");
    assert!(output
        .stdout
        .windows(b"hello graph".len())
        .any(|bytes| bytes == b"hello graph"));
}

#[test]
fn graph_block_runs_and_returns_its_terminal_output() {
    let fixture = Fixture::new(GRAPH);
    let output = fixture.command("run").output().unwrap();
    assert_graph_result(&output);
}

#[test]
fn graph_checkpoint_status_resume_and_source_identity_work_through_the_cli() {
    let fixture = Fixture::new(GRAPH);
    let checkpoint = Fixture::path_text(&fixture.checkpoint);

    let run = fixture
        .command("run")
        .arg("--checkpoint")
        .arg(&checkpoint)
        .output()
        .unwrap();
    assert_graph_result(&run);

    let status = Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(&fixture.root)
        .args(["checkpoint", "status", "--json", &checkpoint])
        .output()
        .unwrap();
    assert!(
        status.status.success(),
        "{}",
        String::from_utf8_lossy(&status.stderr)
    );
    let status: Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(status["journal"]["kind"], "graph", "{status}");
    assert_eq!(status["journal"]["completed_nodes"], 1, "{status}");

    let resumed = fixture
        .command("resume")
        .arg("--checkpoint")
        .arg(&checkpoint)
        .output()
        .unwrap();
    assert_graph_result(&resumed);

    fs::write(&fixture.program, GRAPH.replace("hello", "changed")).unwrap();
    let changed = fixture
        .command("resume")
        .arg("--checkpoint")
        .arg(&checkpoint)
        .output()
        .unwrap();
    assert!(!changed.status.success());
    assert!(
        String::from_utf8_lossy(&changed.stderr).contains("different graph"),
        "{}",
        String::from_utf8_lossy(&changed.stderr)
    );
}
