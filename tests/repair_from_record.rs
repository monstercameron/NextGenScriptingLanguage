#![cfg(feature = "records")]
//! `execute repair --from-record` and `execute resume --from-record`: a failed
//! recorded run is repaired in its failed suffix and resumed without re-running
//! completed native steps (counted with a fake tool). Prefix edits, completed
//! records, missing blobs, truncated or torn records, short or count-mismatched
//! traces, edited program copies, manifest and executable swaps, withheld
//! secrets and unsupported shapes are refused without repeating completed
//! effects; prefix file writes and loop iterations are replayed, not re-run.

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
    repair_from_record_with_manifests(work, record, program, op, &manifests())
}

fn repair_from_record_with_manifests(
    work: &Path,
    record: &Path,
    program: &str,
    op: &str,
    manifest_dir: &Path,
) -> Output {
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
            manifest_dir.to_str().unwrap(),
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
        diagnostic.contains("completed prefix must still match"),
        "{diagnostic}"
    );
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

#[cfg(unix)]
fn failed_counter_record(work: &Path) -> (PathBuf, PathBuf) {
    let custom = counter_manifests(work);
    let source = format!(
        "run counter\nrun \"{}\" with arguments [\"--no-such-option\"]\nrun printf with message \"three\"\n",
        executable_text()
    );
    fs::write(work.join("workflow.program"), &source).unwrap();
    let record = work.join("record");
    let run = execute(
        work,
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
    (record, custom)
}

fn read_record_json(record: &Path) -> serde_json::Value {
    serde_json::from_slice(&fs::read(record.join("run.json")).unwrap()).unwrap()
}

fn write_record_json(record: &Path, document: &serde_json::Value) {
    fs::write(
        record.join("run.json"),
        serde_json::to_string_pretty(document).unwrap(),
    )
    .unwrap();
}

/// A resume replays applet output and suppresses the completed prefix's file
/// writes: an `append` and a `save` in the prefix must not touch their files
/// again, even though only the linear happy path is otherwise covered.
#[cfg(unix)]
#[test]
fn resume_suppresses_prefix_file_writes_without_repeating_them() {
    let work = tempfile::tempdir().unwrap();
    let custom = counter_manifests(work.path());
    let source = format!(
        "run counter\nthen append to \"log.txt\"\nthen save to \"out.txt\"\nrun \"{}\" with arguments [\"--no-such-option\"]\nrun printf with message \"three\"\n",
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
    assert_eq!(
        fs::read_to_string(work.path().join("log.txt")).unwrap(),
        "counted\n"
    );
    // A sentinel proves the resume does not rewrite the file: a repeated save
    // would replace it with the pipeline bytes.
    fs::write(work.path().join("out.txt"), "SENTINEL\n").unwrap();

    let repaired = repair_from_record_with_manifests(
        work.path(),
        &record,
        "workflow.program",
        "replace-command 4 run printf with message \"two\"",
        &custom,
    );
    assert_eq!(
        repaired.status.code(),
        Some(0),
        "{}",
        text(&repaired.stderr)
    );

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
    // The counter ran once, for the original failure: the append did not run
    // again and the save left the sentinel alone.
    assert_eq!(count_lines(work.path()), 1);
    assert_eq!(
        fs::read_to_string(work.path().join("log.txt")).unwrap(),
        "counted\n"
    );
    assert_eq!(
        fs::read_to_string(work.path().join("out.txt")).unwrap(),
        "SENTINEL\n"
    );
}

/// A loop in the completed prefix replays every iteration without launching
/// the native tool again.
#[cfg(unix)]
#[test]
fn resume_replays_a_loop_prefix_without_relaunching() {
    let work = tempfile::tempdir().unwrap();
    let custom = counter_manifests(work.path());
    let source = format!(
        "repeat 3 times\n  run counter\nrun \"{}\" with arguments [\"--no-such-option\"]\nrun printf with message \"done\"\n",
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
    assert_eq!(count_lines(work.path()), 3);

    let repaired = repair_from_record_with_manifests(
        work.path(),
        &record,
        "workflow.program",
        "replace-command 3 run printf with message \"two\"",
        &custom,
    );
    assert_eq!(
        repaired.status.code(),
        Some(0),
        "{}",
        text(&repaired.stderr)
    );

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
    assert_eq!(text(&resumed.stdout), "two\ndone\n");
    assert!(
        text(&resumed.stderr).contains("3 steps replayed"),
        "{}",
        text(&resumed.stderr)
    );
    assert_eq!(count_lines(work.path()), 3);
}

/// Repair refuses a program file edited in the completed prefix: line numbers
/// from the edited file cannot bound the record.
#[test]
fn repair_refuses_a_program_edited_before_the_boundary() {
    let work = tempfile::tempdir().unwrap();
    let source = failing_program();
    let record = failed_record(work.path(), &source);
    fs::write(
        work.path().join("workflow.program"),
        source.replace("with message \"before\"", "with message \"changed\""),
    )
    .unwrap();

    let refused = repair_from_record(
        work.path(),
        &record,
        "workflow.program",
        "replace-command 2 run printf with message \"two\"",
    );
    assert_eq!(refused.status.code(), Some(1));
    assert!(
        text(&refused.stderr).contains("differs from the program")
            && text(&refused.stderr).contains("repair the recorded program"),
        "{}",
        text(&refused.stderr)
    );
    assert!(!record.join("repairs.jsonl").exists());
}

/// A resume whose program keeps the suffix but rewrites the prefix is refused
/// before anything runs, even when the per-step groups would still match.
#[test]
fn resume_refuses_a_rewritten_prefix_without_running_anything() {
    let work = tempfile::tempdir().unwrap();
    let source = failing_program();
    let record = failed_record(work.path(), &source);
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
    // A comment-only edit still moves the completed prefix bytes.
    let program = work.path().join("workflow.program");
    let fixed = fs::read_to_string(&program).unwrap();
    fs::write(&program, format!("# rewritten\n{fixed}")).unwrap();
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
        text(&resumed.stderr).contains("completed prefix must still match"),
        "{}",
        text(&resumed.stderr)
    );
    assert!(resumed.stdout.is_empty());
}

/// Repair and resume refuse a record whose program copy was edited after the
/// run: the recorded hash no longer matches, so no boundary can be trusted.
#[test]
fn repair_and_resume_refuse_an_edited_program_copy() {
    let work = tempfile::tempdir().unwrap();
    let source = failing_program();
    let record = failed_record(work.path(), &source);
    let copy = record.join("program.program");
    let before = fs::read(&copy).unwrap();
    fs::write(&copy, [before.as_slice(), b"\n# edited\n"].concat()).unwrap();

    let repair = repair_from_record(
        work.path(),
        &record,
        "workflow.program",
        "replace-command 2 run printf with message \"two\"",
    );
    assert_eq!(repair.status.code(), Some(1));
    assert!(
        text(&repair.stderr).contains("does not match its recorded hash"),
        "{}",
        text(&repair.stderr)
    );
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
        text(&resumed.stderr).contains("does not match its recorded hash"),
        "{}",
        text(&resumed.stderr)
    );
}

/// A torn trace — the run died mid-line — refuses repair and resume instead
/// of silently dropping the last step and re-running its effect.
#[cfg(unix)]
#[test]
fn repair_and_resume_refuse_a_torn_trace() {
    let work = tempfile::tempdir().unwrap();
    let (record, custom) = failed_counter_record(work.path());
    let trace_path = record.join("trace.jsonl");
    let trace = fs::read(&trace_path).unwrap();
    assert!(trace.ends_with(b"\n"));
    // Cut the final step line in half, as a kill mid-write would.
    let cut = trace.len().saturating_sub(10);
    fs::write(&trace_path, &trace[..cut]).unwrap();
    let mut run_json = read_record_json(&record);
    run_json["finished"] = serde_json::Value::Bool(false);
    write_record_json(&record, &run_json);

    let repair = repair_from_record_with_manifests(
        work.path(),
        &record,
        "workflow.program",
        "replace-command 2 run printf with message \"two\"",
        &custom,
    );
    assert_eq!(repair.status.code(), Some(1));
    assert!(
        text(&repair.stderr).contains("ends mid-line"),
        "{}",
        text(&repair.stderr)
    );
    assert!(!record.join("repairs.jsonl").exists());
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
    assert_eq!(resumed.status.code(), Some(1));
    assert!(
        text(&resumed.stderr).contains("ends mid-line"),
        "{}",
        text(&resumed.stderr)
    );
    assert!(resumed.stdout.is_empty());
    // Nothing re-ran: the counter still shows only the original failure.
    assert_eq!(count_lines(work.path()), 1);
}

/// A record that outgrew its byte budget refuses repair and resume: hashes
/// without bytes cannot replay a prefix.
#[cfg(unix)]
#[test]
fn repair_and_resume_refuse_a_truncated_record() {
    let work = tempfile::tempdir().unwrap();
    let (record, custom) = failed_counter_record(work.path());
    let mut run_json = read_record_json(&record);
    run_json["truncated"] = serde_json::Value::Bool(true);
    write_record_json(&record, &run_json);

    let repair = execute(
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
    assert_eq!(repair.status.code(), Some(1));
    assert!(
        text(&repair.stderr).contains("truncated"),
        "{}",
        text(&repair.stderr)
    );
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
    assert_eq!(resumed.status.code(), Some(1));
    assert!(
        text(&resumed.stderr).contains("truncated"),
        "{}",
        text(&resumed.stderr)
    );
    assert!(resumed.stdout.is_empty());
    assert_eq!(count_lines(work.path()), 1);
}

/// A finished record whose trace lost whole trailing lines is refused: the
/// trace is shorter than `run.json` counts, so the completed prefix cannot be
/// trusted and nothing re-runs.
#[test]
fn repair_and_resume_refuse_a_finished_record_with_a_short_trace() {
    let work = tempfile::tempdir().unwrap();
    let source = failing_program();
    let record = failed_record(work.path(), &source);
    // Drop the final (failed) step line at a line boundary, keeping the
    // finished run document: the trace now holds fewer steps than counted.
    let trace_path = record.join("trace.jsonl");
    let trace = fs::read_to_string(&trace_path).unwrap();
    let mut lines: Vec<&str> = trace.lines().collect();
    assert!(lines.len() >= 2);
    lines.pop();
    fs::write(&trace_path, lines.join("\n") + "\n").unwrap();

    let repair = repair_from_record(
        work.path(),
        &record,
        "workflow.program",
        "replace-command 2 run printf with message \"two\"",
    );
    assert_eq!(repair.status.code(), Some(1));
    assert!(
        text(&repair.stderr).contains("trace holds 1 steps but run.json counts 2"),
        "{}",
        text(&repair.stderr)
    );
    assert!(!record.join("repairs.jsonl").exists());
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
        text(&resumed.stderr).contains("trace holds 1 steps but run.json counts 2"),
        "{}",
        text(&resumed.stderr)
    );
    assert!(resumed.stdout.is_empty());
}

/// Deleting whole trailing trace lines and flipping the run to unfinished is
/// refused: the recorder only writes a step count when a run finishes, so the
/// stale count proves the record was edited after the run rather than
/// interrupted mid-way. Nothing re-runs.
#[test]
fn repair_and_resume_refuse_an_unfinished_record_with_a_stale_step_count() {
    let work = tempfile::tempdir().unwrap();
    let source = failing_program();
    let record = failed_record(work.path(), &source);
    // The forged-interruption shape: whole trailing lines gone at a line
    // boundary, `finished` flipped, but the finished step count left behind.
    let trace_path = record.join("trace.jsonl");
    let trace = fs::read_to_string(&trace_path).unwrap();
    let mut lines: Vec<&str> = trace.lines().collect();
    assert!(lines.len() >= 2);
    lines.pop();
    fs::write(&trace_path, lines.join("\n") + "\n").unwrap();
    let mut run_json = read_record_json(&record);
    run_json["finished"] = serde_json::Value::Bool(false);
    write_record_json(&record, &run_json);

    let repair = repair_from_record(
        work.path(),
        &record,
        "workflow.program",
        "replace-command 2 run printf with message \"two\"",
    );
    assert_eq!(repair.status.code(), Some(1));
    assert!(
        text(&repair.stderr).contains("claims an unfinished run but counts 2 steps"),
        "{}",
        text(&repair.stderr)
    );
    assert!(!record.join("repairs.jsonl").exists());
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
        text(&resumed.stderr).contains("claims an unfinished run but counts 2 steps"),
        "{}",
        text(&resumed.stderr)
    );
    assert!(resumed.stdout.is_empty());
}

