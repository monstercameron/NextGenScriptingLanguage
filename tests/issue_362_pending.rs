#![cfg(feature = "tooling")]
//! `execute run-parts --once STATE --pending` lists the programs with no completion record
//! and exits 0 when any are pending, 1 when none are. It must not run anything.

use std::fs;
use std::path::Path;
use std::process::Command;
use tempfile::TempDir;

fn write_parts(root: &Path) {
    let parts = root.join("parts");
    fs::create_dir(&parts).unwrap();
    for name in ["01-first.program", "02-second.program", "03-third.program"] {
        fs::write(parts.join(name), "run \"true\" with arguments []\n").unwrap();
    }
}

fn pending(root: &Path, state: &str, extra: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(root)
        .arg("run-parts")
        .args(["parts", "--once", state, "--pending"])
        .args(extra)
        .output()
        .unwrap()
}

#[test]
fn pending_lists_unapplied_programs_and_exits_zero() {
    let directory = TempDir::new().unwrap();
    let root = directory.path();
    write_parts(root);
    fs::create_dir(root.join("state")).unwrap();
    fs::write(
        root.join("state/state.json"),
        "{\n  \"01-first.program\": \"0000\"\n}",
    )
    .unwrap();

    let output = pending(root, "state", &[]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(stdout, "02-second.program\n03-third.program\n");
    assert!(!root.join("first.txt").exists());
}

#[test]
fn pending_exits_one_and_prints_nothing_when_all_are_applied() {
    let directory = TempDir::new().unwrap();
    let root = directory.path();
    write_parts(root);
    fs::create_dir(root.join("state")).unwrap();
    fs::write(
        root.join("state/state.json"),
        "{\n  \"01-first.program\": \"a\",\n  \"02-second.program\": \"b\",\n  \"03-third.program\": \"c\"\n}",
    )
    .unwrap();

    let output = pending(root, "state", &[]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
}

#[test]
fn pending_json_reports_the_same_names() {
    let directory = TempDir::new().unwrap();
    let root = directory.path();
    write_parts(root);

    let output = pending(root, "fresh", &["--json"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        report["pending"],
        serde_json::json!(["01-first.program", "02-second.program", "03-third.program"])
    );
}

#[test]
fn pending_requires_a_state_directory() {
    let directory = TempDir::new().unwrap();
    let root = directory.path();
    write_parts(root);

    let output = Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(root)
        .args(["run-parts", "parts", "--pending"])
        .output()
        .unwrap();
    assert!(!output.status.success(), "{output:?}");
}
