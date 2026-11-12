//! Issue #310: a per-command timeout reports how long the group ran, from spawn to reap.
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
    fs::write(dir.path().join("worker"), "#!/usr/bin/env python3\nimport os, sys, time\nopen('pid', 'w').write(str(os.getpid()))\nopen('effect', 'a').write('started\\n')\nif sys.argv[1] == 'closed':\n os.close(1)\n time.sleep(30)\nelse:\n while True:\n  os.write(1, b'x' * 1000)\n  time.sleep(.01)\n").unwrap();
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

#[test]
fn per_command_timeout_catch_record_reports_elapsed_ms_from_spawn_to_reap() {
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
    let elapsed = caught["elapsed_ms"]
        .as_u64()
        .unwrap_or_else(|| panic!("elapsed_ms missing from timeout record: {caught}"));
    assert!(
        (1000..3000).contains(&elapsed),
        "a 1 s bound must report 1000 to 3000 ms, got {elapsed}: {caught}"
    );
}
