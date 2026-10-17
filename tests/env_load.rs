//! `env.load` through a real program: a host read feeds the in-process parser, and
//! nothing from the file is ever run.
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn manifests() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests")
}

fn execute(root: &Path, arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_execute"))
        .args(arguments)
        .arg("--manifest-dir")
        .arg(manifests())
        .current_dir(root)
        .output()
        .unwrap()
}

const PROGRAM: &str = "read \"os-release.env\"\nthen env.load\nthen save to \"out.json\"\n";

#[test]
fn an_environment_file_loads_into_a_json_object_without_being_run() {
    let root = tempfile::tempdir().unwrap();
    fs::write(
        root.path().join("os-release.env"),
        "# release\nNAME=\"Arch Linux\"\nID=arch\nexport PRETTY='Arch $HOME literal'\n",
    )
    .unwrap();
    fs::write(root.path().join("job.program"), PROGRAM).unwrap();

    let output = execute(root.path(), &["run", "job.program"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let loaded: Value =
        serde_json::from_str(&fs::read_to_string(root.path().join("out.json")).unwrap()).unwrap();
    assert_eq!(loaded["NAME"], "Arch Linux");
    assert_eq!(loaded["ID"], "arch");
    assert_eq!(loaded["PRETTY"], "Arch $HOME literal");
}

#[test]
fn a_file_with_command_substitution_is_refused_before_anything_is_written() {
    let root = tempfile::tempdir().unwrap();
    fs::write(
        root.path().join("os-release.env"),
        "ID=arch\nVERSION=$(touch pwned)\n",
    )
    .unwrap();
    fs::write(root.path().join("job.program"), PROGRAM).unwrap();

    let output = execute(root.path(), &["run", "job.program"]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(stderr.contains("line 2"), "{stderr}");
    assert!(!root.path().join("out.json").exists());
    assert!(
        !root.path().join("pwned").exists(),
        "the file must never run"
    );
}

#[test]
fn explain_shows_the_read_that_feeds_the_load() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("os-release.env"), "ID=arch\n").unwrap();
    fs::write(root.path().join("job.program"), PROGRAM).unwrap();

    let output = execute(root.path(), &["explain", "job.program", "--json"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let plan: Value = serde_json::from_slice(&output.stdout).unwrap();
    let steps = plan["steps"].as_array().unwrap();
    assert_eq!(steps[0]["kind"], "read");
    assert_eq!(steps[0]["reads"], true);
    assert_eq!(steps[1]["applet"], "env.load");
}
