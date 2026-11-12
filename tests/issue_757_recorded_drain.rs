#![cfg(unix)]
//! Issue #757: a recorded run must honor `--drain-timeout-ms` the way an ordinary run does,
//! and keep the ten-second default when the flag is omitted. Real SIGINT delivery; the
//! five-second sleep is cancelled by a 300 ms budget and finishes inside the default one.
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

fn manifests() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests")
}

const PROGRAM: &str = "on interrupt\n  run printf with message \"drained\"\n  then save to file \"drained.txt\"\nrun printf with message \"started\"\nthen save to file \"started.txt\"\nrun \"sleep\" with arguments [\"5\"]\nrun printf with message \"main\"\nthen save to file \"main.txt\"\n";

/// What an interrupted run did: its exit code, how long it took to exit after SIGINT, and
/// which of the program's own marker files it left behind.
#[derive(Debug, PartialEq)]
struct Outcome {
    code: Option<i32>,
    drained: bool,
    main: bool,
    elapsed: Duration,
}

/// Runs PROGRAM, interrupts it once its sleep has started, and reports the outcome. A
/// `recorded` run passes `--record`; `drain_timeout_ms` is passed only when it is `Some`.
fn interrupt_run(recorded: bool, drain_timeout_ms: Option<&str>) -> Outcome {
    let directory = tempfile::tempdir().unwrap();
    fs::write(directory.path().join("long.program"), PROGRAM).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_execute"));
    command
        .current_dir(directory.path())
        .arg("run")
        .arg("long.program")
        .arg("--manifest-dir")
        .arg(manifests());
    if recorded {
        command.arg("--record").arg(directory.path().join("record"));
    }
    if let Some(ms) = drain_timeout_ms {
        command.arg("--drain-timeout-ms").arg(ms);
    }
    let mut child = command
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let marker = directory.path().join("started.txt");
    let deadline = Instant::now() + Duration::from_secs(30);
    while !marker.exists() {
        assert!(Instant::now() < deadline, "the run never reached its sleep");
        thread::sleep(Duration::from_millis(10));
    }

    let interrupted = Instant::now();
    // SAFETY: the pid belongs to a child this test spawned and has not yet reaped.
    let result = unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGINT) };
    assert_eq!(result, 0);
    let status = child.wait().unwrap();
    Outcome {
        code: status.code(),
        drained: directory.path().join("drained.txt").exists(),
        main: directory.path().join("main.txt").exists(),
        elapsed: interrupted.elapsed(),
    }
}

#[test]
fn recorded_run_honors_a_short_drain_budget() {
    let recorded = interrupt_run(true, Some("300"));
    // The 300 ms budget cancels the five-second sleep; the default ten-second drain would not.
    assert!(
        recorded.elapsed < Duration::from_secs(4),
        "the recorded drain ignored its budget: {:?}",
        recorded.elapsed
    );
    assert_eq!(recorded.code, Some(130));
    // A budget expiry is a cancel: the handler body is skipped.
    assert!(!recorded.drained);
    assert!(!recorded.main);
}

#[test]
fn recorded_run_matches_ordinary_run_under_a_short_drain_budget() {
    let ordinary = interrupt_run(false, Some("300"));
    let recorded = interrupt_run(true, Some("300"));
    assert!(
        ordinary.elapsed < Duration::from_secs(4),
        "the ordinary drain ignored its budget: {:?}",
        ordinary.elapsed
    );
    assert!(
        recorded.elapsed < Duration::from_secs(4),
        "the recorded drain ignored its budget: {:?}",
        recorded.elapsed
    );
    assert_eq!(recorded.code, ordinary.code);
    assert_eq!(recorded.drained, ordinary.drained);
    assert_eq!(recorded.main, ordinary.main);
}

#[test]
fn recorded_run_without_the_flag_keeps_the_default_drain_budget() {
    let ordinary = interrupt_run(false, None);
    let recorded = interrupt_run(true, None);
    // The sleep finishes inside the default ten-second budget, so a recorded run without the
    // flag must not be cancelled early: it lasts at least the rest of the five-second sleep.
    assert!(
        recorded.elapsed >= Duration::from_secs(3),
        "the recorded run cancelled before the default budget: {:?}",
        recorded.elapsed
    );
    assert_eq!(recorded.code, Some(130));
    assert_eq!(recorded.code, ordinary.code);
    assert_eq!(recorded.drained, ordinary.drained);
    assert_eq!(recorded.main, ordinary.main);
}
