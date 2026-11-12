#![cfg(feature = "tooling")]
//! `execute test --filter NAME` (issue #344). Three sidecar cases sit in one directory;
//! `a` carries a wrong expectation, so it would fail if it ran. Setup follows
//! `tests/issue_344_stderr_expect.rs`.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};
use tempfile::TempDir;

fn run_test(args: &[&str], directory: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_execute"))
        .arg("test")
        .args(args)
        .arg(directory)
        .output()
        .unwrap()
}

fn suite() -> TempDir {
    let directory = TempDir::new().unwrap();
    for name in ["a", "b", "c"] {
        fs::write(
            directory.path().join(format!("{name}.test.program")),
            "fail with message \"stop here\"\n",
        )
        .unwrap();
    }
    fs::write(
        directory.path().join("a.test.json"),
        r#"{"expect": {"status": 0}}"#,
    )
    .unwrap();
    for name in ["b", "c"] {
        fs::write(
            directory.path().join(format!("{name}.test.json")),
            r#"{"expect": {"status": 1}}"#,
        )
        .unwrap();
    }
    directory
}

#[test]
fn filter_runs_only_the_named_test_and_reports_the_rest_as_skipped() {
    let directory = suite();
    let output = run_test(&["--filter", "b"], directory.path());
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(output.status.success(), "{stdout}");
    assert!(stdout.contains("1..3\nok 1 - b\n"), "{stdout}");
    assert!(
        stdout.contains("ok 2 - a # SKIP not selected by --filter"),
        "{stdout}"
    );
    assert!(
        stdout.contains("ok 3 - c # SKIP not selected by --filter"),
        "{stdout}"
    );
}

#[test]
fn json_filter_lists_the_tests_left_out() {
    let directory = suite();
    let output = run_test(&["--json", "--filter", "b"], directory.path());
    assert!(output.status.success());
    let document: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(document["summary"]["total"], 1);
    assert_eq!(document["tests"][0]["name"], "b");
    assert_eq!(document["not_run"], serde_json::json!(["a", "c"]));
}

#[test]
fn unknown_filter_name_fails_and_lists_the_tests_found() {
    let directory = suite();
    let output = run_test(&["--filter", "nope"], directory.path());
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(stderr.contains("matches no test"), "{stderr}");
    assert!(stderr.contains("a, b, c"), "{stderr}");
}
