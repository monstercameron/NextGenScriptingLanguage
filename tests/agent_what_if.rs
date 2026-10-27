//! `execute run FILE --what-if` and `--confirm FINGERPRINT`: the run happens only
//! when the plan still matches the one that was reviewed.

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const HELPER: &str = env!("CARGO_BIN_EXE_execute-model");
const SOURCE: &str = "run plain.helper\nthen save to \"out.txt\"\n";

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

fn what_if(directory: &Path) -> Value {
    let output = execute(directory, &["tool.program", "--what-if"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("one JSON object")
}

fn fingerprint(report: &Value) -> String {
    report["fingerprint"].as_str().unwrap().to_owned()
}

#[test]
fn what_if_prints_the_plan_and_a_fingerprint_and_runs_nothing() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("tool.program"), SOURCE).unwrap();
    let report = what_if(directory.path());
    assert_eq!(report["protocol"], "execute.what_if/2");
    assert_eq!(report["plan"]["protocol"], "execute.explain/2");
    let fp = fingerprint(&report);
    assert_eq!(fp.len(), 64, "a SHA-256 hex digest");
    assert!(fp.bytes().all(|byte| byte.is_ascii_hexdigit()));
    assert!(
        !directory.path().join("out.txt").exists(),
        "what-if must not run the program"
    );
}

#[test]
fn the_fingerprint_is_stable_for_the_same_plan() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("tool.program"), SOURCE).unwrap();
    assert_eq!(
        fingerprint(&what_if(directory.path())),
        fingerprint(&what_if(directory.path()))
    );
}

#[test]
fn confirming_the_reviewed_fingerprint_runs_the_program() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("tool.program"), SOURCE).unwrap();
    let fp = fingerprint(&what_if(directory.path()));
    let output = execute(directory.path(), &["tool.program", "--confirm", &fp]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        directory.path().join("out.txt").exists(),
        "a confirmed run must execute"
    );
}

#[test]
fn a_stale_fingerprint_is_refused_before_any_effect() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("tool.program"), SOURCE).unwrap();
    let stale = "0".repeat(64);
    let output = execute(directory.path(), &["tool.program", "--confirm", &stale]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("plan changed"));
    assert!(
        !directory.path().join("out.txt").exists(),
        "a refused run must not execute"
    );
}

#[test]
fn editing_the_program_after_review_invalidates_the_confirmation() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("tool.program"), SOURCE).unwrap();
    let fp = fingerprint(&what_if(directory.path()));
    std::fs::write(
        directory.path().join("tool.program"),
        "run plain.helper\nthen save to \"other.txt\"\n",
    )
    .unwrap();
    let output = execute(directory.path(), &["tool.program", "--confirm", &fp]);
    assert!(
        !output.status.success(),
        "an edited program must not run on the old confirmation"
    );
    assert!(!directory.path().join("other.txt").exists());
    assert!(!directory.path().join("out.txt").exists());
}

#[test]
fn what_if_and_confirm_are_accepted_only_for_run() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("tool.program"), SOURCE).unwrap();
    for arguments in [
        vec!["explain", "tool.program", "--what-if"],
        vec!["check", "tool.program", "--confirm", "abc"],
    ] {
        let output = execute(directory.path(), &arguments);
        assert!(!output.status.success(), "{arguments:?} must be rejected");
        assert!(output.stdout.is_empty(), "{arguments:?} printed output");
    }
}

#[test]
fn direct_arguments_are_reviewed_and_change_fingerprint() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("tool.program");
    std::fs::write(&path, "run \"rm\" with arguments [\"-f\", \"a.txt\"]\n").unwrap();
    let first = what_if(directory.path());
    std::fs::write(&path, "run \"rm\" with arguments [\"-rf\", \"/home\"]\n").unwrap();
    let second = what_if(directory.path());
    assert_eq!(
        first["plan"]["steps"][0]["argv"],
        serde_json::json!(["-f", "a.txt"])
    );
    assert_eq!(
        second["plan"]["steps"][0]["argv"],
        serde_json::json!(["-rf", "/home"])
    );
    assert_ne!(fingerprint(&first), fingerprint(&second));
}

#[test]
fn sensitive_nested_argument_matches_the_review_settings_marker() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("tool.program"),
        "accept token as text from secret prompt \"Token?\"\nrun \"printf\" with arguments [\"%s\", token]\n").unwrap();
    let output = execute(
        directory.path(),
        &[
            "tool.program",
            "--what-if",
            "--input",
            "token=private-canary-462",
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!String::from_utf8_lossy(&output.stdout).contains("private-canary-462"));
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        report["plan"]["steps"][0]["argv"][1],
        report["settings"]["inputs"][0]["value"]
    );
    assert_eq!(report["plan"]["steps"][0]["argv"][1]["sensitive"], true);
}

#[test]
fn review_versions_have_distinct_fingerprints_and_confirmation_requires_same_version() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("tool.program"), SOURCE).unwrap();
    let v2 = what_if(directory.path());
    let output = execute(
        directory.path(),
        &["tool.program", "--what-if", "--explain-version", "1"],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let v1: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(v1["protocol"], "execute.what_if/1");
    assert_eq!(v1["plan"]["protocol"], "execute.explain/1");
    assert_ne!(fingerprint(&v1), fingerprint(&v2));
    let fp = fingerprint(&v1);
    let wrong_version = execute(directory.path(), &["tool.program", "--confirm", &fp]);
    assert!(!wrong_version.status.success());
    let same_version = execute(
        directory.path(),
        &["tool.program", "--confirm", &fp, "--explain-version", "1"],
    );
    assert!(
        same_version.status.success(),
        "{}",
        String::from_utf8_lossy(&same_version.stderr)
    );
}
