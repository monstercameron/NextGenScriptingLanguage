//! Regression tests for the security fixes of the audit (#87). Each test names the
//! behaviour it guards, so a change that reopens the hole fails here first.
use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn manifests() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests")
}

/// Run `source` as a program with the repository manifests and no capability policy.
fn run_source(source: &str) -> Output {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("workflow.program");
    fs::write(&path, source).unwrap();
    Command::new(env!("CARGO_BIN_EXE_execute"))
        .arg("run")
        .arg(&path)
        .arg("--manifest-dir")
        .arg(manifests())
        .current_dir(directory.path())
        .output()
        .unwrap()
}

#[test]
fn loader_variables_cannot_be_set_from_a_program_without_a_policy() {
    // The dynamic loader reads these before the program starts, so a binding that could
    // set them would run attacker-chosen code in every applet.
    for name in ["LD_PRELOAD", "LD_LIBRARY_PATH", "DYLD_INSERT_LIBRARIES"] {
        let source =
            format!("run printf with message \"x\" and environment {{\"{name}\": \"/tmp/x\"}}\n");
        let output = run_source(&source);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{name} was accepted: {output:?}");
        assert!(stderr.contains("dynamic loader"), "{name}: {stderr}");
        assert!(output.stdout.is_empty(), "{name} ran before it was refused");
    }
}

#[test]
fn ordinary_environment_names_still_pass_without_a_policy() {
    let output =
        run_source("run printf with message \"x\" and environment {\"SAFE_NAME\": \"1\"}\n");
    assert!(output.status.success(), "{output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout), "x\n");
}
