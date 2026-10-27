#![cfg(feature = "tooling")]
//! `execute run-parts`: lexical order, stop-on-failure, `--continue`, `--once` state with
//! content hashes, changed files reported and not rerun, and parseable `--json`.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};
use tempfile::TempDir;

fn run_parts(cwd: &Path, arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(cwd)
        .arg("run-parts")
        .args(arguments)
        .output()
        .unwrap()
}

fn save_program(text: &str, output: &str) -> String {
    format!("run \"printf\" with arguments [\"{text}\"]\nthen save to \"{output}\"\n")
}

fn fixture() -> TempDir {
    let directory = TempDir::new().unwrap();
    let parts = directory.path().join("parts");
    fs::create_dir(&parts).unwrap();
    fs::write(
        parts.join("01-first.program"),
        save_program("first", "first.txt"),
    )
    .unwrap();
    fs::write(
        parts.join("02-fail.program"),
        "run \"no-such-tool-for-run-parts\"\n",
    )
    .unwrap();
    fs::write(
        parts.join("03-last.program"),
        save_program("last", "last.txt"),
    )
    .unwrap();
    directory
}

#[test]
fn programs_run_in_lexical_order_and_stop_at_the_first_failure() {
    let directory = fixture();
    let parts = directory.path().join("parts");
    let output = run_parts(directory.path(), &["parts"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(directory.path().join("first.txt").exists());
    assert!(
        !directory.path().join("last.txt").exists(),
        "a later program ran after a failure"
    );
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(
        stdout.contains("run-parts: 02-fail.program failed"),
        "{stdout}"
    );
    assert!(
        stdout.contains("run-parts: 03-last.program not run"),
        "{stdout}"
    );
    assert!(parts.is_dir());
}

#[test]
fn continue_runs_past_a_failure_and_still_fails_overall() {
    let directory = fixture();
    let output = run_parts(directory.path(), &["--continue", "parts"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(directory.path().join("last.txt").exists());
}

#[test]
fn once_skips_completed_programs_and_reports_changed_ones_without_rerunning() {
    let directory = TempDir::new().unwrap();
    let root = directory.path();
    fs::create_dir(root.join("parts")).unwrap();
    fs::write(
        root.join("parts/01-first.program"),
        save_program("first", "first.txt"),
    )
    .unwrap();
    fs::write(
        root.join("parts/02-second.program"),
        save_program("second", "second.txt"),
    )
    .unwrap();

    let first = run_parts(root, &["--once", "state", "--json", "parts"]);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&first.stdout).unwrap();
    assert_eq!(report["protocol"], "execute.run-parts/1");
    assert_eq!(report["summary"]["ran"], 2);
    assert!(root.join("state/state.json").exists());

    let second = run_parts(root, &["--once", "state", "--json", "parts"]);
    let report: serde_json::Value = serde_json::from_slice(&second.stdout).unwrap();
    assert_eq!(report["summary"]["skipped"], 2);
    assert_eq!(report["summary"]["ran"], 0);

    // Change a completed program: it is reported, and its new output is not produced.
    fs::write(
        root.join("parts/01-first.program"),
        save_program("changed", "first.txt"),
    )
    .unwrap();
    fs::remove_file(root.join("first.txt")).unwrap();
    let third = run_parts(root, &["--once", "state", "--json", "parts"]);
    let report: serde_json::Value = serde_json::from_slice(&third.stdout).unwrap();
    assert_eq!(report["summary"]["changed"], 1);
    assert_eq!(report["summary"]["skipped"], 1);
    assert!(
        !root.join("first.txt").exists(),
        "a changed completed program was rerun"
    );
}

#[test]
fn a_failed_program_is_not_recorded_so_it_runs_again() {
    let directory = fixture();
    let root = directory.path();
    let first = run_parts(root, &["--once", "state", "parts"]);
    assert_eq!(first.status.code(), Some(1));
    let state = fs::read_to_string(root.join("state/state.json")).unwrap();
    assert!(state.contains("01-first.program"), "{state}");
    assert!(!state.contains("02-fail.program"), "{state}");
    assert!(!state.contains("03-last.program"), "{state}");
}

#[test]
fn json_output_is_a_single_document_on_standard_output() {
    let directory = TempDir::new().unwrap();
    fs::create_dir(directory.path().join("parts")).unwrap();
    fs::write(
        directory.path().join("parts/only.program"),
        save_program("only", "only.txt"),
    )
    .unwrap();
    let output = run_parts(directory.path(), &["--json", "parts"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["results"][0]["name"], "only.program");
    assert_eq!(report["results"][0]["status"], "ran");
}
