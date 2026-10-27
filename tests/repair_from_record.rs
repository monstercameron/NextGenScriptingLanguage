#![cfg(feature = "records")]
//! `execute repair --from-record` and `execute resume --from-record`: a failed
//! recorded run is repaired in its failed suffix and resumed without re-running
//! completed native steps (counted with a fake tool). Prefix edits, completed
//! records and missing blobs are refused with a per-step reason.

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

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// The executable path as program text. Forward slashes need no escaping.
fn executable_text() -> String {
    env!("CARGO_BIN_EXE_execute").replace('\\', "/")
}

/// Line 1 completes, line 2 fails, line 3 is the unexecuted suffix.
fn failing_program() -> String {
    format!(
        "run printf with message \"before\"\nrun \"{}\" with arguments [\"--no-such-option\"]\nrun printf with message \"three\"\n",
        executable_text()
    )
}

/// Run `source` with `--record` until it fails, returning the record directory.
fn failed_record(work: &Path, source: &str) -> PathBuf {
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
    assert!(!run.status.success(), "the program should fail");
    assert!(record.join("run.json").is_file(), "{}", text(&run.stderr));
    record
}

/// Repair the failed suffix of `record` in `program`.
fn repair_from_record(work: &Path, record: &Path, program: &str, op: &str) -> Output {
    execute(
        work,
        &[
            "repair",
            "--from-record",
            record.to_str().unwrap(),
            "--program",
            program,
            "--op",
            op,
            "--manifest-dir",
            manifests().to_str().unwrap(),
        ],
    )
}

