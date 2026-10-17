//! Multi-line statements: bracketed continuation and raw `"""` block strings.
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};
use tempfile::TempDir;

fn manifests() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests")
}

fn run_script(directory: &Path, command: &str, script: &str) -> Output {
    let path = directory.join("workflow.program");
    fs::write(&path, script).unwrap();
    Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(directory)
        .arg(command)
        .arg(&path)
        .arg("--manifest-dir")
        .arg(manifests())
        .output()
        .unwrap()
}

fn run(script: &str) -> (TempDir, Output) {
    let directory = tempfile::tempdir().unwrap();
    let output = run_script(directory.path(), "run", script);
    (directory, output)
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn open_bracket_continues_the_statement_onto_following_lines() {
    let script = "use value \"hello\"\npipe to wc with flags [\n  \"-c\"\n]\n";
    let (_directory, output) = run(script);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "5");
}

#[test]
fn raw_block_string_keeps_quotes_backslashes_and_hash_text_verbatim() {
    let script = "use value \"\"\"\nline one \"quoted\" \\back\n# not a comment\n\"\"\"\npipe to wc with flags [\"-c\"]\n";
    let (_directory, output) = run(script);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let expected = "line one \"quoted\" \\back\n# not a comment".len();
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        expected.to_string()
    );
}

#[test]
fn unclosed_bracket_is_rejected_before_effects() {
    let directory = tempfile::tempdir().unwrap();
    let output = run_script(
        directory.path(),
        "check",
        "run printf with message \"effect\"\nuse value \"x\"\npipe to wc with flags [\n  \"-c\"\n",
    );
    assert_ne!(output.status.code(), Some(0));
    // The statement starts on line 3; its own parser names the unclosed list.
    assert!(stderr(&output).contains("line 3"), "{}", stderr(&output));
}

#[test]
fn unclosed_raw_block_is_rejected() {
    let directory = tempfile::tempdir().unwrap();
    let output = run_script(
        directory.path(),
        "check",
        "use value \"\"\"\nno closing line\n",
    );
    assert_ne!(output.status.code(), Some(0));
    assert!(stderr(&output).contains("closing"), "{}", stderr(&output));
}

#[test]
fn single_line_statements_are_unchanged() {
    let (_directory, output) = run("use value \"abc\"\npipe to wc with flags [\"-c\"]\n");
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "3");
}
