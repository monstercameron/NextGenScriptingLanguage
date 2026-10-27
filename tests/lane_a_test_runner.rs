#![cfg(feature = "tooling")]
//! `execute test`: discovery, sidecar expectations, TAP and JSON output, fixtures on
//! PATH. Portable tests use `printf` for output; the fixture test is Unix-only because
//! it uses an executable shell script as the fake tool.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};
use tempfile::TempDir;

fn run_test(arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_execute"))
        .arg("test")
        .args(arguments)
        .output()
        .unwrap()
}

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn suite() -> TempDir {
    let directory = TempDir::new().unwrap();
    let root = directory.path();
    write(
        &root.join("ok.test.program"),
        "accept name as text\nrun \"printf\" with arguments [\"hello %s\", name]\n",
    );
    write(
        &root.join("ok.test.json"),
        r#"{"inputs": {"name": "world"}, "expect": {"stdout": "hello world"}}"#,
    );
    write(
        &root.join("fail.test.program"),
        "accept name as text\nrun \"printf\" with arguments [\"hello %s\", name]\n",
    );
    write(
        &root.join("fail.test.json"),
        r#"{"inputs": {"name": "world"}, "expect": {"stdout": "bye"}}"#,
    );
    write(
        &root.join("files.test.program"),
        "run \"printf\" with arguments [\"saved\"]\nthen save to \"out.txt\"\n",
    );
    write(
        &root.join("files.test.json"),
        r#"{"expect": {"files": {"out.txt": "saved"}}}"#,
    );
    directory
}

#[test]
fn tap_reports_passes_failures_and_exits_nonzero() {
    let directory = suite();
    let output = run_test(&[directory.path().to_str().unwrap()]);
    assert_eq!(output.status.code(), Some(1));
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(stdout.starts_with("TAP version 13\n1..3\n"), "{stdout}");
    // Discovery is sorted by path, so the order is fail, files, ok.
    assert!(stdout.contains("not ok 1 - fail"), "{stdout}");
    assert!(stdout.contains("ok 2 - files"), "{stdout}");
    assert!(stdout.contains("ok 3 - ok"), "{stdout}");
    assert!(
        stdout.contains("expected \\\"bye\\\"") || stdout.contains("expected \"bye\""),
        "{stdout}"
    );
}

#[test]
fn json_output_is_one_document_for_agents() {
    let directory = suite();
    let output = run_test(&["--json", directory.path().to_str().unwrap()]);
    assert_eq!(output.status.code(), Some(1));
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["protocol"], "execute.test/1");
    assert_eq!(report["summary"]["total"], 3);
    assert_eq!(report["summary"]["failed"], 1);
    let fail = report["tests"]
        .as_array()
        .unwrap()
        .iter()
        .find(|test| test["name"] == "fail")
        .unwrap();
    assert_eq!(fail["status"], "fail");
}

#[test]
fn a_passing_directory_exits_zero() {
    let directory = TempDir::new().unwrap();
    write(
        &directory.path().join("only.test.program"),
        "run \"printf\" with arguments [\"done\"]\n",
    );
    let output = run_test(&[directory.path().to_str().unwrap()]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("ok 1 - only"));
}

#[test]
fn a_directory_with_no_tests_is_an_error() {
    let directory = TempDir::new().unwrap();
    let output = run_test(&[directory.path().to_str().unwrap()]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("no .test.program files"));
}

#[cfg(unix)]
#[test]
fn fixtures_bin_is_prepended_to_path_for_each_test() {
    use std::os::unix::fs::PermissionsExt;
    let directory = TempDir::new().unwrap();
    let root = directory.path();
    let tool = root.join("fixtures/bin/fake-tool");
    write(&tool, "#!/bin/sh\nprintf 'fake %s' \"$1\"\n");
    fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();
    write(
        &root.join("tool.test.program"),
        "run \"fake-tool\" with arguments [\"a\"]\n",
    );
    write(
        &root.join("tool.test.json"),
        r#"{"expect": {"stdout": "fake a"}}"#,
    );
    let output = run_test(&[root.to_str().unwrap()]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
}
