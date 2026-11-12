//! All workers and effects are owned temporary fixtures; no host processes targeted.
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::{
    fs,
    io::Read,
    path::Path,
    process::{Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

#[path = "common/portable_shell.rs"]
mod portable_shell;

fn invoke(dir: &Path, args: &[&str]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(dir)
        .args(args)
        .args(["--manifest-dir", "manifests"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let until = Instant::now() + Duration::from_secs(5);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= until {
            let _ = child.kill();
            let _ = child.wait();
            panic!("test exceeded external timeout");
        }
        thread::sleep(Duration::from_millis(5));
    };
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_end(&mut stdout)
        .unwrap();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_end(&mut stderr)
        .unwrap();
    Output {
        status,
        stdout,
        stderr,
    }
}
fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("manifests")).unwrap();
    fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests/printf.json"),
        dir.path().join("manifests/printf.json"),
    )
    .unwrap();
    fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests/cat.json"),
        dir.path().join("manifests/cat.json"),
    )
    .unwrap();
    fs::write(dir.path().join("worker"), "#!/usr/bin/env python3\nimport os, sys, time\nopen('pid', 'w').write(str(os.getpid()))\nopen('effect', 'ab').write(b'started\\n')\nif sys.argv[1] == 'closed':\n os.close(1)\n time.sleep(30)\nelif sys.argv[1] == 'grandchild':\n import subprocess\n child = subprocess.Popen(['sleep', '30'])\n open('grandchild_pid', 'w').write(str(child.pid))\n time.sleep(30)\nelse:\n while True:\n  os.write(1, b'x' * 1000)\n  time.sleep(.01)\n").unwrap();
    #[cfg(unix)]
    fs::set_permissions(dir.path().join("worker"), fs::Permissions::from_mode(0o700)).unwrap();
    // The worker runs through the pinned interpreter on Windows; direct
    // execution with a shebang needs an executable bit it cannot provide.
    let manifest = if cfg!(windows) {
        let script = dir
            .path()
            .join("worker")
            .to_string_lossy()
            .replace('\\', "/");
        serde_json::json!({
            "name": "worker",
            "binary": portable_shell::python(),
            "accepts_stdin": true,
            "parameters": {"mode": {"type": "string", "required": true}},
            "arguments": [{"literal": script}, {"parameter": "mode"}]
        })
    } else {
        serde_json::json!({
            "name": "worker",
            "binary": "./worker",
            "accepts_stdin": true,
            "parameters": {"mode": {"type": "string", "required": true}},
            "arguments": [{"parameter": "mode"}]
        })
    };
    fs::write(
        dir.path().join("manifests/worker.json"),
        manifest.to_string(),
    )
    .unwrap();
    dir
}

fn assert_observed_children_reaped(output: &Output) {
    // The interpreter can correctly time out before Python reaches its PID-file
    // write. Completed spawn trace records identify native children directly.
    for event in String::from_utf8_lossy(&output.stderr)
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
    {
        if event["event"] != "spawn" {
            continue;
        }
        // Unix-only: signal 0 probes whether the reaped child leaked. `pid`
        // comes from a completed `spawn` event for an interpreter child.
        #[cfg(unix)]
        {
            let pid: i32 = event["pid"].as_u64().unwrap().try_into().unwrap();
            // SAFETY: signal 0 only probes whether `pid` exists; it sends no signal and touches no memory.
            let probe = unsafe { libc::kill(pid, 0) };
            assert_eq!(probe, -1, "owned child {pid} leaked");
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::ESRCH)
            );
        }
    }
}

fn observed_effect(dir: &Path) -> Option<Vec<u8>> {
    match fs::read(dir.join("effect")) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => panic!("reading owned effect: {error}"),
    }
}

