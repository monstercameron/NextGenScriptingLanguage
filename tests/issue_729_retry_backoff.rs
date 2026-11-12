#![cfg(unix)]
//! Issue #729: an interrupt during a retry backoff. A first interrupt drains: the
//! backoff ends at once, the `finally` and `on exit` bodies run, and the `on interrupt`
//! body runs. The backoff is long here, so a backoff that ignored the drain would hold
//! the run for the full minute and fail the elapsed-time bound.
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

fn run_program(directory: &Path, source: &str) -> std::process::Child {
    fs::write(directory.join("job.program"), source).unwrap();
    Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(directory)
        .arg("run")
        .arg("job.program")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

#[test]
fn first_interrupt_ends_a_long_backoff_and_runs_the_interrupt_handler() {
    let directory = tempfile::tempdir().unwrap();
    let source = format!(
        "on interrupt\n  run printf with message \"drained\"\n  then save to file \"drained.txt\"\n{FAILING}"
    );
    let mut child = run_program(directory.path(), &source);
    wait_for_first_attempt(directory.path());

    let interrupted = Instant::now();
    interrupt(&child);
    let status = child.wait().unwrap();
    assert!(
        interrupted.elapsed() < Duration::from_secs(20),
        "the drain waited out the backoff: {:?}",
        interrupted.elapsed()
    );
    assert_eq!(status.code(), Some(130));
    assert!(directory.path().join("drained.txt").exists());
    assert_eq!(
        fs::read_to_string(directory.path().join("attempts.txt"))
            .unwrap()
            .lines()
            .count(),
        1,
        "the drained run must not retry the statement"
    );
}

#[test]
fn first_interrupt_during_backoff_runs_finally_and_on_exit() {
    let directory = tempfile::tempdir().unwrap();
    let source = format!(
        "on exit\n  run printf with message \"exit\"\n  then save to file \"exit.txt\"\ntry\n  {FAILING}finally\n  run printf with message \"finally\"\n  then save to file \"finally.txt\"\n"
    );
    let mut child = run_program(directory.path(), &source);
    wait_for_first_attempt(directory.path());

    let interrupted = Instant::now();
    interrupt(&child);
    let _status = child.wait().unwrap();
    assert!(
        interrupted.elapsed() < Duration::from_secs(20),
        "the drain waited out the backoff: {:?}",
        interrupted.elapsed()
    );
    assert!(
        directory.path().join("finally.txt").exists(),
        "the finally body must run when a backoff is interrupted"
    );
    assert!(
        directory.path().join("exit.txt").exists(),
        "the on exit body must run when a backoff is interrupted"
    );
}
