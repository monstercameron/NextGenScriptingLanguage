// Command-line wiring for `execute fmt` (#345), for several `check` syntax errors in one
// pass (#470) and for `check --json=compact` (#464). Each test runs the built binary in a
// temporary directory with its default manifests.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

// Trailing spaces, CRLF line endings and blank runs: the formatter changes all of them.
const UNFORMATTED: &str = "accept greeting as text   \r\n\r\n\r\nuse value greeting   \r\n";
// Three independent syntax errors, on lines 2, 4 and 6 (the first line is valid).
const THREE_ERRORS: &str = concat!(
    "accept a as text\n",
    "run \"true\" |\n",
    "run \"true\" as x\n",
    "rnu \"printf\" with arguments [\"x\"]\n",
    "run \"true\" as y\n",
    "run \"true\" |\n",
);
// Only the first of THREE_ERRORS' errors: the same prefix, so the first diagnostic matches.
const ONE_ERROR: &str = "accept a as text\nrun \"true\" |\n";

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

fn write_program(directory: &Path, name: &str, text: &str) -> std::path::PathBuf {
    let path = directory.join(name);
    fs::write(&path, text).unwrap();
    path
}

/// The error diagnostics of a `check --json` result, in the order they were reported.
fn error_diagnostics(output: &Output) -> Vec<serde_json::Value> {
    let report: serde_json::Value = serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|error| panic!("{error}: {}", describe(output)));
    report["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|diagnostic| diagnostic["severity"] == "error")
        .cloned()
        .collect()
}

#[test]
fn fmt_rewrites_an_unformatted_file_and_is_a_no_op_the_second_time() {
    let directory = tempfile::tempdir().unwrap();
    let program = write_program(directory.path(), "f1.program", UNFORMATTED);
    let expected = flow::format::format_program(UNFORMATTED).unwrap();
    assert_ne!(expected, UNFORMATTED);

    let first = execute(directory.path(), &["fmt", "f1.program"]);
    assert!(first.status.success(), "{}", describe(&first));
    assert!(first.stdout.is_empty(), "{}", describe(&first));
    assert!(
        String::from_utf8_lossy(&first.stderr).contains("formatted"),
        "{}",
        describe(&first)
    );
    assert_eq!(fs::read_to_string(&program).unwrap(), expected);

    let second = execute(directory.path(), &["fmt", "f1.program"]);
    assert!(second.status.success(), "{}", describe(&second));
    assert!(second.stdout.is_empty(), "{}", describe(&second));
    assert!(second.stderr.is_empty(), "{}", describe(&second));
    assert_eq!(fs::read_to_string(&program).unwrap(), expected);
}

#[test]
fn fmt_check_exits_one_on_an_unformatted_file_and_writes_nothing() {
    let directory = tempfile::tempdir().unwrap();
    let program = write_program(directory.path(), "f2.program", UNFORMATTED);

    let output = execute(directory.path(), &["fmt", "--check", "f2.program"]);
    assert_eq!(output.status.code(), Some(1), "{}", describe(&output));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("f2.program"),
        "{}",
        describe(&output)
    );
    assert_eq!(fs::read(&program).unwrap(), UNFORMATTED.as_bytes());
}

#[test]
fn fmt_check_exits_zero_on_a_formatted_file_and_prints_nothing() {
    let directory = tempfile::tempdir().unwrap();
    let expected = flow::format::format_program(UNFORMATTED).unwrap();
    write_program(directory.path(), "f3.program", &expected);

    let output = execute(directory.path(), &["fmt", "--check", "f3.program"]);
    assert!(output.status.success(), "{}", describe(&output));
    assert!(output.stdout.is_empty(), "{}", describe(&output));
    assert!(output.stderr.is_empty(), "{}", describe(&output));
}

#[test]
fn fmt_refuses_a_file_that_does_not_parse_and_leaves_it_alone() {
    let directory = tempfile::tempdir().unwrap();
    let broken = "accept greeting as text\nrnu \"printf\" with arguments [\"x\"]\n";
    let program = write_program(directory.path(), "f4.program", broken);

    let output = execute(directory.path(), &["fmt", "f4.program"]);
    assert_eq!(output.status.code(), Some(1), "{}", describe(&output));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("does not parse"),
        "{}",
        describe(&output)
    );
    assert_eq!(fs::read(&program).unwrap(), broken.as_bytes());
}

