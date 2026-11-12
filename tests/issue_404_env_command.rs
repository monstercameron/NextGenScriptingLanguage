//! Issue #404 (env subcommand), #544 (--record with --checkpoint), #253 (help for
//! test and repair) and the help text for --schedule-state, --hardening and --filter.
//!
//! Tests that load the default manifests fail on the Windows development machine
//! with the trust-gate ACL message; they are kept and run in the clean export.

#[cfg(unix)]
use std::fs;
use std::process::Command;

fn execute() -> Command {
    Command::new(env!("CARGO_BIN_EXE_execute"))
}

#[test]
fn env_refuses_formats_other_than_bash() {
    let output = execute()
        .args(["env", "missing.program", "--format", "fish"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("env supports --format bash only"),
        "{stderr}"
    );
}

#[test]
fn test_and_repair_print_usage_and_exit_zero() {
    for name in ["test", "repair"] {
        let output = execute().args([name, "--help"]).output().unwrap();
        assert!(output.status.success(), "{name} --help failed");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.starts_with(&format!("Usage: execute {name}")),
            "{stdout}"
        );
    }
}

#[test]
fn top_level_help_lists_env_schedule_state_hardening_and_filter() {
    let output = execute().arg("--help").output().unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    for expected in [
        "execute env PROGRAM --format bash",
        "--schedule-state FILE",
        "--hardening MODE",
        "--filter NAME",
    ] {
        assert!(stdout.contains(expected), "missing {expected:?} in help");
    }
}

#[cfg(unix)]
#[test]
fn env_output_evaluates_in_sh_to_the_same_values() {
    let dir = tempfile::tempdir().unwrap();
    let json_path = dir.path().join("values.json");
    let note = "it's \"q\" $(id)\nnext";
    let json = serde_json::json!({"LANG": "C.UTF-8", "NOTE": note});
    fs::write(&json_path, json.to_string()).unwrap();
    let program_path = dir.path().join("locale.program");
    fs::write(
        &program_path,
        format!("produce json\nrun \"cat {}\"\n", json_path.display()),
    )
    .unwrap();

    let output = execute()
        .arg("env")
        .arg(&program_path)
        .args(["--format", "bash"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let lines = String::from_utf8(output.stdout).unwrap();

    let evaluated = Command::new("sh")
        .arg("-c")
        .arg("eval \"$1\"; printf '%s' \"$NOTE\"; printf '|%s' \"$LANG\"")
        .arg("sh")
        .arg(&lines)
        .output()
        .unwrap();
    assert!(evaluated.status.success());
    assert_eq!(
        String::from_utf8(evaluated.stdout).unwrap(),
        format!("{note}|C.UTF-8")
    );
}

#[cfg(unix)]
#[test]
fn record_with_checkpoint_runs_and_writes_a_record() {
    let dir = tempfile::tempdir().unwrap();
    let program_path = dir.path().join("finite.program");
    fs::write(&program_path, "run \"true\"\n").unwrap();
    let record_dir = dir.path().join("record");
    let checkpoint_dir = dir.path().join("checkpoint");

    let output = execute()
        .arg("run")
        .arg(&program_path)
        .arg("--record")
        .arg(&record_dir)
        .arg("--checkpoint")
        .arg(&checkpoint_dir)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(record_dir.exists());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("cannot be combined with checkpoints"),
        "{stderr}"
    );
}
