// Command-line wiring for `check --fix` (#473), `grammar --format gbnf` (#363) and the
// `shim` line of the help text (#400). Each test runs the built binary in a temporary
// directory.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

// `use value` needs no applet, so the check runs without any manifest.
const UNDEFINED: &str = "accept greeting as text\nuse value greting\n";
const FIXED: &str = "accept greeting as text\nuse value greeting\n";

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

#[test]
fn check_fix_rewrites_an_undefined_name_and_exits_zero() {
    let directory = tempfile::tempdir().unwrap();
    let program = directory.path().join("a1.program");
    fs::write(&program, UNDEFINED).unwrap();
    let output = execute(directory.path(), &["check", "a1.program", "--fix"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{}", describe(&output));
    assert_eq!(fs::read_to_string(&program).unwrap(), FIXED);
    assert!(
        stderr.contains("applied 1 machine fix(es)"),
        "{}",
        describe(&output)
    );
}

#[test]
fn check_fix_leaves_a_program_without_fixes_byte_for_byte() {
    let directory = tempfile::tempdir().unwrap();
    let program = directory.path().join("clean.program");
    fs::write(&program, FIXED).unwrap();
    let output = execute(directory.path(), &["check", "clean.program", "--fix"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{}", describe(&output));
    assert_eq!(fs::read_to_string(&program).unwrap(), FIXED);
    assert!(
        stderr.contains("applied 0 machine fix(es)"),
        "{}",
        describe(&output)
    );
}

#[test]
fn check_fix_refuses_to_read_the_program_from_standard_input() {
    let directory = tempfile::tempdir().unwrap();
    let output = execute(directory.path(), &["check", "/dev/stdin", "--fix"]);
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("standard input"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn grammar_prints_the_gbnf_statement_grammar() {
    let directory = tempfile::tempdir().unwrap();
    let output = execute(directory.path(), &["grammar", "--format", "gbnf"]);
    assert!(output.status.success());
    assert_eq!(
        output.stdout,
        flow::parser::gbnf_grammar().as_bytes(),
        "stdout must be the library grammar unchanged"
    );
}

#[test]
fn grammar_rejects_any_other_format_and_names_gbnf() {
    let directory = tempfile::tempdir().unwrap();
    let output = execute(directory.path(), &["grammar", "--format", "ebnf"]);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("accepts only gbnf"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn help_lists_the_shim_subcommand() {
    let directory = tempfile::tempdir().unwrap();
    let output = execute(directory.path(), &["--help"]);
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("execute shim PROGRAM [--name NAME] [--out DIR] [--force]"),
        "{stdout}"
    );
}
