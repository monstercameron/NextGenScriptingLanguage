//! `execute N` as the first statement declares the language version. Only the implemented
//! version is accepted; unmarked programs are that version.
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

fn manifests() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests")
}

fn run_script(directory: &Path, command: &str, script: &str, extra: &[&str]) -> Output {
    let path = directory.join("workflow.program");
    fs::write(&path, script).unwrap();
    Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(directory)
        .arg(command)
        .arg(&path)
        .arg("--manifest-dir")
        .arg(manifests())
        .args(extra)
        .output()
        .unwrap()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn marker_for_the_implemented_version_runs_normally() {
    let directory = tempfile::tempdir().unwrap();
    let output = run_script(
        directory.path(),
        "run",
        "execute 1\nuse value \"abc\"\npipe to wc with flags [\"-c\"]\n",
        &[],
    );
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(stdout(&output).trim(), "3");
}

#[test]
fn marker_after_comments_is_still_the_first_statement() {
    let directory = tempfile::tempdir().unwrap();
    let output = run_script(
        directory.path(),
        "check",
        "# header comment\n\nexecute 1\nuse value \"abc\"\n",
        &[],
    );
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
}

#[test]
fn unmarked_programs_are_the_implemented_version() {
    let directory = tempfile::tempdir().unwrap();
    let output = run_script(
        directory.path(),
        "run",
        "use value \"abc\"\npipe to wc with flags [\"-c\"]\n",
        &[],
    );
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(stdout(&output).trim(), "3");
}

#[test]
fn other_declared_versions_are_rejected_with_an_upgrade_message() {
    for version in ["0", "2", "99"] {
        let directory = tempfile::tempdir().unwrap();
        let script = format!("execute {version}\nuse value \"abc\"\n");
        let output = run_script(directory.path(), "check", &script, &[]);
        assert_ne!(output.status.code(), Some(0), "version {version}");
        let message = stderr(&output);
        assert!(
            message.contains(&format!("declares language version {version}")),
            "{message}"
        );
        assert!(message.contains("implements version 1"), "{message}");
    }
}

#[test]
fn a_marker_that_is_not_the_first_statement_is_not_a_declaration() {
    let directory = tempfile::tempdir().unwrap();
    let output = run_script(
        directory.path(),
        "check",
        "use value \"abc\"\nexecute 2\n",
        &[],
    );
    assert!(
        !stderr(&output).contains("language version"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn inspect_and_explain_report_the_language_version() {
    let directory = tempfile::tempdir().unwrap();
    let script = "execute 1\nuse value \"abc\"\npipe to wc with flags [\"-c\"]\n";
    let inspect = run_script(directory.path(), "inspect", script, &[]);
    assert_eq!(inspect.status.code(), Some(0), "{}", stderr(&inspect));
    let report: serde_json::Value = serde_json::from_slice(&inspect.stdout).unwrap();
    assert_eq!(report["language_version"], 1);

    let explain = run_script(directory.path(), "explain", script, &["--json"]);
    assert_eq!(explain.status.code(), Some(0), "{}", stderr(&explain));
    let plan: serde_json::Value = serde_json::from_slice(&explain.stdout).unwrap();
    assert_eq!(plan["language_version"], 1);
}
