//! `use value <value>` sets the pipeline input from a text value without launching a process.
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};
use tempfile::TempDir;

fn manifests() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests")
}

fn run_script(directory: &Path, command: &str, script: &str, extra: &[&str]) -> Output {
    let path = directory.join("workflow.program");
    fs::write(&path, script).unwrap();
    let mut process = Command::new(env!("CARGO_BIN_EXE_execute"));
    process.current_dir(directory);
    process.arg(command).arg(&path);
    process.arg("--manifest-dir").arg(manifests());
    process.args(extra);
    process.output().unwrap()
}

fn run(script: &str, extra: &[&str]) -> (TempDir, Output) {
    let directory = tempfile::tempdir().unwrap();
    let output = run_script(directory.path(), "run", script, extra);
    (directory, output)
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn spawn_count(output: &Output) -> usize {
    stderr(output)
        .lines()
        .filter(|line| line.contains("\"event\":\"spawn\""))
        .count()
}

#[test]
fn value_reaches_the_next_command_with_one_process() {
    let script = "accept message as text with default \"hello world\"\nuse value message\npipe to wc with flags [\"-c\"]\n";
    let (_directory, output) = run(script, &["--trace"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(spawn_count(&output), 1, "{}", stderr(&output));
    let text = String::from_utf8_lossy(&output.stdout);
    assert_eq!(text.trim(), "11", "{text}");
}

#[test]
fn literal_text_value_is_accepted() {
    let script = "use value \"abc\"\npipe to wc with flags [\"-c\"]\n";
    let (_directory, output) = run(script, &["--trace"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(spawn_count(&output), 1, "{}", stderr(&output));
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "3");
}

#[test]
fn structured_binding_is_rejected_before_any_process_starts() {
    let script = "accept items as list of text with default [\"a\"]\nrun \"printf\" with arguments [\"effect\"]\nuse value items\npipe to wc with flags [\"-c\"]\n";
    let directory = tempfile::tempdir().unwrap();
    let output = run_script(directory.path(), "check", script, &[]);
    assert_ne!(output.status.code(), Some(0));
    assert!(stderr(&output).contains("use value"), "{}", stderr(&output));
}

#[test]
fn integer_literal_is_rejected() {
    let directory = tempfile::tempdir().unwrap();
    let output = run_script(
        directory.path(),
        "check",
        "use value 5\npipe to wc with flags [\"-c\"]\n",
        &[],
    );
    assert_ne!(output.status.code(), Some(0));
    assert!(stderr(&output).contains("use value"), "{}", stderr(&output));
}

#[test]
fn undefined_binding_is_rejected() {
    let directory = tempfile::tempdir().unwrap();
    let output = run_script(
        directory.path(),
        "check",
        "use value missing\npipe to wc with flags [\"-c\"]\n",
        &[],
    );
    assert_ne!(output.status.code(), Some(0));
    assert!(stderr(&output).contains("missing"), "{}", stderr(&output));
}

#[test]
fn use_value_requires_a_value() {
    let directory = tempfile::tempdir().unwrap();
    let output = run_script(directory.path(), "check", "use value\n", &[]);
    assert_ne!(output.status.code(), Some(0));
    assert!(stderr(&output).contains("use value"), "{}", stderr(&output));
}

#[test]
fn explain_names_the_step_without_a_process() {
    let directory = tempfile::tempdir().unwrap();
    let output = run_script(
        directory.path(),
        "explain",
        "accept message as text with default \"x\"\nuse value message\npipe to wc with flags [\"-c\"]\n",
        &[],
    );
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("use value"), "{text}");
    assert!(text.contains("launches no process"), "{text}");
}
