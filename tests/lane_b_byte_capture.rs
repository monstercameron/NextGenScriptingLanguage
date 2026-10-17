//! `then capture bytes as NAME` and `use bytes NAME`: raw, non-UTF-8 pipeline bytes that
//! round-trip through `save` unchanged and cannot be used as text.
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

fn manifests() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests")
}

fn run_script(directory: &Path, command: &str, script: &str) -> Output {
    let path = directory.join("workflow.program");
    fs::write(&path, script).unwrap();
    Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(directory)
        .arg(command)
        .arg(&path)
        .arg("--manifest-dir")
        .arg(manifests())
        .output()
        .unwrap()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Bytes that are not valid UTF-8, including NUL and a lone continuation byte.
const RAW: [u8; 6] = [0xff, 0x00, b'A', 0x80, b'\n', 0xc3];

#[test]
fn captured_bytes_round_trip_through_save_unchanged() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(directory.path().join("in.bin"), RAW).unwrap();
    let output = run_script(
        directory.path(),
        "run",
        "run cat with path \"in.bin\"\nthen capture bytes as raw\nthen save to file \"out.bin\"\n",
    );
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(fs::read(directory.path().join("out.bin")).unwrap(), RAW);
}

#[test]
fn use_bytes_feeds_a_capture_back_unchanged() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(directory.path().join("in.bin"), RAW).unwrap();
    let output = run_script(
        directory.path(),
        "run",
        "run cat with path \"in.bin\"\nthen capture bytes as raw\nrun printf with message \"between\"\nuse bytes raw\nthen save to file \"out.bin\"\n",
    );
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(fs::read(directory.path().join("out.bin")).unwrap(), RAW);
}

#[test]
fn byte_captures_cannot_be_used_as_text() {
    let directory = tempfile::tempdir().unwrap();
    let output = run_script(
        directory.path(),
        "check",
        "run cat with path \"in.bin\"\nthen capture bytes as raw\nrun printf with message raw\n",
    );
    assert_ne!(output.status.code(), Some(0));
    assert!(stderr(&output).contains("raw"), "{}", stderr(&output));
}

#[test]
fn use_bytes_rejects_text_undefined_and_empty_captures() {
    let directory = tempfile::tempdir().unwrap();
    let text = run_script(
        directory.path(),
        "check",
        "accept name as text with default \"x\"\nuse bytes name\n",
    );
    assert!(
        stderr(&text).contains("requires a byte capture"),
        "{}",
        stderr(&text)
    );

    let undefined = run_script(directory.path(), "check", "use bytes missing\n");
    assert!(
        stderr(&undefined).contains("not defined"),
        "{}",
        stderr(&undefined)
    );

    let no_output = run_script(directory.path(), "check", "then capture bytes as raw\n");
    assert!(
        stderr(&no_output).contains("requires preceding output"),
        "{}",
        stderr(&no_output)
    );
}

#[test]
fn byte_captures_are_rejected_with_checkpoints() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(directory.path().join("in.bin"), RAW).unwrap();
    let path = directory.path().join("workflow.program");
    fs::write(
        &path,
        "run cat with path \"in.bin\"\nthen capture bytes as raw\nthen save to file \"out.bin\"\n",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(directory.path())
        .arg("run")
        .arg(&path)
        .arg("--manifest-dir")
        .arg(manifests())
        .arg("--checkpoint")
        .arg(directory.path().join("ckpt"))
        .output()
        .unwrap();
    assert_ne!(output.status.code(), Some(0));
    assert!(
        stderr(&output).contains("byte captures are not supported with checkpointed runs"),
        "{}",
        stderr(&output)
    );
}
