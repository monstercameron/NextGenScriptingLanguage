//! `--json-errors` through the real `execute` binary. The stderr object is the
//! final line of stderr. Child stderr is forwarded unchanged before it, and the
//! exit statuses match the flagless run.

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

#[path = "common/staged_helpers.rs"]
mod staged_helpers;

/// The cargo-built model helper, staged as a private single-link copy: the
/// trusted-executable gate refuses the hardlinked `target/` original.
fn helper() -> &'static std::path::Path {
    staged_helpers::execute_model()
}

fn manifests_with(directory: &Path) -> PathBuf {
    let manifests = directory.join("manifests");
    std::fs::create_dir_all(&manifests).unwrap();
    // A command that exits 1: the helper rejects an unknown argument.
    let failing = serde_json::json!({
        "name": "fail.helper",
        "binary": helper(),
        "accepts_stdin": false,
        "parameters": {},
        "arguments": [{ "literal": "--nope" }]
    });
    std::fs::write(manifests.join("fail.helper.json"), failing.to_string()).unwrap();
    // A command that succeeds, so the flag can be checked on a passing run too.
    let passing = serde_json::json!({
        "name": "ok.helper",
        "binary": helper(),
        "accepts_stdin": false,
        "parameters": {},
        "arguments": [{ "literal": "--version" }]
    });
    std::fs::write(manifests.join("ok.helper.json"), passing.to_string()).unwrap();
    manifests
}

fn execute(directory: &Path, arguments: &[&str]) -> Output {
    let manifests = manifests_with(directory);
    let mut command = Command::new(env!("CARGO_BIN_EXE_execute"));
    command
        .args(arguments)
        .arg("--manifest-dir")
        .arg(&manifests)
        .current_dir(directory)
        .env_remove("EXECUTE_MODELS_OFFLINE")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command.output().expect("start execute")
}

fn status_code(output: &Output) -> Option<i32> {
    output.status.code()
}

/// Split stderr into complete lines. Each line keeps its newline.
fn lines(stderr: &[u8]) -> Vec<String> {
    String::from_utf8(stderr.to_vec())
        .unwrap()
        .split_inclusive('\n')
        .map(str::to_owned)
        .collect()
}

/// The stderr object is the final line. Child stderr may come before it, so
/// `json_only` asserts the strict form for runs where no child wrote anything.
fn last_json_line(stderr: &[u8]) -> Value {
    let all = lines(stderr);
    let last = all
        .last()
        .unwrap_or_else(|| panic!("stderr is empty, expected a JSON object"));
    assert!(
        last.ends_with('\n'),
        "final stderr line is incomplete: {last:?}"
    );
    let value: Value = serde_json::from_str(last.trim_end())
        .unwrap_or_else(|error| panic!("final stderr line is not JSON ({error}): {last:?}"));
    assert_eq!(value["protocol"], "execute.error/1", "{value}");
    value
}

fn json_only(stderr: &[u8]) -> Value {
    assert_eq!(
        lines(stderr).len(),
        1,
        "no child wrote stderr, so the object must be the only line: {:?}",
        String::from_utf8_lossy(stderr)
    );
    last_json_line(stderr)
}

#[test]
fn command_failure_writes_one_structured_object_with_the_same_exit_status() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("fail.program"), "run fail.helper\n").unwrap();

    let plain = execute(directory.path(), &["fail.program"]);
    let flagged = execute(directory.path(), &["fail.program", "--json-errors"]);

    assert_eq!(
        status_code(&plain),
        status_code(&flagged),
        "exit status changed"
    );
    assert!(!plain.status.success());

    // Without the flag, the final stderr line is the human diagnostic. Child
    // stderr, which the helper writes first, comes before it.
    let plain_lines = lines(&plain.stderr);
    assert!(
        plain_lines
            .last()
            .is_some_and(|line| line.starts_with("execute: ")),
        "final plain line is not the diagnostic: {plain_lines:?}"
    );

    // With the flag, child stderr is forwarded unchanged, and the final line is
    // the JSON object. The flag replaces only Execute's own diagnostic line.
    let flagged_lines = lines(&flagged.stderr);
    assert!(
        flagged_lines
            .iter()
            .all(|line| !line.starts_with("execute: ")),
        "flagged stderr still contains the text diagnostic: {flagged_lines:?}"
    );
    assert_eq!(
        plain_lines[..plain_lines.len() - 1],
        flagged_lines[..flagged_lines.len() - 1],
        "child stderr changed in --json-errors mode"
    );

    let object = last_json_line(&flagged.stderr);
    assert_eq!(object["protocol"], "execute.error/1");
    assert_eq!(object["code"], "command_failed");
    assert_eq!(object["category"], "process");
    assert_eq!(object["exit_code"], status_code(&flagged).unwrap());
    assert!(object["message"].as_str().unwrap().contains("fail.helper"));
}

#[test]
fn parse_failure_uses_the_catch_all_code_and_keeps_the_exit_status() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("bad.program"), "then then then\n").unwrap();

    let plain = execute(directory.path(), &["bad.program"]);
    let flagged = execute(directory.path(), &["bad.program", "--json-errors"]);

    assert!(!flagged.status.success());
    assert_eq!(status_code(&plain), status_code(&flagged));
    let object = json_only(&flagged.stderr);
    assert_eq!(object["code"], "execute_error");
    assert_eq!(object["category"], "runtime");
    assert_eq!(object["exit_code"], status_code(&flagged).unwrap());
}

#[test]
fn flag_is_accepted_before_or_after_the_program_and_never_reaches_parsing() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("ok.program"), "run ok.helper\n").unwrap();

    for arguments in [
        vec!["--json-errors", "ok.program"],
        vec!["ok.program", "--json-errors"],
    ] {
        let output = execute(directory.path(), &arguments);
        assert!(
            output.status.success(),
            "{arguments:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            output.stderr.is_empty(),
            "successful run wrote stderr: {arguments:?}"
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("execute-model"));
    }
}

#[test]
fn stdout_and_exit_status_are_identical_with_and_without_the_flag_on_success() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("ok.program"), "run ok.helper\n").unwrap();
    let plain = execute(directory.path(), &["ok.program"]);
    let flagged = execute(directory.path(), &["ok.program", "--json-errors"]);
    assert_eq!(plain.stdout, flagged.stdout);
    assert_eq!(status_code(&plain), status_code(&flagged));
}

#[test]
fn a_missing_program_reports_a_structured_error_with_the_same_status() {
    let directory = tempfile::tempdir().unwrap();
    let plain = execute(directory.path(), &["does-not-exist.program"]);
    let flagged = execute(
        directory.path(),
        &["does-not-exist.program", "--json-errors"],
    );
    assert!(!flagged.status.success());
    assert_eq!(status_code(&plain), status_code(&flagged));
    let object = json_only(&flagged.stderr);
    assert_eq!(object["protocol"], "execute.error/1");
    assert_eq!(object["exit_code"], status_code(&flagged).unwrap());
}
