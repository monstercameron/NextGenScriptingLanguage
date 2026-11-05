//! Runs the language feature suite through the real binary.
//!
//! Each `*.test.program` is executed by `execute test` in a fresh temporary working
//! directory and checked against its sibling `*.test.json`. Portable tests live in
//! `tests/language/`. Tests that need an operating-system executable live in
//! `tests/language-platform/unix/` or `tests/language-platform/windows/`, and run only on
//! that platform.

use std::path::{Path, PathBuf};
use std::process::Command;

fn suite_dir(relative: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join(relative)
}

fn run_suite(dir: &Path) {
    let output = Command::new(env!("CARGO_BIN_EXE_execute"))
        .arg("test")
        .arg(dir)
        .output()
        .expect("start execute test");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "language suite failed in {}:\n{stdout}\n{stderr}",
        dir.display()
    );
}

#[test]
fn portable_language_features_behave_the_same_on_every_platform() {
    run_suite(&suite_dir("language"));
}

#[cfg(unix)]
#[test]
fn unix_language_features_with_posix_commands() {
    run_suite(&suite_dir("language-platform/unix"));
}

#[cfg(windows)]
#[test]
fn windows_language_features_with_windows_commands() {
    run_suite(&suite_dir("language-platform/windows"));
}
