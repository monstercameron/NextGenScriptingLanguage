//! `execute checkpoint status DIR [--json]` against checkpoints produced by the
//! real `execute` binary. The command is read-only and reports the resume action.

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const HELPER: &str = env!("CARGO_BIN_EXE_execute-model");

fn manifests_with(directory: &Path) -> PathBuf {
    let manifests = directory.join("manifests");
    std::fs::create_dir_all(&manifests).unwrap();
    for (name, argument) in [("ok.helper", "--version"), ("fail.helper", "--nope")] {
        let manifest = serde_json::json!({
            "name": name,
            "binary": HELPER,
            "accepts_stdin": false,
            "parameters": {},
            "arguments": [{ "literal": argument }]
        });
        std::fs::write(manifests.join(format!("{name}.json")), manifest.to_string()).unwrap();
    }
    manifests
}

fn execute(directory: &Path, arguments: &[&str]) -> Output {
    let manifests = manifests_with(directory);
    let mut command = Command::new(env!("CARGO_BIN_EXE_execute"));
    command.args(arguments);
    // `checkpoint status` reads only the checkpoint directory. Passing
    // `--manifest-dir` there is a usage error, exactly as in a direct call.
    if !arguments.contains(&"checkpoint") {
        command.arg("--manifest-dir").arg(&manifests);
    }
    command
        .current_dir(directory)
        .env_remove("EXECUTE_MODELS_OFFLINE")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command.output().expect("start execute")
}

fn status(directory: &Path, checkpoint: &Path, extra: &[&str]) -> Output {
    let checkpoint = checkpoint.to_string_lossy().into_owned();
    let mut arguments = vec!["checkpoint", "status", checkpoint.as_str()];
    arguments.extend_from_slice(extra);
    execute(directory, &arguments)
}

fn parse_status(output: &Output) -> Value {
    assert!(
        output.status.success(),
        "status failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("--json prints one object")
}

#[test]
fn completed_checkpoint_reports_completed_and_restores_its_result() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("ok.program"), "run ok.helper\n").unwrap();
    let checkpoint = directory.path().join("ckpt");
    let checkpoint_text = checkpoint.to_string_lossy().into_owned();
    let run = execute(
        directory.path(),
        &["ok.program", "--checkpoint", &checkpoint_text],
    );
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );

    let summary = parse_status(&status(directory.path(), &checkpoint, &["--json"]));
    assert_eq!(summary["protocol"], "execute.checkpoint_status/1");
    assert_eq!(summary["journal"]["status"], "completed");
    assert!(summary["journal"]["completed_nodes"].as_u64().unwrap() >= 1);
    assert!(summary["journal"]["in_flight"].is_null());
    assert!(summary["journal"]["error"].is_null());
    assert_eq!(summary["journal"]["checksum"], "present");
    assert_eq!(summary["journal"]["checksum_verified"], false);
    assert_eq!(summary["resume_action"], "restores_completed_result");
    assert!(summary["journal"]["program_path"]
        .as_str()
        .unwrap()
        .ends_with("ok.program"));
    // Decoding must not leave NUL bytes from the UTF-16 encoding behind.
    assert!(!summary["journal"]["program_path"]
        .as_str()
        .unwrap()
        .contains('\u{0}'));
}

#[test]
fn failed_checkpoint_reports_the_error_and_a_continue_action() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("fail.program"), "run fail.helper\n").unwrap();
    let checkpoint = directory.path().join("ckpt");
    let checkpoint_text = checkpoint.to_string_lossy().into_owned();
    let run = execute(
        directory.path(),
        &["fail.program", "--checkpoint", &checkpoint_text],
    );
    assert!(!run.status.success());

    let summary = parse_status(&status(directory.path(), &checkpoint, &["--json"]));
    assert_eq!(summary["journal"]["status"], "failed");
    assert!(summary["journal"]["error"].as_str().is_some());
    assert_ne!(summary["resume_action"], "restores_completed_result");
}

