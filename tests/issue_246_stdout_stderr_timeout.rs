//! Issue #246: `stdout to stderr` keeps the output a command produced before it
//! timed out. The command writes `one`, then hangs past the timeout.
use std::fs;
use std::process::Command;

/// A direct shell call: `sh -c UNIX` on Unix, `cmd /C WINDOWS` on Windows.
fn shell(unix: &str, windows: &str) -> String {
    let (program, flag, script) = if cfg!(windows) {
        ("cmd", "/C", windows)
    } else {
        ("sh", "-c", unix)
    };
    format!(
        "run \"{program}\" with arguments [\"{flag}\", {}]",
        serde_json::to_string(script).unwrap()
    )
}

#[test]
fn stdout_to_stderr_keeps_output_written_before_a_timeout() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("case.program");
    let script = format!(
        "{} and stdout to stderr\n",
        shell("echo one; sleep 6", "echo one& ping -n 7 127.0.0.1 >nul")
    );
    fs::write(&path, script).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(directory.path())
        .arg("run")
        .arg(&path)
        .args(["--command-timeout-ms", "1500"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(!output.status.success(), "the command must time out");
    assert!(
        stderr.contains("one"),
        "stdout lost before the timeout: {stderr:?}"
    );
    assert!(
        stderr.contains("timed out"),
        "no timeout diagnostic: {stderr:?}"
    );
    assert!(output.stdout.is_empty());
}
