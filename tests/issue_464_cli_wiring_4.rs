// Command-line wiring for `execute check` on standard input (#464), for several files in one
// `check` (#464) and for `execute migrate scan` (#402). Each test runs the built binary in a
// temporary directory with its default manifests.

use serde_json::Value;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

// A syntax error on line 2 (the first line is valid).
const ONE_ERROR: &str = "accept a as text\nrun \"true\" |\n";
const VALID: &str = "run \"true\"\n";

fn describe(output: &Output) -> String {
    format!(
        "status {:?} stdout {:?} stderr {:?}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn execute(directory: &Path, arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(directory)
        .args(arguments)
        .output()
        .unwrap()
}

fn execute_with_stdin(directory: &Path, arguments: &[&str], text: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(directory)
        .args(arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(text.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

/// The (code, line) of each error diagnostic of one check report, in report order.
fn error_sites(report: &Value) -> Vec<(String, u64)> {
    report["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|diagnostic| diagnostic["severity"] == "error")
        .map(|diagnostic| {
            (
                diagnostic["code"].as_str().unwrap_or_default().to_owned(),
                diagnostic["span"]["line"].as_u64().unwrap_or(0),
            )
        })
        .collect()
}

fn parse_json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|error| panic!("{error}: {}", describe(output)))
}

#[test]
fn stdin_check_reports_the_same_diagnostics_as_the_file() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(directory.path().join("bad.program"), ONE_ERROR).unwrap();
    let from_file = execute(directory.path(), &["check", "bad.program", "--json"]);
    let from_stdin = execute_with_stdin(
        directory.path(),
        &["check", "--stdin", "--path", "bad.program", "--json"],
        ONE_ERROR,
    );
    let from_dash = execute_with_stdin(
        directory.path(),
        &["check", "-", "--path", "bad.program", "--json"],
        ONE_ERROR,
    );
    let file_sites = error_sites(&parse_json(&from_file));
    assert!(!file_sites.is_empty(), "{}", describe(&from_file));
    assert_eq!(
        error_sites(&parse_json(&from_stdin)),
        file_sites,
        "{}",
        describe(&from_stdin)
    );
    assert_eq!(
        error_sites(&parse_json(&from_dash)),
        file_sites,
        "{}",
        describe(&from_dash)
    );
    assert_eq!(from_file.status.code(), from_stdin.status.code());
}

#[test]
fn stdin_check_of_a_valid_program_succeeds_and_names_standard_input_by_default() {
    let directory = tempfile::tempdir().unwrap();
    let output = execute_with_stdin(directory.path(), &["check", "--stdin", "--json"], VALID);
    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
    let report = parse_json(&output);
    assert_eq!(report["ok"], true, "{}", describe(&output));
    assert_eq!(report["path"], "<stdin>");
}

#[test]
fn stdin_check_refuses_fix_and_oversized_input() {
    let directory = tempfile::tempdir().unwrap();
    let fixed = execute_with_stdin(directory.path(), &["check", "--stdin", "--fix"], VALID);
    assert!(!fixed.status.success(), "{}", describe(&fixed));
    assert!(
        String::from_utf8_lossy(&fixed.stderr).contains("standard input"),
        "{}",
        describe(&fixed)
    );
    // Larger than the one mebibyte a called program file may be.
    let oversized = VALID.repeat(100_000);
    let output = execute_with_stdin(directory.path(), &["check", "--stdin"], &oversized);
    assert!(!output.status.success(), "{}", describe(&output));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("larger than 1 MiB"),
        "{}",
        describe(&output)
    );
}

#[test]
fn several_files_each_report_their_error_in_argument_order() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(directory.path().join("a.program"), ONE_ERROR).unwrap();
    fs::write(directory.path().join("b.program"), ONE_ERROR).unwrap();

    let output = execute(
        directory.path(),
        &["check", "a.program", "b.program", "--json"],
    );
    assert_eq!(output.status.code(), Some(1), "{}", describe(&output));
    let reports = parse_json(&output);
    let reports = reports.as_array().expect("an array of per-file documents");
    assert_eq!(reports.len(), 2, "{}", describe(&output));
    assert_eq!(reports[0]["path"], "a.program");
    assert_eq!(reports[1]["path"], "b.program");
    for report in reports {
        assert!(!error_sites(report).is_empty(), "{report}");
    }

    let compact = execute(
        directory.path(),
        &["check", "a.program", "b.program", "--json=compact"],
    );
    assert_eq!(compact.status.code(), Some(1), "{}", describe(&compact));
    let lines: Vec<&str> = std::str::from_utf8(&compact.stdout)
        .unwrap()
        .lines()
        .collect();
    assert_eq!(lines.len(), 2, "{}", describe(&compact));
    let first: Value = serde_json::from_str(lines[0]).unwrap();
    let second: Value = serde_json::from_str(lines[1]).unwrap();
    assert_eq!(first["path"], "a.program");
    assert_eq!(second["path"], "b.program");
}

#[test]
fn a_single_file_keeps_the_object_document() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(directory.path().join("ok.program"), VALID).unwrap();
    let output = execute(directory.path(), &["check", "ok.program", "--json"]);
    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
    let report = parse_json(&output);
    assert!(report.is_object(), "{}", describe(&output));
    assert_eq!(report["protocol"], "execute.check/1");
    assert_eq!(report["validation"], "static");
    assert_eq!(report["ok"], true);
    for key in ["path", "path_bytes", "diagnostics"] {
        assert!(report.get(key).is_some(), "missing {key}: {report}");
    }
}

#[test]
fn migrate_scan_reports_the_home_it_was_given() {
    let directory = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let home_text = home.path().to_str().unwrap();
    let output = execute(
        directory.path(),
        &["migrate", "scan", "--home", home_text, "--json"],
    );
    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
    let report = parse_json(&output);
    assert_eq!(report["protocol"], "execute.migrate-scan/1");
}
