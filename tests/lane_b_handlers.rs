//! Program-level `on exit` and `on interrupt` blocks: exit-path cleanup, parse rules,
//! and rejection on runners that cannot drain.
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

fn run(script: &str) -> (TempDir, Output) {
    let directory = tempfile::tempdir().unwrap();
    let output = run_script(directory.path(), "run", script, &[]);
    (directory, output)
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn on_exit_runs_after_normal_completion() {
    let (_directory, output) =
        run("on exit\n  run printf with message \"exit\"\nrun printf with message \"main\"\n");
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(stdout(&output), "exit\n");
}

#[test]
fn on_exit_runs_after_exit_and_keeps_the_status() {
    let (_directory, output) = run(
        "on exit\n  run printf with message \"exit\"\nrun printf with message \"main\"\nexit with status 3\n",
    );
    assert_eq!(output.status.code(), Some(3), "{}", stderr(&output));
    assert_eq!(stdout(&output), "exit\n");
}

#[test]
fn on_exit_runs_after_fail_and_keeps_the_message() {
    let (_directory, output) = run(
        "on exit\n  run printf with message \"exit\"\nrun printf with message \"main\"\nfail with message \"boom\"\n",
    );
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(stderr(&output).contains("boom"), "{}", stderr(&output));
    assert_eq!(stdout(&output), "exit\n");
}

#[test]
fn interrupt_handler_does_not_run_without_an_interrupt() {
    let (_directory, output) = run(
        "on interrupt\n  run printf with message \"drained\"\nrun printf with message \"main\"\n",
    );
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(stdout(&output), "main\n");
}

#[test]
fn handler_needs_a_statement_to_wrap() {
    let directory = tempfile::tempdir().unwrap();
    let output = run_script(
        directory.path(),
        "check",
        "on exit\n  run printf with message \"only\"\n",
        &[],
    );
    assert_ne!(output.status.code(), Some(0));
    assert!(
        stderr(&output).contains("need at least one statement to wrap"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn duplicate_and_triggered_handlers_are_rejected() {
    let directory = tempfile::tempdir().unwrap();
    let duplicate = run_script(
        directory.path(),
        "check",
        "on exit\n  run printf with message \"x\"\non exit\n  run printf with message \"y\"\nrun printf with message \"m\"\n",
        &[],
    );
    assert!(
        stderr(&duplicate).contains("duplicate on exit block"),
        "{}",
        stderr(&duplicate)
    );

    let triggered = run_script(
        directory.path(),
        "check",
        "every 10 milliseconds\n  run printf with message \"t\"\non exit\n  run printf with message \"x\"\n",
        &[],
    );
    assert!(
        stderr(&triggered).contains("not triggers"),
        "{}",
        stderr(&triggered)
    );
}

#[test]
fn on_exit_runs_with_checkpoints_and_on_interrupt_is_refused() {
    // A resume rebuilds the `on exit` frame, so checkpointed runs accept it. An
    // interrupt handler still needs a drain, which checkpointed runs do not provide.
    let directory = tempfile::tempdir().unwrap();
    let checkpoint = directory.path().join("ckpt");
    let exit = run_script(
        directory.path(),
        "run",
        "on exit\n  run printf with message \"exit\"\nrun printf with message \"main\"\n",
        &["--checkpoint", checkpoint.to_str().unwrap()],
    );
    assert_eq!(exit.status.code(), Some(0), "{}", stderr(&exit));
    assert_eq!(stdout(&exit), "exit\n");

    let interrupt = run_script(
        directory.path(),
        "run",
        "on interrupt\n  run printf with message \"drained\"\nrun printf with message \"main\"\n",
        &[
            "--checkpoint",
            directory.path().join("ckpt2").to_str().unwrap(),
        ],
    );
    assert_ne!(interrupt.status.code(), Some(0));
    assert!(
        stderr(&interrupt).contains("not supported with checkpointed runs"),
        "{}",
        stderr(&interrupt)
    );
    assert_eq!(
        stdout(&interrupt),
        "",
        "a refused run must not run the program"
    );
}