#[test]
fn timeout_kills_owned_worker_with_closed_stdout_and_live_pipe() {
    for source in [
        "run worker with mode \"closed\"\n",
        "run worker with mode \"stream\"\npipe to cat\n",
    ] {
        let dir = fixture();
        fs::write(dir.path().join("case.program"), source).unwrap();
        let result = invoke(
            dir.path(),
            &["case.program", "--command-timeout-ms", "1000", "--trace"],
        );
        assert!(!result.status.success());
        assert!(
            String::from_utf8_lossy(&result.stderr).contains("timed out"),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_observed_children_reaped(&result);
    }
}
#[test]
fn timed_out_effect_is_uncertain_and_requires_explicit_retry_even_after_repair() {
    let dir = fixture();
    fs::write(
        dir.path().join("case.program"),
        "run worker with mode \"closed\"\n",
    )
    .unwrap();
    let first = invoke(
        dir.path(),
        &[
            "case.program",
            "--checkpoint",
            "journal",
            "--command-timeout-ms",
            "1000",
            "--trace",
        ],
    );
    assert!(!first.status.success());
    assert_observed_children_reaped(&first);
    let before = observed_effect(dir.path());
    if let Some(bytes) = &before {
        assert!(
            b"started\n".starts_with(bytes),
            "unexpected initial effect: {bytes:?}"
        );
    }
    fs::write(
        dir.path().join("case.program"),
        "run printf with message \"repaired\"\n",
    )
    .unwrap();
    let refused = invoke(
        dir.path(),
        &["resume", "case.program", "--checkpoint", "journal"],
    );
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("--retry-uncertain"));
    let repaired = invoke(
        dir.path(),
        &[
            "resume",
            "case.program",
            "--checkpoint",
            "journal",
            "--retry-uncertain",
        ],
    );
    assert!(
        repaired.status.success(),
        "{}",
        String::from_utf8_lossy(&repaired.stderr)
    );
    assert_eq!(repaired.stdout, b"repaired\n");
    assert_eq!(
        observed_effect(dir.path()),
        before,
        "repair replayed the timed-out effect"
    );
}
#[test]
fn per_command_timeout_kills_owned_worker_without_global_flag() {
    // Issue #310: one flaky call carries its own bound, with no CLI flag.
    let dir = fixture();
    fs::write(
        dir.path().join("case.program"),
        "run worker with mode \"closed\" and timeout 1 seconds\n",
    )
    .unwrap();
    let started = Instant::now();
    let result = invoke(dir.path(), &["case.program", "--trace"]);
    let elapsed = started.elapsed();
    assert!(!result.status.success());
    assert!(
        String::from_utf8_lossy(&result.stderr).contains("timed out"),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    // The 1 s bound fired instead of the worker's 30 s sleep.
    assert!(
        elapsed < Duration::from_secs(4),
        "per-command timeout did not bound the run: {elapsed:?}"
    );
    assert!(elapsed >= Duration::from_millis(800), "{elapsed:?}");
    assert_observed_children_reaped(&result);
}

#[test]
fn smaller_bound_wins_between_per_command_and_global_timeout() {
    // A 30 s global flag must not extend a 1 s declared bound.
    let dir = fixture();
    fs::write(
        dir.path().join("case.program"),
        "run worker with mode \"closed\" and timeout 1 seconds\n",
    )
    .unwrap();
    let result = invoke(
        dir.path(),
        &["case.program", "--command-timeout-ms", "30000"],
    );
    assert!(!result.status.success());
    assert!(
        String::from_utf8_lossy(&result.stderr).contains("timed out"),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    // A 1 s global flag still bounds a 30 s declared bound.
    fs::write(
        dir.path().join("case.program"),
        "run worker with mode \"closed\" and timeout 30 seconds\n",
    )
    .unwrap();
    let result = invoke(
        dir.path(),
        &["case.program", "--command-timeout-ms", "1000"],
    );
    assert!(!result.status.success());
    assert!(
        String::from_utf8_lossy(&result.stderr).contains("timed out"),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
}

#[test]
fn per_command_timeout_reports_typed_timeout_in_catch() {
    let dir = fixture();
    fs::write(
        dir.path().join("case.program"),
        "try\n  run worker with mode \"closed\" and timeout 1 seconds\ncatch failure\n  then encode failure as json\n",
    )
    .unwrap();
    let result = invoke(dir.path(), &["case.program"]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let caught: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(caught["kind"], "timeout", "{caught}");
}

#[test]
fn per_command_timeout_is_retried_only_with_the_opt_in() {
    // Without `even if uncertain` a timed-out effect refuses to run again.
    let dir = fixture();
    fs::write(
        dir.path().join("case.program"),
        "run worker with mode \"closed\" and timeout 1 seconds and retry up to 1 times\n",
    )
    .unwrap();
    let refused = invoke(dir.path(), &["case.program"]);
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("retry refused"),
        "{}",
        String::from_utf8_lossy(&refused.stderr)
    );
    assert_eq!(observed_effect(dir.path()).unwrap(), b"started\n");

    // With the opt-in the timeout runs again, then exhausts.
    fs::write(
        dir.path().join("case.program"),
        "run worker with mode \"closed\" and timeout 1 seconds and retry up to 1 times even if uncertain\n",
    )
    .unwrap();
    let exhausted = invoke(dir.path(), &["case.program"]);
    assert!(!exhausted.status.success());
    assert!(
        String::from_utf8_lossy(&exhausted.stderr).contains("retry exhausted after 2 attempts"),
        "{}",
        String::from_utf8_lossy(&exhausted.stderr)
    );
    assert_eq!(observed_effect(dir.path()).unwrap(), b"started\nstarted\n");
}

#[test]
fn timed_out_per_command_effect_is_uncertain_and_requires_explicit_retry() {
    let dir = fixture();
    fs::write(
        dir.path().join("case.program"),
        "run worker with mode \"closed\" and timeout 1 seconds\n",
    )
    .unwrap();
    let first = invoke(
        dir.path(),
        &["case.program", "--checkpoint", "journal", "--trace"],
    );
    assert!(!first.status.success());
    assert_observed_children_reaped(&first);
    let before = observed_effect(dir.path());
    assert!(before.is_some());
    fs::write(
        dir.path().join("case.program"),
        "run printf with message \"repaired\"\n",
    )
    .unwrap();
    let refused = invoke(
        dir.path(),
        &["resume", "case.program", "--checkpoint", "journal"],
    );
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("--retry-uncertain"));
    let repaired = invoke(
        dir.path(),
        &[
            "resume",
            "case.program",
            "--checkpoint",
            "journal",
            "--retry-uncertain",
        ],
    );
    assert!(
        repaired.status.success(),
        "{}",
        String::from_utf8_lossy(&repaired.stderr)
    );
    assert_eq!(repaired.stdout, b"repaired\n");
    assert_eq!(
        observed_effect(dir.path()),
        before,
        "repair replayed the timed-out effect"
    );
}

#[test]
fn per_command_timeout_appears_in_explain_json_with_ms() {
    let dir = fixture();
    fs::write(
        dir.path().join("case.program"),
        "run worker with mode \"closed\" and timeout 2 seconds\n",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(dir.path())
        .args(["explain", "case.program", "--json"])
        .args(["--manifest-dir", "manifests"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let plan: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let step = &plan["steps"][0];
    assert_eq!(step["timeout_ms"], 2000, "{step}");
}

// Unix-only: `sleep` is absent on stock Windows, and the grandchild probe
// below needs `kill(2)`. The portable worker tests above cover this host.
#[cfg(unix)]
#[test]
fn acceptance_shape_times_out_sleep_with_keep_status() {
    let dir = fixture();
    fs::write(
        dir.path().join("case.program"),
        "run \"sleep\" with arguments [5] and timeout 1 seconds and keep status as st\n",
    )
    .unwrap();
    let started = Instant::now();
    let result = invoke(dir.path(), &["case.program"]);
    let elapsed = started.elapsed();
    assert!(!result.status.success());
    assert!(
        String::from_utf8_lossy(&result.stderr).contains("timed out"),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(
        elapsed < Duration::from_secs(4),
        "the 1 s bound did not stop sleep 5 promptly: {elapsed:?}"
    );
}

#[cfg(unix)]
#[test]
fn per_command_timeout_kills_the_whole_process_group() {
    let dir = fixture();
    fs::write(
        dir.path().join("case.program"),
        "run worker with mode \"grandchild\" and timeout 1 seconds\n",
    )
    .unwrap();
    let result = invoke(dir.path(), &["case.program"]);
    assert!(!result.status.success());
    assert!(
        String::from_utf8_lossy(&result.stderr).contains("timed out"),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let pid: i32 = fs::read_to_string(dir.path().join("grandchild_pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    // SAFETY: signal 0 probes only; the pid comes from the owned fixture.
    assert_eq!(unsafe { libc::kill(pid, 0) }, -1, "grandchild {pid} leaked");
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH)
    );
}

#[test]
fn invalid_command_timeout_is_rejected_before_any_effect() {
    for options in [
        vec!["--command-timeout-ms", "0"],
        vec!["--command-timeout-ms", "oops"],
        vec!["--command-timeout-ms", "1", "--command-timeout-ms", "2"],
    ] {
        let dir = fixture();
        fs::write(
            dir.path().join("case.program"),
            "run worker with mode \"closed\"\n",
        )
        .unwrap();
        let mut args = vec!["case.program"];
        args.extend(options);
        assert!(!invoke(dir.path(), &args).status.success());
        assert!(!dir.path().join("effect").exists());
    }
}
