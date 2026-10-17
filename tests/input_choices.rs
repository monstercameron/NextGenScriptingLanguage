//! `accept NAME as text one of [...]`: choices are checked when inputs are parsed
//! and when values arrive, before any step runs, and every inspection surface
//! lists them.
use serde_json::Value;
use std::fs;
use std::path::Path;
use std::process::{Command, Output};

const SAVE_MARKER: &str = "use value \"hello\"\nthen save to \"marker.txt\"\n";

fn program(root: &Path, source: &str) {
    fs::write(root.join("job.program"), source).unwrap();
}

fn execute(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_execute"))
        .args(args)
        .current_dir(root)
        .output()
        .unwrap()
}

fn json_of(output: Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn stderr_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn a_supplied_choice_runs_and_the_pipeline_reaches_the_step() {
    let root = tempfile::tempdir().unwrap();
    program(
        root.path(),
        &format!("accept mode as text one of [\"light\", \"dark\"]\n{SAVE_MARKER}"),
    );
    let output = execute(root.path(), &["run", "job.program", "--input", "mode=dark"]);
    assert!(output.status.success(), "{}", stderr_of(&output));
    assert_eq!(
        fs::read_to_string(root.path().join("marker.txt")).unwrap(),
        "hello"
    );
}

#[test]
fn a_value_outside_the_choices_is_refused_before_any_step_runs() {
    let root = tempfile::tempdir().unwrap();
    program(
        root.path(),
        &format!("accept mode as text one of [\"light\", \"dark\"]\n{SAVE_MARKER}"),
    );
    let output = execute(root.path(), &["run", "job.program", "--input", "mode=blue"]);
    assert!(!output.status.success());
    let stderr = stderr_of(&output);
    assert!(
        stderr.contains("must be one of \"light\", \"dark\""),
        "{stderr}"
    );
    assert!(stderr.contains("got \"blue\""), "{stderr}");
    assert!(!root.path().join("marker.txt").exists());
}

#[test]
fn environment_and_argument_sources_are_checked_against_the_choices() {
    let root = tempfile::tempdir().unwrap();
    program(
        root.path(),
        &format!(
            "accept mode as text one of [\"light\", \"dark\"] from environment \"EXECUTE_CHOICE_TEST\"\n{SAVE_MARKER}"
        ),
    );
    let output = Command::new(env!("CARGO_BIN_EXE_execute"))
        .args(["run", "job.program"])
        .env("EXECUTE_CHOICE_TEST", "blue")
        .current_dir(root.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        stderr_of(&output).contains("must be one of"),
        "{}",
        stderr_of(&output)
    );
    assert!(!root.path().join("marker.txt").exists());

    program(
        root.path(),
        &format!("accept mode as text one of [\"light\", \"dark\"] from argument 1\n{SAVE_MARKER}"),
    );
    let output = execute(root.path(), &["run", "job.program", "--", "blue"]);
    assert!(!output.status.success());
    assert!(
        stderr_of(&output).contains("must be one of"),
        "{}",
        stderr_of(&output)
    );
    assert!(!root.path().join("marker.txt").exists());
    let output = execute(root.path(), &["run", "job.program", "--", "dark"]);
    assert!(output.status.success(), "{}", stderr_of(&output));
    assert!(root.path().join("marker.txt").exists());
}

#[test]
fn check_refuses_an_unlisted_value_too() {
    let root = tempfile::tempdir().unwrap();
    program(
        root.path(),
        &format!("accept mode as text one of [\"light\", \"dark\"]\n{SAVE_MARKER}"),
    );
    let output = execute(
        root.path(),
        &["check", "job.program", "--input", "mode=blue"],
    );
    assert!(!output.status.success());
    assert!(stderr_of(&output).contains("must be one of"));
    assert!(!root.path().join("marker.txt").exists());
}

#[test]
fn a_default_outside_the_choices_is_a_parse_error() {
    let root = tempfile::tempdir().unwrap();
    program(
        root.path(),
        &format!(
            "accept mode as text one of [\"light\", \"dark\"] with default \"blue\"\n{SAVE_MARKER}"
        ),
    );
    let output = execute(root.path(), &["run", "job.program"]);
    assert!(!output.status.success());
    let stderr = stderr_of(&output);
    assert!(
        stderr.contains("is not one of the allowed values"),
        "{stderr}"
    );
    assert!(stderr.contains("\"light\", \"dark\""), "{stderr}");
    assert!(!root.path().join("marker.txt").exists());
}

#[test]
fn malformed_choice_lists_are_parse_errors() {
    let cases = [
        (
            "accept count as integer one of [\"1\"]",
            "applies only to text inputs",
        ),
        ("accept mode as text one of []", "at least one choice"),
        (
            "accept mode as text one of [\"a\", \"a\"]",
            "more than once",
        ),
        ("accept mode as text one of [\"\"]", "nonempty"),
        ("accept mode as text one of [1, 2]", "must be quoted text"),
        (
            "accept mode as text one of \"light\"",
            "must be followed by a list",
        ),
    ];
    for (declaration, expected) in cases {
        let root = tempfile::tempdir().unwrap();
        program(root.path(), &format!("{declaration}\n{SAVE_MARKER}"));
        let output = execute(root.path(), &["run", "job.program", "--input", "count=1"]);
        assert!(!output.status.success(), "{declaration} should fail");
        let stderr = stderr_of(&output);
        assert!(stderr.contains(expected), "{declaration}: {stderr}");
    }
}

#[test]
fn described_as_works_before_or_after_the_default_with_choices() {
    for declaration in [
        "accept mode as text one of [\"light\", \"dark\"] with default \"light\" described as \"Theme\"",
        "accept mode as text one of [\"light\", \"dark\"] described as \"Theme\" with default \"light\"",
    ] {
        let root = tempfile::tempdir().unwrap();
        program(root.path(), &format!("{declaration}\n{SAVE_MARKER}"));
        let report = json_of(execute(root.path(), &["inspect", "job.program"]));
        let input = &report["inputs"][0];
        assert_eq!(input["description"], "Theme", "{declaration}");
        assert_eq!(input["default"], "light", "{declaration}");
        assert_eq!(input["choices"], serde_json::json!(["light", "dark"]));
    }
}

#[test]
fn inspect_lists_choices_and_omits_them_for_plain_inputs() {
    let root = tempfile::tempdir().unwrap();
    program(
        root.path(),
        &format!(
            "accept mode as text one of [\"light\", \"dark\"]\naccept name as text\n{SAVE_MARKER}"
        ),
    );
    let report = json_of(execute(root.path(), &["inspect", "job.program"]));
    let inputs = report["inputs"].as_array().unwrap();
    assert_eq!(inputs[0]["choices"], serde_json::json!(["light", "dark"]));
    assert!(inputs[1].get("choices").is_none());
    assert_eq!(
        report["missing_inputs"],
        serde_json::json!(["mode", "name"])
    );
}

#[test]
fn explain_json_lists_declared_inputs_with_their_choices() {
    let root = tempfile::tempdir().unwrap();
    let manifests = Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests");
    program(
        root.path(),
        &format!("accept mode as text one of [\"light\", \"dark\"]\naccept name as text with default \"x\"\n{SAVE_MARKER}"),
    );
    let plan = json_of(execute(
        root.path(),
        &[
            "explain",
            "job.program",
            "--json",
            "--manifest-dir",
            manifests.to_str().unwrap(),
        ],
    ));
    let inputs = plan["inputs"].as_array().unwrap();
    assert_eq!(inputs.len(), 2);
    assert_eq!(inputs[0]["name"], "mode");
    assert_eq!(inputs[0]["type"], "text");
    assert_eq!(inputs[0]["required"], true);
    assert_eq!(inputs[0]["choices"], serde_json::json!(["light", "dark"]));
    assert_eq!(inputs[1]["required"], false);
    assert!(inputs[1].get("choices").is_none());
}
