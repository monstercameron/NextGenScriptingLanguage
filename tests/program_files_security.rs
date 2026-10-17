//! `run program file "PATH"` is a host read. It is authorized through the capability
//! policy, capped at 1 MiB, limited to regular files, and its errors never quote the
//! file's text. Each test reproduces a probe from the security review.
use serde_json::Value;
use std::fs;
use std::path::Path;
use std::process::{Command, Output};

const SECRET: &str = "SECRET-TOKEN-123";

fn write(root: &Path, relative: &str, text: &str) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

/// A capability policy file. `read` names the directories the policy may read.
fn policy(root: &Path, read: Option<&str>) -> String {
    let body = match read {
        Some(directory) => format!(
            r#"{{"version":1,"applets":{{"text.concat":{{"capabilities":["filesystem.read","filesystem.write"],"filesystem":{{"read":["{directory}"],"write":["{directory}"]}}}}}}}}"#
        ),
        None => r#"{"version":1,"applets":{}}"#.to_owned(),
    };
    write(root, "policy.json", &body);
    "policy.json".to_owned()
}

fn execute(root: &Path, arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_execute"))
        .args(arguments)
        .current_dir(root)
        .output()
        .unwrap()
}

fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn run(root: &Path, policy_file: &str) -> Output {
    execute(
        root,
        &["run", "main.program", "--capability-policy", policy_file],
    )
}

fn check(root: &Path, policy_file: &str) -> Output {
    execute(
        root,
        &[
            "check",
            "main.program",
            "--json",
            "--capability-policy",
            policy_file,
        ],
    )
}

#[test]
fn a_policy_that_denies_reads_refuses_the_call_and_nothing_echoes_the_file() {
    // The probe: the policy grants no reads, and the file's first line is a secret.
    let root = tempfile::tempdir().unwrap();
    write(
        root.path(),
        "secret.txt",
        &format!("{SECRET} not a valid line\n"),
    );
    write(
        root.path(),
        "main.program",
        "run program file \"secret.txt\"\n",
    );
    let policy_file = policy(root.path(), None);

    let output = run(root.path(), &policy_file);
    assert!(!output.status.success());
    let shown = text(&output);
    assert!(
        shown.contains("not allowed by the capability policy"),
        "{shown}"
    );
    assert!(shown.contains("secret.txt"), "{shown}");
    assert!(!shown.contains(SECRET), "{shown}");

    let report = check(root.path(), &policy_file);
    assert!(!String::from_utf8_lossy(&report.stdout).contains(SECRET));
    assert!(!String::from_utf8_lossy(&report.stderr).contains(SECRET));
}

#[test]
fn an_undeclared_applet_in_a_permitted_file_is_reported_by_line_without_its_text() {
    // The read is granted, so the file loads, and its line names no declared applet.
    // The error names the file and line and never the line's text.
    let root = tempfile::tempdir().unwrap();
    write(
        root.path(),
        "secret.txt",
        &format!("{SECRET} not a valid line\n"),
    );
    write(
        root.path(),
        "main.program",
        "run program file \"secret.txt\"\n",
    );
    let policy_file = policy(root.path(), Some("."));

    let output = run(root.path(), &policy_file);
    assert!(!output.status.success());
    let shown = text(&output);
    assert!(
        shown.contains("calls an applet that is not declared"),
        "{shown}"
    );
    assert!(shown.contains("line 1"), "{shown}");
    assert!(!shown.contains(SECRET), "{shown}");

    let report = check(root.path(), &policy_file);
    let json: Value = serde_json::from_slice(&report.stdout).unwrap();
    assert_eq!(json["ok"], false);
    assert!(!String::from_utf8_lossy(&report.stdout).contains(SECRET));
}

#[test]
fn invalid_program_text_is_reported_by_line_without_quoting_it() {
    let root = tempfile::tempdir().unwrap();
    write(
        root.path(),
        "secret.txt",
        &format!("accept {SECRET} as bogus\n"),
    );
    write(
        root.path(),
        "main.program",
        "run program file \"secret.txt\"\n",
    );
    let policy_file = policy(root.path(), Some("."));

    let output = run(root.path(), &policy_file);
    assert!(!output.status.success());
    let shown = text(&output);
    assert!(shown.contains("is not valid program text"), "{shown}");
    assert!(shown.contains("line 1"), "{shown}");
    assert!(!shown.contains(SECRET), "{shown}");
}

#[test]
fn an_oversized_program_file_is_refused_without_reading_its_text() {
    let root = tempfile::tempdir().unwrap();
    let big = "A".repeat(2 * 1024 * 1024);
    write(root.path(), "big.txt", &big);
    write(
        root.path(),
        "main.program",
        "run program file \"big.txt\"\n",
    );
    let policy_file = policy(root.path(), Some("."));

    let output = run(root.path(), &policy_file);
    assert!(!output.status.success());
    let shown = text(&output);
    assert!(shown.contains("larger than 1 MiB"), "{shown}");
    assert!(shown.contains("big.txt"), "{shown}");
    assert!(!shown.contains("AAAAAAAAAAAAAAAAAAAAAAAA"), "{shown}");
}

#[test]
fn a_directory_is_refused_as_not_a_regular_file() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join("lib")).unwrap();
    write(root.path(), "main.program", "run program file \"lib\"\n");
    let policy_file = policy(root.path(), Some("."));

    let output = run(root.path(), &policy_file);
    assert!(!output.status.success());
    assert!(
        text(&output).contains("is not a regular file"),
        "{}",
        text(&output)
    );
}

#[test]
fn a_permitted_read_runs_the_called_file() {
    // The sanity case: with a read grant, the call loads and runs as before.
    let root = tempfile::tempdir().unwrap();
    write(
        root.path(),
        "lib/echo.program",
        "accept who as text\nuse value who\nthen save to \"out.txt\"\n",
    );
    write(
        root.path(),
        "main.program",
        "run program file \"lib/echo.program\" with who \"hello\" as echoed\n",
    );
    let policy_file = policy(root.path(), Some("."));

    let output = run(root.path(), &policy_file);
    assert!(output.status.success(), "{}", text(&output));
    assert_eq!(
        fs::read_to_string(root.path().join("out.txt")).unwrap(),
        "hello"
    );
}

#[test]
fn a_nested_file_call_is_shown_relative_to_the_file_that_makes_it() {
    // main calls lib/a.program, which calls sub/b.program inside lib. The inner call is
    // shown relative to lib, and the outer call relative to the main program.
    let root = tempfile::tempdir().unwrap();
    write(
        root.path(),
        "lib/sub/b.program",
        "accept who as text\nuse value who\n",
    );
    write(
        root.path(),
        "lib/a.program",
        "run program file \"sub/b.program\" with who \"x\"\n",
    );
    write(
        root.path(),
        "main.program",
        "run program file \"lib/a.program\"\n",
    );
    let policy_file = policy(root.path(), Some("."));
    let output = execute(
        root.path(),
        &[
            "explain",
            "main.program",
            "--json",
            "--capability-policy",
            &policy_file,
        ],
    );
    assert!(output.status.success(), "{}", text(&output));
    let plan: Value = serde_json::from_slice(&output.stdout).unwrap();
    let applets: Vec<&str> = plan["steps"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|step| step["kind"] == "call_program")
        .map(|step| step["applet"].as_str().unwrap())
        .collect();
    assert_eq!(applets, vec!["file:lib/a.program", "file:sub/b.program"]);
}
