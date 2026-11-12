// `run --model-budget` and `EXECUTE_MODEL_BUDGET` (#377): the option is parsed before any
// step runs, a malformed budget is an error that names it, and a program that makes no
// model call is unaffected by a budget.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

const PROGRAM: &str = "use value \"ready\"\n";

fn describe(output: &Output) -> String {
    format!(
        "status {:?} stdout {:?} stderr {:?}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn execute(directory: &Path, budget_variable: Option<&str>, arguments: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_execute"));
    command
        .current_dir(directory)
        .env_remove("EXECUTE_MODEL_BUDGET")
        .args(arguments);
    if let Some(value) = budget_variable {
        command.env("EXECUTE_MODEL_BUDGET", value);
    }
    command.output().unwrap()
}

fn program(directory: &Path) {
    fs::write(directory.join("plain.program"), PROGRAM).unwrap();
}

#[test]
fn a_valid_budget_does_not_change_a_run_without_model_calls() {
    let directory = tempfile::tempdir().unwrap();
    program(directory.path());
    let plain = execute(directory.path(), None, &["run", "plain.program"]);
    assert!(plain.status.success(), "{}", describe(&plain));
    let budgeted = execute(
        directory.path(),
        None,
        &[
            "run",
            "plain.program",
            "--model-budget",
            "calls=1,tokens=1000",
        ],
    );
    assert!(budgeted.status.success(), "{}", describe(&budgeted));
    assert_eq!(budgeted.stdout, plain.stdout);
}

#[test]
fn a_malformed_budget_is_refused_before_the_run() {
    let directory = tempfile::tempdir().unwrap();
    program(directory.path());
    for bad in ["calls", "calls=0", "minutes=5", "calls=1,calls=2"] {
        let output = execute(
            directory.path(),
            None,
            &["run", "plain.program", "--model-budget", bad],
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{bad}: {}", describe(&output));
        assert!(
            stderr.contains("model budget"),
            "{bad}: {}",
            describe(&output)
        );
        assert!(output.stdout.is_empty(), "{bad}: {}", describe(&output));
    }
}

#[test]
fn the_environment_variable_is_used_only_when_the_option_is_absent() {
    let directory = tempfile::tempdir().unwrap();
    program(directory.path());
    let from_variable = execute(directory.path(), Some("bogus"), &["run", "plain.program"]);
    assert!(
        !from_variable.status.success(),
        "{}",
        describe(&from_variable)
    );
    let option_wins = execute(
        directory.path(),
        Some("bogus"),
        &["run", "plain.program", "--model-budget", "calls=3"],
    );
    assert!(option_wins.status.success(), "{}", describe(&option_wins));
}

#[test]
fn the_option_is_refused_outside_run_and_resume() {
    let directory = tempfile::tempdir().unwrap();
    program(directory.path());
    let output = execute(
        directory.path(),
        None,
        &["check", "plain.program", "--model-budget", "calls=1"],
    );
    assert!(!output.status.success(), "{}", describe(&output));
}
