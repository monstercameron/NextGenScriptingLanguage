//! Interrupted runs clean up their children, then exit with the shell status
//! for the signal: 130 SIGINT, 143 SIGTERM, 129 SIGHUP. Other errors stay 1.
#![cfg(unix)]

use std::fs;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};
use tempfile::tempdir;

/// Start a run that writes a marker file just before its 30-second sleep, and return once the
/// marker exists. Signals sent after this point hit a run that is inside its sleep.
fn spawn_sleeping_run() -> (tempfile::TempDir, Child) {
    let directory = tempdir().unwrap();
    let program = directory.path().join("sleep.program");
    let marker = directory.path().join("started.txt");
    fs::write(
        &program,
        format!(
            "run printf with message \"started\"\nthen save to file {:?}\nrun \"sleep\" with arguments [\"30\"]\n",
            marker.display().to_string()
        ),
    )
    .unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_execute"))
        .arg("run")
        .arg(&program)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while !marker.exists() {
        assert!(Instant::now() < deadline, "the run never reached its sleep");
        std::thread::sleep(Duration::from_millis(10));
    }
    (directory, child)
}

fn wait_for(child: &mut Child, limit: Duration) -> Option<ExitStatus> {
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

fn interrupt_with(signal: libc::c_int) -> Option<i32> {
    let (_directory, mut child) = spawn_sleeping_run();
    // SAFETY: the pid belongs to the child spawned above and is not reaped yet.
    unsafe { libc::kill(child.id() as libc::pid_t, signal) };
    let status = wait_for(&mut child, Duration::from_secs(20))
        .expect("interrupted run should exit after cleanup");
    status.code()
}

#[test]
fn sigint_exits_130() {
    assert_eq!(interrupt_with(libc::SIGINT), Some(130));
}

#[test]
fn sigterm_exits_143() {
    assert_eq!(interrupt_with(libc::SIGTERM), Some(143));
}

#[test]
fn sighup_exits_129() {
    assert_eq!(interrupt_with(libc::SIGHUP), Some(129));
}

#[test]
fn other_errors_still_exit_1() {
    let directory = tempdir().unwrap();
    let status = Command::new(env!("CARGO_BIN_EXE_execute"))
        .arg("run")
        .arg(directory.path().join("missing.program"))
        .output()
        .unwrap()
        .status;
    assert_eq!(status.code(), Some(1));
}
