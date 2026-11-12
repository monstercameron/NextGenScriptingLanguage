//! A checkpointed applet listener over a replayable growing file resumes from its persisted
//! producer offset: each line is handled once across a restart, and a replayable producer that
//! no longer reaches the committed offset is refused. A producer without `replayable_stream`
//! is a live source: a restart handles its fresh stream from the first byte, never seeked.

use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Output};

/// The producer prints the whole file from its first byte on every start. `replayable` sets
/// the manifest flag that lets a restart skip the committed prefix of that stream.
fn source_manifest(file: &Path, replayable: bool) -> serde_json::Value {
    #[cfg(unix)]
    let (binary, arguments) = (
        "/bin/cat".to_string(),
        serde_json::json!([{"literal": file.display().to_string()}]),
    );
    #[cfg(windows)]
    let (binary, arguments) = (
        std::env::var("ComSpec").unwrap_or_else(|_| r"C:\Windows\System32\cmd.exe".to_string()),
        serde_json::json!([
            {"literal": "/C"},
            {"literal": "type"},
            {"literal": file.display().to_string()}
        ]),
    );
    serde_json::json!({
        "name": "source",
        "binary": binary,
        "arguments": arguments,
        "replayable_stream": replayable
    })
}

fn write_fixture(
    directory: &Path,
    file: &Path,
    replayable: bool,
) -> (std::path::PathBuf, std::path::PathBuf) {
    let manifests = directory.join("manifests");
    fs::create_dir_all(&manifests).unwrap();
    fs::write(
        manifests.join("source.json"),
        serde_json::to_vec(&source_manifest(file, replayable)).unwrap(),
    )
    .unwrap();
    let workflow = directory.join("listener.program");
    fs::write(
        &workflow,
        "listen to source\nwhenever a line arrives as record\n  then encode record as json\n",
    )
    .unwrap();
    (workflow, manifests)
}

fn execute(directory: &Path, arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(directory)
        .args(arguments)
        .output()
        .unwrap()
}

#[test]
fn restarted_listener_resumes_after_the_committed_record() {
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("growing.log");
    fs::write(&file, "line one\nline two\n").unwrap();
    let (workflow, manifests) = write_fixture(directory.path(), &file, true);
    let workflow = workflow.to_string_lossy().into_owned();
    let manifests = manifests.to_string_lossy().into_owned();
    let checkpoint = directory.path().join("checkpoint");
    let checkpoint = checkpoint.to_string_lossy().into_owned();

    // The first process admits and handles one record, then stops at the run limit. Its
    // committed offset is the end of "line one".
    let first = execute(
        directory.path(),
        &[
            "run",
            &workflow,
            "--manifest-dir",
            &manifests,
            "--checkpoint",
            &checkpoint,
            "--max-runs",
            "1",
        ],
    );
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let first_out = String::from_utf8_lossy(&first.stdout).into_owned();
    assert_eq!(first_out.matches("line one").count(), 1, "{first_out}");
    assert_eq!(first_out.matches("line two").count(), 0, "{first_out}");

    // The file grows while the listener is stopped.
    let mut growing = fs::OpenOptions::new().append(true).open(&file).unwrap();
    growing.write_all(b"line three\n").unwrap();
    drop(growing);

    // The restart replays the producer, discards the committed prefix, and handles only the
    // two records that were not yet committed.
    let second = execute(
        directory.path(),
        &[
            "resume",
            &workflow,
            "--manifest-dir",
            &manifests,
            "--checkpoint",
            &checkpoint,
            "--max-runs",
            "2",
        ],
    );
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    let second_out = String::from_utf8_lossy(&second.stdout).into_owned();
    assert_eq!(second_out.matches("line one").count(), 0, "{second_out}");
    assert_eq!(second_out.matches("line two").count(), 1, "{second_out}");
    assert_eq!(second_out.matches("line three").count(), 1, "{second_out}");
}

#[test]
fn producer_shorter_than_the_committed_offset_is_refused() {
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("shrinking.log");
    fs::write(&file, "line one\nline two\n").unwrap();
    let (workflow, manifests) = write_fixture(directory.path(), &file, true);
    let workflow = workflow.to_string_lossy().into_owned();
    let manifests = manifests.to_string_lossy().into_owned();
    let checkpoint = directory.path().join("checkpoint");
    let checkpoint = checkpoint.to_string_lossy().into_owned();

    let first = execute(
        directory.path(),
        &[
            "run",
            &workflow,
            "--manifest-dir",
            &manifests,
            "--checkpoint",
            &checkpoint,
            "--max-runs",
            "1",
        ],
    );
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );

    // Replacing the file with something shorter than the committed offset must not be
    // silently replayed from the start of the new content.
    fs::write(&file, "x\n").unwrap();
    let second = execute(
        directory.path(),
        &[
            "resume",
            &workflow,
            "--manifest-dir",
            &manifests,
            "--checkpoint",
            &checkpoint,
            "--max-runs",
            "2",
        ],
    );
    assert!(!second.status.success());
    let stderr = String::from_utf8_lossy(&second.stderr).into_owned();
    assert!(
        stderr.contains("committed stream offset"),
        "unexpected diagnostic: {stderr}"
    );
    assert!(!String::from_utf8_lossy(&second.stdout).contains("\"x\\n\""));
}

#[test]
fn live_source_without_the_flag_is_not_seeked_or_refused_on_restart() {
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("live.log");
    fs::write(&file, "line one\nline two\n").unwrap();
    let (workflow, manifests) = write_fixture(directory.path(), &file, false);
    let workflow = workflow.to_string_lossy().into_owned();
    let manifests = manifests.to_string_lossy().into_owned();
    let checkpoint = directory.path().join("checkpoint");
    let checkpoint = checkpoint.to_string_lossy().into_owned();

    // The first process commits one record, so the journal records an offset of 9 bytes.
    let first = execute(
        directory.path(),
        &[
            "run",
            &workflow,
            "--manifest-dir",
            &manifests,
            "--checkpoint",
            &checkpoint,
            "--max-runs",
            "1",
        ],
    );
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );

    // The restarted producer is a fresh stream shorter than the committed offset. Without the
    // flag it is neither seeked past its first byte nor refused.
    fs::write(&file, "x\n").unwrap();
    let second = execute(
        directory.path(),
        &[
            "resume",
            &workflow,
            "--manifest-dir",
            &manifests,
            "--checkpoint",
            &checkpoint,
            "--max-runs",
            "1",
        ],
    );
    assert!(
        second.status.success(),
        "status {:?}\nstdout: {}\nstderr: {}",
        second.status,
        String::from_utf8_lossy(&second.stdout),
        String::from_utf8_lossy(&second.stderr)
    );
    let second_out = String::from_utf8_lossy(&second.stdout).into_owned();
    assert_eq!(second_out.matches("\"x\\n\"").count(), 1, "{second_out}");
}