/// Resuming under a different manifest for the same applet name is refused:
/// the recorded blobs belong to the recorded manifests.
#[cfg(unix)]
#[test]
fn resume_refuses_a_manifest_swap_without_repeating_effects() {
    let work = tempfile::tempdir().unwrap();
    let (record, _custom) = failed_counter_record(work.path());
    let alternate = work.path().join("alternate-manifests");
    fs::create_dir(&alternate).unwrap();
    fs::copy(
        manifests().join("printf.json"),
        alternate.join("printf.json"),
    )
    .unwrap();
    fs::write(
        alternate.join("counter.json"),
        serde_json::json!({
            "name": "counter",
            "binary": "/nonexistent-adversarial-counter",
            "arguments": []
        })
        .to_string(),
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
            alternate.to_str().unwrap(),
        ],
    );
    assert_eq!(resumed.status.code(), Some(1));
    assert!(
        text(&resumed.stderr).contains("manifests changed"),
        "{}",
        text(&resumed.stderr)
    );
    assert!(resumed.stdout.is_empty());
    assert_eq!(count_lines(work.path()), 1);
}

/// Swapping an executable behind an identical manifest is refused: the record
/// pins the bytes each observed binary resolved to, so stale blobs never
/// replay for different bytes. The refusal lands before anything launches, so
/// the swapped binary never runs and the count never moves.
#[cfg(unix)]
#[test]
fn repair_and_resume_refuse_an_executable_swap_behind_an_identical_manifest() {
    let work = tempfile::tempdir().unwrap();
    let (record, custom) = failed_counter_record(work.path());
    assert_eq!(count_lines(work.path()), 1);
    // Same path, same manifest body, different bytes. Rewriting the file keeps
    // its executable mode, so only the pinned identity changes.
    fs::write(
        work.path().join("counter.sh"),
        "#!/bin/sh\necho TAMPERED >> 'count.txt'\nprintf 'tampered\\n'\n",
    )
    .unwrap();

    let repair = repair_from_record_with_manifests(
        work.path(),
        &record,
        "workflow.program",
        "replace-command 2 run printf with message \"two\"",
        &custom,
    );
    assert_eq!(repair.status.code(), Some(1));
    assert!(
        text(&repair.stderr).contains("the executable for"),
        "{}",
        text(&repair.stderr)
    );
    assert!(!record.join("repairs.jsonl").exists());
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
    assert_eq!(resumed.status.code(), Some(1));
    assert!(
        text(&resumed.stderr).contains("the executable for"),
        "{}",
        text(&resumed.stderr)
    );
    assert!(resumed.stdout.is_empty());
    // Neither the original nor the swapped binary ran again.
    assert_eq!(count_lines(work.path()), 1);
}

