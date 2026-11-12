#![cfg(feature = "tooling")]
//! `execute test` with `expect.stderr_contains` and `expect.stderr` (issue #344). The
//! program fails with a message, which the runtime writes to stderr with status 1. Setup
//! follows `tests/lane_a_test_runner.rs`. The run uses an empty temporary `--manifest-dir`:
//! the repository's `manifests` folder is refused by the trust gate on this Windows machine.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};
use tempfile::TempDir;

fn run_suite(directory: &Path, manifests: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_execute"))
        .arg("test")
        .arg("--manifest-dir")
        .arg(manifests)
        .arg(directory)
        .output()
        .unwrap()
}

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

/// One test program that fails with "stop here" on stderr, plus the given sidecar.
fn suite(sidecar: &str) -> TempDir {
    let directory = TempDir::new().unwrap();
    write(
        &directory.path().join("stop.test.program"),
        "fail with message \"stop here\"\n",
    );
    write(&directory.path().join("stop.test.json"), sidecar);
    directory
}

#[test]
fn stderr_contains_passes_when_the_message_is_written() {
    let directory = suite(r#"{"expect": {"status": 1, "stderr_contains": ["stop here"]}}"#);
    let manifests = TempDir::new().unwrap();
    let output = run_suite(directory.path(), manifests.path());
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(output.status.success(), "{stdout}");
    assert!(stdout.contains("ok 1 - stop"), "{stdout}");
}

#[test]
fn stderr_expectation_reports_a_readable_failure() {
    let directory = suite(r#"{"expect": {"status": 1, "stderr_contains": ["missing text"]}}"#);
    let manifests = TempDir::new().unwrap();
    let output = run_suite(directory.path(), manifests.path());
    assert_eq!(output.status.code(), Some(1));
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(stdout.contains("not ok 1 - stop"), "{stdout}");
    assert!(stdout.contains("does not contain"), "{stdout}");
    assert!(stdout.contains("missing text"), "{stdout}");
}

#[test]
fn exact_stderr_mismatch_is_reported_as_a_failure() {
    let directory = suite(r#"{"expect": {"status": 1, "stderr": "not the message"}}"#);
    let manifests = TempDir::new().unwrap();
    let output = run_suite(directory.path(), manifests.path());
    assert_eq!(output.status.code(), Some(1));
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(stdout.contains("not ok 1 - stop"), "{stdout}");
    assert!(stdout.contains("but expected"), "{stdout}");
    assert!(stdout.contains("not the message"), "{stdout}");
}
