//! `execute check --format github`: one workflow-command annotation per diagnostic,
//! relative file names, exit status unchanged, and no change to `--json`.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};
use tempfile::TempDir;

fn manifests() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests")
}

fn check(directory: &Path, source: &str, extra: &[&str]) -> Output {
    fs::write(directory.join("workflow.program"), source).unwrap();
    Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(directory)
        .arg("check")
        .arg("workflow.program")
        .arg("--manifest-dir")
        .arg(manifests())
        .args(extra)
        .output()
        .unwrap()
}

#[test]
fn unknown_applet_becomes_an_error_annotation_with_position() {
    let directory = TempDir::new().unwrap();
    let output = check(
        directory.path(),
        "run no.such.applet with x \"y\"\n",
        &["--format", "github"],
    );
    assert_eq!(output.status.code(), Some(1));
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 1, "{stdout}");
    assert!(
        lines[0].starts_with("::error file=workflow.program,line=1,col=5,"),
        "{stdout}"
    );
    assert!(lines[0].contains("title=applet.unknown::"), "{stdout}");
}

#[test]
fn clean_program_prints_no_annotations_and_succeeds() {
    let directory = TempDir::new().unwrap();
    let output = check(
        directory.path(),
        "run printf with message \"hi\"\n",
        &["--format", "github"],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stdout.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn json_output_is_unchanged_and_unknown_formats_are_refused() {
    let directory = TempDir::new().unwrap();
    let json = check(
        directory.path(),
        "run printf with message \"hi\"\n",
        &["--json"],
    );
    assert!(String::from_utf8_lossy(&json.stdout).contains("\"protocol\": \"execute.check/1\""));

    let refused = check(
        directory.path(),
        "run printf with message \"hi\"\n",
        &["--format", "yaml"],
    );
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("github or json"));
}
