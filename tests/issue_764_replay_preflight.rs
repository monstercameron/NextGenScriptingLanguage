#![cfg(feature = "records")]
//! Issue #764: a replay answers recorded native calls from the record, so it must
//! not preflight their executables. An ordinary run still does.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn execute(directory: &Path, arguments: &[&str], path: Option<&str>) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_execute"));
    command.current_dir(directory).args(arguments);
    if let Some(path) = path {
        command.env("PATH", path);
    }
    command.output().unwrap()
}

/// Copies the repository's manifests into `work`. The repository folder can carry
/// permissive Windows ACLs that the manifest trust gate refuses, so tests load a copy.
fn copy_manifests(work: &Path) -> PathBuf {
    let destination = work.join("manifests");
    fs::create_dir(&destination).unwrap();
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests");
    for entry in fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_file() {
            fs::copy(entry.path(), destination.join(entry.file_name())).unwrap();
        }
    }
    destination
}

fn text(output: &[u8]) -> String {
    String::from_utf8_lossy(output).into_owned()
}

const PRINTF_PROGRAM: &str = "run printf with message \"hello\"\n";

#[test]
fn replay_with_empty_path_reproduces_the_recorded_result_without_launches() {
    let work = tempfile::tempdir().unwrap();
    fs::write(work.path().join("workflow.program"), PRINTF_PROGRAM).unwrap();
    let record = work.path().join("record");
    let manifests = copy_manifests(work.path());
    let recorded = execute(
        work.path(),
        &[
            "run",
            "workflow.program",
            "--record",
            record.to_str().unwrap(),
            "--manifest-dir",
            manifests.to_str().unwrap(),
        ],
        None,
    );
    assert_eq!(
        recorded.status.code(),
        Some(0),
        "{}",
        text(&recorded.stderr)
    );
    let original = text(&recorded.stdout);

    let replayed = execute(
        work.path(),
        &[
            "replay",
            record.to_str().unwrap(),
            "--manifest-dir",
            manifests.to_str().unwrap(),
        ],
        Some(""),
    );
    assert_eq!(
        replayed.status.code(),
        Some(0),
        "{}",
        text(&replayed.stderr)
    );
    assert_eq!(text(&replayed.stdout), original);
    assert!(
        text(&replayed.stderr).contains("0 processes launched"),
        "{}",
        text(&replayed.stderr)
    );
}

#[test]
fn ordinary_run_with_empty_path_still_preflights_the_executable() {
    let work = tempfile::tempdir().unwrap();
    fs::write(work.path().join("workflow.program"), PRINTF_PROGRAM).unwrap();
    let manifests = copy_manifests(work.path());
    let run = execute(
        work.path(),
        &[
            "run",
            "workflow.program",
            "--manifest-dir",
            manifests.to_str().unwrap(),
        ],
        Some(""),
    );
    assert_ne!(run.status.code(), Some(0), "{}", text(&run.stdout));
    assert_eq!(text(&run.stdout), "", "a refused run must not start");
}
