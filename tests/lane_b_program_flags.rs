//! `from flag` and `from option` inputs: parsing after `--`, generated help, refusal of
//! unknown flags before any effect, declaration checks, and inspect output.
use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

const TOOL: &str = "accept verbose as boolean from flag \"--verbose\" alias \"-v\" described as \"Print progress\"\naccept output as text from option \"--output\" alias \"-o\" with default \"out.txt\" described as \"Where to write\"\naccept count as integer from option \"--count\" described as \"How many\"\nrun \"printf\" with arguments [\"%s %s\\n\", output, count]\n";

fn manifests() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests")
}

fn run(directory: &Path, command: &str, script: &str, extra: &[&str]) -> Output {
    let path = directory.join("tool.program");
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

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn flags_and_options_bind_by_long_name_alias_and_equals_form() {
    let directory = tempfile::tempdir().unwrap();
    let output = run(
        directory.path(),
        "run",
        TOOL,
        &["--", "-v", "--output=x.txt", "--count", "7"],
    );
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(stdout(&output), "x.txt 7\n");
}

#[test]
fn absent_options_take_their_defaults() {
    let directory = tempfile::tempdir().unwrap();
    let output = run(directory.path(), "run", TOOL, &["--", "--count", "3"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(stdout(&output), "out.txt 3\n");
}

#[test]
fn an_unknown_flag_is_refused_before_any_effect() {
    let directory = tempfile::tempdir().unwrap();
    let script = format!(
        "{TOOL}run \"printf\" with arguments [\"effect\\n\"]\nthen save to file \"effect.txt\"\n"
    );
    let output = run(
        directory.path(),
        "run",
        &script,
        &["--", "--bogus", "--count", "1"],
    );
    assert_ne!(output.status.code(), Some(0));
    assert!(
        stderr(&output).contains("unknown flag \"--bogus\""),
        "{}",
        stderr(&output)
    );
    assert!(stdout(&output).is_empty());
    assert!(!directory.path().join("effect.txt").exists());
}

#[test]
fn a_flag_takes_no_value_and_an_option_needs_one() {
    let directory = tempfile::tempdir().unwrap();
    let with_value = run(
        directory.path(),
        "run",
        TOOL,
        &["--", "--verbose=yes", "--count", "1"],
    );
    assert!(
        stderr(&with_value).contains("--verbose takes no value"),
        "{}",
        stderr(&with_value)
    );

    let missing = run(directory.path(), "run", TOOL, &["--", "--count"]);
    assert!(
        stderr(&missing).contains("--count needs a value"),
        "{}",
        stderr(&missing)
    );
}

#[test]
fn generated_help_lists_flags_options_and_descriptions() {
    let directory = tempfile::tempdir().unwrap();
    let output = run(directory.path(), "run", TOOL, &["--", "--help"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("[OPTIONS]"), "{text}");
    assert!(text.contains("--verbose, -v"), "{text}");
    assert!(text.contains("Print progress"), "{text}");
    assert!(text.contains("--output VALUE, -o VALUE"), "{text}");
    assert!(text.contains("default \"out.txt\""), "{text}");
    assert!(text.contains("-h, --help"), "{text}");
}

#[test]
fn bad_flag_declarations_are_rejected_at_check_time() {
    let directory = tempfile::tempdir().unwrap();
    let cases = [
        (
            "accept v as boolean from flag \"verbose\"\n",
            "must start with --",
        ),
        (
            "accept v as boolean from flag \"--verbose\" alias \"v\"\n",
            "must be one dash",
        ),
        (
            "accept v as text from flag \"--verbose\"\n",
            "must be a boolean input",
        ),
        (
            "accept v as boolean from option \"--verbose\"\n",
            "must be a text or integer input",
        ),
        ("accept v as boolean from flag \"--help\"\n", "reserved"),
        (
            "accept a as boolean from flag \"--x\"\naccept b as boolean from flag \"--x\"\n",
            "declared more than once",
        ),
    ];
    for (script, message) in cases {
        let output = run(directory.path(), "check", script, &[]);
        assert_ne!(output.status.code(), Some(0), "{script}");
        assert!(
            stderr(&output).contains(message),
            "{script}: {}",
            stderr(&output)
        );
    }
}

#[test]
fn inspect_reports_each_flag_and_option_as_its_source() {
    let directory = tempfile::tempdir().unwrap();
    let output = run(directory.path(), "inspect", TOOL, &[]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    let inputs = report["inputs"].as_array().unwrap();
    assert_eq!(inputs[0]["source"]["kind"], "flag");
    assert_eq!(inputs[0]["source"]["name"], "--verbose");
    assert_eq!(inputs[0]["source"]["alias"], "-v");
    assert_eq!(inputs[1]["source"]["kind"], "option");
    assert_eq!(inputs[1]["source"]["name"], "--output");
}

#[test]
fn a_program_without_flags_keeps_its_arguments_positional() {
    let directory = tempfile::tempdir().unwrap();
    let script =
        "accept value as text from argument 1\nrun \"printf\" with arguments [\"%s\\n\", value]\n";
    let output = run(directory.path(), "run", script, &["--", "-x"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(stdout(&output).trim_end(), "-x");
}
