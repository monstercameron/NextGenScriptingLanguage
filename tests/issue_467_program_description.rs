//! Issue #467: `execute help FILE` shows the program's `# execute-description:`
//! header (from the leading comment block) above the usage line, and in the
//! `description` field of the JSON object. The header adds no grammar.

use serde_json::Value;
use std::path::Path;
use std::process::{Command, Output};

const DESCRIPTION: &str = "Switch the desktop theme and reload the compositor";

fn program() -> String {
    format!(
        "# execute-description: {DESCRIPTION}\n\
         accept theme_name as text\n\
         run printf with message theme_name\n"
    )
}

fn help(directory: &Path, arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_execute"))
        .arg("help")
        .args(arguments)
        .current_dir(directory)
        .env_remove("EXECUTE_MODELS_OFFLINE")
        .output()
        .expect("start execute")
}

fn write_program(directory: &Path, source: &str) {
    std::fs::write(directory.join("theme.program"), source).unwrap();
}

#[test]
fn text_help_shows_the_description_above_the_usage_line() {
    let directory = tempfile::tempdir().unwrap();
    write_program(directory.path(), &program());
    let output = help(directory.path(), &["theme.program"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(
        text.starts_with(&format!("{DESCRIPTION}\n\nexecute theme.program ")),
        "{text}"
    );
}

#[test]
fn json_help_carries_the_description_field() {
    let directory = tempfile::tempdir().unwrap();
    write_program(directory.path(), &program());
    let output = help(directory.path(), &["theme.program", "--json"]);
    assert!(output.status.success());
    let object: Value = serde_json::from_slice(&output.stdout).expect("one JSON object");
    assert_eq!(object["description"], DESCRIPTION);
    assert_eq!(object["inputs"][0]["name"], "theme_name");
}

#[test]
fn a_program_without_the_header_has_no_description() {
    let directory = tempfile::tempdir().unwrap();
    write_program(
        directory.path(),
        "accept theme_name as text\nrun printf with message theme_name\n",
    );
    let output = help(directory.path(), &["theme.program", "--json"]);
    assert!(output.status.success());
    let object: Value = serde_json::from_slice(&output.stdout).expect("one JSON object");
    assert!(object["description"].is_null(), "{object}");
    let text = help(directory.path(), &["theme.program"]);
    assert!(String::from_utf8_lossy(&text.stdout).starts_with("execute theme.program "));
}

#[test]
fn a_repeated_header_fails_help_with_no_output() {
    let directory = tempfile::tempdir().unwrap();
    write_program(
        directory.path(),
        &format!(
            "# execute-description: one\n# execute-description: two\n{}",
            "accept theme_name as text\n"
        ),
    );
    let output = help(directory.path(), &["theme.program"]);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("more than one"));
}

#[test]
fn an_empty_header_fails_help_with_no_output() {
    let directory = tempfile::tempdir().unwrap();
    write_program(
        directory.path(),
        "# execute-description:\naccept theme_name as text\n",
    );
    let output = help(directory.path(), &["theme.program"]);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
}
