#![cfg(unix)]
//! Real SIGINT delivery to a running program with an `on interrupt` body. The first
//! interrupt drains (running work finishes, then the handler runs); a second cancels.
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::Duration,
};

fn manifests() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests")
}

const PROGRAM: &str = "on interrupt\n  run printf with message \"drained\"\n  then save to file \"drained.txt\"\nrun sleep with arguments [\"1\"]\nrun printf with message \"main\"\nthen save to file \"main.txt\"\n";

fn spawn(directory: &Path) -> Child {
    fs::write(directory.join("drain.program"), PROGRAM).unwrap();
    Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(directory)
        .arg("run")
        .arg("drain.program")
        .arg("--manifest-dir")
        .arg(manifests())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

fn interrupt(child: &Child) {
    // SAFETY: the pid belongs to a child this test spawned and has not yet reaped.
    let result = unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGINT) };
    assert_eq!(result, 0);
}

#[test]
fn first_interrupt_drains_then_runs_the_handler() {
    let directory = tempfile::tempdir().unwrap();
    let mut child = spawn(directory.path());
    // Give the run time to start the one-second sleep before interrupting it.
    thread::sleep(Duration::from_millis(300));
    interrupt(&child);
    let status = child.wait().unwrap();
    assert_eq!(status.code(), Some(130));
    // The in-flight sleep finished; the statement after it never started.
    assert!(directory.path().join("drained.txt").exists());
    assert!(!directory.path().join("main.txt").exists());
}

#[test]
fn second_interrupt_cancels_without_running_the_handler() {
    let directory = tempfile::tempdir().unwrap();
    let mut child = spawn(directory.path());
    thread::sleep(Duration::from_millis(300));
    interrupt(&child);
    thread::sleep(Duration::from_millis(100));
    interrupt(&child);
    let status = child.wait().unwrap();
    assert_eq!(status.code(), Some(130));
    assert!(!directory.path().join("drained.txt").exists());
    assert!(!directory.path().join("main.txt").exists());
}
