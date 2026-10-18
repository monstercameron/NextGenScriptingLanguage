//! `exit with status` and `fail with message`: exit codes, stderr text, cleanup,
//! validation, explain/trace output and checkpoint terminal nodes.
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Instant;
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

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn exit_sets_status_and_keeps_pipeline_output() {
    let (_directory, output) = run(r#"run printf with message "out"
exit with status 2
"#);
    assert_eq!(status(&output), 2, "{}", stderr(&output));
    assert_eq!(stdout(&output), "out\n");
    assert!(stderr(&output).is_empty(), "{}", stderr(&output));
}

#[test]
fn exit_zero_after_output_prints_and_exits_zero() {
    let (_directory, output) = run(r#"run printf with message "done"
exit with status 0
"#);
    assert_eq!(status(&output), 0, "{}", stderr(&output));
    assert_eq!(stdout(&output), "done\n");
}

#[test]
fn fail_writes_exact_message_and_defaults_to_status_one() {
    let (_directory, output) = run(r#"fail with message "boom: disk full"
"#);
    assert_eq!(status(&output), 1);
    assert_eq!(stderr(&output), "boom: disk full\n");
    assert!(stdout(&output).is_empty());
}

#[test]
fn fail_with_explicit_status_and_output_delivered() {
    let (_directory, output) = run(r#"run printf with message "partial"
fail with message "bad input" and status 7
"#);
    assert_eq!(status(&output), 7);
    assert_eq!(stderr(&output), "bad input\n");
    assert_eq!(stdout(&output), "partial\n");
}

#[test]
fn finally_runs_before_exit_and_catch_does_not_catch_exit() {
    let (_directory, output) = run(r#"try
  run printf with message "body"
  exit with status 3
catch failure
  run printf with message "caught"
finally
  run printf with message "cleanup"
"#);
    assert_eq!(status(&output), 3, "{}", stderr(&output));
    assert_eq!(stdout(&output), "body\ncleanup\n");
}

#[test]
fn catch_does_not_intercept_fail() {
    let (_directory, output) = run(r#"try
  fail with message "inner" and status 4
catch failure
  run printf with message "caught"
"#);
    assert_eq!(status(&output), 4);
    assert_eq!(stderr(&output), "inner\n");
    assert!(stdout(&output).is_empty(), "{}", stdout(&output));
}

#[test]
fn exit_inside_reusable_program_ends_the_whole_run() {
    let (_directory, output) = run(r#"define a program named stop
  run printf with message "in stop"
  exit with status 5
  run printf with message "never"
run program stop
run printf with message "after"
"#);
    assert_eq!(status(&output), 5, "{}", stderr(&output));
    assert!(!stdout(&output).contains("after"), "{}", stdout(&output));
    assert!(!stdout(&output).contains("never"), "{}", stdout(&output));
}

#[test]
fn exit_inside_loop_stops_iteration() {
    let (_directory, output) = run(r#"repeat 5 times
  run printf with message "tick"
  exit with status 6
run printf with message "after"
"#);
    assert_eq!(status(&output), 6);
    assert_eq!(stdout(&output), "tick\n");
}

#[test]
fn fail_inside_if_body_is_taken_only_when_matched() {
    let (_directory, output) = run(r#"run printf with message "start"
if "a" is "b"
  fail with message "not taken" and status 9
run printf with message "end"
"#);
    assert_eq!(status(&output), 0, "{}", stderr(&output));
    assert_eq!(stdout(&output), "start\nend\n");

    let (_directory, output) = run(r#"if "a" is "a"
  fail with message "taken" and status 9
"#);
    assert_eq!(status(&output), 9);
    assert_eq!(stderr(&output), "taken\n");
}

#[test]
fn exit_status_can_come_from_an_integer_input() {
    let directory = tempfile::tempdir().unwrap();
    let script =
        "accept code as integer\nrun printf with message \"code\"\nexit with status code\n";
    let output = run_script(directory.path(), "run", script, &["--input", "code=42"]);
    assert_eq!(status(&output), 42, "{}", stderr(&output));
    assert_eq!(stdout(&output), "code\n");
}

#[test]
fn out_of_range_runtime_status_fails_without_exiting_with_it() {
    let directory = tempfile::tempdir().unwrap();
    let script = "accept code as integer\nexit with status code\n";
    let output = run_script(directory.path(), "run", script, &["--input", "code=300"]);
    assert_eq!(status(&output), 1);
    assert!(
        stderr(&output).contains("exit status must be between 0 and 255, received 300"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn out_of_range_literal_is_rejected_before_any_effect() {
    let directory = tempfile::tempdir().unwrap();
    let script = "run printf with message \"effect\\n\"\nexit with status 256\n";
    let output = run_script(directory.path(), "check", script, &[]);
    assert_ne!(status(&output), 0);
    assert!(
        stderr(&output).contains("exit status must be between 0 and 255, received 256"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn fail_status_zero_is_rejected() {
    let directory = tempfile::tempdir().unwrap();
    let output = run_script(
        directory.path(),
        "check",
        "fail with message \"x\" and status 0\n",
        &[],
    );
    assert_ne!(status(&output), 0);
    assert!(
        stderr(&output).contains("fail status must be between 1 and 255"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn non_integer_status_and_non_text_message_are_rejected() {
    let directory = tempfile::tempdir().unwrap();
    let output = run_script(directory.path(), "check", "exit with status \"2\"\n", &[]);
    assert_ne!(status(&output), 0);
    assert!(
        stderr(&output).contains("exit status must be an integer literal or integer binding"),
        "{}",
        stderr(&output)
    );

    let output = run_script(
        directory.path(),
        "check",
        "accept code as integer\nfail with message code\n",
        &[],
    );
    assert_ne!(status(&output), 0);
    assert!(
        stderr(&output).contains("fail message \"code\" must be text, received integer"),
        "{}",
        stderr(&output)
    );

    let output = run_script(
        directory.path(),
        "check",
        "accept name as text\nexit with status name\n",
        &[],
    );
    assert_ne!(status(&output), 0);
    assert!(
        stderr(&output).contains("exit status \"name\" must be integer, received text"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn service_handlers_reject_exit_and_fail() {
    let directory = tempfile::tempdir().unwrap();
    let script = "every 1 seconds\n  run printf with message \"tick\\n\"\n  exit with status 1\n";
    let output = run_script(directory.path(), "check", script, &[]);
    assert_ne!(status(&output), 0);
    assert!(
        stderr(&output).contains("service and listener handlers cannot use them"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn explain_shows_exit_and_fail_steps() {
    let directory = tempfile::tempdir().unwrap();
    let script = "exit with status 2\n";
    let output = run_script(directory.path(), "explain", script, &[]);
    assert_eq!(status(&output), 0, "{}", stderr(&output));
    assert!(
        stdout(&output).contains("exit with status 2"),
        "{}",
        stdout(&output)
    );

    let output = run_script(
        directory.path(),
        "explain",
        "fail with message \"bad\"\n",
        &[],
    );
    assert_eq!(status(&output), 0, "{}", stderr(&output));
    assert!(
        stdout(&output).contains("fail with message \"bad\" and status 1"),
        "{}",
        stdout(&output)
    );
}

#[test]
fn trace_emits_exit_event() {
    let directory = tempfile::tempdir().unwrap();
    let output = run_script(
        directory.path(),
        "run",
        "exit with status 2\n",
        &["--trace"],
    );
    assert_eq!(status(&output), 2);
    assert!(
        stderr(&output).contains("\"event\":\"exit\"") && stderr(&output).contains("\"status\":2"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn exit_cancels_and_reaps_owned_job_without_waiting_for_it() {
    // A job that would run for a minute must not delay the explicit exit.
    let sleeper = if cfg!(windows) {
        "run \"ping\" with arguments [\"-n\", \"60\", \"127.0.0.1\"]"
    } else {
        "run \"sleep\" with arguments [\"60\"]"
    };
    let script = format!(
        "spawn as sleeper\n  {sleeper}\nrun printf with message \"done\"\nexit with status 4\n"
    );
    let start = Instant::now();
    let (_directory, output) = run(&script);
    assert_eq!(status(&output), 4, "{}", stderr(&output));
    assert_eq!(stdout(&output), "done\n");
    assert!(
        start.elapsed().as_secs() < 30,
        "exit waited for the owned job: {:?}",
        start.elapsed()
    );
}

#[test]
fn checkpointed_exit_is_terminal_and_resume_keeps_status() {
    let directory = tempfile::tempdir().unwrap();
    let checkpoint = directory.path().join("ckpt");
    let script = "run printf with message \"one\"\nexit with status 3\n";
    let path = directory.path().join("workflow.program");
    fs::write(&path, script).unwrap();
    let mut first = Command::new(env!("CARGO_BIN_EXE_execute"));
    first
        .current_dir(directory.path())
        .arg("run")
        .arg(&path)
        .arg("--manifest-dir")
        .arg(manifests())
        .arg("--checkpoint")
        .arg(&checkpoint);
    let output = first.output().unwrap();
    assert_eq!(status(&output), 3, "{}", stderr(&output));
    assert_eq!(stdout(&output), "one\n");

    let mut resumed = Command::new(env!("CARGO_BIN_EXE_execute"));
    resumed
        .current_dir(directory.path())
        .arg("resume")
        .arg(&path)
        .arg("--manifest-dir")
        .arg(manifests())
        .arg("--checkpoint")
        .arg(&checkpoint);
    let output = resumed.output().unwrap();
    assert_eq!(status(&output), 3, "{}", stderr(&output));
    assert!(
        stdout(&output).is_empty(),
        "committed output must not replay"
    );
}

#[test]
fn checkpointed_programs_reject_nested_exit() {
    let directory = tempfile::tempdir().unwrap();
    let checkpoint = directory.path().join("ckpt");
    let script = "if \"a\" is \"a\"\n  exit with status 3\n";
    let path = directory.path().join("workflow.program");
    fs::write(&path, script).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(directory.path())
        .arg("run")
        .arg(&path)
        .arg("--manifest-dir")
        .arg(manifests())
        .arg("--checkpoint")
        .arg(&checkpoint)
        .output()
        .unwrap();
    assert_eq!(status(&output), 1);
    assert!(
        stderr(&output).contains("can exit or fail only at top level"),
        "{}",
        stderr(&output)
    );
}
