//! Binary-level coverage for runtime lifetimes, run options, and inspection.
use serde_json::Value;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

fn execute(cwd: &Path, args: &[&str]) -> Output {
    command(cwd).args(args).output().expect("start execute")
}

fn command(cwd: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_execute"));
    command.current_dir(cwd);
    command
}

/// A Windows path with forward slashes, safe to place inside a single-quoted
/// PowerShell string and inside a language text literal without escaping.
fn windows_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn quote(text: &str) -> String {
    serde_json::to_string(text).unwrap()
}

fn write_program(root: &Path, source: &str) -> PathBuf {
    let path = root.join("job.program");
    std::fs::write(&path, source).unwrap();
    path
}

fn success(output: &Output) {
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn wait_for(path: &Path, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if path.exists() {
            return true;
        }
        thread::sleep(Duration::from_millis(20));
    }
    path.exists()
}

fn long_sleep_source() -> String {
    if cfg!(windows) {
        "run \"C:/Windows/System32/WindowsPowerShell/v1.0/powershell.exe\" with arguments [\"-NoProfile\", \"-NonInteractive\", \"-Command\", \"Start-Sleep -Seconds 30\"]\n".into()
    } else {
        "run \"/bin/sh\" with arguments [\"-c\", \"sleep 30\"]\n".into()
    }
}