#[test]
fn text_output_is_a_short_human_summary() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("ok.program"), "run ok.helper\n").unwrap();
    let checkpoint = directory.path().join("ckpt");
    let checkpoint_text = checkpoint.to_string_lossy().into_owned();
    execute(
        directory.path(),
        &["ok.program", "--checkpoint", &checkpoint_text],
    );

    let output = status(directory.path(), &checkpoint, &[]);
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("status: completed"), "{text}");
    assert!(
        text.contains("resume action: restores_completed_result"),
        "{text}"
    );
    assert!(
        text.contains("checksum: present, not verified by this command"),
        "{text}"
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn status_never_changes_the_checkpoint_directory() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("ok.program"), "run ok.helper\n").unwrap();
    let checkpoint = directory.path().join("ckpt");
    let checkpoint_text = checkpoint.to_string_lossy().into_owned();
    execute(
        directory.path(),
        &["ok.program", "--checkpoint", &checkpoint_text],
    );
    let before = std::fs::read(checkpoint.join("journal.json")).unwrap();
    let blobs_before: Vec<_> = std::fs::read_dir(checkpoint.join("blobs"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();

    for _ in 0..2 {
        status(directory.path(), &checkpoint, &["--json"]);
    }

    assert_eq!(
        std::fs::read(checkpoint.join("journal.json")).unwrap(),
        before
    );
    let blobs_after: Vec<_> = std::fs::read_dir(checkpoint.join("blobs"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(blobs_before, blobs_after);
}

#[test]
fn missing_checkpoint_fails_without_creating_anything() {
    let directory = tempfile::tempdir().unwrap();
    let checkpoint = directory.path().join("never-created");
    let output = status(directory.path(), &checkpoint, &["--json"]);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("no checkpoint journal"));
    assert!(!checkpoint.exists(), "status created the directory");
}

#[test]
fn corrupt_journal_is_rejected_and_reports_malformed() {
    let directory = tempfile::tempdir().unwrap();
    let checkpoint = directory.path().join("ckpt");
    std::fs::create_dir_all(&checkpoint).unwrap();
    std::fs::write(checkpoint.join("journal.json"), b"{not json").unwrap();
    let output = status(directory.path(), &checkpoint, &["--json"]);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("malformed"));
}

#[test]
fn unknown_options_and_extra_directories_are_usage_errors() {
    let directory = tempfile::tempdir().unwrap();
    let checkpoint = directory.path().join("ckpt");
    let checkpoint_text = checkpoint.to_string_lossy().into_owned();
    let bad_option = execute(
        directory.path(),
        &["checkpoint", "status", &checkpoint_text, "--verbose"],
    );
    assert!(!bad_option.status.success());
    assert!(String::from_utf8_lossy(&bad_option.stderr).contains("unknown option"));

    let two = execute(
        directory.path(),
        &["checkpoint", "status", &checkpoint_text, &checkpoint_text],
    );
    assert!(!two.status.success());
    assert!(String::from_utf8_lossy(&two.stderr).contains("one directory"));

    let no_action = execute(directory.path(), &["checkpoint"]);
    assert!(!no_action.status.success());
    assert!(String::from_utf8_lossy(&no_action.stderr).contains("usage: execute checkpoint status"));
}

#[test]
fn json_errors_flag_applies_to_checkpoint_status_failures() {
    let directory = tempfile::tempdir().unwrap();
    let checkpoint = directory.path().join("never-created");
    let checkpoint_text = checkpoint.to_string_lossy().into_owned();
    let output = execute(
        directory.path(),
        &["--json-errors", "checkpoint", "status", &checkpoint_text],
    );
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    let last = stderr.lines().last().expect("stderr has a final line");
    let object: Value = serde_json::from_str(last).expect("final line is JSON");
    assert_eq!(object["protocol"], "execute.error/1");
    assert_eq!(object["exit_code"], output.status.code().unwrap());
}
