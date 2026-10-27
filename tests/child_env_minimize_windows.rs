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

#[test]
fn cmd_crlf_argument_matches_native_command_baseline() {
    let directory = tempfile::tempdir().unwrap();
    let workflow = directory.path().join("multiline.program");
    let command_text = "echo first\r\necho second";
    fs::write(
        &workflow,
        format!(
            "run \"cmd.exe\" with arguments [\"/d\", \"/c\", {}]\n",
            serde_json::to_string(command_text).unwrap()
        ),
    )
    .unwrap();

    // cmd.exe has its own /c command parsing, which is not the MSVC argv
    // parser. Compare Execute's child with Rust's ordinary Windows Command
    // encoder so the runtime preserves cmd.exe's actual multiline behavior.
    let baseline = Command::new("cmd.exe")
        .args(["/d", "/c", command_text])
        .current_dir(directory.path())
        .output()
        .unwrap();
    assert!(
        baseline.status.success(),
        "direct cmd.exe baseline failed: {}",
        String::from_utf8_lossy(&baseline.stderr)
    );
    let baseline_stdout = String::from_utf8_lossy(&baseline.stdout);
    assert_eq!(
        baseline_stdout.lines().next(),
        Some("first"),
        "direct cmd.exe baseline produced unexpected output: {baseline_stdout:?}"
    );

    let output = Command::new(env!("CARGO_BIN_EXE_execute"))
        .arg("run")
        .arg(&workflow)
        .current_dir(directory.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.status.code(), baseline.status.code());
    assert_eq!(output.stdout, baseline.stdout);
}
