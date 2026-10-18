//! Redirection: stderr to a file (replace or append), merged stderr, stdout to
//! stderr, `then append to`, and `and stdin from`. The failing command is this
//! test's own `execute` binary, which writes to stderr and exits nonzero.
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tempfile::TempDir;

fn manifests() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests")
}

/// The `execute` binary with forward slashes, so it parses as a quoted string.
fn execute_path() -> String {
    env!("CARGO_BIN_EXE_execute").replace('\\', "/")
}

fn run_script(directory: &Path, command: &str, script: &str) -> Output {
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

fn run(script: &str) -> (TempDir, Output) {
    let directory = tempfile::tempdir().unwrap();
    let output = run_script(directory.path(), "run", script);
    (directory, output)
}

/// The failing command: one missing-workflow error on stderr, exit 1.
fn failing_command(suffix: &str) -> String {
    failing_command_as(suffix, "code")
}

fn failing_command_as(suffix: &str, binding: &str) -> String {
    format!(
        "run \"{}\" with arguments [\"run\", \"missing.program\"] and keep status as {binding} {suffix}\n",
        execute_path()
    )
}

#[test]
fn stderr_to_file_replaces_it_and_stdout_is_unchanged() {
    let (directory, output) = run(&format!(
        "{}run printf with message \"code kept\"\n",
        failing_command("and stderr to \"err.txt\"")
    ));
    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    assert_eq!(String::from_utf8_lossy(&output.stdout), "code kept\n");
    let file = fs::read_to_string(directory.path().join("err.txt")).unwrap();
    assert!(file.contains("could not read workflow"), "{file}");
}

#[test]
fn stderr_appended_accumulates_across_commands() {
    let script = format!(
        "{}{}",
        failing_command_as("and stderr appended to \"log.txt\"", "first"),
        failing_command_as("and stderr appended to \"log.txt\"", "second")
    );
    let (directory, output) = run(&script);
    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    let file = fs::read_to_string(directory.path().join("log.txt")).unwrap();
    assert_eq!(file.matches("could not read workflow").count(), 2, "{file}");
}

#[test]
fn stderr_merged_moves_stderr_into_stdout() {
    let (_directory, output) = run(&failing_command("and stderr merged"));
    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("could not read workflow"),
        "{:?}",
        output.stdout
    );
    assert!(output.stderr.is_empty(), "{:?}", output.stderr);
}

#[test]
fn stdout_to_stderr_leaves_the_pipeline_empty() {
    let (_directory, output) = run(
        "run printf with message \"to-err\" and stdout to stderr\nrun printf with message \"after\"\n",
    );
    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    assert_eq!(String::from_utf8_lossy(&output.stdout), "after\n");
    assert_eq!(String::from_utf8_lossy(&output.stderr), "to-err\n");
}

#[test]
fn then_append_adds_to_the_file_and_keeps_the_value() {
    let (directory, output) = run(
        "run printf with message \"a\"\nthen append to \"log.txt\"\nrun printf with message \"b\"\nthen append to \"log.txt\"\n",
    );
    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    assert_eq!(String::from_utf8_lossy(&output.stdout), "b\n");
    let file = fs::read_to_string(directory.path().join("log.txt")).unwrap();
    assert_eq!(file, "a\nb\n");
}

#[test]
fn and_stdin_from_feeds_a_file_to_one_command() {
    let (_directory, output) = run(
        "run printf with message \"one\"\nthen save to \"in.txt\"\nrun count.lines and stdin from \"in.txt\"\n",
    );
    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    // wc pads its count on some platforms (macOS), so compare the number alone.
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "1");
}

#[test]
fn conflicting_or_piped_redirections_are_rejected_before_effects() {
    let directory = tempfile::tempdir().unwrap();
    let output = run_script(
        directory.path(),
        "check",
        "run printf with message \"x\" and stderr to \"a.txt\" and stderr merged\n",
    );
    assert_ne!(output.status.code(), Some(0));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("only one stderr or stdout redirection"),
        "{:?}",
        output.stderr
    );

    let output = run_script(
        directory.path(),
        "check",
        "run printf with message \"x\" and stdout to stderr\npipe to \"tr\" with arguments [\"a\", \"b\"]\n",
    );
    assert_ne!(output.status.code(), Some(0));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("single command"),
        "{:?}",
        output.stderr
    );
}

#[test]
fn explain_shows_the_redirection() {
    let directory = tempfile::tempdir().unwrap();
    let output = run_script(
        directory.path(),
        "explain",
        &failing_command("and stderr to \"err.txt\""),
    );
    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(
        text.contains("next command's stderr replaces file \"err.txt\""),
        "{text}"
    );
}

#[test]
fn a_failing_command_still_writes_its_stderr_to_the_file() {
    let directory = tempfile::tempdir().unwrap();
    let script = format!(
        "run \"{}\" with arguments [\"run\", \"missing.program\"] and stderr to \"err.txt\"\n",
        execute_path()
    );
    let output = run_script(directory.path(), "run", &script);
    assert_ne!(output.status.code(), Some(0));
    let file = fs::read_to_string(directory.path().join("err.txt")).unwrap();
    assert!(file.contains("could not read workflow"), "{file}");
}

#[cfg(unix)]
#[test]
fn stderr_redirect_does_not_stop_at_the_diagnostic_capture_limit() {
    let (directory, output) = run(
        "run \"sh\" with arguments [\"-c\", \"head -c 200000 /dev/zero >&2\"] and stderr to \"err.bin\"\n",
    );
    assert!(output.status.success(), "{:?}", output.stderr);
    let file = fs::read(directory.path().join("err.bin")).unwrap();
    assert_eq!(file.len(), 200_000);
}

#[cfg(unix)]
#[test]
fn merged_stderr_does_not_stop_at_the_diagnostic_capture_limit() {
    let (_directory, output) = run(
        "run \"sh\" with arguments [\"-c\", \"head -c 200000 /dev/zero >&2\"] and stderr merged as combined\nuse value combined\n",
    );
    assert!(output.status.success(), "{:?}", output.stderr);
    assert_eq!(output.stdout.len(), 200_000);
}
