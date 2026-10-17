//! Writing program output to a reader that has gone away ends the run quietly
//! with 141, the SIGPIPE convention, and prints no diagnostic.
use std::fs;
use std::process::{Command, Stdio};
use tempfile::tempdir;

#[test]
fn closed_stdout_reader_exits_141_without_a_diagnostic() {
    let directory = tempdir().unwrap();
    let program = directory.path().join("output.program");
    fs::write(&program, "run text.trim_final_lf with text \"a\"\n").unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_execute"))
        .arg("run")
        .arg(&program)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Close the reader before the program can write its output.
    drop(child.stdout.take());
    let output = child.wait_with_output().unwrap();

    assert_eq!(output.status.code(), Some(141));
    assert!(
        output.stderr.is_empty(),
        "unexpected diagnostic: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
