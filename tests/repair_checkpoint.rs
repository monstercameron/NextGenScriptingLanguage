//! `execute repair CHECKPOINT --op ...`: a failed checkpointed program is repaired
//! in its unexecuted suffix, the journal is untouched, and resume completes the run.

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

/// Line 1 completes, line 2 fails, line 3 is never reached.
fn failing_program() -> String {
    let executable = env!("CARGO_BIN_EXE_execute").replace('\\', "/");
    format!(
        "run printf with message \"one\"\n\
         run \"{executable}\" with arguments [\"--no-such-option\"]\n\
         run printf with message \"three\"\n"
    )
}

/// Run the program until its checkpoint fails, returning the program and checkpoint.
fn failed_checkpoint(work: &Path) -> (PathBuf, PathBuf) {
    let program = work.join("prog.program");
    fs::write(&program, failing_program()).unwrap();
    let checkpoint = work.join("ckpt");
    let run = execute(
        work,
        &[
            "run",
            "prog.program",
            "--checkpoint",
            checkpoint.to_str().unwrap(),
            "--manifest-dir",
            manifests().to_str().unwrap(),
        ],
    );
    assert_eq!(run.status.code(), Some(1), "{}", text(&run.stderr));
    (program, checkpoint)
}

fn repair(work: &Path, checkpoint: &Path, op: &str, extra: &[&str]) -> Output {
    let mut arguments = vec![
        "repair",
        checkpoint.to_str().unwrap(),
        "--op",
        op,
        "--manifest-dir",
    ];
    let manifests = manifests();
    arguments.push(manifests.to_str().unwrap());
    arguments.extend_from_slice(extra);
    execute(work, &arguments)
}

#[test]
fn a_repaired_suffix_resumes_and_the_journal_is_never_modified() {
    let work = tempfile::tempdir().unwrap();
    let (program, checkpoint) = failed_checkpoint(work.path());
    let journal_before = fs::read(checkpoint.join("journal.json")).unwrap();

    let repaired = repair(
        work.path(),
        &checkpoint,
        "replace-command 2 run printf with message \"two\"",
        &[],
    );
    assert_eq!(
        repaired.status.code(),
        Some(0),
        "{}",
        text(&repaired.stderr)
    );
    let report = text(&repaired.stdout);
    assert!(
        report.contains("--- ") && report.contains("+++ "),
        "{report}"
    );
    assert!(
        report.contains("+    2 | run printf with message \"two\""),
        "{report}"
    );
    assert!(report.contains("repair 1 applied"), "{report}");

    assert_eq!(
        fs::read(checkpoint.join("journal.json")).unwrap(),
        journal_before,
        "repairs must not modify journal.json"
    );
    let log = fs::read_to_string(checkpoint.join("repairs.jsonl")).unwrap();
    let record: serde_json::Value = serde_json::from_str(log.lines().next().unwrap()).unwrap();
    assert_eq!(record["protocol"], "execute.repair/1");
    assert_eq!(record["kind"], "replace-command");
    assert_eq!(record["line"], 2);
    assert!(
        checkpoint
            .join(record["before_copy"].as_str().unwrap())
            .is_file(),
        "the program as it was before the repair is kept"
    );
    assert!(fs::read_to_string(&program).unwrap().contains("\"two\""));

    // The failed node is not retry-safe and may have had partial effects, so resuming
    // past it is an explicit decision even after a repair replaced it (#150).
    let resumed = execute(
        work.path(),
        &[
            "resume",
            "prog.program",
            "--checkpoint",
            checkpoint.to_str().unwrap(),
            "--manifest-dir",
            manifests().to_str().unwrap(),
            "--retry-uncertain",
        ],
    );
    assert_eq!(resumed.status.code(), Some(0), "{}", text(&resumed.stderr));
    let output = text(&resumed.stdout);
    assert!(
        output.contains("two\n") && output.contains("three\n"),
        "{output}"
    );
}

#[test]
fn a_completed_prefix_line_cannot_be_repaired() {
    let work = tempfile::tempdir().unwrap();
    let (program, checkpoint) = failed_checkpoint(work.path());
    let before = fs::read_to_string(&program).unwrap();
    let refused = repair(work.path(), &checkpoint, "remove 1", &[]);
    assert_eq!(refused.status.code(), Some(1));
    assert!(
        text(&refused.stderr).contains("completed steps"),
        "{}",
        text(&refused.stderr)
    );
    assert_eq!(fs::read_to_string(&program).unwrap(), before);
    assert!(!checkpoint.join("repairs.jsonl").exists());
}

#[test]
fn a_repair_that_does_not_check_leaves_the_program_untouched() {
    let work = tempfile::tempdir().unwrap();
    let (program, checkpoint) = failed_checkpoint(work.path());
    let before = fs::read_to_string(&program).unwrap();
    let refused = repair(
        work.path(),
        &checkpoint,
        "replace-command 2 run printf with argumens \"x\"",
        &[],
    );
    assert_eq!(refused.status.code(), Some(1));
    assert!(
        text(&refused.stderr).contains("does not check"),
        "{}",
        text(&refused.stderr)
    );
    assert_eq!(fs::read_to_string(&program).unwrap(), before);
    assert!(!checkpoint.join("repairs.jsonl").exists());
}

#[test]
fn a_dry_run_shows_the_diff_and_writes_nothing() {
    let work = tempfile::tempdir().unwrap();
    let (program, checkpoint) = failed_checkpoint(work.path());
    let before = fs::read_to_string(&program).unwrap();
    let dry = repair(
        work.path(),
        &checkpoint,
        "replace-command 2 run printf with message \"two\"",
        &["--dry-run"],
    );
    assert_eq!(dry.status.code(), Some(0), "{}", text(&dry.stderr));
    assert!(
        text(&dry.stdout).contains("dry run"),
        "{}",
        text(&dry.stdout)
    );
    assert_eq!(fs::read_to_string(&program).unwrap(), before);
    assert!(!checkpoint.join("repairs.jsonl").exists());
}

#[test]
fn a_completed_checkpoint_has_nothing_to_repair() {
    let work = tempfile::tempdir().unwrap();
    fs::write(
        work.path().join("ok.program"),
        "run printf with message \"x\"\n",
    )
    .unwrap();
    let checkpoint = work.path().join("ckpt");
    let run = execute(
        work.path(),
        &[
            "run",
            "ok.program",
            "--checkpoint",
            checkpoint.to_str().unwrap(),
            "--manifest-dir",
            manifests().to_str().unwrap(),
        ],
    );
    assert_eq!(run.status.code(), Some(0), "{}", text(&run.stderr));
    let refused = repair(work.path(), &checkpoint, "remove 1", &[]);
    assert_eq!(refused.status.code(), Some(1));
    assert!(
        text(&refused.stderr).contains("completed"),
        "{}",
        text(&refused.stderr)
    );
}
