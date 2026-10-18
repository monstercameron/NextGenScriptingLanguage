//! `execute compile --to sh`: the emitted script must pass `dash -n`, and on Unix its
//! behaviour must match `execute run` for the same program. Unsupported statements
//! are refused by name and line, never approximated.
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tempfile::tempdir;

fn execute(args: &[&str], directory: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_execute"))
        .args(args)
        .current_dir(directory)
        .output()
        .unwrap()
}

fn write_program(directory: &Path, text: &str) -> PathBuf {
    let program = directory.join("workflow.program");
    fs::write(&program, text).unwrap();
    program
}

/// Compile `text` to `directory/out.sh` and return the script path and the compile output.
fn compile(directory: &Path, text: &str) -> (PathBuf, Output) {
    let program = write_program(directory, text);
    let script = directory.join("out.sh");
    let output = execute(
        &[
            "compile",
            "--to",
            "sh",
            program.to_str().unwrap(),
            "--out",
            script.to_str().unwrap(),
        ],
        directory,
    );
    (script, output)
}

#[cfg(unix)]
fn dash_check(script: &Path) {
    let output = Command::new("dash").arg("-n").arg(script).output().unwrap();
    assert!(
        output.status.success(),
        "dash -n rejected the script: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    if Command::new("shellcheck").arg("--version").output().is_ok() {
        let output = Command::new("shellcheck")
            .args(["--shell=sh", "--severity=warning"])
            .arg(script)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "shellcheck reported: {}",
            String::from_utf8_lossy(&output.stdout)
        );
    } else {
        eprintln!("note: shellcheck is not installed; the sh lint was skipped");
    }
}

/// Run the compiled script under dash and the program under execute, then compare
/// stdout and whether each run succeeded. Exit codes are compared only for success,
/// because execute reports native failures with its own status.
#[cfg(unix)]
fn assert_matches_execute(directory: &Path, text: &str) {
    let (script, compile_output) = compile(directory, text);
    assert!(
        compile_output.status.success(),
        "compile failed: {}",
        String::from_utf8_lossy(&compile_output.stderr)
    );
    dash_check(&script);
    let program = directory.join("workflow.program");
    let sh = Command::new("dash").arg(&script).output().unwrap();
    let run = execute(&["run", program.to_str().unwrap()], directory);
    assert_eq!(
        String::from_utf8_lossy(&sh.stdout),
        String::from_utf8_lossy(&run.stdout),
        "stdout differs between the script and execute"
    );
    assert_eq!(
        sh.status.success(),
        run.status.success(),
        "success differs: dash {:?}, execute {:?}",
        sh.status,
        run.status
    );
}

#[test]
fn a_direct_command_compiles_to_a_checked_script() {
    let directory = tempdir().unwrap();
    let (script, output) = compile(
        directory.path(),
        "run \"printf\" with arguments [\"hello\"]\n",
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = fs::read_to_string(&script).unwrap();
    assert!(text.starts_with("#!/bin/sh\n"));
    assert!(text.contains("set -e\n"));
    assert!(text.contains("'printf' 'hello'"));
    #[cfg(unix)]
    dash_check(&script);
}

#[test]
fn an_in_process_applet_is_refused_by_name() {
    let directory = tempdir().unwrap();
    let (script, output) = compile(directory.path(), "run number.to_text with value 7\n");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("number.to_text"), "{stderr}");
    assert!(stderr.contains("not a native command applet"), "{stderr}");
    assert!(
        !script.exists(),
        "a refused compile must not write the script"
    );
}

#[test]
fn a_statement_with_no_sh_equivalent_is_refused_with_its_line() {
    let directory = tempdir().unwrap();
    let (_, output) = compile(
        directory.path(),
        "run \"printf\" with arguments [\"x\"]\nthen save to \"out.txt\"\n",
    );
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("line 2"), "{stderr}");
    assert!(stderr.contains("save to a file"), "{stderr}");
}

#[test]
fn only_sh_is_a_compile_target() {
    let directory = tempdir().unwrap();
    let program = write_program(directory.path(), "run \"printf\" with arguments [\"x\"]\n");
    let output = execute(
        &["compile", "--to", "python", program.to_str().unwrap()],
        directory.path(),
    );
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("supports only --to sh"));
}

