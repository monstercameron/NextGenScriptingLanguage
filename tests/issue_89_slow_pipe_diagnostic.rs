//! Issue #89: a final diagnostic must reach a stderr reader that drains the pipe slowly. The whole
//! message takes longer than the old single write bound to deliver, but every chunk is accepted
//! within its own bound, so nothing is cut off.
#![cfg(unix)]
use std::{
    fs,
    io::Read,
    path::Path,
    process::{Command, Stdio},
    thread,
    time::Duration,
};

#[test]
fn a_slowly_draining_stderr_reader_receives_the_whole_final_diagnostic() {
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
    // The pipe holds 64 KiB, so the rest of the message waits on this reader: 4 KiB every 100 ms
    // takes about 3.4 s to drain, longer than the 2 s bound on the whole message.
    let mut received = Vec::new();
    let mut buffer = [0u8; 4096];
    loop {
        let read = stderr.read(&mut buffer).unwrap();
        if read == 0 {
            break;
        }
        received.extend_from_slice(&buffer[..read]);
        thread::sleep(Duration::from_millis(100));
    }
    let status = child.wait().unwrap();
    let received = String::from_utf8_lossy(&received);
    assert_eq!(status.code(), Some(1));
    assert!(
        received.contains(&message),
        "diagnostic was truncated: {} of {} bytes",
        received.len(),
        message.len()
    );
}
