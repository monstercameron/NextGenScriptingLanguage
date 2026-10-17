//! A leading `~/` (or bare `~`) expands to the home directory in path-typed values
//! only. Argv strings stay literal, and `./~` is the way to name a literal `~`.
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tempfile::TempDir;

fn manifests() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests")
}

/// Run a program in `cwd` with HOME (and USERPROFILE, for Windows) set to `home`.
fn run(cwd: &Path, home: &Path, script: &str) -> Output {
    let path = cwd.join("workflow.program");
    fs::write(&path, script).unwrap();
    Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(cwd)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .arg("run")
        .arg(&path)
        .arg("--manifest-dir")
        .arg(manifests())
        .output()
        .unwrap()
}

fn setup() -> (TempDir, PathBuf, PathBuf) {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let cwd = root.path().join("cwd");
    fs::create_dir_all(home.join("sub")).unwrap();
    fs::create_dir_all(&cwd).unwrap();
    (root, home, cwd)
}

#[test]
fn read_expands_a_leading_tilde_slash() {
    let (_root, home, cwd) = setup();
    fs::write(home.join("x.txt"), b"hi").unwrap();
    let output = run(&cwd, &home, "read \"~/x.txt\"\n");
    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    assert_eq!(output.stdout, b"hi");
}

#[test]
fn save_expands_a_leading_tilde_slash() {
    let (_root, home, cwd) = setup();
    let output = run(
        &cwd,
        &home,
        "run printf with message \"saved\"\nthen save to \"~/out.txt\"\n",
    );
    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    assert_eq!(fs::read(home.join("out.txt")).unwrap(), b"saved\n");
    assert!(!cwd.join("~").exists());
}

#[test]
fn cd_expands_a_leading_tilde_slash() {
    let (_root, home, cwd) = setup();
    fs::write(home.join("sub").join("inner.txt"), b"nested").unwrap();
    let output = run(
        &cwd,
        &home,
        "run cd with path \"~/sub\"\nread \"inner.txt\"\n",
    );
    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    assert_eq!(output.stdout, b"nested");
}

#[test]
fn argv_strings_containing_tilde_stay_literal() {
    let (_root, home, cwd) = setup();
    let output = run(&cwd, &home, "run printf with message \"~/x.txt\"\n");
    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    assert_eq!(output.stdout, b"~/x.txt\n");
}

#[test]
fn dot_slash_tilde_names_a_literal_tilde_directory() {
    let (_root, home, cwd) = setup();
    fs::create_dir_all(cwd.join("~")).unwrap();
    fs::write(cwd.join("~").join("y.txt"), b"literal").unwrap();
    fs::write(home.join("y.txt"), b"home").unwrap();
    let output = run(&cwd, &home, "read \"./~/y.txt\"\n");
    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    assert_eq!(output.stdout, b"literal");
}