#[test]
fn fmt_refuses_standard_input() {
    let directory = tempfile::tempdir().unwrap();
    let output = execute(directory.path(), &["fmt", "/dev/stdin"]);
    assert_eq!(output.status.code(), Some(1), "{}", describe(&output));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("standard input"),
        "{}",
        describe(&output)
    );
}

#[test]
fn check_reports_one_diagnostic_per_independent_syntax_error() {
    let directory = tempfile::tempdir().unwrap();
    write_program(directory.path(), "three.program", THREE_ERRORS);

    let output = execute(directory.path(), &["check", "--json", "three.program"]);
    assert_eq!(output.status.code(), Some(1), "{}", describe(&output));
    let errors = error_diagnostics(&output);
    let lines: Vec<Option<u64>> = errors
        .iter()
        .map(|diagnostic| diagnostic["span"]["line"].as_u64())
        .collect();
    assert_eq!(
        lines,
        [Some(2), Some(4), Some(6)],
        "{errors:#?}\n{}",
        describe(&output)
    );
    assert_eq!(errors.len(), 3, "{errors:#?}\n{}", describe(&output));
}

#[test]
fn check_first_diagnostic_matches_a_file_with_only_the_first_error() {
    let three_dir = tempfile::tempdir().unwrap();
    write_program(three_dir.path(), "p.program", THREE_ERRORS);
    let one_dir = tempfile::tempdir().unwrap();
    write_program(one_dir.path(), "p.program", ONE_ERROR);

    let three = error_diagnostics(&execute(
        three_dir.path(),
        &["check", "--json", "p.program"],
    ));
    let one = error_diagnostics(&execute(one_dir.path(), &["check", "--json", "p.program"]));
    assert_eq!(one.len(), 1, "{one:?}");
    let first = &three[0];
    assert_eq!(first["code"], one[0]["code"]);
    assert_eq!(first["message"], one[0]["message"]);
    assert_eq!(first["span"]["line"], one[0]["span"]["line"]);
    assert_eq!(first["span"]["column"], one[0]["span"]["column"]);
    assert_eq!(first["span"]["start_byte"], one[0]["span"]["start_byte"]);
    assert_eq!(first["span"]["end_byte"], one[0]["span"]["end_byte"]);
}

#[test]
fn check_json_compact_is_one_line_with_the_same_codes_as_json() {
    let directory = tempfile::tempdir().unwrap();
    write_program(directory.path(), "c.program", THREE_ERRORS);

    let full = execute(directory.path(), &["check", "--json", "c.program"]);
    let compact = execute(directory.path(), &["check", "--json=compact", "c.program"]);
    assert_eq!(compact.status.code(), Some(1), "{}", describe(&compact));

    let text = String::from_utf8(compact.stdout.clone()).unwrap();
    let body = text.strip_suffix('\n').unwrap_or(&text);
    assert!(!body.contains('\n'), "{text:?}");
    let parsed: serde_json::Value = serde_json::from_str(body).unwrap();
    assert_eq!(parsed["protocol"], "execute.check/1");

    let codes = |output: &Output| -> Vec<String> {
        error_diagnostics(output)
            .iter()
            .map(|diagnostic| diagnostic["code"].as_str().unwrap().to_owned())
            .collect()
    };
    assert_eq!(codes(&compact), codes(&full), "{}", describe(&compact));
    assert_eq!(codes(&compact).len(), 3, "{}", describe(&compact));
}

#[test]
fn check_json_compact_cannot_be_combined_with_json() {
    let directory = tempfile::tempdir().unwrap();
    write_program(directory.path(), "d.program", THREE_ERRORS);
    let output = execute(
        directory.path(),
        &["check", "--json", "--json=compact", "d.program"],
    );
    assert!(!output.status.success(), "{}", describe(&output));
    assert!(output.stdout.is_empty(), "{}", describe(&output));
}
