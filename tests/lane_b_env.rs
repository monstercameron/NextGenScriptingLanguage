//! Per-command environment, the existing tested equivalent: `FOO=bar cmd` is
//! `run "env" with arguments ["FOO=bar", "cmd", ...]`, and `env -u NAME cmd` drops
//! a variable for that command only.
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
        .env("LANE_B_UNSET", "present")
        .arg("run")
        .arg(&path)
        .arg("--manifest-dir")
        .arg(manifests())
        .output()
        .unwrap()
}

#[test]
fn env_assignment_applies_to_one_command() {
    let output =
        run("run \"env\" with arguments [\"LANE_B_VALUE=bar\", \"printenv\", \"LANE_B_VALUE\"]\n");
    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    assert_eq!(String::from_utf8_lossy(&output.stdout), "bar\n");
}

#[test]
fn env_unset_removes_a_variable_for_one_command() {
    // The Execute process has LANE_B_UNSET=present. Without `-u` printenv would
    // print it; with `-u` the variable is gone and printenv exits nonzero.
    let output = run(
        "run \"env\" with arguments [\"-u\", \"LANE_B_UNSET\", \"printenv\", \"LANE_B_UNSET\"]\n",
    );
    assert_ne!(output.status.code(), Some(0));
    assert!(!String::from_utf8_lossy(&output.stdout).contains("present"));
}