#[cfg(unix)]
fn counter_manifests(work: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let custom = work.join("custom-manifests");
    fs::create_dir(&custom).unwrap();
    fs::copy(manifests().join("printf.json"), custom.join("printf.json")).unwrap();
    // The fake tool records every launch by appending to the count file, so a
    // resumed prefix that re-ran it would leave more than one line behind.
    let count = work.join("count.txt").display().to_string();
    let applet = work.join("counter.sh");
    fs::write(
        &applet,
        format!("#!/bin/sh\necho invoke >> '{count}'\nprintf 'counted\\n'\n"),
    )
    .unwrap();
    fs::set_permissions(&applet, fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(
        custom.join("counter.json"),
        serde_json::json!({
            "name": "counter",
            "binary": applet,
            "arguments": []
        })
        .to_string(),
    )
    .unwrap();
    custom
}

#[cfg(unix)]
fn count_lines(work: &Path) -> usize {
    fs::read_to_string(work.join("count.txt"))
        .map(|text| text.lines().count())
        .unwrap_or(0)
}

/// A failed recorded run is repaired with one operation and resumes without
/// re-running the completed native step, which the counting fake tool proves.
#[cfg(unix)]
#[test]
fn recorded_failure_repairs_and_resumes_without_repeating_completed_effects() {
    let work = tempfile::tempdir().unwrap();
    let custom = counter_manifests(work.path());
    let source = format!(
        "run counter\nrun \"{}\" with arguments [\"--no-such-option\"]\nrun printf with message \"three\"\n",
        executable_text()
    );
    fs::write(work.path().join("workflow.program"), &source).unwrap();
    let record = work.path().join("record");
    let run = execute(
        work.path(),
        &[
            "run",
            "workflow.program",
            "--record",
            record.to_str().unwrap(),
            "--manifest-dir",
            custom.to_str().unwrap(),
        ],
    );
    assert!(!run.status.success(), "the program should fail");
    // The failure itself names the repair loop, since the first failure usually
    // has no checkpoint.
    assert!(
        text(&run.stderr).contains("repair --from-record"),
        "{}",
        text(&run.stderr)
    );
    assert_eq!(count_lines(work.path()), 1);
    let run_json = fs::read(record.join("run.json")).unwrap();
    let trace = fs::read(record.join("trace.jsonl")).unwrap();

    let repaired = execute(
        work.path(),
        &[
            "repair",
            "--from-record",
            record.to_str().unwrap(),
            "--program",
            "workflow.program",
            "--op",
            "replace-command 2 run printf with message \"two\"",
            "--manifest-dir",
            custom.to_str().unwrap(),
        ],
    );
    assert_eq!(
        repaired.status.code(),
        Some(0),
        "{}",
        text(&repaired.stderr)
    );
    assert!(
        text(&repaired.stdout).contains("repair 1 applied"),
        "{}",
        text(&repaired.stdout)
    );
    assert!(fs::read_to_string(work.path().join("workflow.program"))
        .unwrap()
        .contains("with message \"two\""));

    let resumed = execute(
        work.path(),
        &[
            "resume",
            "workflow.program",
            "--from-record",
            record.to_str().unwrap(),
            "--manifest-dir",
            custom.to_str().unwrap(),
        ],
    );
    assert_eq!(resumed.status.code(), Some(0), "{}", text(&resumed.stderr));
    assert_eq!(text(&resumed.stdout), "two\nthree\n");
    assert!(
        text(&resumed.stderr).contains("1 steps replayed"),
        "{}",
        text(&resumed.stderr)
    );
    // The completed prefix replayed from captured blobs: the fake tool ran only
    // for the original failure, never for the resume.
    assert_eq!(count_lines(work.path()), 1);
    // The repair is logged beside the record; the record itself is untouched.
    assert_eq!(fs::read(record.join("run.json")).unwrap(), run_json);
    assert_eq!(fs::read(record.join("trace.jsonl")).unwrap(), trace);
    assert!(record.join("repairs.jsonl").is_file());
}

#[test]
fn repair_from_record_refuses_a_completed_prefix_line() {
    let work = tempfile::tempdir().unwrap();
    let source = failing_program();
    let record = failed_record(work.path(), &source);
    let before = fs::read_to_string(work.path().join("workflow.program")).unwrap();

    let refused = repair_from_record(
        work.path(),
        &record,
        "workflow.program",
        "replace-command 1 run printf with message \"changed\"",
    );
    assert_eq!(refused.status.code(), Some(1));
    assert!(
        text(&refused.stderr).contains("completed steps"),
        "{}",
        text(&refused.stderr)
    );
    assert_eq!(
        fs::read_to_string(work.path().join("workflow.program")).unwrap(),
        before
    );
    assert!(!record.join("repairs.jsonl").exists());
}

#[test]
fn resume_from_record_refuses_an_edited_prefix_without_rerunning_it() {
    let work = tempfile::tempdir().unwrap();
    let record = failed_record(work.path(), &failing_program());

    let repaired = repair_from_record(
        work.path(),
        &record,
        "workflow.program",
        "replace-command 2 run printf with message \"two\"",
    );
    assert_eq!(
        repaired.status.code(),
        Some(0),
        "{}",
        text(&repaired.stderr)
    );

    let program = work.path().join("workflow.program");
    let fixed = fs::read_to_string(&program).unwrap();
    fs::write(
        program,
        fixed.replace("with message \"before\"", "with message \"changed\""),
    )
    .unwrap();
    let resumed = execute(
        work.path(),
        &[
            "resume",
            "workflow.program",
            "--from-record",
            record.to_str().unwrap(),
            "--manifest-dir",
            manifests().to_str().unwrap(),
        ],
    );
    assert_eq!(resumed.status.code(), Some(1));
    let diagnostic = text(&resumed.stderr);
    assert!(
        diagnostic.contains("resume diverged at step 0"),
        "{diagnostic}"
    );
    assert!(diagnostic.contains("completed prefix"), "{diagnostic}");
    assert!(
        resumed.stdout.is_empty(),
        "a refused prefix must not run anything"
    );
}

#[test]
fn a_completed_record_has_no_failed_suffix_to_repair_or_resume() {
    let work = tempfile::tempdir().unwrap();
    fs::write(
        work.path().join("ok.program"),
        "run printf with message \"ok\"\n",
    )
    .unwrap();
    let record = work.path().join("record");
    let run = execute(
        work.path(),
        &[
            "run",
            "ok.program",
            "--record",
            record.to_str().unwrap(),
            "--manifest-dir",
            manifests().to_str().unwrap(),
        ],
    );
    assert_eq!(run.status.code(), Some(0), "{}", text(&run.stderr));

    let repair = repair_from_record(
        work.path(),
        &record,
        "ok.program",
        "replace-command 1 run printf with message \"x\"",
    );
    assert_eq!(repair.status.code(), Some(1));
    assert!(
        text(&repair.stderr).contains("completed successfully"),
        "{}",
        text(&repair.stderr)
    );
    let resumed = execute(
        work.path(),
        &[
            "resume",
            "ok.program",
            "--from-record",
            record.to_str().unwrap(),
            "--manifest-dir",
            manifests().to_str().unwrap(),
        ],
    );
    assert_eq!(resumed.status.code(), Some(1));
    assert!(
        text(&resumed.stderr).contains("completed successfully"),
        "{}",
        text(&resumed.stderr)
    );
}

#[test]
fn resume_from_record_refuses_a_prefix_step_with_missing_bytes() {
    let work = tempfile::tempdir().unwrap();
    let record = failed_record(work.path(), &failing_program());

    let repaired = repair_from_record(
        work.path(),
        &record,
        "workflow.program",
        "replace-command 2 run printf with message \"two\"",
    );
    assert_eq!(
        repaired.status.code(),
        Some(0),
        "{}",
        text(&repaired.stderr)
    );

    for entry in fs::read_dir(record.join("blobs")).unwrap() {
        fs::remove_file(entry.unwrap().path()).unwrap();
    }
    let resumed = execute(
        work.path(),
        &[
            "resume",
            "workflow.program",
            "--from-record",
            record.to_str().unwrap(),
            "--manifest-dir",
            manifests().to_str().unwrap(),
        ],
    );
    assert_eq!(resumed.status.code(), Some(1));
    assert!(
        text(&resumed.stderr).contains("record incomplete at step 0"),
        "{}",
        text(&resumed.stderr)
    );
}
