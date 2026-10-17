//! Native executables are resolved before a fresh finite run starts any command:
//! every missing one is reported together, and no earlier step runs.
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn manifests() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests")
}

fn run(script: &str) -> Output {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("workflow.program");
    fs::write(&path, script).unwrap();
    Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(directory.path())
        .arg("run")
        .arg(&path)
        .arg("--manifest-dir")
        .arg(manifests())
        .output()
        .unwrap()
}

#[test]
fn every_missing_executable_is_reported_before_any_step_runs() {
    let output = run("run printf with message \"effect\"
run \"no-such-tool-alpha\" with arguments []
if \"a\" is \"b\"
  run \"no-such-tool-beta\" with arguments []
");
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("line 2"), "{stderr}");
    assert!(stderr.contains("no-such-tool-alpha"), "{stderr}");
    assert!(stderr.contains("line 4"), "{stderr}");
    assert!(stderr.contains("no-such-tool-beta"), "{stderr}");
    assert!(output.stdout.is_empty(), "{:?}", output.stdout);
}

#[test]
fn a_program_with_available_executables_still_runs() {
    let output = run("run printf with message \"ok\"\n");
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(String::from_utf8_lossy(&output.stdout), "ok\n");
}