fn stop(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn triggers_repeat_and_schedule_is_inspectable() {
    let root = tempfile::tempdir().unwrap();
    let marker = root.path().join("ticks");
    let source = if cfg!(windows) {
        format!("every 1 seconds\n  run \"C:/Windows/System32/WindowsPowerShell/v1.0/powershell.exe\" with arguments [\"-NoProfile\", \"-NonInteractive\", \"-Command\", \"Add-Content -Path '{}' -Value tick\"]\n", windows_path(&marker))
    } else {
        format!("every 1 seconds\n  run \"/bin/sh\" with arguments [\"-c\", \"echo tick >> \\\"$1\\\"\", \"tick\", {}]\n", quote(&marker.to_string_lossy()))
    };
    let program = write_program(root.path(), &source);
    let mut child = command(root.path())
        .args(["run", program.to_str().unwrap()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    // Wait for a second tick, so the body is shown to repeat rather than run once.
    let deadline = Instant::now() + Duration::from_secs(8);
    let repeated = loop {
        let ticked_twice = std::fs::read_to_string(&marker)
            .map(|text| text.lines().count() >= 2)
            .unwrap_or(false);
        if ticked_twice {
            break true;
        }
        if Instant::now() >= deadline {
            break false;
        }
        thread::sleep(Duration::from_millis(50));
    };
    stop(&mut child);
    assert!(repeated, "interval body did not run");
    let ticks = std::fs::read_to_string(&marker).unwrap();
    assert!(
        ticks.lines().count() >= 2,
        "expected repeated body, got {ticks:?}"
    );

    let schedule = write_program(
        root.path(),
        "on schedule \"0 9 * * 1-5\"\n  run \"printf\" with arguments [\"scheduled\"]\n",
    );
    let report = execute(
        root.path(),
        &["explain", schedule.to_str().unwrap(), "--json"],
    );
    success(&report);
    let json: Value = serde_json::from_slice(&report.stdout).unwrap();
    let rendered = json.to_string();
    // Explain reports the body of an on-schedule program; it does not report the
    // trigger itself, so assert that the scheduled body is present.
    assert!(
        rendered.contains("scheduled"),
        "schedule body missing from explain: {json}"
    );
}

#[test]
fn stdin_listener_delivers_each_line() {
    let root = tempfile::tempdir().unwrap();
    let program = write_program(root.path(), "listen to stdin\nwhenever a line arrives as message\n  then \"printf\" with arguments [\"%s\\n\", message]\n");
    let mut child = command(root.path())
        .args(["run", program.to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"alpha\nbeta\n")
        .unwrap();
    let output = child.wait_with_output().unwrap();
    success(&output);
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(
        text.contains("alpha") && text.contains("beta"),
        "each input line should reach the handler: {text:?}"
    );
}

#[test]
fn review_confirm_checkpoint_and_inspection_cli() {
    let root = tempfile::tempdir().unwrap();
    let marker = root.path().join("effect.txt");
    let program = write_program(
        root.path(),
        &format!(
            "run \"printf\" with arguments [\"ran\"]\nthen save to {}\n",
            quote(&marker.to_string_lossy())
        ),
    );

    let reviewed = execute(
        root.path(),
        &["run", program.to_str().unwrap(), "--what-if"],
    );
    success(&reviewed);
    let report: Value = serde_json::from_slice(&reviewed.stdout).unwrap();
    assert!(report.get("plan").is_some());
    let fingerprint = report["fingerprint"].as_str().unwrap();
    assert!(!marker.exists(), "what-if must not execute the plan");

    let wrong = execute(
        root.path(),
        &[
            "run",
            program.to_str().unwrap(),
            "--confirm",
            "wrong-fingerprint",
        ],
    );
    assert!(!wrong.status.success());
    assert!(
        !marker.exists(),
        "a mismatched confirmation must refuse before effects"
    );
    let confirmed = execute(
        root.path(),
        &["run", program.to_str().unwrap(), "--confirm", fingerprint],
    );
    success(&confirmed);
    assert!(marker.exists());

    let check = execute(root.path(), &["check", program.to_str().unwrap()]);
    // A valid program passes `check` with exit status 0; the command prints nothing.
    success(&check);
    let explain = execute(
        root.path(),
        &["explain", program.to_str().unwrap(), "--json"],
    );
    success(&explain);
    let explained: Value = serde_json::from_slice(&explain.stdout).unwrap();
    assert!(explained.get("protocol").is_some());
    assert!(explained.get("steps").is_some());

    let checkpoint = root.path().join("checkpoint");
    let checkpoint_arg = checkpoint.to_string_lossy().into_owned();
    let checkpointed = write_program(root.path(), "run \"printf\" with arguments [\"done\"]\n");
    success(&execute(
        root.path(),
        &[
            "run",
            checkpointed.to_str().unwrap(),
            "--checkpoint",
            &checkpoint_arg,
        ],
    ));
    success(&execute(
        root.path(),
        &[
            "resume",
            checkpointed.to_str().unwrap(),
            "--checkpoint",
            &checkpoint_arg,
        ],
    ));
    let status = execute(
        root.path(),
        &["checkpoint", "status", &checkpoint_arg, "--json"],
    );
    success(&status);
    let summary: Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(summary["journal"]["status"], "completed");
}

#[test]
fn single_instance_lock_refuses_a_second_run() {
    let root = tempfile::tempdir().unwrap();
    let program = write_program(root.path(), &long_sleep_source());
    let lock = root.path().join("instance.lock");
    let mut first = command(root.path())
        .args([
            "run",
            program.to_str().unwrap(),
            "--single-instance",
            lock.to_str().unwrap(),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    thread::sleep(Duration::from_millis(500));
    let second = execute(
        root.path(),
        &[
            "run",
            program.to_str().unwrap(),
            "--single-instance",
            lock.to_str().unwrap(),
        ],
    );
    stop(&mut first);
    assert!(
        !second.status.success(),
        "second run must be refused while lock is held"
    );
}

#[test]
fn named_job_inspection_and_cancel_report_terminal_state() {
    let root = tempfile::tempdir().unwrap();
    let marker = root.path().join("started");
    let source = if cfg!(windows) {
        let path = windows_path(&marker);
        format!("spawn as worker\n  run \"C:/Windows/System32/WindowsPowerShell/v1.0/powershell.exe\" with arguments [\"-NoProfile\", \"-NonInteractive\", \"-Command\", \"Set-Content -Path '{path}' -Value started; Start-Sleep -Seconds 30\"]\nrun \"C:/Windows/System32/WindowsPowerShell/v1.0/powershell.exe\" with arguments [\"-NoProfile\", \"-NonInteractive\", \"-Command\", \"while (-not (Test-Path '{path}')) {{ Start-Sleep -Milliseconds 20 }}\"]\ninspect worker as before\ncancel worker\ninspect worker as after\nthen encode after as json\n")
    } else {
        format!("spawn as worker\n  run \"/bin/sh\" with arguments [\"-c\", \"touch \\\"$1\\\"; exec sleep 30\", \"job\", {}]\nrun \"/bin/sh\" with arguments [\"-c\", \"while [ ! -e \\\"$1\\\" ]; do sleep 0.02; done\", \"wait\", {}]\ninspect worker as before\ncancel worker\ninspect worker as after\nthen encode after as json\n", quote(&marker.to_string_lossy()), quote(&marker.to_string_lossy()))
    };
    let program = write_program(root.path(), &source);
    let output = execute(root.path(), &["run", program.to_str().unwrap()]);
    success(&output);
    let state: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(state["state"], "cancelled");
}
