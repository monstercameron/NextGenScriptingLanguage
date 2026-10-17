//! CLI stdout streaming: a `run` whose output has no consumer writes to the
//! terminal as it runs; a consumed value keeps buffering and prints once.
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

fn run(script: &str) -> (TempDir, Output) {
    let directory = tempfile::tempdir().unwrap();
    let output = run_script(directory.path(), "run", script, &[]);
    (directory, output)
}

fn status(output: &Output) -> i32 {
    output.status.code().expect("process exit code")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn unconsumed_output_of_each_command_is_printed_in_order() {
    let (_directory, output) = run(r#"run printf with message "a"
run printf with message "b"
"#);
    assert_eq!(status(&output), 0, "{}", stderr(&output));
    assert_eq!(stdout(&output), "a\nb\n");
}

#[test]
fn consumed_output_is_buffered_and_printed_once() {
    let (_directory, output) = run(r#"run printf with message "x"
then tr with from "[:lower:]" and to "[:upper:]"
"#);
    assert_eq!(status(&output), 0, "{}", stderr(&output));
    assert_eq!(stdout(&output), "X\n");
}

#[test]
fn final_command_output_is_printed_once() {
    let (_directory, output) = run(r#"run printf with message "only"
"#);
    assert_eq!(status(&output), 0, "{}", stderr(&output));
    assert_eq!(stdout(&output), "only\n");
}

#[test]
fn output_before_a_failing_command_is_delivered() {
    let (_directory, output) = run(r#"run printf with message "before"
run "false" with arguments []
"#);
    assert_eq!(status(&output), 1, "{}", stderr(&output));
    assert_eq!(stdout(&output), "before\n");
}

#[test]
fn block_output_without_consumer_is_not_duplicated() {
    let (_directory, output) = run(r#"if "a" is "a"
  run printf with message "in"
"#);
    assert_eq!(status(&output), 0, "{}", stderr(&output));
    assert_eq!(stdout(&output), "in\n");
}

#[test]
fn checkpoint_resume_does_not_replay_streamed_output() {
    let directory = tempfile::tempdir().unwrap();
    let checkpoint = directory.path().join("ckpt");
    let script = "run printf with message \"a\"\nrun printf with message \"b\"\n";
    let path = directory.path().join("workflow.program");
    fs::write(&path, script).unwrap();
    let first = Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(directory.path())
        .arg("run")
        .arg(&path)
        .arg("--manifest-dir")
        .arg(manifests())
        .arg("--checkpoint")
        .arg(&checkpoint)
        .output()
        .unwrap();
    assert_eq!(status(&first), 0, "{}", stderr(&first));
    assert_eq!(stdout(&first), "a\nb\n");

    let resumed = Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(directory.path())
        .arg("resume")
        .arg(&path)
        .arg("--manifest-dir")
        .arg(manifests())
        .arg("--checkpoint")
        .arg(&checkpoint)
        .output()
        .unwrap();
    assert_eq!(status(&resumed), 0, "{}", stderr(&resumed));
    assert_eq!(stdout(&resumed), "");
}