/// A secret input answer is withheld from the record, so the resume that
/// would need it is refused rather than run with a marker in its place.
#[cfg(unix)]
#[test]
fn resume_refuses_a_withheld_secret_input_without_running() {
    let work = tempfile::tempdir().unwrap();
    let custom = counter_manifests(work.path());
    let source = format!(
        "accept key as text from secret prompt \"Key?\"\nuse value key\nthen \"cat\"\nrun counter\nrun \"{}\" with arguments [\"--no-such-option\"]\nrun printf with message \"done\"\n",
        executable_text()
    );
    fs::write(work.path().join("workflow.program"), &source).unwrap();
    fs::write(
        work.path().join("answers.json"),
        "{\"key\":\"s3cret-value\"}",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(
            work.path().join("answers.json"),
            fs::Permissions::from_mode(0o600),
        )
        .unwrap();
    }
    let record = work.path().join("record");
    let run = Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(work.path())
        .args([
            "run",
            "workflow.program",
            "--record",
            record.to_str().unwrap(),
            "--manifest-dir",
            custom.to_str().unwrap(),
            "--answers",
            "answers.json",
        ])
        .output()
        .unwrap();
    assert!(
        !run.status.success(),
        "the program should fail: {}",
        text(&run.stderr)
    );
    assert_eq!(count_lines(work.path()), 1);

    let repaired = repair_from_record_with_manifests(
        work.path(),
        &record,
        "workflow.program",
        "replace-command 5 run printf with message \"two\"",
        &custom,
    );
    assert_eq!(
        repaired.status.code(),
        Some(0),
        "{}",
        text(&repaired.stderr)
    );
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
    assert_eq!(resumed.status.code(), Some(1));
    assert!(
        text(&resumed.stderr).contains("withhold"),
        "{}",
        text(&resumed.stderr)
    );
    assert!(resumed.stdout.is_empty());
    assert_eq!(count_lines(work.path()), 1);
}

