//! A final diagnostic larger than the stderr pipe buffer (64 KiB here) must reach a reader that is slow
//! to start. The child blocks on its write until the reader drains the pipe, so the
//! message must not be cut short by a write deadline.
use std::{
    fs,
    io::Read,
    path::Path,
    process::{Command, Stdio},
    thread,
    time::Duration,
};

#[test]
fn a_delayed_stderr_reader_still_receives_a_large_final_diagnostic() {
    let directory = tempfile::tempdir().unwrap();
    let message = format!("{}END-OF-DIAGNOSTIC", "x".repeat(200_000));
    let program = directory.path().join("fail.program");
    fs::write(&program, format!("fail with message \"{message}\"\n")).unwrap();
    let manifests = Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests");
    let mut child = Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(directory.path())
        .arg("run")
        .arg(&program)
        .arg("--manifest-dir")
        .arg(&manifests)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stderr = child.stderr.take().unwrap();
    // The reader is slow to start: the child's write fills the pipe and waits.
    thread::sleep(Duration::from_millis(1500));
    let mut received = String::new();
    stderr.read_to_string(&mut received).unwrap();
    let status = child.wait().unwrap();
    assert_eq!(status.code(), Some(1));
    assert!(
        received.contains(&message),
        "diagnostic was truncated: {} of {} bytes",
        received.len(),
        message.len()
    );
}
