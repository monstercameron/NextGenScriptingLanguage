//! `execute help FILE [--json]`: usage generated from accept declarations. The
//! command parses the program and runs nothing.

use serde_json::Value;
use std::path::Path;
use std::process::{Command, Output};

const PROGRAM: &str = "accept name as text\n\
accept count as integer with default 3\n\
accept live as boolean with default false\n\
accept items as list of text\n\
run printf with message name\n\
then save to \"wrote.txt\"\n";

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
    std::fs::write(directory.join("tool.program"), source).unwrap();
}

#[test]
fn text_help_lists_every_input_with_its_type_and_requirement() {
    let directory = tempfile::tempdir().unwrap();
    write_program(directory.path(), PROGRAM);
    let output = help(directory.path(), &["tool.program"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("  name  text  required"), "{text}");
    assert!(text.contains("  count  integer  default 3"), "{text}");
    assert!(text.contains("  live  boolean  default false"), "{text}");
    assert!(text.contains("  items  list of text  required"), "{text}");
}

#[test]
fn help_never_runs_the_program() {
    let directory = tempfile::tempdir().unwrap();
    write_program(directory.path(), PROGRAM);
    let output = help(directory.path(), &["tool.program", "--json"]);
    assert!(output.status.success());
    assert!(
        !directory.path().join("wrote.txt").exists(),
        "help must not run the program's save step"
    );
}

#[test]
fn json_help_has_stable_fields_per_input() {
    let directory = tempfile::tempdir().unwrap();
    write_program(directory.path(), PROGRAM);
    let output = help(directory.path(), &["tool.program", "--json"]);
    let object: Value = serde_json::from_slice(&output.stdout).expect("one JSON object");
    assert_eq!(object["protocol"], "execute.help/1");
    assert!(object["usage"]
        .as_str()
        .unwrap()
        .contains("--input NAME=VALUE"));
    let inputs = object["inputs"].as_array().unwrap();
    assert_eq!(inputs.len(), 4, "{object}");
    assert_eq!(inputs[0]["name"], "name");
    assert_eq!(inputs[0]["type"], "text");
    assert_eq!(inputs[0]["required"], true);
    assert!(inputs[0]["default"].is_null());
    assert_eq!(inputs[1]["type"], "integer");
    assert_eq!(inputs[1]["required"], false);
    assert_eq!(inputs[1]["default"], 3);
    assert_eq!(inputs[2]["type"], "boolean");
    assert_eq!(inputs[2]["default"], false);
    assert_eq!(inputs[3]["type"], "list of text");
    for input in inputs {
        assert!(
            input["description"].is_null(),
            "no described-as text exists yet"
        );
    }
}

#[test]
fn a_program_without_inputs_says_so() {
    let directory = tempfile::tempdir().unwrap();
    write_program(directory.path(), "run printf with message \"hi\"\n");
    let output = help(directory.path(), &["tool.program"]);
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("Inputs: none"));
}

#[test]
fn help_reports_usage_errors_and_parse_failures_with_nonzero_status() {
    let directory = tempfile::tempdir().unwrap();
    write_program(directory.path(), PROGRAM);
    let no_file = help(directory.path(), &[]);
    assert!(!no_file.status.success());
    assert!(String::from_utf8_lossy(&no_file.stderr).contains("usage: execute help"));

    let two_files = help(directory.path(), &["tool.program", "tool.program"]);
    assert!(!two_files.status.success());

    let unknown = help(directory.path(), &["tool.program", "--verbose"]);
    assert!(!unknown.status.success());
    assert!(String::from_utf8_lossy(&unknown.stderr).contains("unknown option"));

    let missing = help(directory.path(), &["does-not-exist.program"]);
    assert!(!missing.status.success());
    assert!(missing.stdout.is_empty());

    write_program(directory.path(), "accept x as bogus\n");
    let broken = help(directory.path(), &["tool.program"]);
    assert!(!broken.status.success());
    assert!(broken.stdout.is_empty());
}

#[test]
fn a_shebang_style_help_request_prints_the_program_help_and_runs_nothing() {
    // The interpreter line supplies the leading `--`, so the program sees `--help`.
    let directory = tempfile::tempdir().unwrap();
    let source = "accept arguments as list of text from arguments\nthen encode arguments as json\n";
    write_program(directory.path(), source);
    let output = Command::new(env!("CARGO_BIN_EXE_execute"))
        .args(["--", "tool.program", "--help"])
        .current_dir(directory.path())
        .env_remove("EXECUTE_MODELS_OFFLINE")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("Inputs:"), "{text}");
    assert!(text.contains("arguments  list of text  required"), "{text}");
    assert!(
        !text.contains("[\"--help\"]"),
        "the program must not have run: {text}"
    );
}

#[test]
fn a_second_separator_makes_a_leading_help_argument_literal_for_the_program() {
    // `./prog -- --help`: the shebang separator, then a literal separator.
    let directory = tempfile::tempdir().unwrap();
    let source = "accept arguments as list of text from arguments\nthen encode arguments as json\n";
    write_program(directory.path(), source);
    let output = Command::new(env!("CARGO_BIN_EXE_execute"))
        .args(["--", "tool.program", "--", "--help"])
        .current_dir(directory.path())
        .env_remove("EXECUTE_MODELS_OFFLINE")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8(output.stdout).unwrap(), "[\"--help\"]");
}

#[test]
fn an_explicit_separator_after_the_file_also_keeps_help_literal() {
    let directory = tempfile::tempdir().unwrap();
    let source = "accept arguments as list of text from arguments\nthen encode arguments as json\n";
    write_program(directory.path(), source);
    let output = Command::new(env!("CARGO_BIN_EXE_execute"))
        .args(["tool.program", "--", "--help"])
        .current_dir(directory.path())
        .env_remove("EXECUTE_MODELS_OFFLINE")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8(output.stdout).unwrap(), "[\"--help\"]");
}

#[test]
fn the_direct_option_still_prints_the_global_usage() {
    let directory = tempfile::tempdir().unwrap();
    let source = "accept arguments as list of text from arguments\nthen encode arguments as json\n";
    write_program(directory.path(), source);
    let output = Command::new(env!("CARGO_BIN_EXE_execute"))
        .args(["tool.program", "--help"])
        .current_dir(directory.path())
        .env_remove("EXECUTE_MODELS_OFFLINE")
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("Usage: execute"));
}
