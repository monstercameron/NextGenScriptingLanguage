#![cfg(all(unix, feature = "records"))]
//! Issue #761: a recorded failure replays with the metadata the caught failure carried:
//! the signal that killed a step, and the failing stage of a pipeline with its status.
//! Each program catches its failure, so the caught value is printed; the replay must
//! print the same bytes without launching the commands again.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn execute(directory: &Path, arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(directory)
        .args(arguments)
        .output()
        .unwrap()
}

fn manifests() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests")
}

fn text(output: &[u8]) -> String {
    String::from_utf8_lossy(output).into_owned()
}

/// Record `source` as a finite run and replay the record, returning both outputs.
fn record_and_replay(source: &str) -> (Output, Output) {
    let work = tempfile::tempdir().unwrap();
    fs::write(work.path().join("caught.program"), source).unwrap();
    let record = work.path().join("record");
    let original = execute(
        work.path(),
        &[
            "run",
            "caught.program",
            "--record",
            record.to_str().unwrap(),
            "--manifest-dir",
            manifests().to_str().unwrap(),
        ],
    );
    assert_eq!(
        original.status.code(),
        Some(0),
        "{}",
        text(&original.stderr)
    );
    let replayed = execute(
        work.path(),
        &[
            "replay",
            record.to_str().unwrap(),
            "--manifest-dir",
            manifests().to_str().unwrap(),
        ],
    );
    (original, replayed)
}

#[test]
fn a_step_killed_by_a_signal_replays_the_same_caught_failure() {
    let source = "try\n  run \"sh\" with arguments [\"-c\", \"kill -TERM $$\"]\ncatch problem\n  then encode problem as json\n";
    let (original, replayed) = record_and_replay(source);
    let caught = text(&original.stdout);
    assert!(
        caught.contains("15"),
        "the caught failure should name SIGTERM: {caught}"
    );
    assert_eq!(
        replayed.status.code(),
        Some(0),
        "{}",
        text(&replayed.stderr)
    );
    assert_eq!(text(&replayed.stdout), caught);
}

#[test]
fn a_pipeline_whose_producer_exits_7_replays_the_same_failing_stage() {
    let source = "try\n  run \"sh\" with arguments [\"-c\", \"exit 7\"]\n  then \"sh\" with arguments [\"-c\", \"cat >/dev/null\"]\ncatch problem\n  then encode problem as json\n";
    let (original, replayed) = record_and_replay(source);
    let caught = text(&original.stdout);
    assert!(
        caught.contains("7"),
        "the caught failure should carry status 7: {caught}"
    );
    assert_eq!(
        replayed.status.code(),
        Some(0),
        "{}",
        text(&replayed.stderr)
    );
    assert_eq!(text(&replayed.stdout), caught);
}

#[test]
fn an_ordinary_nonzero_exit_still_replays_the_same_caught_failure() {
    let source = "try\n  run \"sh\" with arguments [\"-c\", \"exit 3\"]\ncatch problem\n  then encode problem as json\n";
    let (original, replayed) = record_and_replay(source);
    let caught = text(&original.stdout);
    assert!(
        caught.contains("3"),
        "the caught failure should carry status 3: {caught}"
    );
    assert_eq!(
        replayed.status.code(),
        Some(0),
        "{}",
        text(&replayed.stderr)
    );
    assert_eq!(text(&replayed.stdout), caught);
}
