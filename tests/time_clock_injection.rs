//! The injectable clock behind `time.now`: `EXECUTE_NOW`, `run --now`,
//! the `execute test` sidecar `now` field, and record/replay determinism.
#![cfg(feature = "tooling")]

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

/// 2026-10-14T09:41:00Z: Wednesday, ISO week 42.
#[cfg(feature = "records")]
const PINNED: i64 = 1_791_970_860;
const RENDERED: &str = "clock=2026-10-14T09:41:00Z";
/// A different instant proving the pin wins: 2038-01-19T03:14:08Z.
const OTHER: i64 = 2_147_483_648;
const OTHER_RENDERED: &str = "clock=2038-01-19T03:14:08Z";

const CLOCK_PROGRAM: &str = "run time.now as now\n\
    take integer at \"/epoch_s\" from now as epoch\n\
    run time.format with epoch epoch and format \"%Y-%m-%dT%H:%M:%SZ\" and zone \"utc\" as summary\n\
    run \"printf\" with arguments [\"clock=%s\", summary]\n";

fn text(output: &[u8]) -> String {
    String::from_utf8_lossy(output).into_owned()
}

fn execute() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_execute"));
    // A pinned clock in the ambient environment would hide a missing pin, so
    // every case starts unpinned and opts in explicitly.
    command.env_remove("EXECUTE_NOW");
    command
}

fn run_program(directory: &Path, extra: &[&str], now_env: Option<i64>) -> Output {
    let program = directory.join("clock.program");
    fs::write(&program, CLOCK_PROGRAM).unwrap();
    let mut command = execute();
    command.arg("run").arg(&program);
    command.args(extra);
    if let Some(pinned) = now_env {
        command.env("EXECUTE_NOW", pinned.to_string());
    }
    command.output().unwrap()
}

fn write_suite(directory: &Path, sidecar: Option<&str>) {
    fs::write(directory.join("clock.test.program"), CLOCK_PROGRAM).unwrap();
    if let Some(sidecar) = sidecar {
        fs::write(directory.join("clock.test.json"), sidecar).unwrap();
    }
}

fn run_suite(directory: &Path, extra: &[&str]) -> Output {
    let mut command = execute();
    command.arg("test");
    command.args(extra);
    command.arg(directory);
    command.output().unwrap()
}

#[test]
fn sidecar_now_pins_the_clock_for_execute_test() {
    let directory = tempfile::tempdir().unwrap();
    write_suite(
        directory.path(),
        Some(r#"{"now": 1791970860, "expect": {"stdout": "clock=2026-10-14T09:41:00Z"}}"#),
    );
    let output = run_suite(directory.path(), &[]);
    assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
}

#[test]
fn global_now_flag_pins_tests_without_a_sidecar_pin() {
    let directory = tempfile::tempdir().unwrap();
    write_suite(
        directory.path(),
        Some(r#"{"expect": {"stdout": "clock=2026-10-14T09:41:00Z"}}"#),
    );
    let output = run_suite(directory.path(), &["--now", "1791970860"]);
    assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
}

#[test]
fn sidecar_now_overrides_the_global_flag() {
    let directory = tempfile::tempdir().unwrap();
    write_suite(
        directory.path(),
        Some(r#"{"now": 1791970860, "expect": {"stdout": "clock=2026-10-14T09:41:00Z"}}"#),
    );
    let output = run_suite(directory.path(), &["--now", "2147483648"]);
    assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
}

#[test]
fn run_now_flag_pins_the_clock() {
    let directory = tempfile::tempdir().unwrap();
    let output = run_program(directory.path(), &["--now", "1791970860"], None);
    assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
    assert_eq!(text(&output.stdout), RENDERED);
}

#[test]
fn run_now_flag_wins_over_the_environment() {
    let directory = tempfile::tempdir().unwrap();
    let output = run_program(directory.path(), &["--now", "1791970860"], Some(OTHER));
    assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
    assert_eq!(text(&output.stdout), RENDERED);
}

#[test]
fn ambient_execute_now_pins_the_clock() {
    let directory = tempfile::tempdir().unwrap();
    let output = run_program(directory.path(), &[], Some(OTHER));
    assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
    assert_eq!(text(&output.stdout), OTHER_RENDERED);
}

#[cfg(feature = "records")]
#[test]
fn replay_returns_the_recorded_time_when_the_clock_moves_on() {
    let directory = tempfile::tempdir().unwrap();
    let work = directory.path();
    let program = work.join("clock.program");
    fs::write(&program, CLOCK_PROGRAM).unwrap();
    let record = work.join("record");
    let recorded = execute()
        .current_dir(work)
        .arg("run")
        .arg(&program)
        .arg("--record")
        .arg(&record)
        .env("EXECUTE_NOW", PINNED.to_string())
        .output()
        .unwrap();
    assert_eq!(
        recorded.status.code(),
        Some(0),
        "{}",
        text(&recorded.stderr)
    );
    assert_eq!(text(&recorded.stdout), RENDERED);
    // The clock now reads a different instant, but the replay answers every
    // step from the trace, so the recorded time comes back.
    let replayed = execute()
        .current_dir(work)
        .arg("replay")
        .arg(&record)
        .env("EXECUTE_NOW", OTHER.to_string())
        .output()
        .unwrap();
    assert_eq!(
        replayed.status.code(),
        Some(0),
        "{}",
        text(&replayed.stderr)
    );
    assert_eq!(text(&replayed.stdout), RENDERED);
    assert!(
        text(&replayed.stderr).contains("0 processes launched"),
        "{}",
        text(&replayed.stderr)
    );
}
