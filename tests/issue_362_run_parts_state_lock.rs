#![cfg(feature = "tooling")]
//! Two overlapping `execute run-parts --once` invocations sharing one state directory
//! must run each program once between them, and neither may lose the other's record.

use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use tempfile::TempDir;

fn save_program(text: &str, output: &str) -> String {
    format!("run \"printf\" with arguments [\"{text}\"]\nthen save to \"{output}\"\n")
}

fn spawn_run_parts(cwd: &Path, parts: &str, state: &str) -> std::process::Child {
    Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(cwd)
        .arg("run-parts")
        .args(["--once", state, "--json", parts])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

#[test]
fn overlapping_invocations_run_each_program_once_and_keep_every_record() {
    for round in 0..5 {
        let directory = TempDir::new().unwrap();
        let root = directory.path();
        let parts = root.join("parts");
        fs::create_dir(&parts).unwrap();
        fs::write(
            parts.join("01-first.program"),
            save_program("first", "first.txt"),
        )
        .unwrap();
        fs::write(
            parts.join("02-second.program"),
            save_program("second", "second.txt"),
        )
        .unwrap();
        let state_dir = format!("state{round}");

        let first = spawn_run_parts(root, "parts", &state_dir);
        let second = spawn_run_parts(root, "parts", &state_dir);
        let outputs = [
            first.wait_with_output().unwrap(),
            second.wait_with_output().unwrap(),
        ];

        let mut ran = 0;
        for output in &outputs {
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
            ran += report["summary"]["ran"].as_u64().unwrap();
        }
        assert_eq!(
            ran, 2,
            "round {round}: programs were run more than once or not at all"
        );

        let state = fs::read_to_string(root.join(&state_dir).join("state.json")).unwrap();
        assert!(state.contains("01-first.program"), "round {round}: {state}");
        assert!(
            state.contains("02-second.program"),
            "round {round}: {state}"
        );
    }
}
