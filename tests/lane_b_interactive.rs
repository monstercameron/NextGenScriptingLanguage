//! `interactively` (terminal handoff) parser and validation hook. Execution is
//! owned by the terminal-handoff runtime, so these tests cover parsing, placement
//! rules, and explain text only.
use flow::ast::StepKind;
use flow::parser;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn manifests() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests")
}

fn run_subcommand(directory: &Path, command: &str, script: &str) -> Output {
    let path = directory.join("workflow.program");
    fs::write(&path, script).unwrap();
    Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(directory)
        .arg(command)
        .arg(&path)
        .arg("--manifest-dir")
        .arg(manifests())
        .output()
        .unwrap()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn interactively_is_a_marker_before_one_fresh_command() {
    let program =
        parser::parse("run \"nvim\" with arguments [\"notes.txt\"] interactively\n").unwrap();
    assert_eq!(program.steps.len(), 2);
    assert!(matches!(program.steps[0].kind, StepKind::Interactive));
    let StepKind::Invoke { call, .. } = &program.steps[1].kind else {
        panic!("interactively must be followed by the command")
    };
    assert_eq!(call.operation, "nvim");
}

#[test]
fn interactively_without_arguments_still_parses() {
    let program = parser::parse("run \"vim\" interactively\n").unwrap();
    assert!(matches!(program.steps[0].kind, StepKind::Interactive));
}

#[test]
fn interactively_rejects_then_pipe_and_redirection_forms_at_parse_time() {
    for script in [
        "then \"vim\" interactively\n",
        "run \"vim\" with arguments [] and stdin from \"in.txt\" interactively\n",
        "run \"vim\" with arguments [] and stderr to \"e.txt\" interactively\n",
        "run \"vim\" with arguments [] and environment {\"A\": \"b\"} interactively\n",
        "run program helper interactively\n",
    ] {
        assert!(parser::parse(script).is_err(), "{script}");
    }
}

#[test]
fn interactively_is_rejected_inside_loops_and_try_blocks() {
    let directory = tempfile::tempdir().unwrap();
    for script in [
        "repeat 2 times\n  run \"vim\" with arguments [] interactively\n",
        "try\n  run \"vim\" with arguments [] interactively\ncatch failure\n  run printf with message \"x\"\n",
    ] {
        let output = run_subcommand(directory.path(), "check", script);
        assert_ne!(output.status.code(), Some(0), "{script}");
        assert!(stderr(&output).contains("top level"), "{script}: {}", stderr(&output));
    }
}

#[test]
fn interactively_is_rejected_for_in_process_applets_and_result_captures() {
    let directory = tempfile::tempdir().unwrap();
    let output = run_subcommand(
        directory.path(),
        "check",
        "run text.concat with parts [\"a\"] interactively\n",
    );
    assert_ne!(output.status.code(), Some(0));
    assert!(
        stderr(&output).contains("native command"),
        "{}",
        stderr(&output)
    );

    let output = run_subcommand(
        directory.path(),
        "check",
        "run printf with message \"x\" as result interactively\n",
    );
    assert_ne!(output.status.code(), Some(0));
    assert!(
        stderr(&output).contains("cannot be combined"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn interactively_is_rejected_for_builtins_and_service_handlers() {
    let directory = tempfile::tempdir().unwrap();
    let output = run_subcommand(
        directory.path(),
        "check",
        "run cd with path \".\" interactively\n",
    );
    assert_ne!(output.status.code(), Some(0));
    assert!(stderr(&output).contains("builtin"), "{}", stderr(&output));

    let output = run_subcommand(
        directory.path(),
        "check",
        "every 1 seconds\n  run \"vim\" with arguments [] interactively\n",
    );
    assert_ne!(output.status.code(), Some(0));
    assert!(
        stderr(&output).contains("service or listener"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn explain_shows_the_terminal_handoff() {
    let directory = tempfile::tempdir().unwrap();
    let output = run_subcommand(
        directory.path(),
        "explain",
        "run \"vim\" with arguments [\"notes.txt\"] interactively\n",
    );
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("interactive (terminal handoff)"), "{text}");
}
