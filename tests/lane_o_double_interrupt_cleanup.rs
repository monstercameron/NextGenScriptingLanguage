//! Double interrupt during cleanup (#554): a repeat Ctrl-C while a `finally` or
//! `on exit` body runs aborts the cleanup at once and kills its running command
//! instead of waiting out the 5 s cleanup budget. Unix-only: real SIGINT
//! delivery. The Windows signal path shares the same flag
//! (`src/platform/windows.rs`) but still needs verification on Windows CI.
//! A single interrupt keeps running cleanup to completion.
#![cfg(unix)]

use std::fs;
use std::io::Read;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

/// `sh -c` body that records `marker` immediately before execing a 30-second
/// sleep, so a test can tell exactly when the sleep is the running command.
fn sleep_body() -> String {
    serde_json::to_string("touch \"$1\"; exec sleep 30").unwrap()
}

fn touch_body() -> String {
    serde_json::to_string("touch \"$1\"").unwrap()
}

fn on_exit_program(started: &Path, entered: &Path, finished: &Path) -> String {
    let sleep = sleep_body();
    let touch = touch_body();
    format!(
        "on exit\n  run \"sh\" with arguments [\"-c\", {sleep}, \"idle\", {:?}]\n  run \"sh\" with arguments [\"-c\", {touch}, \"idle\", {:?}]\nrun \"sh\" with arguments [\"-c\", {sleep}, \"idle\", {:?}]\n",
        entered.display().to_string(),
        finished.display().to_string(),
        started.display().to_string(),
    )
}

fn finally_program(started: &Path, entered: &Path, finished: &Path) -> String {
    let sleep = sleep_body();
    let touch = touch_body();
    format!(
        "try\n  run \"sh\" with arguments [\"-c\", {sleep}, \"idle\", {:?}]\nfinally\n  run \"sh\" with arguments [\"-c\", {sleep}, \"idle\", {:?}]\n  run \"sh\" with arguments [\"-c\", {touch}, \"idle\", {:?}]\n",
        started.display().to_string(),
        entered.display().to_string(),
        finished.display().to_string(),
    )
}

/// Wait until `path` exists. Markers are written immediately before the sleep
/// they gate, so no guessed delay decides when the run is interruptible; the
/// short settle only lets the `exec` after the marker take over.
fn wait_for_marker(path: &Path, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "the run never reached {what}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    std::thread::sleep(Duration::from_millis(100));
}

fn interrupt(child: &Child) {
    // SAFETY: the pid belongs to a child this test spawned and has not yet reaped.
    let result = unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGINT) };
    assert_eq!(result, 0);
}

fn wait_for_exit(child: &mut Child, limit: Duration) -> Option<ExitStatus> {
    let deadline = Instant::now() + limit;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return Some(status);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn stderr_of(child: &mut Child) -> String {
    let mut text = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut text)
        .unwrap();
    text
}

/// Drive the acceptance scenario: interrupt the main sleep so cleanup starts,
/// then interrupt again once the cleanup sleep is running. Returns the exit
/// status, the time from the second interrupt to exit, and stderr.
fn double_interrupt(
    directory: &Path,
    program: &str,
    started: &Path,
    entered: &Path,
) -> (ExitStatus, Duration, String) {
    fs::write(directory.join("cleanup.program"), program).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_execute"))
        .arg("run")
        .arg(directory.join("cleanup.program"))
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    wait_for_marker(started, "its main sleep");
    interrupt(&child);
    wait_for_marker(entered, "its cleanup sleep");
    let second = Instant::now();
    interrupt(&child);
    let Some(status) = wait_for_exit(&mut child, Duration::from_secs(10)) else {
        let _ = child.kill();
        let _ = child.wait();
        panic!("the second interrupt did not abort the cleanup sleep");
    };
    let elapsed = second.elapsed();
    let stderr = stderr_of(&mut child);
    (status, elapsed, stderr)
}

fn check_aborted_at_once(status: ExitStatus, elapsed: Duration, stderr: &str, finished: &Path) {
    // Status 130 like any interrupt; the point is it arrives at once instead of
    // after the 5 s cleanup budget or the 30 s cleanup sleep. Four seconds is
    // strictly under both, mirroring the 4 s bound for a 5 s budget in
    // `lane_b_drain_signals.rs`.
    assert_eq!(status.code(), Some(130), "stderr:\n{stderr}");
    assert!(
        elapsed < Duration::from_secs(4),
        "the run waited out its cleanup instead of aborting: {elapsed:?}\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("cleanup interrupted"),
        "the diagnostic must name the interruption, stderr:\n{stderr}"
    );
    assert!(
        !finished.exists(),
        "the cleanup ran past the second interrupt"
    );
}

#[test]
fn double_interrupt_during_on_exit_aborts_cleanup_at_once() {
    let directory = tempfile::tempdir().unwrap();
    let started = directory.path().join("started.txt");
    let entered = directory.path().join("entered-cleanup.txt");
    let finished = directory.path().join("finished.txt");
    let (status, elapsed, stderr) = double_interrupt(
        directory.path(),
        &on_exit_program(&started, &entered, &finished),
        &started,
        &entered,
    );
    check_aborted_at_once(status, elapsed, &stderr, &finished);
}

#[test]
fn double_interrupt_during_finally_aborts_cleanup_at_once() {
    let directory = tempfile::tempdir().unwrap();
    let started = directory.path().join("started.txt");
    let entered = directory.path().join("entered-cleanup.txt");
    let finished = directory.path().join("finished.txt");
    let (status, elapsed, stderr) = double_interrupt(
        directory.path(),
        &finally_program(&started, &entered, &finished),
        &started,
        &entered,
    );
    check_aborted_at_once(status, elapsed, &stderr, &finished);
}

#[test]
fn single_interrupt_still_runs_on_exit_cleanup() {
    // First-interrupt semantics: one Ctrl-C unwinds into cleanup and the body
    // runs to completion with no "cleanup interrupted" diagnostic.
    let directory = tempfile::tempdir().unwrap();
    let started = directory.path().join("started.txt");
    let cleaned = directory.path().join("cleaned.txt");
    let program = format!(
        "on exit\n  run \"sh\" with arguments [\"-c\", {}, \"idle\", {:?}]\nrun \"sh\" with arguments [\"-c\", {}, \"idle\", {:?}]\n",
        touch_body(),
        cleaned.display().to_string(),
        sleep_body(),
        started.display().to_string(),
    );
    fs::write(directory.path().join("cleanup.program"), program).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_execute"))
        .arg("run")
        .arg(directory.path().join("cleanup.program"))
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    wait_for_marker(&started, "its main sleep");
    interrupt(&child);
    let Some(status) = wait_for_exit(&mut child, Duration::from_secs(10)) else {
        let _ = child.kill();
        let _ = child.wait();
        panic!("a single interrupt never finished cleanup");
    };
    let stderr = stderr_of(&mut child);
    assert_eq!(status.code(), Some(130), "stderr:\n{stderr}");
    assert!(
        cleaned.exists(),
        "a single interrupt must still run cleanup, stderr:\n{stderr}"
    );
    assert!(
        !stderr.contains("cleanup interrupted"),
        "one interrupt is not a repeat interrupt, stderr:\n{stderr}"
    );
}