#[cfg(unix)]
#[test]
fn repeat_and_conditions_match_execute() {
    let directory = tempdir().unwrap();
    assert_matches_execute(
        directory.path(),
        "run \"printf\" with arguments [\"a\"]\n\
         repeat 2 times\n  run \"printf\" with arguments [\"b\"]\n\
         if run \"false\" succeeds\n  run \"printf\" with arguments [\"never\"]\n\
         otherwise\n  run \"printf\" with arguments [\"fallback\"]\n\
         exit with status 0\n",
    );
}

#[cfg(unix)]
#[test]
fn for_each_and_break_match_execute() {
    let directory = tempdir().unwrap();
    assert_matches_execute(
        directory.path(),
        "for each item in [\"x\", \"y\", \"z\"]\n  run \"printf\" with arguments [\"<>\"]\n  if run \"false\" succeeds\n    run \"printf\" with arguments [\"never\"]\n  repeat 1 times\n    run \"printf\" with arguments [\"!\"]\n",
    );
}

#[cfg(unix)]
#[test]
fn a_failing_command_stops_the_script_like_execute() {
    let directory = tempdir().unwrap();
    assert_matches_execute(
        directory.path(),
        "run \"printf\" with arguments [\"before\"]\nrun \"false\"\nrun \"printf\" with arguments [\"after\"]\n",
    );
}

#[cfg(unix)]
#[test]
fn shell_metacharacters_in_arguments_stay_literal() {
    let directory = tempdir().unwrap();
    assert_matches_execute(
        directory.path(),
        "run \"printf\" with arguments [\"%s\", \"it's $HOME `id` ; rm -rf x\"]\n",
    );
}

#[cfg(unix)]
#[test]
fn a_pipeline_matches_execute() {
    let directory = tempdir().unwrap();
    assert_matches_execute(
        directory.path(),
        "run \"printf\" with arguments [\"piped\"]\npipe to \"tr\" with arguments [\"p\", \"P\"]\n",
    );
}

#[cfg(unix)]
#[test]
fn discarding_stderr_matches_execute() {
    let directory = tempdir().unwrap();
    assert_matches_execute(
        directory.path(),
        "run \"sh\" with arguments [\"-c\", \"printf noise >&2; printf out\"] and discard stderr\n",
    );
}

#[cfg(unix)]
#[test]
fn fail_exits_nonzero_with_its_message_on_stderr() {
    let directory = tempdir().unwrap();
    let (script, output) = compile(directory.path(), "fail with message \"stopped here\"\n");
    assert!(output.status.success());
    dash_check(&script);
    let sh = Command::new("dash").arg(&script).output().unwrap();
    assert!(!sh.status.success());
    assert_eq!(
        String::from_utf8_lossy(&sh.stderr).trim_end(),
        "stopped here"
    );
}

#[cfg(unix)]
#[test]
fn a_failing_last_command_prints_nothing_of_an_earlier_value() {
    let directory = tempdir().unwrap();
    assert_matches_execute(
        directory.path(),
        "run \"printf\" with arguments [\"a\"]\nrepeat 1 times\n  run \"printf\" with arguments [\"b\"]\nrun \"false\"\n",
    );
}

#[cfg(unix)]
#[test]
fn a_failure_prints_the_pipeline_value_first() {
    let directory = tempdir().unwrap();
    assert_matches_execute(
        directory.path(),
        "run \"printf\" with arguments [\"kept\"]\nrun \"false\"\nrun \"printf\" with arguments [\"after\"]\n",
    );
}

#[cfg(unix)]
#[test]
fn trailing_newlines_are_kept() {
    let directory = tempdir().unwrap();
    assert_matches_execute(
        directory.path(),
        "run \"sh\" with arguments [\"-c\", \"printf 'x\\n\\n'\"]\n",
    );
}

#[test]
fn manifest_source_flags_are_mutually_exclusive() {
    let directory = tempdir().unwrap();
    let program = write_program(directory.path(), "run \"printf\" with arguments [\"x\"]\n");
    let output = execute(
        &[
            "compile",
            "--to",
            "sh",
            program.to_str().unwrap(),
            "--manifest-dir",
            ".",
            "--manifest-catalog",
            "catalog.json",
        ],
        directory.path(),
    );
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("mutually exclusive"));
}

