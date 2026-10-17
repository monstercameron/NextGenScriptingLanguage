#![cfg(unix)]
use std::os::unix::{ffi::OsStringExt, fs::PermissionsExt};
use std::{
    fs,
    path::Path,
    process::{Command, Output},
};

fn command(source: &str, directory: &Path) -> Command {
    let file = directory.join("process.program");
    fs::write(&file, source).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_execute"));
    command.arg(file).current_dir(directory);
    command
}
fn success(output: Output) -> Output {
    assert!(
        output.status.success(),
        "{:?}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}
#[test]
fn environment_and_positional_values_are_typed_and_named_overrides_win() {
    let directory = tempfile::tempdir().unwrap();
    let source = "accept root as text from environment \"EXECUTE_TEST_ROOT\" with default \"fallback\"\naccept count as integer from argument 1 with default 2\nrun \"printf\" with arguments [\"%s|%s\", root, count]\n";
    let output = success(
        command(source, directory.path())
            .env("EXECUTE_TEST_ROOT", "a path; $(touch forbidden)")
            .args(["--", "7"])
            .output()
            .unwrap(),
    );
    assert_eq!(output.stdout, b"a path; $(touch forbidden)|7");
    assert!(!directory.path().join("forbidden").exists());
    let output = success(
        command(source, directory.path())
            .env("EXECUTE_TEST_ROOT", "ambient")
            .args([
                "--input",
                "root=explicit",
                "--input",
                "count=17",
                "--",
                "not an integer",
            ])
            .output()
            .unwrap(),
    );
    assert_eq!(output.stdout, b"explicit|17");
    let output = success(
        command(source, directory.path())
            .env_remove("EXECUTE_TEST_ROOT")
            .output()
            .unwrap(),
    );
    assert_eq!(output.stdout, b"fallback|2");
    let output = success(
        command(source, directory.path())
            .env("EXECUTE_TEST_ROOT", "")
            .output()
            .unwrap(),
    );
    assert_eq!(output.stdout, b"|2");
}
#[test]
fn variadic_arguments_preserve_empty_values_flags_and_literal_paths() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(
        directory.path().join("a ;$(touch forbidden) *"),
        b"literal data",
    )
    .unwrap();
    let source = "accept paths as list of text from arguments\nrun \"cat\" with arguments paths\n";
    let output = success(
        command(source, directory.path())
            .args(["--", "--", "a ;$(touch forbidden) *"])
            .output()
            .unwrap(),
    );
    assert_eq!(output.stdout, b"literal data");
    assert!(!directory.path().join("forbidden").exists());
    let source = "accept arguments as list of text from arguments\nthen encode arguments as json\n";
    let output = success(
        command(source, directory.path())
            .args(["--", "", "--help", "café"])
            .output()
            .unwrap(),
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value, serde_json::json!(["", "--help", "café"]));
    let output = success(command(source, directory.path()).output().unwrap());
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap(),
        serde_json::json!([])
    );
}
#[test]
fn process_input_errors_prevent_earlier_effects_and_do_not_echo_environment_values() {
    let directory = tempfile::tempdir().unwrap();
    let source = "accept count as integer from environment \"EXECUTE_TEST_INTEGER\"\nrun \"printf\" with arguments [\"forbidden\"]\nthen save to \"forbidden\"\n";
    for output in [
        command(source, directory.path())
            .env("EXECUTE_TEST_INTEGER", "secret-invalid-integer")
            .output()
            .unwrap(),
        command(source, directory.path())
            .env(
                "EXECUTE_TEST_INTEGER",
                std::ffi::OsString::from_vec(vec![0xff]),
            )
            .output()
            .unwrap(),
        command(source, directory.path())
            .env_remove("EXECUTE_TEST_INTEGER")
            .output()
            .unwrap(),
    ] {
        assert!(!output.status.success());
        assert!(!directory.path().join("forbidden").exists());
        assert!(!String::from_utf8_lossy(&output.stderr).contains("secret-invalid-integer"));
    }
    let source = "accept count as integer from argument 1\nrun \"printf\" with arguments [\"forbidden\"]\nthen save to \"forbidden\"\n";
    for args in [vec!["--", "bad"], vec!["--", "1", "unexpected"]] {
        let output = command(source, directory.path())
            .args(args)
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(!directory.path().join("forbidden").exists());
    }
}
#[test]
fn static_inspection_describes_sources_without_reading_environment_secrets() {
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("interface.program");
    fs::write(&file, "accept key as integer from environment \"EXECUTE_PRIVATE_VALUE\"\naccept name as text from argument 1\nrun \"printf\" with arguments [\"%s\", name]\n").unwrap();
    let output = success(
        Command::new(env!("CARGO_BIN_EXE_execute"))
            .arg("inspect")
            .arg(file)
            .env("EXECUTE_PRIVATE_VALUE", "never-print-secret")
            .output()
            .unwrap(),
    );
    assert!(!String::from_utf8_lossy(&output.stdout).contains("never-print-secret"));
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["inputs"][0]["source"]["kind"], "environment");
    assert_eq!(value["inputs"][1]["source"]["position"], 1);
    assert!(value["inputs"][0].get("value").is_none());
}
#[test]
fn executable_program_shebang_forwards_options_as_program_arguments() {
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("command.program");
    fs::write(&file, "#!/usr/bin/env -S execute --\naccept message as text from argument 1\nrun \"printf\" with arguments [\"%s\", message]\n").unwrap();
    fs::set_permissions(&file, fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!(
        "{}:/usr/bin:/bin",
        Path::new(env!("CARGO_BIN_EXE_execute"))
            .parent()
            .unwrap()
            .display()
    );
    let output = success(
        Command::new(file)
            .env("PATH", path)
            .arg("--verbose")
            .output()
            .unwrap(),
    );
    assert_eq!(output.stdout, b"--verbose");
}
#[test]
fn process_input_headers_reject_ambiguous_or_invalid_sources() {
    for source in [
        "accept value as text from arguments",
        "accept value as text from environment \"BAD=NAME\"",
        "accept value as text from environment unquoted",
        "accept value as text from argument 0",
        "accept value as text from argument -1",
        "accept value as text from argument 1000001",
        "define a program named helper\n  accept value as text from environment \"HOME\"\n  run \"printf\" with arguments [value]\nrun program helper with value \"x\"\n",
    ] {
        assert!(flow::parser::parse(source).is_err(), "{source}");
    }
    let legacy =
        flow::parser::parse("accept value as text\nrun \"printf\" with arguments [value]\n")
            .unwrap();
    assert!(!serde_json::to_string(&legacy.inputs)
        .unwrap()
        .contains("source"));
}
#[test]
fn checkpoint_environment_values_are_frozen_and_changed_values_do_not_replay_commands() {
    let directory = tempfile::tempdir().unwrap();
    let source = "accept value as text from environment \"EXECUTE_CHECKPOINT_VALUE\"\nrun \"printf\" with arguments [\"%s\", value]\n";
    let output = success(
        command(source, directory.path())
            .env("EXECUTE_CHECKPOINT_VALUE", "first")
            .args(["--checkpoint", "checkpoint"])
            .output()
            .unwrap(),
    );
    assert_eq!(output.stdout, b"first");
    let file = directory.path().join("process.program");
    let output = Command::new(env!("CARGO_BIN_EXE_execute"))
        .arg("resume")
        .arg(&file)
        .args(["--checkpoint", "checkpoint"])
        .env("EXECUTE_CHECKPOINT_VALUE", "changed")
        .current_dir(directory.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let output = success(
        Command::new(env!("CARGO_BIN_EXE_execute"))
            .arg("resume")
            .arg(file)
            .args(["--checkpoint", "checkpoint"])
            .env("EXECUTE_CHECKPOINT_VALUE", "first")
            .current_dir(directory.path())
            .output()
            .unwrap(),
    );
    assert!(output.stdout.is_empty(), "{:?}", output.stdout);
}
