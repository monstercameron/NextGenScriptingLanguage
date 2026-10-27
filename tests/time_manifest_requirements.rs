//! Time applet contracts must reject incomplete calls before execution.
use serde_json::{json, Value};
use std::{fs, path::Path, process::Command};

fn execute(root: &Path, command: &str, source: &str, disk: bool) -> std::process::Output {
    let program = root.join("time.program");
    fs::write(&program, source).unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_execute"));
    cmd.arg(command).arg(&program).current_dir(root);
    if disk {
        cmd.arg("--manifest-dir")
            .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/manifests"));
    }
    cmd.output().unwrap()
}

#[test]
fn each_unconditional_time_parameter_is_required_during_check() {
    let cases = [
        ("run time.format with format \"%Y\"\n", "epoch"),
        ("run time.format with epoch 0\n", "format"),
        (
            "run time.parse with format \"%Y-%m-%dT%H:%M:%S%z\"\n",
            "text",
        ),
        (
            "run time.parse with text \"1970-01-01T00:00:00+0000\"\n",
            "format",
        ),
        ("run time.add with amount 1 and unit \"seconds\"\n", "epoch"),
        ("run time.add with epoch 0 and unit \"seconds\"\n", "amount"),
        ("run time.add with epoch 0 and amount 1\n", "unit"),
        ("run time.diff with right 0 and unit \"seconds\"\n", "left"),
        ("run time.diff with left 0 and unit \"seconds\"\n", "right"),
        ("run time.diff with left 0 and right 0\n", "unit"),
    ];
    let root = tempfile::tempdir().unwrap();
    for disk in [false, true] {
        for (source, missing) in cases {
            let result = execute(root.path(), "check", source, disk);
            assert!(!result.status.success(), "accepted {source:?}, disk={disk}");
            assert!(
                String::from_utf8_lossy(&result.stderr).contains(missing),
                "{source:?}: {}",
                String::from_utf8_lossy(&result.stderr)
            );
        }
    }
}

#[test]
fn complete_time_calls_check_and_run_with_optional_zone() {
    let cases = [
        ("run time.format with epoch 0 and format \"%Y-%m-%d %H:%M\"\n", json!("1970-01-01 00:00")),
        ("run time.format with epoch 0 and format \"%Y-%m-%d %H:%M\" and zone \"3600\"\n", json!("1970-01-01 01:00")),
        ("run time.parse with text \"1970-01-01T00:00:00+0000\" and format \"%Y-%m-%dT%H:%M:%S%z\"\n", json!(0)),
        ("run time.add with epoch 0 and amount 2 and unit \"minutes\"\n", json!(120)),
        ("run time.diff with left 120 and right 0 and unit \"minutes\"\n", json!(2)),
    ];
    let root = tempfile::tempdir().unwrap();
    for disk in [false, true] {
        for (source, expected) in &cases {
            for command in ["check", "run"] {
                let result = execute(root.path(), command, source, disk);
                assert!(
                    result.status.success(),
                    "{command} {source:?}, disk={disk}: {}",
                    String::from_utf8_lossy(&result.stderr)
                );
                if command == "run" {
                    if let Some(text) = expected.as_str() {
                        assert_eq!(
                            String::from_utf8_lossy(&result.stdout).trim_end(),
                            text,
                            "{source:?}, disk={disk}"
                        );
                    } else {
                        let actual: Value = serde_json::from_slice(&result.stdout).unwrap();
                        assert_eq!(&actual, expected, "{source:?}, disk={disk}");
                    }
                }
            }
        }
    }
}
