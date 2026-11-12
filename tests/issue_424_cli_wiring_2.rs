// Command-line wiring for `execute doctor` (#424), `run --model-fixtures` and
// `EXECUTE_MODEL_FIXTURES` (#503), the `condition` of `if` rows in `explain --json` (#463),
// and `run --confirm-report` (#466). Each test runs the built binary in a temporary
// directory with an empty manifest directory, so no repository manifest is loaded.

use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const GUARDED: &str = "accept mode as text with default \"x\"
if mode is \"x\"
  use value mode
otherwise
  use value mode
";

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
        .env_remove("EXECUTE_MODEL_FIXTURES")
        .args(arguments)
        .output()
        .unwrap()
}

fn write_program(directory: &Path, name: &str, source: &str) -> PathBuf {
    let path = directory.join(name);
    fs::write(&path, source).unwrap();
    path
}

// --- A: execute doctor ------------------------------------------------------

#[test]
fn doctor_json_has_the_protocol_and_exits_with_the_ok_status() {
    let directory = tempfile::tempdir().unwrap();
    let output = execute(directory.path(), &["doctor", "--json"]);
    let report: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!("doctor --json is not JSON ({error}): {}", describe(&output))
    });
    assert_eq!(
        report["protocol"],
        "execute.doctor/1",
        "{}",
        describe(&output)
    );
    let checks = report["checks"].as_array().expect("checks array");
    assert!(
        checks
            .iter()
            .any(|check| check["id"] == "helpers.execute-applet"),
        "{}",
        describe(&output)
    );
    // Exit status 1 exactly when a check failed.
    let expected = if report["ok"] == true {
        Some(0)
    } else {
        Some(1)
    };
    assert_eq!(output.status.code(), expected, "{}", describe(&output));
}

#[test]
fn doctor_reports_a_binary_without_its_helpers_and_exits_nonzero() {
    // A copy of the binary in a directory with no execute-applet or execute-model beside it.
    let directory = tempfile::tempdir().unwrap();
    let bin = directory.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let built = PathBuf::from(env!("CARGO_BIN_EXE_execute"));
    let copy = bin.join(built.file_name().unwrap());
    fs::copy(&built, &copy).unwrap();
    let output = Command::new(&copy)
        .current_dir(directory.path())
        .args(["doctor", "--json"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1), "{}", describe(&output));
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["ok"], false, "{}", describe(&output));
    let helper = report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["id"] == "helpers.execute-applet")
        .expect("helper check");
    assert_eq!(helper["status"], "fail", "{}", describe(&output));
    assert!(helper["remedy"].is_string(), "{}", describe(&output));
}

#[test]
fn doctor_text_output_tags_each_check_and_ends_with_a_summary() {
    let directory = tempfile::tempdir().unwrap();
    let output = execute(directory.path(), &["doctor"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("[ok]") || stdout.contains("[fail]"),
        "{}",
        describe(&output)
    );
    assert!(
        stdout.contains("(helpers.execute-applet)"),
        "{}",
        describe(&output)
    );
    assert!(stdout.contains(" failed, "), "{}", describe(&output));
    assert_eq!(
        output.status.code(),
        Some(if stdout.contains("[fail]") { 1 } else { 0 }),
        "{}",
        describe(&output)
    );
}

#[test]
fn doctor_rejects_unknown_arguments_with_its_usage() {
    let directory = tempfile::tempdir().unwrap();
    let output = execute(directory.path(), &["doctor", "--verbose"]);
    assert!(!output.status.success(), "{}", describe(&output));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("usage: execute doctor [--json]"),
        "{}",
        describe(&output)
    );
}

// --- B: --model-fixtures and EXECUTE_MODEL_FIXTURES -------------------------

#[test]
fn model_fixtures_option_refuses_an_empty_directory() {
    let directory = tempfile::tempdir().unwrap();
    let program = write_program(directory.path(), "b1.program", GUARDED);
    let output = execute(
        directory.path(),
        &["run", program.to_str().unwrap(), "--model-fixtures", ""],
    );
    assert!(!output.status.success(), "{}", describe(&output));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("--model-fixtures requires a directory"),
        "{}",
        describe(&output)
    );
}

#[test]
fn empty_model_fixtures_environment_variable_is_an_error_unless_the_option_wins() {
    let directory = tempfile::tempdir().unwrap();
    let program = write_program(directory.path(), "b2.program", GUARDED);
    let program = program.to_str().unwrap();

    let mut refused = Command::new(env!("CARGO_BIN_EXE_execute"));
    let output = refused
        .current_dir(directory.path())
        .env("EXECUTE_MODEL_FIXTURES", "")
        .args(["run", program])
        .output()
        .unwrap();
    assert!(!output.status.success(), "{}", describe(&output));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("EXECUTE_MODEL_FIXTURES is set but empty"),
        "{}",
        describe(&output)
    );

    // The option is present, so the empty variable is never read.
    let fixtures = directory.path().to_string_lossy().into_owned();
    let output = Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(directory.path())
        .env("EXECUTE_MODEL_FIXTURES", "")
        .args(["run", program, "--model-fixtures", &fixtures])
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", describe(&output));
}

