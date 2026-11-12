#![cfg(unix)]
//! Issue #757: a recorded run must honor `--drain-timeout-ms` the way an ordinary run does.
//! Real SIGINT delivery; the budget cancels a five-second sleep well before it ends.
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::Duration,
};

fn manifests() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests")
}

const PROGRAM: &str = "on interrupt\n  run printf with message \"drained\"\n  then save to file \"drained.txt\"\nrun printf with message \"started\"\nthen save to file \"started.txt\"\nrun \"sleep\" with arguments [\"5\"]\nrun printf with message \"main\"\nthen save to file \"main.txt\"\n";

#[test]
fn recorded_run_honors_a_short_drain_budget() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(directory.path().join("long.program"), PROGRAM).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(directory.path())
        .arg("run")
        .arg("long.program")
        .arg("--manifest-dir")
        .arg(manifests())
        .arg("--record")
        .arg(directory.path().join("record"))
        .arg("--drain-timeout-ms")
        .arg("300")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let marker = directory.path().join("started.txt");
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while !marker.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "the recorded run never reached its sleep"
        );
        thread::sleep(Duration::from_millis(10));
    }

    let interrupted = std::time::Instant::now();
    // SAFETY: the pid belongs to a child this test spawned and has not yet reaped.
    let result = unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGINT) };
    assert_eq!(result, 0);
    let status = child.wait().unwrap();
    // The 300 ms budget cancels the five-second sleep; the default ten-second drain would not.
    assert!(
        interrupted.elapsed() < Duration::from_secs(4),
        "the recorded drain ignored its budget: {:?}",
        interrupted.elapsed()
    );
    assert_eq!(status.code(), Some(130));
    // A budget expiry is a cancel: the handler body is skipped.
    assert!(!directory.path().join("drained.txt").exists());
    assert!(!directory.path().join("main.txt").exists());
}
