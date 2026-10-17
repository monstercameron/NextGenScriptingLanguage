//! End-to-end `.program` checks for fs.test, fs.glob and text.lines through the
//! CLI, including the typed binding paths used by `if ... is true` and
//! `for each`. Unix-only because the programs call `cat` and `printf`.
#![cfg(unix)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tempfile::TempDir;

fn manifests() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests")
}

fn example(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("examples/stdlib")
        .join(name)
}

fn run_program(
    working_directory: &Path,
    program: &Path,
    inputs: &[(&str, &str)],
    home: Option<&Path>,
) -> Output {
    let mut process = Command::new(env!("CARGO_BIN_EXE_execute"));
    process.current_dir(working_directory);
    process.arg(program);
    process.arg("--manifest-dir").arg(manifests());
    for (name, value) in inputs {
        process.arg("--input").arg(format!("{name}={value}"));
    }
    if let Some(home) = home {
        process.env("HOME", home);
    }
    process.output().unwrap()
}

fn write_program(directory: &TempDir, name: &str, source: &str) -> PathBuf {
    let path = directory.path().join(name);
    fs::write(&path, source).unwrap();
    path
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn fs_test_booleans_drive_conditions_directly() {
    let directory = tempfile::tempdir().unwrap();
    let folder = directory.path().join("folder");
    fs::create_dir(&folder).unwrap();
    let file = directory.path().join("file.txt");
    fs::write(&file, b"x").unwrap();
    let missing = directory.path().join("missing");
    let program = example("file-tests.program");

    let output = run_program(
        directory.path(),
        &program,
        &[("target", folder.to_str().unwrap())],
        None,
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        stdout(&output),
        format!("{} is a directory\n", folder.display())
    );

    let output = run_program(
        directory.path(),
        &program,
        &[("target", file.to_str().unwrap())],
        None,
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        stdout(&output),
        format!("{} is a file or other entry\n", file.display())
    );

    let output = run_program(
        directory.path(),
        &program,
        &[("target", missing.to_str().unwrap())],
        None,
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        stdout(&output),
        format!("{} does not exist\n", missing.display())
    );
}

#[test]
fn fs_glob_lists_feed_for_each_and_argv_without_helpers() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(directory.path().join("b.txt"), "second\n").unwrap();
    fs::write(directory.path().join("a.txt"), "first\n").unwrap();
    fs::write(directory.path().join("c.md"), "not matched\n").unwrap();
    fs::write(directory.path().join(".hidden.txt"), "hidden\n").unwrap();

    let output = run_program(
        directory.path(),
        &example("glob-files.program"),
        &[("pattern", "*.txt")],
        None,
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(stdout(&output), "matches: a.txt b.txt \n");
}

#[test]
fn fs_glob_empty_match_runs_the_loop_zero_times() {
    let directory = tempfile::tempdir().unwrap();
    let output = run_program(
        directory.path(),
        &example("glob-files.program"),
        &[("pattern", "*.none")],
        None,
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(stdout(&output), "matches: \n");
}

#[test]
fn fs_glob_expands_a_leading_tilde_from_home() {
    let home = tempfile::tempdir().unwrap();
    fs::write(home.path().join("note.txt"), "from home\n").unwrap();
    let working = tempfile::tempdir().unwrap();
    let program = write_program(
        &working,
        "tilde.program",
        "accept pattern as text\nrun fs.glob with pattern pattern as files\nfor each f in files\n  run \"cat\" with arguments [f]\n",
    );
    let output = run_program(
        working.path(),
        &program,
        &[("pattern", "~/*.txt")],
        Some(home.path()),
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(stdout(&output), "from home\n");
}

#[test]
fn text_lines_binds_a_list_for_each() {
    let directory = tempfile::tempdir().unwrap();
    let output = run_program(
        directory.path(),
        &example("lines-to-list.program"),
        &[("listing", "one\r\ntwo\n")],
        None,
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(stdout(&output), "items: [one][two]\n");
}

#[test]
fn unknown_fs_test_kind_is_rejected_before_running() {
    let directory = tempfile::tempdir().unwrap();
    let program = write_program(
        &directory,
        "typo.program",
        "run fs.test with path \".\" and kind \"dirr\" as result\nif result is true\n  run \"printf\" with arguments [\"yes\\n\"]\n",
    );
    let output = Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(directory.path())
        .arg("check")
        .arg(&program)
        .arg("--manifest-dir")
        .arg(manifests())
        .output()
        .unwrap();
    assert!(!output.status.success());
    let combined = format!(
        "{}{}",
        stdout(&output),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(combined.contains("kind"), "{combined}");
}
