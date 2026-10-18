//! Cleanup and handler regressions (lane RT3): a cleanup budget overrun keeps a
//! successful run's output, and `on exit` programs can own background jobs.
use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn run(directory: &Path, script: &str, command: &str) -> Output {
    let path = directory.join("case.program");
    fs::write(&path, script).unwrap();
    Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(directory)
        .arg(command)
        .arg(&path)
        .output()
        .unwrap()
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[test]
fn cleanup_budget_overrun_keeps_the_output_of_a_successful_run() {
    let directory = tempfile::tempdir().unwrap();
    let output = run(
        directory.path(),
        "use value \"main work ok\"\non exit\n  repeat 6000 times\n    continue\n",
        "run",
    );
    assert_eq!(output.status.code(), Some(1), "{}", text(&output.stderr));
    let stderr = text(&output.stderr);
    assert!(stderr.contains("cleanup step budget of 16384"), "{stderr}");
    assert!(stderr.contains("on exit"), "{stderr}");
    assert_eq!(text(&output.stdout), "main work ok");
}

#[test]
fn a_failed_run_still_reports_its_own_error_when_cleanup_overruns() {
    let directory = tempfile::tempdir().unwrap();
    let output = run(
        directory.path(),
        "use value \"partial\"\nfail with message \"boom\"\non exit\n  repeat 6000 times\n    continue\n",
        "run",
    );
    assert_ne!(output.status.code(), Some(0));
    assert!(
        text(&output.stderr).contains("cleanup step budget"),
        "{}",
        text(&output.stderr)
    );
}

#[test]
fn on_exit_programs_can_spawn_and_await_jobs() {
    let directory = tempfile::tempdir().unwrap();
    let script = "spawn as j\n  run printf with message \"hi\"\nawait j\nrun printf with message \"main\"\non exit\n  run printf with message \"cleanup\"\n";
    let checked = run(directory.path(), script, "check");
    assert!(checked.status.success(), "{}", text(&checked.stderr));
    let output = run(directory.path(), script, "run");
    assert!(output.status.success(), "{}", text(&output.stderr));
    assert_eq!(text(&output.stdout), "hi\nmain\ncleanup\n");
}

#[test]
fn job_operations_inside_an_on_exit_handler_are_refused_by_name() {
    let directory = tempfile::tempdir().unwrap();
    let output = run(
        directory.path(),
        "run printf with message \"main\"\non exit\n  spawn as j\n    run printf with message \"x\"\n  await j\n",
        "check",
    );
    assert!(!output.status.success());
    let stderr = text(&output.stderr);
    assert!(stderr.contains("on exit"), "{stderr}");
    assert!(!stderr.contains("try/catch/finally blocks"), "{stderr}");
}

#[test]
fn an_unawaited_job_in_an_on_exit_program_is_still_refused() {
    let directory = tempfile::tempdir().unwrap();
    let output = run(
        directory.path(),
        "spawn as j\n  run printf with message \"x\"\nrun printf with message \"main\"\non exit\n  run printf with message \"cleanup\"\n",
        "check",
    );
    assert!(!output.status.success());
    assert!(
        text(&output.stderr).contains("must be awaited or cancelled"),
        "{}",
        text(&output.stderr)
    );
}