#[test]
fn a_manifest_dir_without_the_applet_refuses_the_compile() {
    let directory = tempdir().unwrap();
    let manifests = directory.path().join("empty-manifests");
    fs::create_dir(&manifests).unwrap();
    let program = write_program(directory.path(), "run text.trim_final_lf with text \"x\"\n");
    let output = execute(
        &[
            "compile",
            "--to",
            "sh",
            program.to_str().unwrap(),
            "--manifest-dir",
            manifests.to_str().unwrap(),
        ],
        directory.path(),
    );
    assert!(!output.status.success());
}

#[cfg(unix)]
#[test]
fn environment_on_one_command_matches_execute() {
    let directory = tempdir().unwrap();
    assert_matches_execute(
        directory.path(),
        "run \"sh\" with arguments [\"-c\", \"printf '%s' \\\"$GREETING\\\"\"] and environment {\"GREETING\": \"hi it's here\"}\n",
    );
}

#[test]
fn loader_environment_variables_are_refused() {
    let directory = tempdir().unwrap();
    for name in ["LD_PRELOAD", "DYLD_INSERT_LIBRARIES"] {
        let text = format!(
            "run \"uname\" with arguments [\"-s\"] and environment {{\"{name}\": \"/tmp/x.so\"}}\n"
        );
        let (_, output) = compile(directory.path(), &text);
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains(&format!(
            "{name} is read by the dynamic loader before the program starts; a program cannot set it"
        )));
    }
}

#[test]
fn loop_variables_are_mapped_to_private_shell_names() {
    let directory = tempdir().unwrap();
    for binding in ["PATH", "IFS", "HOME", "__execute_value"] {
        let program = format!(
            "for each {binding} in [\"value\"]\n  run \"printf\" with arguments [\"%s\", {binding}]\n"
        );
        let (script, output) = compile(directory.path(), &program);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let text = fs::read_to_string(script).unwrap();
        assert!(text.contains("for __execute_loop_"), "{binding}: {text}");
        assert!(
            !text.contains(&format!("for {binding} in")),
            "{binding}: {text}"
        );
        assert!(text.contains("__execute_loop_") && text.contains("%s"));
    }
}

#[test]
fn pipelines_fail_on_any_stage_and_refuse_shells_without_pipefail() {
    let directory = tempdir().unwrap();
    let (script, output) = compile(directory.path(), "run \"false\"\npipe to \"cat\"\n");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = fs::read_to_string(script).unwrap();
    assert!(
        text.contains("(set -o pipefail) 2>/dev/null || {"),
        "{text}"
    );
    assert!(text.contains("\nset -o pipefail\n"), "{text}");
    // Only the first stage gets a closed stdin; the second reads the pipe.
    assert!(text.contains("'false' < /dev/null | 'cat'"), "{text}");
    assert!(!text.contains("'cat' < /dev/null"), "{text}");
}

#[test]
fn single_commands_do_not_require_pipefail() {
    let directory = tempdir().unwrap();
    let (script, output) = compile(directory.path(), "run \"true\"\n");
    assert!(output.status.success());
    assert!(!fs::read_to_string(script).unwrap().contains("pipefail"));
}

#[test]
fn ordinary_commands_have_closed_stdin() {
    let directory = tempdir().unwrap();
    let (script, output) = compile(directory.path(), "run \"cat\"\n");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(fs::read_to_string(script)
        .unwrap()
        .contains("'cat' < /dev/null"));
}

#[test]
fn merged_stderr_is_refused_when_order_cannot_be_preserved() {
    let directory = tempdir().unwrap();
    let (_, output) = compile(
        directory.path(),
        "run \"sh\" with arguments [\"-c\", \"echo err >&2; echo out\"] and stderr merged\n",
    );
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("merged stderr"));
}

#[cfg(unix)]
#[test]
fn stderr_redirected_command_is_captured_when_followed_by_fresh_command() {
    let directory = tempdir().unwrap();
    assert_matches_execute(
        directory.path(),
        "run \"printf\" with arguments [\"first\"] and stderr to \"err.txt\"\nrun \"printf\" with arguments [\"second\"]\n",
    );
}

#[cfg(unix)]
#[test]
fn exit_prints_the_pipeline_value_and_keeps_its_status() {
    let directory = tempdir().unwrap();
    let (script, output) = compile(
        directory.path(),
        "run \"printf\" with arguments [\"kept\"]\nexit with status 4\n",
    );
    assert!(output.status.success());
    dash_check(&script);
    let sh = Command::new("dash").arg(&script).output().unwrap();
    assert_eq!(sh.status.code(), Some(4));
    assert_eq!(sh.stdout, b"kept");
}