#[test]
fn model_fixtures_option_is_accepted_for_resume_as_well() {
    let directory = tempfile::tempdir().unwrap();
    let program = write_program(directory.path(), "b3.program", GUARDED);
    let output = execute(
        directory.path(),
        &[
            "resume",
            program.to_str().unwrap(),
            "--model-fixtures",
            "",
            "--checkpoint",
            &directory.path().to_string_lossy(),
        ],
    );
    // The option parses for resume; the empty directory is refused by the run setup.
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("--model-fixtures requires a directory"),
        "{}",
        describe(&output)
    );
}

// --- C: explain --json gives each if row its condition ----------------------

#[test]
fn explain_json_gives_an_if_row_its_guard_without_its_input_value() {
    let directory = tempfile::tempdir().unwrap();
    let program = write_program(directory.path(), "c1.program", GUARDED);
    let output = execute(
        directory.path(),
        &["explain", program.to_str().unwrap(), "--json"],
    );
    assert!(output.status.success(), "{}", describe(&output));
    let plan: Value = serde_json::from_slice(&output.stdout).unwrap();
    let ifs: Vec<&Value> = plan["steps"]
        .as_array()
        .expect("steps array")
        .iter()
        .filter(|row| row["kind"] == "if")
        .collect();
    assert_eq!(ifs.len(), 1, "{}", describe(&output));
    assert_eq!(
        ifs[0]["condition"],
        serde_json::json!({
            "left": {"ref": "mode"},
            "op": "equals",
            "negated": false,
            "right": {"literal": "x"}
        }),
        "{}",
        describe(&output)
    );
    for row in plan["steps"].as_array().unwrap() {
        if row["kind"] != "if" {
            assert!(row.get("condition").is_none(), "{row}");
        }
    }
}

// --- D: run --confirm-report ------------------------------------------------

#[test]
fn confirm_report_that_is_not_json_is_refused_by_name() {
    let directory = tempfile::tempdir().unwrap();
    let program = write_program(directory.path(), "d1.program", GUARDED);
    let report = write_program(directory.path(), "bad-report.json", "{ not json");
    let output = execute(
        directory.path(),
        &[
            "run",
            program.to_str().unwrap(),
            "--confirm",
            "0000",
            "--confirm-report",
            report.to_str().unwrap(),
        ],
    );
    assert!(!output.status.success(), "{}", describe(&output));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("bad-report.json"), "{}", describe(&output));
    assert!(
        stderr.contains("not a JSON what-if report"),
        "{}",
        describe(&output)
    );
}

#[test]
fn confirm_report_lists_the_input_that_changed_after_review() {
    let directory = tempfile::tempdir().unwrap();
    let program = write_program(directory.path(), "d2.program", GUARDED);
    let program = program.to_str().unwrap();

    // Review with mode=x; the report is what --what-if prints.
    let review = execute(
        directory.path(),
        &["run", program, "--what-if", "--input", "mode=x"],
    );
    assert!(review.status.success(), "{}", describe(&review));
    let report: Value = serde_json::from_slice(&review.stdout).unwrap();
    let fingerprint = report["fingerprint"]
        .as_str()
        .expect("fingerprint")
        .to_owned();
    let report_path = directory.path().join("reviewed.json");
    fs::write(&report_path, &review.stdout).unwrap();

    // Run with mode=y: the fingerprint no longer matches, and the diff names the input.
    let changed = execute(
        directory.path(),
        &[
            "run",
            program,
            "--confirm",
            &fingerprint,
            "--confirm-report",
            report_path.to_str().unwrap(),
            "--input",
            "mode=y",
        ],
    );
    assert!(!changed.status.success(), "{}", describe(&changed));
    let stderr = String::from_utf8_lossy(&changed.stderr);
    assert!(
        stderr.contains("plan changed since --what-if"),
        "{}",
        describe(&changed)
    );
    assert!(
        stderr.contains("input mode changed"),
        "{}",
        describe(&changed)
    );
}

#[test]
fn confirm_without_a_report_keeps_the_plain_refusal() {
    let directory = tempfile::tempdir().unwrap();
    let program = write_program(directory.path(), "d3.program", GUARDED);
    let output = execute(
        directory.path(),
        &["run", program.to_str().unwrap(), "--confirm", "0000"],
    );
    assert!(!output.status.success(), "{}", describe(&output));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("plan changed since --what-if"),
        "{}",
        describe(&output)
    );
    assert!(!stderr.contains("changed:"), "{}", describe(&output));
}

#[test]
fn confirm_report_requires_confirm() {
    let directory = tempfile::tempdir().unwrap();
    let program = write_program(directory.path(), "d4.program", GUARDED);
    let output = execute(
        directory.path(),
        &[
            "run",
            program.to_str().unwrap(),
            "--confirm-report",
            "report.json",
        ],
    );
    assert!(!output.status.success(), "{}", describe(&output));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("--confirm-report requires --confirm"),
        "{}",
        describe(&output)
    );
}
