//! CLI coverage for hand-over, detached launch, interactive invocation,
//! daemon selection, and attach control.
use serde_json::Value;
use std::fs;
use std::io::{BufRead, BufReader};
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};
use tempfile::tempdir;

fn execute(args: &[&str], cwd: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_execute"))
        .args(args)
        .current_dir(cwd)
        .env_remove("EXECUTE_DAEMON")
        .env_remove("EXECUTE_DAEMON_ACCEPT_CONTEXT")
        .output()
        .unwrap()
}

fn write_program(dir: &Path, name: &str, source: &str) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, source).unwrap();
    path
}

struct KillOnDrop(std::process::Child);

impl Deref for KillOnDrop {
    type Target = std::process::Child;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for KillOnDrop {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

#[test]
fn hand_over_runs_the_child_and_returns_its_status() {
    let dir = tempdir().unwrap();
    let marker = dir.path().join("handover-marker");
    let command = if cfg!(windows) {
        format!(
            "\"C:/Windows/System32/WindowsPowerShell/v1.0/powershell.exe\" with arguments [\"-NoProfile\", \"-NonInteractive\", \"-Command\", {}]",
            serde_json::to_string(&format!(
                "Start-Sleep -Milliseconds 250; 'ok' | Set-Content -NoNewline '{}'",
                marker.display()
            ))
            .unwrap()
        )
    } else {
        format!(
            "\"/bin/sh\" with arguments [\"-c\", {}]",
            serde_json::to_string(&format!("sleep 0.25; printf ok > '{}'", marker.display()))
                .unwrap()
        )
    };
    let program = write_program(
        dir.path(),
        "handover.program",
        &format!("hand over to {command}\n"),
    );
    let output = execute(&["run", program.to_str().unwrap()], dir.path());
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        marker.exists(),
        "handed-over program did not finish and write its marker"
    );
    assert_eq!(fs::read_to_string(marker).unwrap(), "ok");
}

#[test]
fn detached_launch_returns_before_child_writes_its_marker() {
    let dir = tempdir().unwrap();
    let marker = dir.path().join("detached-marker");
    let command = if cfg!(windows) {
        format!(
            "\"C:/Windows/System32/WindowsPowerShell/v1.0/powershell.exe\" with arguments [\"-NoProfile\", \"-NonInteractive\", \"-Command\", {}]",
            serde_json::to_string(&format!(
                "Start-Sleep -Seconds 3; 'ok' | Set-Content -NoNewline '{}'",
                marker.display()
            ))
            .unwrap()
        )
    } else {
        format!(
            "\"/bin/sh\" with arguments [\"-c\", {}]",
            serde_json::to_string(&format!("sleep 3; printf ok > '{}'", marker.display())).unwrap()
        )
    };
    let program = write_program(
        dir.path(),
        "detached.program",
        &format!("launch detached {command}\nrun text.trim_final_lf with text \"parent-done\"\n"),
    );
    // The detached child inherits the parent's stdio. Reading the parent's pipes
    // would wait for that child to exit, so the parent's exit is timed with null stdio.
    let started = Instant::now();
    let status = Command::new(env!("CARGO_BIN_EXE_execute"))
        .args(["run", program.to_str().unwrap()])
        .current_dir(dir.path())
        .env_remove("EXECUTE_DAEMON")
        .env_remove("EXECUTE_DAEMON_ACCEPT_CONTEXT")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "detached launch parent failed: {status}");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "parent waited for detached child"
    );
    assert!(
        !marker.exists(),
        "child marker must be written after parent exits"
    );
    let deadline = Instant::now() + Duration::from_secs(15);
    while !marker.exists() {
        assert!(
            Instant::now() < deadline,
            "detached child did not write marker"
        );
        thread::sleep(Duration::from_millis(25));
    }
    assert_eq!(fs::read_to_string(marker).unwrap(), "ok");
}

