//! Staged helper binaries pass the trusted-executable gate while the gate
//! itself still refuses genuinely multi-linked executables.
//!
//! `cargo build --release` hardlinks `target/release/<bin>` to its
//! `deps/<bin>-<hash>` twin, and the shared target cache can restore that
//! aliasing, so trust-gated tests must not point manifests at the
//! `CARGO_BIN_EXE_*` originals. The shared staging helper copies each binary
//! into a privately owned temporary directory (a fresh copy always has exactly
//! one link), which mirrors an installed layout where helpers are real files.
//!
//! These tests pin both sides of that contract: the staged copy is an owned
//! single-link duplicate that runs through the real gate, and adding a second
//! link to an identical copy is still refused with the link error instead of
//! launching anything. The hardlink fixture is built from a private copy so
//! parallel tests sharing the staged original never observe a second link.
#![cfg(unix)]

use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;
use std::process::{Command, Output};

#[path = "common/staged_helpers.rs"]
mod staged_helpers;

fn run_with_manifests(directory: &Path, program: &str, manifest: &serde_json::Value) -> Output {
    let manifests = directory.join("manifests");
    std::fs::create_dir_all(&manifests).unwrap();
    std::fs::write(
        manifests.join("staged.fixture.json"),
        serde_json::to_string(manifest).unwrap(),
    )
    .unwrap();
    std::fs::write(directory.join("staged.program"), program).unwrap();
    Command::new(env!("CARGO_BIN_EXE_execute"))
        .arg("staged.program")
        .arg("--manifest-dir")
        .arg(&manifests)
        .current_dir(directory)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .expect("start execute")
}

fn staged_manifest(binary: &Path) -> serde_json::Value {
    serde_json::json!({
        "name": "staged.fixture",
        "binary": binary,
        "arguments": [{"literal": "write"}, {"literal": "hello"}],
    })
}

#[test]
fn staged_native_fixture_is_an_owned_single_link_duplicate() {
    let staged = staged_helpers::native_fixture();
    let metadata = std::fs::symlink_metadata(staged).unwrap();
    assert_eq!(
        metadata.nlink(),
        1,
        "the staged helper must have exactly one link, like an installed file"
    );
    assert_eq!(
        metadata.uid(),
        euid(),
        "the staged helper must stay owned by the current user"
    );
    assert_eq!(
        metadata.permissions().mode() & 0o777,
        0o755,
        "the staged helper must be executable without group/world writes"
    );
    let original = std::fs::read(env!("CARGO_BIN_EXE_native-fixture")).unwrap();
    assert_eq!(
        std::fs::read(staged).unwrap(),
        original,
        "staging must copy the helper byte for byte"
    );
}

fn euid() -> u32 {
    // SAFETY: geteuid takes no arguments and cannot fail.
    unsafe { libc::geteuid() }
}

#[test]
fn staged_native_fixture_passes_the_trusted_executable_gate() {
    let directory = tempfile::tempdir().unwrap();
    let output = run_with_manifests(
        directory.path(),
        "run staged.fixture\n",
        &staged_manifest(staged_helpers::native_fixture()),
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"hello");
}

#[test]
fn a_hardlinked_copy_of_a_trusted_executable_is_still_refused() {
    let directory = tempfile::tempdir().unwrap();
    // Copy first so the extra link never touches the staged original that
    // parallel tests in this process are launching.
    let alone = directory.path().join("alone-fixture");
    std::fs::copy(staged_helpers::native_fixture(), &alone).unwrap();
    let alias = directory.path().join("alias-fixture");
    std::fs::hard_link(&alone, &alias).unwrap();

    let output = run_with_manifests(
        directory.path(),
        "run staged.fixture\n",
        &staged_manifest(&alias),
    );
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(
        error.contains("hardlink"),
        "a multi-linked executable must be refused with the link error: {error}"
    );
    assert!(
        !error.contains("\"event\":\"spawn\""),
        "an untrusted input must never launch a native stage: {error}"
    );
}
