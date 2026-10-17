#![cfg(unix)]
//! All workers and effects are owned temporary fixtures; no host processes targeted.
use std::{
    fs,
    io::Read,
    os::unix::fs::PermissionsExt,
    path::Path,
    process::{Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

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
    fs::write(dir.path().join("worker"), "#!/usr/bin/env python3\nimport os, sys, time\nopen('pid', 'w').write(str(os.getpid()))\nopen('effect', 'a').write('started\\n')\nif sys.argv[1] == 'closed':\n os.close(1)\n time.sleep(30)\nelse:\n while True:\n  os.write(1, b'x' * 1000)\n  time.sleep(.01)\n").unwrap();
    fs::set_permissions(dir.path().join("worker"), fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(dir.path().join("manifests/worker.json"), r#"{"name":"worker","binary":"./worker","accepts_stdin":true,"parameters":{"mode":{"type":"string","required":true}},"arguments":[{"parameter":"mode"}]}"#).unwrap();
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
        let pid: i32 = event["pid"].as_u64().unwrap().try_into().unwrap();
        assert_eq!(
            // SAFETY: signal 0 delivers nothing. `pid` comes from a completed `spawn` event that
            // the interpreter logged for a child it started, so it names an interpreter child.
            unsafe { libc::kill(pid, 0) },
            -1,
            "owned child {pid} leaked"
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
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