#[test]
fn interactive_command_runs_with_inherited_redirected_stdio_without_a_terminal() {
    let dir = tempdir().unwrap();
    let command = if cfg!(windows) {
        "run \"C:/Windows/System32/WindowsPowerShell/v1.0/powershell.exe\" with arguments [\"-NoProfile\", \"-NonInteractive\", \"-Command\", \"Write-Output interactive-ran\"] interactively\n"
    } else {
        "run \"/bin/sh\" with arguments [\"-c\", \"printf 'interactive-ran\\\\n'\"] interactively\n"
    };
    let program = write_program(dir.path(), "interactive.program", command);
    // No pseudo-terminal is allocated: interactively still runs the native
    // command with the caller's redirected standard streams.
    let output = execute(&["run", program.to_str().unwrap()], dir.path());
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("interactive-ran"));
}

#[test]
fn daemon_option_accepts_run_locally_and_prints_notice() {
    let dir = tempdir().unwrap();
    let program = write_program(
        dir.path(),
        "daemon.program",
        "run text.trim_final_lf with text \"ok\"\n",
    );
    let output = Command::new(env!("CARGO_BIN_EXE_execute"))
        .args(["run", program.to_str().unwrap(), "--daemon"])
        .current_dir(dir.path())
        .env_remove("EXECUTE_DAEMON")
        .env_remove("EXECUTE_DAEMON_ACCEPT_CONTEXT")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout), "ok");
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .to_lowercase()
            .contains("daemon"),
        "expected documented local-run notice: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(feature = "live")]
#[test]
fn run_control_attach_reads_state_and_stops_the_run() {
    const PREFIX: &str = "execute: control endpoint ";
    let dir = tempdir().unwrap();
    let command = if cfg!(windows) {
        "run \"C:/Windows/System32/WindowsPowerShell/v1.0/powershell.exe\" with arguments [\"-NoProfile\", \"-NonInteractive\", \"-Command\", \"Start-Sleep -Seconds 30\"]\n"
    } else {
        "run \"/bin/sleep\" with arguments [\"30\"]\n"
    };
    let program = write_program(dir.path(), "controlled.program", command);
    let mut child = KillOnDrop(
        Command::new(env!("CARGO_BIN_EXE_execute"))
            // Mutating control actions need an agent policy or this explicit same-user
            // opt-in; without either, `attach --control stop` is refused.
            .args([
                "run",
                program.to_str().unwrap(),
                "--control",
                "--allow-unauthenticated",
            ])
            .current_dir(dir.path())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let stderr = child.stderr.take().unwrap();
    let (tx, rx) = mpsc::channel();
    let reader = thread::spawn(move || {
        let mut lines = Vec::new();
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            if let Some(endpoint) = line.strip_prefix(PREFIX) {
                let _ = tx.send(endpoint.to_owned());
            }
            lines.push(line);
        }
        lines
    });
    let endpoint = match rx.recv_timeout(Duration::from_secs(20)) {
        Ok(path) => PathBuf::from(path),
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            panic!("control endpoint not announced: {error}");
        }
    };
    let status_output = execute(&["attach", endpoint.to_str().unwrap()], dir.path());
    assert!(
        status_output.status.success(),
        "{}",
        String::from_utf8_lossy(&status_output.stderr)
    );
    let state: Value = serde_json::from_slice(&status_output.stdout).unwrap();
    assert!(
        state.get("runtime").is_some(),
        "attach omitted runtime state: {state}"
    );
    let stop = execute(
        &["attach", endpoint.to_str().unwrap(), "--control", "stop"],
        dir.path(),
    );
    assert!(
        stop.status.success(),
        "{}",
        String::from_utf8_lossy(&stop.stderr)
    );
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("stopped controlled run did not exit");
        }
        thread::sleep(Duration::from_millis(20));
    }
    let _ = child.wait();
    let _ = reader.join().unwrap();
    assert!(!endpoint.exists(), "endpoint should be removed after stop");
}