/// A dry-run repair reports the diff without touching the program or logging
/// a repair.
#[test]
fn repair_dry_run_changes_nothing() {
    let work = tempfile::tempdir().unwrap();
    let source = failing_program();
    let record = failed_record(work.path(), &source);
    let before = fs::read_to_string(work.path().join("workflow.program")).unwrap();

    let dry = execute(
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
            manifests().to_str().unwrap(),
            "--dry-run",
        ],
    );
    assert_eq!(dry.status.code(), Some(0), "{}", text(&dry.stderr));
    assert!(
        text(&dry.stdout).contains("dry run"),
        "{}",
        text(&dry.stdout)
    );
    assert_eq!(
        fs::read_to_string(work.path().join("workflow.program")).unwrap(),
        before
    );
    assert!(!record.join("repairs.jsonl").exists());
}

/// A program shape the record cannot replay is refused before anything runs.
#[cfg(unix)]
#[test]
fn resume_refuses_an_unsupported_program_shape() {
    let work = tempfile::tempdir().unwrap();
    let (record, custom) = failed_counter_record(work.path());
    fs::write(
        work.path().join("detached.program"),
        "run counter\nlaunch detached \"true\"\n",
    )
    .unwrap();
    let resumed = execute(
        work.path(),
        &[
            "resume",
            "detached.program",
            "--from-record",
            record.to_str().unwrap(),
            "--manifest-dir",
            custom.to_str().unwrap(),
        ],
    );
    assert_eq!(resumed.status.code(), Some(1));
    assert!(
        text(&resumed.stderr).contains("does not support"),
        "{}",
        text(&resumed.stderr)
    );
    assert!(resumed.stdout.is_empty());
    assert_eq!(count_lines(work.path()), 1);
}

