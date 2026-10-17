//! The gate and the what-if checks cover finite programs only. On graph, screen,
//! and stream programs they must fail with a clear error, never fall through to an
//! unchecked text explain.

use std::process::{Command, Output, Stdio};

const GRAPH: &str = "graph demo\n";
const SCREEN: &str = "open a screen titled \"Gate\"\nplace app on the left\ndefine a program named app\n  show a text panel named status with text \"ready\"\n";
const STREAM: &str = "stream from \"printf\"\nshow each line\n";

fn run_on(source: &str, extra: &[&str]) -> Output {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("program.txt"), source).unwrap();
    Command::new(env!("CARGO_BIN_EXE_execute"))
        .arg("explain")
        .arg("program.txt")
        .args(extra)
        .current_dir(directory.path())
        .env_remove("EXECUTE_MODELS_OFFLINE")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("start execute")
}

fn assert_refused(output: &Output, kind: &str, what: &str) {
    assert!(!output.status.success(), "{kind}: {what} was accepted");
    assert!(output.stdout.is_empty(), "{kind}: {what} printed a plan");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(&format!("not {kind} programs")),
        "{kind}: {what} gave an unclear error: {stderr}"
    );
}

#[test]
fn allow_is_refused_on_graph_screen_and_stream_programs() {
    for (kind, source) in [("graph", GRAPH), ("screen", SCREEN), ("stream", STREAM)] {
        let output = run_on(source, &["--json", "--allow", "undeclared"]);
        assert_refused(&output, kind, "--allow");
    }
}

#[test]
fn explain_json_is_refused_on_graph_screen_and_stream_programs() {
    for (kind, source) in [("graph", GRAPH), ("screen", SCREEN), ("stream", STREAM)] {
        let output = run_on(source, &["--json"]);
        assert_refused(&output, kind, "explain --json");
    }
}

#[test]
fn what_if_and_confirm_are_refused_on_non_finite_programs() {
    let directory = tempfile::tempdir().unwrap();
    for (kind, source) in [("graph", GRAPH), ("screen", SCREEN), ("stream", STREAM)] {
        std::fs::write(directory.path().join("program.txt"), source).unwrap();
        for option in [
            vec!["run", "program.txt", "--what-if"],
            vec!["run", "program.txt", "--confirm", "abc"],
        ] {
            let output = Command::new(env!("CARGO_BIN_EXE_execute"))
                .args(&option)
                .current_dir(directory.path())
                .env_remove("EXECUTE_MODELS_OFFLINE")
                .stdin(Stdio::null())
                .output()
                .unwrap();
            assert!(!output.status.success(), "{kind}: {option:?} was accepted");
            assert!(
                output.stdout.is_empty(),
                "{kind}: {option:?} printed output"
            );
        }
    }
}
