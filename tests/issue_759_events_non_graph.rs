//! Issue #759: `--events` selects graph terminal-event rendering only. A finite, screen,
//! or stream program must refuse it with a named diagnostic before any effect runs.
use std::{fs, process::Command};

const FINITE: &str = "run printf with message \"started\"\nthen save to file \"started.txt\"\n";
const SCREEN: &str = "open a screen \"Demo\"\n";
const STREAM: &str = "stream from side.effect with marker \"launched.marker\"\nshow each line\n";

/// Runs `execute <command> <file> --events <value>` in a fresh directory and asserts
/// the refusal: a failing exit, the named diagnostic, empty stdout, and no new files.
fn assert_events_refused(command: &str, file: &str, source: &str, kind: &str, value: &str) {
    let directory = tempfile::tempdir().unwrap();
    fs::write(directory.path().join(file), source).unwrap();
    fs::create_dir(directory.path().join("manifests")).unwrap();
    let mut process = Command::new(env!("CARGO_BIN_EXE_execute"));
    process
        .current_dir(directory.path())
        .arg(command)
        .arg(file)
        .arg("--manifest-dir")
        .arg(directory.path().join("manifests"))
        .arg("--events")
        .arg(value);
    if command == "resume" {
        // `resume` requires a checkpoint or record source; the directory is never created.
        process
            .arg("--checkpoint")
            .arg(directory.path().join("checkpoint"));
    }
    let output = process.output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "{command} of a {kind} program must fail"
    );
    assert!(
        stderr.contains(&format!(
            "--events applies only to graph programs, not {kind} programs"
        )),
        "missing diagnostic for {command} of a {kind} program: {stderr}"
    );
    assert!(
        output.stdout.is_empty(),
        "a refused {kind} program printed output"
    );
    let mut entries: Vec<String> = fs::read_dir(directory.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    entries.sort();
    let mut expected = vec![file.to_string(), "manifests".to_string()];
    expected.sort();
    assert_eq!(
        entries, expected,
        "a refused {kind} program left files behind"
    );
}

#[test]
fn finite_run_and_resume_refuse_events_jsonl_before_effects() {
    assert_events_refused("run", "finite.program", FINITE, "finite", "jsonl");
    assert_events_refused("resume", "finite.program", FINITE, "finite", "jsonl");
}

#[test]
fn finite_run_refuses_events_document_before_effects() {
    assert_events_refused("run", "finite.program", FINITE, "finite", "document");
}

#[cfg(feature = "screens")]
#[test]
fn screen_run_and_resume_refuse_events_jsonl_before_effects() {
    assert_events_refused("run", "screen.program", SCREEN, "screen", "jsonl");
    assert_events_refused("resume", "screen.program", SCREEN, "screen", "jsonl");
}

#[test]
fn stream_run_and_resume_refuse_events_jsonl_before_effects() {
    assert_events_refused("run", "stream.program", STREAM, "stream", "jsonl");
    assert_events_refused("resume", "stream.program", STREAM, "stream", "jsonl");
}
