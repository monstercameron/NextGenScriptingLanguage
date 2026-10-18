//! Named background jobs under checkpoints. A spawn and its await form one checkpoint
//! node. A run interrupted while a job is in flight leaves an uncertain node, which
//! resume refuses without `--retry-uncertain`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

fn execute(directory: &Path, arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(directory)
        .args(arguments)
        .stdin(Stdio::null())
        .output()
        .expect("start execute")
}

fn default_manifests() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests")
}

/// A native applet that blocks on its first attempt and succeeds on its retry. The
/// marker file records the first attempt.
fn controlled_manifests(directory: &Path, marker: &Path) -> PathBuf {
    let manifests = directory.join("custom-manifests");
    fs::create_dir(&manifests).unwrap();
    let (binary, arguments) = if cfg!(windows) {
        let script = directory.join("controlled.bat");
        fs::write(
            &script,
            format!(
                "@echo off\r\nif exist \"{m}\" goto retry\r\ntype nul > \"{m}\"\r\nping -n 30 127.0.0.1 > nul\r\nexit /b 0\r\n:retry\r\necho retried\r\n",
                m = marker.display()
            ),
        )
        .unwrap();
        (
            "cmd.exe".to_owned(),
            serde_json::json!([{"literal": "/d"}, {"literal": "/c"}, {"literal": script.to_string_lossy()}]),
        )
    } else {
        let script = directory.join("controlled.sh");
        fs::write(
            &script,
            format!(
                "#!/bin/sh\nif [ ! -f '{m}' ]; then\n  : > '{m}'\n  exec /bin/sleep 30\nfi\nprintf 'retried\\n'\n",
                m = marker.display()
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        }
        (
            "/bin/sh".to_owned(),
            serde_json::json!([{"literal": script.to_string_lossy()}]),
        )
    };
    fs::write(
        manifests.join("controlled.json"),
        serde_json::json!({"name": "controlled", "binary": binary, "arguments": arguments})
            .to_string(),
    )
    .unwrap();
    manifests
}

/// A child that is killed if the test panics before cleaning it up.
struct Cleanup(Child);

impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn wait_for(path: &Path, limit: Duration) -> bool {
    let deadline = Instant::now() + limit;
    while !path.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    path.exists()
}

fn interrupt(child: &mut Child) {
    #[cfg(unix)]
    // SAFETY: `child` is the `Cleanup`-owned child the test spawned. Nothing has reaped it, since
    // `wait_for` only polls a marker file, so `child.id()` still names this child. SIGTERM is a
    // valid signal number.
    unsafe {
        libc::kill(child.id() as libc::pid_t, libc::SIGTERM);
    }
    #[cfg(not(unix))]
    let _ = child.kill();
}

fn wait_bounded(child: &mut Child, limit: Duration) -> Option<ExitStatus> {
    let deadline = Instant::now() + limit;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return Some(status);
        }
        if Instant::now() >= deadline {
            return None;
        }
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn interrupted_job_is_an_uncertain_node_that_needs_explicit_retry() {
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("started.marker");
    let manifests = controlled_manifests(directory.path(), &marker);
    let workflow = directory.path().join("workflow.program");
    fs::write(&workflow, "spawn as work\n  run controlled\nawait work\n").unwrap();
    let checkpoint = directory.path().join("checkpoint");
    let manifests_text = manifests.to_string_lossy().into_owned();

    let child = Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(directory.path())
        .arg(&workflow)
        .args(["--manifest-dir", &manifests_text, "--checkpoint"])
        .arg(&checkpoint)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut child = Cleanup(child);
    assert!(
        wait_for(&marker, Duration::from_secs(15)),
        "the job never reached its uncertain side effect"
    );
    interrupt(&mut child.0);
    assert!(
        wait_bounded(&mut child.0, Duration::from_secs(20)).is_some(),
        "the run ignored interruption while a job was in flight"
    );

    let workflow_text = workflow.to_string_lossy().into_owned();
    let checkpoint_text = checkpoint.to_string_lossy().into_owned();
    let automatic = execute(
        directory.path(),
        &[
            "resume",
            &workflow_text,
            "--manifest-dir",
            &manifests_text,
            "--checkpoint",
            &checkpoint_text,
        ],
    );
    assert!(!automatic.status.success());
    assert!(
        String::from_utf8_lossy(&automatic.stderr)
            .to_lowercase()
            .contains("retry-uncertain"),
        "unexpected diagnostic: {}",
        String::from_utf8_lossy(&automatic.stderr)
    );

    let retried = execute(
        directory.path(),
        &[
            "resume",
            &workflow_text,
            "--manifest-dir",
            &manifests_text,
            "--checkpoint",
            &checkpoint_text,
            "--retry-uncertain",
        ],
    );
    assert!(
        retried.status.success(),
        "{}",
        String::from_utf8_lossy(&retried.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&retried.stdout).trim(), "retried");
}

#[test]
fn completed_job_checkpoint_matches_plain_run_and_resumes_without_repeating() {
    let directory = tempfile::tempdir().unwrap();
    let manifests = default_manifests();
    let manifests_text = manifests.to_string_lossy().into_owned();
    let workflow = directory.path().join("workflow.program");
    fs::write(
        &workflow,
        "spawn as work\n  run printf with message \"done\"\ninspect work as state\nawait work\n",
    )
    .unwrap();
    let workflow_text = workflow.to_string_lossy().into_owned();
    let checkpoint = directory.path().join("checkpoint");
    let checkpoint_text = checkpoint.to_string_lossy().into_owned();

    let plain = execute(
        directory.path(),
        &["run", &workflow_text, "--manifest-dir", &manifests_text],
    );
    assert!(
        plain.status.success(),
        "{}",
        String::from_utf8_lossy(&plain.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&plain.stdout), "done\n");

    let checkpointed = execute(
        directory.path(),
        &[
            "run",
            &workflow_text,
            "--manifest-dir",
            &manifests_text,
            "--checkpoint",
            &checkpoint_text,
        ],
    );
    assert!(
        checkpointed.status.success(),
        "{}",
        String::from_utf8_lossy(&checkpointed.stderr)
    );
    assert_eq!(checkpointed.stdout, plain.stdout);

    let resumed = execute(
        directory.path(),
        &[
            "resume",
            &workflow_text,
            "--manifest-dir",
            &manifests_text,
            "--checkpoint",
            &checkpoint_text,
        ],
    );
    assert!(
        resumed.status.success(),
        "{}",
        String::from_utf8_lossy(&resumed.stderr)
    );
    assert!(
        resumed.stdout.is_empty(),
        "committed job output must not replay"
    );
}
