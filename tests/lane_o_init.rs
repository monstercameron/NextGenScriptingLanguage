//! `run --init` supervises the workflow as PID 1 would: it returns the worker's
//! status and forwards termination signals to it.
#![cfg(unix)]

use std::fs;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};
use tempfile::tempdir;

fn write_program(text: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let directory = tempdir().unwrap();
    let program = directory.path().join("init.program");
    fs::write(&program, text).unwrap();
    (directory, program)
}

fn spawn_init(program: &std::path::Path) -> Child {
    Command::new(env!("CARGO_BIN_EXE_execute"))
        .args(["run", "--init"])
        .arg(program)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
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

#[test]
fn init_returns_the_worker_exit_status() {
    let (_directory, program) = write_program("exit with status 5\n");
    let status = Command::new(env!("CARGO_BIN_EXE_execute"))
        .args(["run", "--init"])
        .arg(&program)
        .output()
        .unwrap()
        .status;
    assert_eq!(status.code(), Some(5));
}

#[test]
fn init_forwards_sigterm_to_the_worker_and_exits() {
    let (_directory, program) = write_program("run \"sleep\" with arguments [\"30\"]\n");
    let mut child = spawn_init(&program);
    std::thread::sleep(Duration::from_millis(500));
    // SAFETY: the pid belongs to the child spawned above and is not reaped yet.
    unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
    let status = wait_for(&mut child, Duration::from_secs(20))
        .expect("supervisor should exit after SIGTERM");
    assert!(!status.success());
}

#[test]
fn init_reports_worker_death_by_sighup_as_129() {
    let (_directory, program) = write_program("run \"sleep\" with arguments [\"30\"]\n");
    let mut child = spawn_init(&program);
    std::thread::sleep(Duration::from_millis(500));
    // SAFETY: as above.
    unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGHUP) };
    let status =
        wait_for(&mut child, Duration::from_secs(20)).expect("supervisor should exit after SIGHUP");
    // The worker has no SIGHUP handler, so it dies by the signal: 128 + 1.
    assert_eq!(status.code(), Some(129));
}

#[test]
fn init_applies_only_to_run() {
    let (_directory, program) = write_program("exit with status 0\n");
    let output = Command::new(env!("CARGO_BIN_EXE_execute"))
        .args(["check", "--init"])
        .arg(&program)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("--init applies only to `run`"));
}
