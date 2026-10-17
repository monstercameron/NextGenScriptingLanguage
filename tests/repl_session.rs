//! `execute repl`: a scripted session keeps bindings across lines, survives a
//! validation error, and `:save` writes a program that runs the same way.

use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

fn manifests() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests")
}

fn repl(directory: &Path, input: &str) -> std::process::Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(directory)
        .args(["repl", "--manifest-dir"])
        .arg(manifests())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn a_scripted_session_keeps_bindings_survives_errors_and_saves_a_runnable_program() {
    let work = tempfile::tempdir().unwrap();
    let saved = work.path().join("session.program");
    let input = format!(
        "run printf with message \"a\" as first\n\
         run printf with message \"second\"\n\
         run printf with argumens \"typo\"\n\
         run printf with message \"still here\"\n\
         :bindings\n\
         :save {}\n\
         :quit\n",
        saved.display()
    );
    let session = repl(work.path(), &input);
    let stdout = String::from_utf8_lossy(&session.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&session.stderr).into_owned();
    assert!(session.status.success(), "{stderr}");

    // The bound statement prints nothing; the others print in order.
    assert!(stdout.contains("second\n"), "{stdout}");
    assert!(stdout.contains("still here\n"), "{stdout}");
    // The typo is rejected with the static diagnostic, and the session carries on.
    assert!(stdout.contains("rejected: "), "{stdout}");
    assert!(stdout.contains("argumens"), "{stdout}");
    assert!(stdout.contains("first = \"a\""), "{stdout}");
    assert!(stdout.contains("saved "), "{stdout}");

    // The saved program has the three valid statements and runs to the same output.
    let source = fs::read_to_string(&saved).unwrap();
    assert!(!source.contains("argumens"), "{source}");
    let run = Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(work.path())
        .args(["run"])
        .arg(&saved)
        .arg("--manifest-dir")
        .arg(manifests())
        .output()
        .unwrap();
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    let saved_output = String::from_utf8_lossy(&run.stdout).into_owned();
    let session_prints: String = stdout
        .lines()
        .take_while(|line| !line.starts_with("rejected:"))
        .map(|line| format!("{line}\n"))
        .collect();
    assert!(
        saved_output.ends_with("still here\n") && session_prints.contains("second\n"),
        "saved: {saved_output:?}\nsession: {stdout:?}"
    );
}

#[test]
fn save_refuses_to_replace_an_existing_file() {
    let work = tempfile::tempdir().unwrap();
    let existing = work.path().join("keep.program");
    fs::write(&existing, "mine\n").unwrap();
    let session = repl(
        work.path(),
        &format!(
            "run printf with message \"x\"\n:save {}\n:quit\n",
            existing.display()
        ),
    );
    let stdout = String::from_utf8_lossy(&session.stdout).into_owned();
    assert!(stdout.contains("error:"), "{stdout}");
    assert_eq!(fs::read_to_string(&existing).unwrap(), "mine\n");
}

#[test]
fn indented_bodies_continue_until_a_blank_line() {
    let work = tempfile::tempdir().unwrap();
    let session = repl(
        work.path(),
        "repeat 2 times\n  run printf with message \"x\"\n\nrun printf with message \"done\"\n",
    );
    let stdout = String::from_utf8_lossy(&session.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&session.stderr).into_owned();
    assert!(session.status.success(), "{stderr}");
    // The indented line is part of the block, so the block was accepted as one unit.
    assert!(
        !stdout.contains("rejected:") && !stdout.contains("error:"),
        "{stdout}"
    );
    assert!(stdout.contains("done\n"), "{stdout}");
}