/// An interrupted record with a clean trace still repairs and resumes: every
/// recorded step replays and the suffix runs once.
#[cfg(unix)]
#[test]
fn interrupted_record_with_a_clean_trace_still_repairs_and_resumes() {
    let work = tempfile::tempdir().unwrap();
    let (record, custom) = failed_counter_record(work.path());
    let mut run_json = read_record_json(&record);
    run_json["finished"] = serde_json::Value::Bool(false);
    // A killed run never publishes a step count: the recorder only writes
    // counts when a run finishes. Zero it to simulate the interruption
    // faithfully; a stale nonzero count is refused (see the stale-step-count
    // test above) rather than mistaken for an interruption.
    run_json["steps"] = serde_json::Value::from(0);
    write_record_json(&record, &run_json);

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
    assert_eq!(count_lines(work.path()), 1);
}

/// A second resume from the same record replays the same prefix without
/// relaunching it: resume leaves the record reusable and never double-runs
/// completed effects, however often it resumes.
#[cfg(unix)]
#[test]
fn a_second_resume_replays_the_prefix_without_relaunching() {
    let work = tempfile::tempdir().unwrap();
    let (record, custom) = failed_counter_record(work.path());
    let repaired = repair_from_record_with_manifests(
        work.path(),
        &record,
        "workflow.program",
        "replace-command 2 run printf with message \"two\"",
        &custom,
    );
    assert_eq!(
        repaired.status.code(),
        Some(0),
        "{}",
        text(&repaired.stderr)
    );
    for _ in 0..2 {
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
    }
    assert_eq!(count_lines(work.path()), 1);
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

/// An MSYS `printf.exe` next to its runtime, mirroring `tests/windows_native.rs`:
/// the Git for Windows layout first, then `PATH`, and only when the sibling
/// `msys-2.0.dll` proves it is the MSYS build the portable assertions pin.
#[cfg(windows)]
fn msys_printf() -> Option<PathBuf> {
    let candidates = ["ProgramFiles", "ProgramFiles(x86)"]
        .into_iter()
        .filter_map(std::env::var_os)
        .map(|root| Path::new(&root).join("Git/usr/bin/printf.exe"))
        .chain(
            std::env::var_os("PATH")
                .into_iter()
                .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
                .map(|directory| directory.join("printf.exe")),
        );
    candidates.find(|path| {
        path.is_file()
            && path
                .parent()
                .is_some_and(|directory| directory.join("msys-2.0.dll").is_file())
    })
}

/// The absolute `printf` binary the portable acceptance pins: MSYS on Windows,
/// the system binary elsewhere. Absolute so neither the failing run nor the
/// resume depends on whatever a `PATH` lookup happens to find.
fn portable_printf() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        msys_printf()
    }
    #[cfg(not(windows))]
    {
        ["/usr/bin/printf", "/bin/printf"]
            .into_iter()
            .map(PathBuf::from)
            .find(|path| path.is_file())
    }
}

