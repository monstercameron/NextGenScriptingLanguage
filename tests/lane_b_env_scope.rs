//! Per-command environment: `and environment {...}` sets variables for one native
//! command, and `and without environment [...]` removes them for that command only.
//! Values are secrets: explain shows names only.
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tempfile::TempDir;

fn manifests() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests")
}

fn run_script(directory: &Path, command: &str, script: &str, extra: &[&str]) -> Output {
    let path = directory.join("workflow.program");
    fs::write(&path, script).unwrap();
    Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(directory)
        .env("LANE_B_UNSET", "present")
        .arg(command)
        .arg(&path)
        .arg("--manifest-dir")
        .arg(manifests())
        .args(extra)
        .output()
        .unwrap()
}

fn run(script: &str, extra: &[&str]) -> (TempDir, Output) {
    let directory = tempfile::tempdir().unwrap();
    let output = run_script(directory.path(), "run", script, extra);
    (directory, output)
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn environment_set_applies_to_that_command_only() {
    let (_directory, output) = run(
        "run \"env\" with arguments [] and environment {\"LANE_B_SCOPED\": \"yes\"}\nrun \"env\" with arguments []\n",
        &[],
    );
    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    let text = stdout(&output);
    // Both commands print their environment. Only the first has the variable,
    // so the marker appears exactly once.
    assert_eq!(text.matches("LANE_B_SCOPED=yes").count(), 1, "{text}");
}

#[test]
fn environment_without_removes_for_that_command_only() {
    let (_directory, output) = run(
        "run \"env\" with arguments [] and without environment [\"LANE_B_UNSET\"]\nrun \"env\" with arguments []\n",
        &[],
    );
    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    // The second command still inherits LANE_B_UNSET from the Execute process.
    assert_eq!(stdout(&output).matches("LANE_B_UNSET=present").count(), 1);
}

#[test]
fn environment_value_can_come_from_a_binding() {
    let (_directory, output) = run(
        "accept mode as text with default \"zz\"\nrun \"env\" with arguments [] and environment {\"LANE_B_MODE\": mode}\n",
        &["--input", "mode=from-input"],
    );
    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    assert!(
        stdout(&output).contains("LANE_B_MODE=from-input"),
        "{}",
        stdout(&output)
    );
}

#[test]
fn explain_shows_names_but_not_values() {
    let directory = tempfile::tempdir().unwrap();
    let output = run_script(
        directory.path(),
        "explain",
        "run \"env\" with arguments [] and environment {\"LANE_B_SECRET\": \"hunter2-value\"} and without environment [\"LANE_B_OLD\"]\n",
        &[],
    );
    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    let text = stdout(&output);
    assert!(text.contains("LANE_B_SECRET"), "{text}");
    assert!(text.contains("LANE_B_OLD"), "{text}");
    assert!(!text.contains("hunter2-value"), "{text}");
}

#[test]
fn invalid_environment_forms_are_rejected_before_effects() {
    let directory = tempfile::tempdir().unwrap();
    let cases = [
        (
            "run \"env\" with arguments [] and environment {\"BAD=NAME\": \"x\"}\n",
            "invalid environment name",
        ),
        (
            "run \"env\" with arguments [] and environment {\"LANE_B_NUM\": 1}\n",
            "must be a text literal",
        ),
        (
            "run \"env\" with arguments [] and environment {\"LANE_B_BOTH\": \"x\"} and without environment [\"LANE_B_BOTH\"]\n",
            "both set and removed",
        ),
        (
            "run \"env\" with arguments [] and environment {\"LANE_B_X\": \"x\"}\npipe to \"tr\" with arguments [\"a\", \"b\"]\n",
            "single command",
        ),
        (
            "run cd with path \".\" and environment {\"LANE_B_X\": \"x\"}\n",
            "builtin",
        ),
    ];
    for (script, expected) in cases {
        let output = run_script(directory.path(), "check", script, &[]);
        assert_ne!(output.status.code(), Some(0), "{script}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(expected), "{script}: {stderr}");
    }
}
