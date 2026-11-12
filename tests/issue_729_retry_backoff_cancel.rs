#![cfg(unix)]
//! Issue #729: a cancellation or the total deadline during a retry backoff. A cancel
//! ends the backoff at once and takes the normal failure path, so `try`/`finally` and
//! `on exit` still run and the statement is not retried. A deadline shorter than the
//! backoff ends the run with the deadline failure, not a retry.
use std::{
    fs,
    path::Path,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

/// A command that fails every time, logging each attempt to `attempts.txt`. The retry
/// waits a minute, so the run is always in backoff after the first attempt.
const FAILING: &str = "run \"sh\" with arguments [\"-c\", \"echo attempt >> attempts.txt; exit 1\"] and retry up to 3 times even if uncertain with backoff 60 seconds\n";

fn wait_for_first_attempt(directory: &Path) {
    let marker = directory.join("attempts.txt");
    let deadline = Instant::now() + Duration::from_secs(30);
    while !marker.exists() {
        assert!(
            Instant::now() < deadline,
            "the run never made its first attempt"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn interrupt(child: &std::process::Child) {
    // SAFETY: the pid belongs to a child this test spawned and has not yet reaped.
    let result = unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGINT) };
    assert_eq!(result, 0);
}

fn write_program(directory: &Path, source: &str) {
    fs::write(directory.join("job.program"), source).unwrap();
}

fn spawn_run(directory: &Path, extra: &[&str]) -> std::process::Child {
    Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(directory)
        .arg("run")
        .arg("job.program")
        .args(extra)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

fn attempt_count(directory: &Path) -> usize {
    fs::read_to_string(directory.join("attempts.txt"))
        .unwrap()
        .lines()
        .count()
}

#[test]
fn cancel_during_backoff_runs_finally_and_on_exit_without_retrying() {
    let directory = tempfile::tempdir().unwrap();
    // No `on interrupt` block, so the first interrupt is a cancellation.
    let source = format!(
        "on exit\n  run printf with message \"exit\"\n  then save to file \"exit.txt\"\ntry\n  {FAILING}finally\n  run printf with message \"finally\"\n  then save to file \"finally.txt\"\n"
    );
    write_program(directory.path(), &source);
    let mut child = spawn_run(directory.path(), &[]);
    wait_for_first_attempt(directory.path());

    let cancelled = Instant::now();
    interrupt(&child);
    let status = child.wait().unwrap();
    assert!(
        cancelled.elapsed() < Duration::from_secs(20),
        "the cancel waited out the backoff: {:?}",
        cancelled.elapsed()
    );
    assert_ne!(status.code(), Some(0), "a cancelled run must not succeed");
    assert!(
        directory.path().join("finally.txt").exists(),
        "the finally body must run when a backoff is cancelled"
    );
    assert!(
        directory.path().join("exit.txt").exists(),
        "the on exit body must run when a backoff is cancelled"
    );
    assert_eq!(
        attempt_count(directory.path()),
        1,
        "the cancelled run must not retry the statement"
    );
}

#[test]
fn deadline_during_backoff_ends_the_run_without_retrying() {
    let directory = tempfile::tempdir().unwrap();
    let source = format!("try\n  {FAILING}finally\n  run printf with message \"finally\"\n  then save to file \"finally.txt\"\n");
    write_program(directory.path(), &source);
    // The total deadline (3 seconds) is far shorter than the 60 second backoff.
    let started = Instant::now();
    let child = Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(directory.path())
        .arg("run")
        .arg("job.program")
        .args(["--total-timeout-ms", "3000"])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "the deadline waited out the backoff: {:?}",
        started.elapsed()
    );
    assert!(
        !output.status.success(),
        "a run past its deadline must fail"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("deadline"),
        "the run must end with the deadline outcome: {stderr}"
    );
    assert_eq!(
        attempt_count(directory.path()),
        1,
        "a run past its deadline must not retry the statement"
    );
}