/// The pinned `printf` fixture, or `None` after recording why the portable
/// acceptance cannot run here: CI must provide it, other machines skip.
fn require_portable_printf() -> Option<PathBuf> {
    let printf = portable_printf();
    if printf.is_none() {
        if std::env::var_os("CI").is_some() {
            panic!(
                "CI requires an MSYS printf.exe with sibling msys-2.0.dll on Windows \
                 (see the Windows workflow fixture-verification step), or \
                 /usr/bin/printf or /bin/printf elsewhere"
            );
        }
        eprintln!("skipping portable acceptance: no system printf fixture was found");
    }
    printf
}

/// A counting fake tool that runs on Unix and Windows. The manifest invokes
/// the platform shell on a generated script (batch files never run without an
/// explicit `cmd.exe` invocation), so the same acceptance test counts native
/// invocations on both systems.
fn portable_counter_manifests(work: &Path, printf: &Path) -> PathBuf {
    let custom = work.join("portable-manifests");
    fs::create_dir(&custom).unwrap();
    // Pin the absolute fixture binary instead of the shipped `PATH`-resolved
    // manifest, so the run and its resume prove the record logic rather than
    // the runner's `PATH`.
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(manifests().join("printf.json")).unwrap()).unwrap();
    manifest["binary"] = serde_json::Value::String(printf.to_string_lossy().replace('\\', "/"));
    fs::write(custom.join("printf.json"), manifest.to_string()).unwrap();
    let count = work.join("portable-count.txt");
    let (binary, arguments) = if cfg!(windows) {
        let script = work.join("portable-counter.cmd");
        fs::write(
            &script,
            format!(
                "@echo off\r\n>>\"{}\" echo invoke\r\necho counted\r\n",
                count.display()
            ),
        )
        .unwrap();
        (
            "cmd.exe".to_owned(),
            serde_json::json!([
                {"literal": "/d"},
                {"literal": "/c"},
                {"literal": script.to_string_lossy()},
            ]),
        )
    } else {
        let script = work.join("portable-counter.sh");
        fs::write(
            &script,
            format!(
                "echo invoke >> '{}'\nprintf 'counted\\n'\n",
                count.display()
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        }
        (
            "/bin/sh".to_owned(),
            serde_json::json!([{"literal": script.to_string_lossy()}]),
        )
    };
    // The fake tool records every launch by appending to the count file, so a
    // resumed prefix that re-ran it would leave more than one line behind.
    fs::write(
        custom.join("counter.json"),
        serde_json::json!({
            "name": "counter",
            "binary": binary,
            "arguments": arguments,
        })
        .to_string(),
    )
    .unwrap();
    custom
}

