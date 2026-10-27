#![cfg(windows)]

use std::fs;
use std::process::Command;

#[test]
fn command_line_allowlist_filters_a_native_windows_child() {
    let directory = tempfile::tempdir().unwrap();
    let workflow = directory.path().join("environment.program");
    fs::write(
        &workflow,
        "run \"cmd.exe\" with arguments [\"/c\", \"if defined LANE_PRIVATE (echo leaked) else (echo clean) & echo %LANE_SAFE%\"]\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_execute"))
        .arg("run")
        .arg(&workflow)
        .args(["--env-allow", "PATH,SystemRoot,LANE_SAFE"])
        .env_remove("EXECUTE_CHILD_ENV_ALLOW")
        .env("LANE_SAFE", "yes")
        .env("LANE_PRIVATE", "private-windows-child-canary")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("clean"), "{stdout}");
    assert!(stdout.contains("yes"), "{stdout}");
    assert!(!stdout.contains("leaked"), "{stdout}");
    assert!(!stdout.contains("private-windows-child-canary"), "{stdout}");
}
