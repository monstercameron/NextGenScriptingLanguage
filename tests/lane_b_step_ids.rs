//! Step labels (`fetch:`) and derived step ids. Labels are validated when parsed. A
//! labelled id stays the same when a statement is inserted above it, in explain output,
//! trace events and run records.
use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

fn manifests() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests")
}

fn run_cli(directory: &Path, command: &str, script: &str, extra: &[&str]) -> Output {
    let path = directory.join("workflow.program");
    fs::write(&path, script).unwrap();
    Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(directory)
        .arg(command)
        .arg(&path)
        .arg("--manifest-dir")
        .arg(manifests())
        .args(extra)
        .output()
        .unwrap()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

const BASE: &str = "fetch: run printf with message \"a\"\nrun printf with message \"b\"\n";

fn inserted_above() -> String {
    format!("run printf with message \"new\"\n{BASE}")
}

/// Every `id` in an explain plan, in row order.
fn explain_ids(directory: &Path, script: &str) -> Vec<String> {
    let output = run_cli(directory, "explain", script, &["--json"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let plan: Value = serde_json::from_slice(&output.stdout).unwrap();
    plan["steps"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|row| row["id"].as_str().map(str::to_owned))
        .collect()
}

#[test]
fn duplicate_labels_are_rejected() {
    let directory = tempfile::tempdir().unwrap();
    let output = run_cli(
        directory.path(),
        "check",
        "fetch: run printf with message \"a\"\nfetch: run printf with message \"b\"\n",
        &[],
    );
    assert_ne!(output.status.code(), Some(0));
    assert!(
        stderr(&output).contains("duplicate step label"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn keywords_cannot_be_labels() {
    let directory = tempfile::tempdir().unwrap();
    let output = run_cli(
        directory.path(),
        "check",
        "if: run printf with message \"a\"\n",
        &[],
    );
    assert_ne!(output.status.code(), Some(0));
    assert!(
        stderr(&output).contains("is a keyword"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn a_label_needs_a_statement_and_a_statement_target() {
    let directory = tempfile::tempdir().unwrap();
    let alone = run_cli(directory.path(), "check", "fetch:\n", &[]);
    assert!(
        stderr(&alone).contains("must precede a statement"),
        "{}",
        stderr(&alone)
    );

    let declaration = run_cli(
        directory.path(),
        "check",
        "fetch: accept name as text\n",
        &[],
    );
    assert!(
        stderr(&declaration).contains("cannot precede a declaration"),
        "{}",
        stderr(&declaration)
    );
}

#[test]
fn explain_keeps_a_labelled_id_when_a_statement_is_inserted_above_it() {
    let before_dir = tempfile::tempdir().unwrap();
    let after_dir = tempfile::tempdir().unwrap();
    let before = explain_ids(before_dir.path(), BASE);
    let after = explain_ids(after_dir.path(), &inserted_above());
    assert!(before.contains(&"fetch".to_owned()), "{before:?}");
    assert!(after.contains(&"fetch".to_owned()), "{after:?}");
    // The unlabelled statement after the label has the same content, so its derived id
    // is unchanged too.
    let derived = |ids: &[String]| {
        ids.iter()
            .filter(|id| id.contains(':'))
            .cloned()
            .collect::<Vec<_>>()
    };
    assert!(derived(&before)
        .iter()
        .all(|id| derived(&after).contains(id)));
}

#[test]
fn trace_events_keep_the_labelled_id_when_a_statement_is_inserted_above_it() {
    let labelled_events = |script: &str| {
        let directory = tempfile::tempdir().unwrap();
        let output = run_cli(directory.path(), "run", script, &["--trace"]);
        assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
        stderr(&output)
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|event| event["id"] == "fetch")
            .count()
    };
    assert!(labelled_events(BASE) > 0);
    assert_eq!(labelled_events(BASE), labelled_events(&inserted_above()));
}

#[test]
fn run_records_name_the_labelled_step() {
    let directory = tempfile::tempdir().unwrap();
    let record = directory.path().join("record");
    let output = run_cli(
        directory.path(),
        "run",
        BASE,
        &["--record", record.to_str().unwrap()],
    );
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    // Recorded step events live in trace.jsonl; each carries the step's id.
    let trace = fs::read_to_string(record.join("trace.jsonl")).unwrap();
    assert!(trace.contains("\"id\":\"fetch\""), "{trace}");
}