fn portable_count_lines(work: &Path) -> usize {
    fs::read_to_string(work.join("portable-count.txt"))
        .map(|text| text.lines().count())
        .unwrap_or(0)
}

/// The counting acceptance test, portable across Unix and Windows: a failed
/// recorded run is repaired with one operation and resumes without re-running
/// the completed native step, which the counting fake tool proves.
#[test]
fn recorded_failure_repairs_and_resumes_without_repeating_completed_effects_portable() {
    let work = tempfile::tempdir().unwrap();
    let Some(printf) = require_portable_printf() else {
        return;
    };
    let custom = portable_counter_manifests(work.path(), &printf);
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
    assert_eq!(portable_count_lines(work.path()), 1);
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
    assert_eq!(portable_count_lines(work.path()), 1);
    // The repair is logged beside the record; the record itself is untouched.
    assert_eq!(fs::read(record.join("run.json")).unwrap(), run_json);
    assert_eq!(fs::read(record.join("trace.jsonl")).unwrap(), trace);
    assert!(record.join("repairs.jsonl").is_file());
}

/// An executable the record cannot prove refuses repair and resume on every
/// platform: the forged length-only `oversize:<bytes>` marker below is the
/// shape files larger than the 64 MiB identity limit pin, and equal markers
/// never prove equal bytes, so nothing re-runs.
#[test]
fn repair_and_resume_refuse_an_oversize_executable_without_rerunning() {
    let work = tempfile::tempdir().unwrap();
    let Some(printf) = require_portable_printf() else {
        return;
    };
    let custom = portable_counter_manifests(work.path(), &printf);
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
    assert_eq!(portable_count_lines(work.path()), 1);
    // Forge the recorded side into the oversize shape: a large executable pins
    // only its length, so this marker stands for any same-length bytes.
    let mut run_json = read_record_json(&record);
    let executables = run_json["provenance"]["executables"]
        .as_object_mut()
        .expect("the record pins executable provenance");
    assert!(!executables.is_empty());
    for identity in executables.values_mut() {
        *identity = serde_json::Value::from("oversize:67108865");
    }
    write_record_json(&record, &run_json);

    let repair = execute(
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
    assert_eq!(repair.status.code(), Some(1));
    assert!(
        text(&repair.stderr).contains("larger than"),
        "{}",
        text(&repair.stderr)
    );
    assert!(!record.join("repairs.jsonl").exists());
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
    assert_eq!(resumed.status.code(), Some(1));
    assert!(
        text(&resumed.stderr).contains("larger than"),
        "{}",
        text(&resumed.stderr)
    );
    assert!(resumed.stdout.is_empty());
    // Neither refusal re-ran the completed prefix.
    assert_eq!(portable_count_lines(work.path()), 1);
}
