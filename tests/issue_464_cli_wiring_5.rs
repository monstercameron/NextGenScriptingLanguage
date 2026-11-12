// Command-line wiring for issue #464 slice 5: `--hardening` on `service generate`, the
// `test` usage text that names `--filter`, and the rejected `--schedule-state` combinations.
// Each test runs the built binary with its default manifests. The argument errors used here
// are raised before any manifest is loaded, so they do not depend on the trust gate.

use std::path::Path;
use std::process::{Command, Output};

fn execute(directory: &Path, arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(directory)
        .args(arguments)
        .output()
        .unwrap()
}

fn describe(output: &Output) -> String {
    format!(
        "status {:?} stdout {:?} stderr {:?}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[test]
fn test_usage_names_the_filter_option() {
    let directory = tempfile::tempdir().unwrap();
    let output = execute(directory.path(), &["test", "--help"]);
    assert!(output.status.success(), "{}", describe(&output));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("--filter NAME"), "{stdout}");
}

#[test]
fn service_generate_rejects_an_unknown_hardening_mode() {
    let directory = tempfile::tempdir().unwrap();
    let output = execute(
        directory.path(),
        &[
            "service",
            "generate",
            "--hardening",
            "strict",
            "systemd",
            "workflow.program",
        ],
    );
    assert!(!output.status.success(), "{}", describe(&output));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("hardening must be `derived`, `baseline`, or `none`"),
        "{stderr}"
    );
}

#[test]
fn service_generate_refuses_hardening_with_a_manifest_catalog() {
    let directory = tempfile::tempdir().unwrap();
    let output = execute(
        directory.path(),
        &[
            "service",
            "generate",
            "--hardening",
            "none",
            "--manifest-catalog",
            "catalog.bin",
            "systemd",
            "workflow.program",
        ],
    );
    assert!(!output.status.success(), "{}", describe(&output));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--hardening cannot be combined with --manifest-catalog"),
        "{stderr}"
    );
}

#[test]
fn check_of_a_plain_program_prints_no_deprecation_warning() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("plain.program"), "run \"true\"\n").unwrap();
    let output = execute(directory.path(), &["check", "plain.program"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("applet_deprecated"),
        "{}",
        describe(&output)
    );
}

#[test]
fn schedule_state_is_refused_for_a_program_without_a_trigger() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("plain.program"), "run \"true\"\n").unwrap();
    let output = execute(
        directory.path(),
        &["run", "plain.program", "--schedule-state", "state.json"],
    );
    assert!(!output.status.success(), "{}", describe(&output));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--schedule-state applies only to triggered programs"),
        "{stderr}"
    );
}
