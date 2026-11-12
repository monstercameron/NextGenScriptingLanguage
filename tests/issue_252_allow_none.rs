//! #252: a program that declares `# execute-policy: allow none` performs no write. The
//! header is enforced by `run` before the first effect, so a file write fails and leaves
//! no file behind.
#![cfg(feature = "agent-advisory")]

use std::fs;
use std::path::Path;
use std::process::Command;

fn run(directory: &Path, program: &str) -> std::process::Output {
    fs::write(directory.join("p.program"), program).unwrap();
    Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(directory)
        .arg("run")
        .arg("p.program")
        .output()
        .expect("execute starts")
}

#[test]
fn allow_none_program_that_writes_a_file_fails_before_the_write() {
    let directory = tempfile::tempdir().unwrap();
    let output = run(
        directory.path(),
        "# execute-policy: allow none\nuse value \"x\"\nthen save to \"written.txt\"\n",
    );
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr).replace('\n', " | ");
    assert!(
        stderr.contains("execute-policy header denies effects"),
        "STDERR-DIAG: {}",
        stderr.chars().take(400).collect::<String>()
    );
    assert!(!directory.path().join("written.txt").exists());
}
