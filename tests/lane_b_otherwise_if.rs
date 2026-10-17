//! `otherwise if <condition>` chains: each clause runs at most one body, in order,
//! and a chain without a final `otherwise` may match nothing.
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

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

const CHAIN: &str = "accept mode as text with default \"b\"
if mode is \"a\"
  run printf with message \"A\"
otherwise if mode is \"b\"
  run printf with message \"B\"
otherwise if mode is \"c\"
  run printf with message \"C\"
otherwise
  run printf with message \"other\"
";

#[test]
fn chain_runs_the_first_matching_clause_only() {
    let (_directory, output) = run(CHAIN, &[]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(stdout(&output), "B\n");

    let (_directory, output) = run(CHAIN, &["--input", "mode=c"]);
    assert_eq!(stdout(&output), "C\n", "{}", stderr(&output));

    let (_directory, output) = run(CHAIN, &["--input", "mode=z"]);
    assert_eq!(stdout(&output), "other\n", "{}", stderr(&output));
}

#[test]
fn chain_without_final_otherwise_may_match_nothing() {
    let script = "accept mode as text with default \"z\"
if mode is \"a\"
  run printf with message \"A\"
otherwise if mode is \"b\"
  run printf with message \"B\"
run printf with message \"after\"
";
    let (_directory, output) = run(script, &[]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(stdout(&output), "after\n");
}

#[test]
fn chain_clauses_can_be_command_conditions() {
    let script = "if run false succeeds
  run printf with message \"no\"
otherwise if run printf with message \"x\" succeeds
  run printf with message \"command clause\"
";
    let (_directory, output) = run(script, &[]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(stdout(&output), "command clause\n");
}

#[test]
fn otherwise_if_without_a_body_is_rejected_before_effects() {
    let directory = tempfile::tempdir().unwrap();
    let output = run_script(
        directory.path(),
        "check",
        "run printf with message \"effect\"\nif \"a\" is \"a\"\n  run printf with message \"x\"\notherwise if \"b\" is \"b\"\n",
        &[],
    );
    assert_ne!(output.status.code(), Some(0));
    assert!(
        stderr(&output).contains("if body must contain an indented statement"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn explain_shows_the_nested_clause() {
    let directory = tempfile::tempdir().unwrap();
    let output = run_script(directory.path(), "explain", CHAIN, &[]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("otherwise"), "{text}");
    assert!(text.contains("\"b\""), "{text}");
}
