use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};

fn execute() -> Command {
    Command::new(env!("CARGO_BIN_EXE_execute"))
}

#[test]
fn compile_rejects_repeated_options_source_overwrite_and_writes_atomically() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("program.program");
    fs::write(&source, "run \"echo\" with arguments [\"hi\"]\n").unwrap();
    let binary = env!("CARGO_BIN_EXE_execute");

    let repeated = Command::new(binary)
        .args(["compile", "--to", "sh", "--to", "sh"])
        .arg(&source)
        .output()
        .unwrap();
    assert!(!repeated.status.success());
    assert!(String::from_utf8_lossy(&repeated.stderr).contains("--to may be specified only once"));

    let repeated_output = Command::new(binary)
        .args(["compile", "--to", "sh"])
        .arg(&source)
        .args(["--out", "one.sh", "--out", "two.sh"])
        .current_dir(directory.path())
        .output()
        .unwrap();
    assert!(!repeated_output.status.success());
    assert!(String::from_utf8_lossy(&repeated_output.stderr)
        .contains("--out may be specified only once"));

    let compiled = Command::new(binary)
        .args(["compile", "--to", "sh"])
        .arg(&source)
        .args(["--out", "compiled.sh"])
        .current_dir(directory.path())
        .output()
        .unwrap();
    assert!(
        compiled.status.success(),
        "{}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    assert!(fs::read_to_string(directory.path().join("compiled.sh"))
        .unwrap()
        .starts_with("#!/bin/sh"));
    fs::write(directory.path().join("compiled.sh"), "old output\n").unwrap();
    let replaced = Command::new(binary)
        .args(["compile", "--to", "sh"])
        .arg(&source)
        .args(["--out", "compiled.sh"])
        .current_dir(directory.path())
        .output()
        .unwrap();
    assert!(
        replaced.status.success(),
        "{}",
        String::from_utf8_lossy(&replaced.stderr)
    );
    assert!(fs::read_to_string(directory.path().join("compiled.sh"))
        .unwrap()
        .starts_with("#!/bin/sh"));
    assert!(!fs::read_dir(directory.path()).unwrap().any(|entry| {
        entry
            .map(|value| value.file_name().to_string_lossy().ends_with(".tmp"))
            .unwrap_or(false)
    }));

    let overwrite = Command::new(binary)
        .args(["compile", "--to", "sh"])
        .arg(&source)
        .arg("--out")
        .arg(Path::new("program.program"))
        .current_dir(directory.path())
        .output()
        .unwrap();
    assert!(!overwrite.status.success());
    assert!(String::from_utf8_lossy(&overwrite.stderr).contains("must not name the source"));
    assert!(fs::read_to_string(source)
        .unwrap()
        .starts_with("run \"echo\""));
}

#[test]
fn what_if_does_not_create_single_instance_lock_and_help_has_one_usage_text() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("program.program");
    let lock = directory.path().join("run.lock");
    fs::write(&source, "run \"echo\" with arguments [\"hi\"]\n").unwrap();
    let result = execute()
        .arg("run")
        .arg(&source)
        .arg("--what-if")
        .arg("--single-instance")
        .arg(&lock)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(!lock.exists());

    let help = execute().arg("--help").output().unwrap();
    assert!(help.status.success());
    assert!(String::from_utf8_lossy(&help.stdout).contains("--total-timeout-ms"));
    let missing_file = execute().arg("run").output().unwrap();
    assert!(!missing_file.status.success());
    assert!(String::from_utf8_lossy(&missing_file.stderr)
        .contains(String::from_utf8_lossy(&help.stdout).trim()));
}

#[test]
fn every_reported_subcommand_accepts_both_help_spellings() {
    for command in [
        "test",
        "run-parts",
        "show-run",
        "replay",
        "repl",
        "repair",
        "lsp",
        "help",
        "attach",
    ] {
        for flag in ["--help", "-h"] {
            let result = execute().arg(command).arg(flag).output().unwrap();
            assert!(
                result.status.success(),
                "{command} {flag}: {}",
                String::from_utf8_lossy(&result.stderr)
            );
            assert!(
                !result.stdout.is_empty(),
                "{command} {flag} printed no usage"
            );
        }
    }
}

#[test]
fn json_errors_after_separator_remains_a_literal_program_argument() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("argument.program");
    let command = if cfg!(windows) {
        "run \"cmd\" with arguments [\"/c\", \"echo\", word]"
    } else {
        "run \"printf\" with arguments [\"%s\\n\", word]"
    };
    fs::write(
        &source,
        format!("accept word as text from argument 1 with default \"none\"\n{command}\n"),
    )
    .unwrap();
    let result = execute()
        .arg("run")
        .arg(&source)
        .args(["--", "--json-errors"])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let expected = if cfg!(windows) {
        &b"--json-errors\r\n"[..]
    } else {
        &b"--json-errors\n"[..]
    };
    assert_eq!(result.stdout, expected);
}

#[test]
fn run_parts_exits_quietly_with_sigpipe_status_when_stdout_is_closed() {
    let directory = tempfile::tempdir().unwrap();
    let parts = directory.path().join("parts");
    fs::create_dir(&parts).unwrap();
    let command = if cfg!(windows) {
        "run \"cmd\" with arguments [\"/c\", \"exit\", \"0\"]"
    } else {
        "run \"true\" with arguments []"
    };
    fs::write(parts.join("one.program"), format!("{command}\n")).unwrap();
    let mut child = execute()
        .arg("run-parts")
        .arg(&parts)
        .arg("--json")
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    drop(child.stdout.take());
    let status = child.wait().unwrap();
    assert_eq!(status.code(), Some(141));
}
