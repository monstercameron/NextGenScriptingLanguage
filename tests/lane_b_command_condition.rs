//! `if run <command> succeeds|fails` and `while run <command> ...`: branch on a
//! command's declared outcome without changing the pipeline value.
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
fn failing_command_takes_otherwise_and_succeeding_command_takes_body() {
    let (_directory, output) = run(r#"if run false succeeds
  run printf with message "yes"
otherwise
  run printf with message "no"
"#);
    assert_eq!(status(&output), 0, "{}", stderr(&output));
    assert_eq!(stdout(&output), "no\n");

    let (_directory, output) = run(r#"if run printf with message "x" succeeds
  run printf with message "yes"
otherwise
  run printf with message "no"
"#);
    assert_eq!(status(&output), 0, "{}", stderr(&output));
    assert_eq!(stdout(&output), "yes\n");
}

#[test]
fn fails_form_takes_body_when_command_fails() {
    let (_directory, output) = run(r#"if run false fails
  run printf with message "failed"
"#);
    assert_eq!(status(&output), 0, "{}", stderr(&output));
    assert_eq!(stdout(&output), "failed\n");
}

#[test]
fn command_condition_keeps_the_pipeline_value_unchanged() {
    let (_directory, output) = run(r#"run printf with message "value"
if run false succeeds
  run printf with message "bad"
"#);
    assert_eq!(status(&output), 0, "{}", stderr(&output));
    assert_eq!(stdout(&output), "value\n");
}

#[test]
fn missing_executable_is_an_error_not_false() {
    let (_directory, output) = run(r#"if run "no-such-command-for-lane-b" succeeds
  run printf with message "bad"
otherwise
  run printf with message "not-taken"
"#);
    assert_ne!(status(&output), 0);
    assert!(stderr(&output).contains("not found"), "{}", stderr(&output));
    assert!(
        !stdout(&output).contains("not-taken"),
        "{}",
        stdout(&output)
    );
}

#[test]
fn while_command_with_failing_command_runs_no_iterations() {
    let (_directory, output) = run(r#"while run false succeeds
  run printf with message "never"
run printf with message "after"
"#);
    assert_eq!(status(&output), 0, "{}", stderr(&output));
    assert_eq!(stdout(&output), "after\n");
}

#[test]
fn command_condition_rejects_builtins_bindings_and_bad_shapes() {
    let directory = tempfile::tempdir().unwrap();
    let output = run_script(
        directory.path(),
        "check",
        "if run cd with path \".\" succeeds\n  run printf with message \"x\"\n",
        &[],
    );
    assert_ne!(status(&output), 0);
    assert!(
        stderr(&output).contains("cannot run the builtin"),
        "{}",
        stderr(&output)
    );

    let output = run_script(
        directory.path(),
        "check",
        "if run printf with message \"x\" as result succeeds\n  run printf with message \"y\"\n",
        &[],
    );
    assert_ne!(status(&output), 0);
    assert!(
        stderr(&output).contains("cannot bind"),
        "{}",
        stderr(&output)
    );

    let output = run_script(
        directory.path(),
        "check",
        "if run succeeds\n  run printf with message \"y\"\n",
        &[],
    );
    assert_ne!(status(&output), 0);
    assert!(
        stderr(&output).contains("run <command> succeeds"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn explain_and_trace_show_command_conditions() {
    let directory = tempfile::tempdir().unwrap();
    let script = "if run false succeeds\n  run printf with message \"x\"\n";
    let explain = run_script(directory.path(), "explain", script, &[]);
    assert_eq!(status(&explain), 0, "{}", stderr(&explain));
    assert!(
        stdout(&explain).contains("if run \"false\" succeeds"),
        "{}",
        stdout(&explain)
    );

    let traced = run_script(directory.path(), "run", script, &["--trace"]);
    assert!(
        stderr(&traced).contains("\"event\":\"command_condition\"")
            && stderr(&traced).contains("\"matched\":false"),
        "{}",
        stderr(&traced)
    );
}

#[test]
fn command_condition_works_inside_reusable_programs() {
    let (_directory, output) = run(r#"define a program named choose
  if run false succeeds
    run printf with message "yes"
  otherwise
    run printf with message "no"
run program choose
"#);
    assert_eq!(status(&output), 0, "{}", stderr(&output));
    assert_eq!(stdout(&output), "no\n");
}

#[test]
fn command_condition_works_in_checkpointed_programs() {
    let directory = tempfile::tempdir().unwrap();
    let checkpoint = directory.path().join("ckpt");
    let script = "if run false succeeds\n  run printf with message \"bad\"\notherwise\n  run printf with message \"checkpointed\"\n";
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
    assert_eq!(stdout(&first), "checkpointed\n");

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
    assert!(
        stdout(&resumed).is_empty(),
        "committed branch output must not replay"
    );
}

#[test]
fn while_command_enters_body_when_command_succeeds() {
    let (_directory, output) = run(r#"while run printf with message "probe" succeeds
  exit with status 6
run printf with message "after"
"#);
    assert_eq!(status(&output), 6, "{}", stderr(&output));
    assert!(stdout(&output).is_empty(), "{}", stdout(&output));
}
