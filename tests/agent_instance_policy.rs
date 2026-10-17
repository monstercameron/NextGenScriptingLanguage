//! `run --single-instance LOCK` and `explain --json --policy-header`.

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const HELPER: &str = env!("CARGO_BIN_EXE_execute-model");
const RUNNABLE: &str = "accept name as text with default \"x\"\nthen encode name as json\n";

fn write_manifests(directory: &Path) -> PathBuf {
    let manifests = directory.join("manifests");
    std::fs::create_dir_all(&manifests).unwrap();
    let plain = serde_json::json!({
        "name": "plain.helper", "binary": HELPER, "accepts_stdin": false,
        "parameters": {}, "arguments": [{ "literal": "--version" }]
    });
    std::fs::write(manifests.join("plain.helper.json"), plain.to_string()).unwrap();
    manifests
}

fn execute(directory: &Path, arguments: &[&str]) -> Output {
    let manifests = write_manifests(directory);
    Command::new(env!("CARGO_BIN_EXE_execute"))
        .args(arguments)
        .arg("--manifest-dir")
        .arg(&manifests)
        .current_dir(directory)
        .env_remove("EXECUTE_MODELS_OFFLINE")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("start execute")
}

fn policy_program(directory: &Path, header: &str) {
    let source = format!("{header}run plain.helper\n");
    std::fs::write(directory.join("policy.program"), source).unwrap();
}

#[test]
fn a_second_run_is_refused_while_the_lock_is_held() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("tool.program"), RUNNABLE).unwrap();
    let lock_path = directory.path().join("tool.lock");
    let holder = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .unwrap();
    holder.try_lock().unwrap();

    let refused = execute(
        directory.path(),
        &["tool.program", "--single-instance", "tool.lock"],
    );
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("another instance holds"));
    assert!(
        refused.stdout.is_empty(),
        "a refused run must print nothing"
    );

    drop(holder);
    let allowed = execute(
        directory.path(),
        &["tool.program", "--single-instance", "tool.lock"],
    );
    assert!(
        allowed.status.success(),
        "{}",
        String::from_utf8_lossy(&allowed.stderr)
    );
}

#[test]
fn the_lock_is_released_when_a_run_finishes() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("tool.program"), RUNNABLE).unwrap();
    for _ in 0..2 {
        let output = execute(
            directory.path(),
            &["tool.program", "--single-instance", "tool.lock"],
        );
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn a_policy_header_approves_its_categories_and_the_gate_passes() {
    let directory = tempfile::tempdir().unwrap();
    policy_program(directory.path(), "# execute-policy: allow undeclared\n");
    let output = execute(
        directory.path(),
        &["explain", "policy.program", "--json", "--policy-header"],
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let verdict: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(verdict["protocol"], "execute.gate/1");
    assert_eq!(verdict["allowed"], true);
}

#[test]
fn allow_none_approves_nothing_and_the_gate_denies() {
    let directory = tempfile::tempdir().unwrap();
    policy_program(directory.path(), "# execute-policy: allow none\n");
    let output = execute(
        directory.path(),
        &["explain", "policy.program", "--json", "--policy-header"],
    );
    assert_eq!(output.status.code(), Some(1));
    let verdict: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(verdict["allowed"], false);
}

#[test]
fn the_header_and_allow_flags_combine() {
    let directory = tempfile::tempdir().unwrap();
    policy_program(directory.path(), "# execute-policy: allow undeclared\n");
    let output = execute(
        directory.path(),
        &[
            "explain",
            "policy.program",
            "--json",
            "--policy-header",
            "--allow",
            "reads",
        ],
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn malformed_and_duplicate_headers_are_refused_with_nothing_printed() {
    let directory = tempfile::tempdir().unwrap();
    for header in [
        "# execute-policy: allow everything\n",
        "# execute-policy: allow\n",
        "# execute-policy: deny writes\n",
        "# execute-policy: allow none writes\n",
        "# execute-policy: allow undeclared\n# execute-policy: allow reads\n",
    ] {
        policy_program(directory.path(), header);
        let output = execute(
            directory.path(),
            &["explain", "policy.program", "--json", "--policy-header"],
        );
        assert!(!output.status.success(), "{header:?} was accepted");
        assert!(output.stdout.is_empty(), "{header:?} printed a verdict");
    }
}

#[test]
fn a_missing_header_is_refused_and_the_header_must_be_in_the_leading_comments() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(
        directory.path().join("policy.program"),
        "run plain.helper\n",
    )
    .unwrap();
    let missing = execute(
        directory.path(),
        &["explain", "policy.program", "--json", "--policy-header"],
    );
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("no `# execute-policy:"));

    // A header after the first statement is not a header, so it is not honoured.
    std::fs::write(
        directory.path().join("policy.program"),
        "run plain.helper\n# execute-policy: allow undeclared\n",
    )
    .unwrap();
    let late = execute(
        directory.path(),
        &["explain", "policy.program", "--json", "--policy-header"],
    );
    assert!(
        !late.status.success(),
        "a late header must not approve anything"
    );
}

#[test]
fn policy_header_is_refused_for_text_output_and_for_non_finite_programs() {
    let directory = tempfile::tempdir().unwrap();
    policy_program(directory.path(), "# execute-policy: allow undeclared\n");
    let text = execute(
        directory.path(),
        &["explain", "policy.program", "--policy-header"],
    );
    assert!(!text.status.success());
    assert!(String::from_utf8_lossy(&text.stderr).contains("--policy-header requires --json"));

    std::fs::write(directory.path().join("graph.program"), "graph demo\n").unwrap();
    let graph = execute(
        directory.path(),
        &["explain", "graph.program", "--json", "--policy-header"],
    );
    assert!(!graph.status.success());
    assert!(String::from_utf8_lossy(&graph.stderr).contains("not graph programs"));
    assert!(graph.stdout.is_empty());
}
