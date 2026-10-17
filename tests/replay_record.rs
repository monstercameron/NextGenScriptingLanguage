//! `execute replay RECORD`: a recorded run replays with its recorded outputs, no
//! process launched and no file written. An edited program reports the first
//! divergence, `--until` stops early, and `--as-test` writes a test fixture.

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

/// Record `source` in `work`, returning the record directory.
fn record(work: &Path, source: &str) -> PathBuf {
    fs::write(work.join("workflow.program"), source).unwrap();
    let record = work.join("record");
    let run = execute(
        work,
        &[
            "run",
            "workflow.program",
            "--record",
            record.to_str().unwrap(),
            "--manifest-dir",
            manifests().to_str().unwrap(),
        ],
    );
    assert_eq!(run.status.code(), Some(0), "{}", text(&run.stderr));
    record
}

fn replay(work: &Path, record: &Path, extra: &[&str]) -> Output {
    let mut arguments = vec!["replay", record.to_str().unwrap(), "--manifest-dir"];
    let manifests = manifests();
    arguments.push(manifests.to_str().unwrap());
    arguments.extend_from_slice(extra);
    execute(work, &arguments)
}

const TWO_STEPS: &str = "run printf with message \"one\"\n\
    then tr with from \"[:lower:]\" and to \"[:upper:]\"\n\
    then save to \"upper.txt\"\n";

#[test]
fn replay_reproduces_output_launches_nothing_and_writes_no_files() {
    let work = tempfile::tempdir().unwrap();
    let record = record(work.path(), TWO_STEPS);
    let original = fs::read(work.path().join("upper.txt")).unwrap();
    assert_eq!(original, b"ONE\n");
    fs::remove_file(work.path().join("upper.txt")).unwrap();

    let replayed = replay(work.path(), &record, &[]);
    assert_eq!(
        replayed.status.code(),
        Some(0),
        "{}",
        text(&replayed.stderr)
    );
    assert_eq!(text(&replayed.stdout), "ONE\n");
    assert!(
        text(&replayed.stderr).contains("2 steps replayed, no divergence"),
        "{}",
        text(&replayed.stderr)
    );
    assert!(
        text(&replayed.stderr).contains("0 processes launched"),
        "{}",
        text(&replayed.stderr)
    );
    assert!(
        !work.path().join("upper.txt").exists(),
        "a replay must not save its output"
    );
}

#[test]
fn an_edited_program_reports_the_first_divergence() {
    let work = tempfile::tempdir().unwrap();
    let record = record(work.path(), TWO_STEPS);
    fs::write(
        work.path().join("edited.program"),
        TWO_STEPS.replace("\"one\"", "\"uno\""),
    )
    .unwrap();
    let replayed = replay(
        work.path(),
        &record,
        &[
            "--program",
            work.path().join("edited.program").to_str().unwrap(),
        ],
    );
    assert_eq!(
        replayed.status.code(),
        Some(1),
        "{}",
        text(&replayed.stdout)
    );
    let stderr = text(&replayed.stderr);
    assert!(
        stderr.contains("replay diverged at step 0 (line 1)"),
        "{stderr}"
    );
    assert!(stderr.contains("uno"), "{stderr}");
}

#[test]
fn until_stops_before_the_first_line_past_the_limit() {
    let work = tempfile::tempdir().unwrap();
    let record = record(work.path(), TWO_STEPS);
    let replayed = replay(work.path(), &record, &["--until", "1"]);
    assert_eq!(
        replayed.status.code(),
        Some(0),
        "{}",
        text(&replayed.stderr)
    );
    let stderr = text(&replayed.stderr);
    assert!(stderr.contains("stopped before line 2"), "{stderr}");
    assert!(stderr.contains("no divergence"), "{stderr}");
}

#[test]
fn a_recorded_failure_replays_as_the_same_failure() {
    let work = tempfile::tempdir().unwrap();
    let executable = env!("CARGO_BIN_EXE_execute").replace('\\', "/");
    let source = format!(
        "run printf with message \"before\"\nrun \"{executable}\" with arguments [\"--no-such-option\"]\n"
    );
    fs::write(work.path().join("failing.program"), &source).unwrap();
    let record = work.path().join("record");
    let original = execute(
        work.path(),
        &[
            "run",
            "failing.program",
            "--record",
            record.to_str().unwrap(),
            "--manifest-dir",
            manifests().to_str().unwrap(),
        ],
    );
    assert_eq!(original.status.code(), Some(1));

    let replayed = replay(work.path(), &record, &[]);
    assert_eq!(
        replayed.status.code(),
        Some(1),
        "{}",
        text(&replayed.stderr)
    );
    let stderr = text(&replayed.stderr);
    assert!(!stderr.contains("diverged"), "{stderr}");
    assert_eq!(text(&replayed.stdout), text(&original.stdout));
}

#[test]
fn as_test_writes_a_fixture_that_execute_test_accepts() {
    let work = tempfile::tempdir().unwrap();
    let record = record(work.path(), "run printf with message \"hello\"\n");
    let fixture = work.path().join("fixture");
    let replayed = replay(
        work.path(),
        &record,
        &["--as-test", fixture.to_str().unwrap()],
    );
    assert_eq!(
        replayed.status.code(),
        Some(0),
        "{}",
        text(&replayed.stderr)
    );
    assert!(fixture.join("fixture.test.program").is_file());
    let sidecar: serde_json::Value =
        serde_json::from_slice(&fs::read(fixture.join("fixture.test.json")).unwrap()).unwrap();
    assert_eq!(sidecar["expect"]["stdout"], "hello\n");
    assert_eq!(sidecar["expect"]["status"], 0);

    let test = execute(
        work.path(),
        &[
            "test",
            fixture.to_str().unwrap(),
            "--manifest-dir",
            manifests().to_str().unwrap(),
        ],
    );
    assert_eq!(test.status.code(), Some(0), "{}", text(&test.stdout));
    assert!(
        text(&test.stdout).contains("ok 1"),
        "{}",
        text(&test.stdout)
    );
}
