#![cfg(unix)]
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;

fn fixture() -> (TempDir, PathBuf) {
    let directory = tempfile::tempdir().unwrap();
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests");
    (directory, manifest_dir)
}

fn invoke(directory: &Path, manifests: &Path, args: &[&str]) -> Output {
    let mut process = Command::new(env!("CARGO_BIN_EXE_execute"));
    process
        .current_dir(directory)
        .args(args)
        .arg("--manifest-dir")
        .arg(manifests);
    // This helper waits before collecting output. Temporary files keep trace
    // volume from filling a child pipe while preserving the bounded watchdog.
    let mut stdout = tempfile::tempfile().unwrap();
    let mut stderr = tempfile::tempfile().unwrap();
    let mut child = process
        .stdout(Stdio::from(stdout.try_clone().unwrap()))
        .stderr(Stdio::from(stderr.try_clone().unwrap()))
        .spawn()
        .unwrap();
    let status = wait_bounded(&mut child, Duration::from_secs(5)).unwrap_or_else(|| {
        unsafe {
            libc::kill(child.id() as libc::pid_t, libc::SIGTERM);
        }
        if wait_bounded(&mut child, Duration::from_secs(1)).is_none() {
            let _ = child.kill();
            let _ = child.wait();
        }
        panic!("workflow process exceeded five-second test deadline")
    });
    stdout.seek(SeekFrom::Start(0)).unwrap();
    stderr.seek(SeekFrom::Start(0)).unwrap();
    let mut stdout_bytes = Vec::new();
    let mut stderr_bytes = Vec::new();
    stdout.read_to_end(&mut stdout_bytes).unwrap();
    stderr.read_to_end(&mut stderr_bytes).unwrap();
    Output {
        status,
        stdout: stdout_bytes,
        stderr: stderr_bytes,
    }
}

fn wait_bounded(child: &mut Child, timeout: Duration) -> Option<ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            Ok(None) | Err(_) => return None,
        }
    }
}

fn collect_output(child: &mut Child, status: ExitStatus) -> Output {
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

struct ChildCleanup(Child);

impl Drop for ChildCleanup {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            unsafe {
                libc::kill(self.0.id() as libc::pid_t, libc::SIGTERM);
            }
            if wait_bounded(&mut self.0, Duration::from_secs(1)).is_none() {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
    }
}

#[test]
fn checkpointed_failure_resumes_suffix_without_repeating_saved_effect() {
    let (directory, manifests) = fixture();
    let workflow = directory.path().join("workflow.program");
    let checkpoint = directory.path().join("checkpoint");
    let saved = directory.path().join("effect.txt");
    fs::write(
        &workflow,
        "run printf with message \"checkpointed prefix\"\nthen save to file \"effect.txt\"\nthen fail\n",
    )
    .unwrap();

    let first = invoke(
        directory.path(),
        &manifests,
        &[
            workflow.to_str().unwrap(),
            "--checkpoint",
            checkpoint.to_str().unwrap(),
        ],
    );
    assert!(!first.status.success(), "the failure stage must fail");
    assert_eq!(fs::read(&saved).unwrap(), b"checkpointed prefix\n");

    // If resume replays the completed prefix, this sentinel will be replaced.
    fs::write(&saved, b"leave this completed effect alone\n").unwrap();
    fs::write(
        &workflow,
        "run printf with message \"checkpointed prefix\"\nthen save to file \"effect.txt\"\nthen printf with message \"recovered suffix\"\n",
    )
    .unwrap();

    let resumed = invoke(
        directory.path(),
        &manifests,
        &[
            "resume",
            workflow.to_str().unwrap(),
            "--checkpoint",
            checkpoint.to_str().unwrap(),
            "--trace",
        ],
    );
    assert!(
        resumed.status.success(),
        "{}",
        String::from_utf8_lossy(&resumed.stderr)
    );
    assert_eq!(resumed.stdout, b"recovered suffix\n");
    assert_eq!(
        fs::read(saved).unwrap(),
        b"leave this completed effect alone\n"
    );
}

#[test]
fn resume_restores_working_directory_and_named_binding() {
    let (directory, manifests) = fixture();
    let nested = directory.path().join("nested");
    fs::create_dir(&nested).unwrap();
    let workflow = directory.path().join("workflow.program");
    let checkpoint = directory.path().join("checkpoint");
    fs::write(
        &workflow,
        "run cd with path \"nested\"\nthen printf with message \"restored value\" as saved_text\nthen fail\n",
    )
    .unwrap();

    let failed = invoke(
        directory.path(),
        &manifests,
        &[
            workflow.to_str().unwrap(),
            "--checkpoint",
            checkpoint.to_str().unwrap(),
        ],
    );
    assert!(!failed.status.success());

    fs::write(
        &workflow,
        "run cd with path \"nested\"\nthen printf with message \"restored value\" as saved_text\nthen printf with message saved_text\nthen save to file \"resumed.txt\"\n",
    )
    .unwrap();
    let resumed = invoke(
        directory.path(),
        &manifests,
        &[
            "resume",
            workflow.to_str().unwrap(),
            "--checkpoint",
            checkpoint.to_str().unwrap(),
        ],
    );
    assert!(
        resumed.status.success(),
        "{}",
        String::from_utf8_lossy(&resumed.stderr)
    );
    assert_eq!(resumed.stdout, b"restored value\n\n");
    assert_eq!(
        fs::read(nested.join("resumed.txt")).unwrap(),
        b"restored value\n\n"
    );
    assert!(
        !directory.path().join("resumed.txt").exists(),
        "resume did not restore the completed cd state"
    );
}

#[test]
fn resume_rejects_changes_to_a_completed_prefix() {
    let (directory, manifests) = fixture();
    let workflow = directory.path().join("workflow.program");
    let checkpoint = directory.path().join("checkpoint");
    fs::write(
        &workflow,
        "run printf with message \"completed prefix\"\nthen fail\n",
    )
    .unwrap();

    let first = invoke(
        directory.path(),
        &manifests,
        &[
            workflow.to_str().unwrap(),
            "--checkpoint",
            checkpoint.to_str().unwrap(),
        ],
    );
    assert!(!first.status.success());

    fs::write(
        &workflow,
        "run printf with message \"changed prefix\"\nthen printf with message \"fixed suffix\"\n",
    )
    .unwrap();
    let resumed = invoke(
        directory.path(),
        &manifests,
        &[
            "resume",
            workflow.to_str().unwrap(),
            "--checkpoint",
            checkpoint.to_str().unwrap(),
            "--trace",
        ],
    );
    assert!(!resumed.status.success());
    let diagnostic = String::from_utf8_lossy(&resumed.stderr).to_lowercase();
    assert!(
        diagnostic.contains("prefix")
            || diagnostic.contains("changed")
            || diagnostic.contains("checkpoint"),
        "unexpected resume diagnostic: {diagnostic}"
    );
}

#[test]
fn comments_and_line_number_changes_do_not_invalidate_completed_nodes() {
    let (directory, manifests) = fixture();
    let workflow = directory.path().join("workflow.program");
    let checkpoint = directory.path().join("checkpoint");
    fs::write(
        &workflow,
        "run printf with message \"completed prefix\"\nthen fail\n",
    )
    .unwrap();
    let first = invoke(
        directory.path(),
        &manifests,
        &[
            workflow.to_str().unwrap(),
            "--checkpoint",
            checkpoint.to_str().unwrap(),
        ],
    );
    assert!(!first.status.success());

    fs::write(
        &workflow,
        "# whitespace and comment-only edits preserve node identity\n\nrun printf with message \"completed prefix\"\n# the failing suffix can be edited\nthen printf with message \"fixed suffix\"\n",
    )
    .unwrap();
    let resumed = invoke(
        directory.path(),
        &manifests,
        &[
            "resume",
            workflow.to_str().unwrap(),
            "--checkpoint",
            checkpoint.to_str().unwrap(),
        ],
    );
    assert!(
        resumed.status.success(),
        "{}",
        String::from_utf8_lossy(&resumed.stderr)
    );
    assert_eq!(resumed.stdout, b"fixed suffix\n");
}

#[test]
fn resume_rejects_a_changed_binary_in_a_completed_node() {
    let (directory, standard_manifests) = fixture();
    let manifest_dir = directory.path().join("custom-manifests");
    fs::create_dir(&manifest_dir).unwrap();
    fs::copy(
        standard_manifests.join("printf.json"),
        manifest_dir.join("printf.json"),
    )
    .unwrap();
    fs::copy(
        standard_manifests.join("false.json"),
        manifest_dir.join("false.json"),
    )
    .unwrap();
    let applet = directory.path().join("versioned-applet.sh");
    fs::write(&applet, "#!/bin/sh\nprintf 'before change\\n'\n").unwrap();
    fs::set_permissions(&applet, fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(
        manifest_dir.join("versioned.json"),
        serde_json::json!({
            "name": "versioned",
            "binary": applet,
            "arguments": []
        })
        .to_string(),
    )
    .unwrap();
    let workflow = directory.path().join("workflow.program");
    let checkpoint = directory.path().join("checkpoint");
    fs::write(&workflow, "run versioned\nthen fail\n").unwrap();

    let failed = invoke(
        directory.path(),
        &manifest_dir,
        &[
            workflow.to_str().unwrap(),
            "--checkpoint",
            checkpoint.to_str().unwrap(),
        ],
    );
    assert!(!failed.status.success());
    assert!(failed.stdout.is_empty());

    fs::write(&applet, "#!/bin/sh\nprintf 'after change\\n'\n").unwrap();
    fs::write(
        &workflow,
        "run versioned\nthen printf with message \"fixed suffix\"\n",
    )
    .unwrap();
    let resumed = invoke(
        directory.path(),
        &manifest_dir,
        &[
            "resume",
            workflow.to_str().unwrap(),
            "--checkpoint",
            checkpoint.to_str().unwrap(),
        ],
    );
    assert!(!resumed.status.success());
    assert!(resumed.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&resumed.stderr)
            .to_lowercase()
            .contains("completed checkpoint node"),
        "unexpected completed-binary diagnostic: {}",
        String::from_utf8_lossy(&resumed.stderr)
    );
}

#[test]
fn retry_uncertain_is_resume_only_and_resume_requires_checkpoint() {
    let (directory, manifests) = fixture();
    let workflow = directory.path().join("workflow.program");
    fs::write(&workflow, "run printf with message \"hello\"\n").unwrap();

    for args in [
        vec!["resume", workflow.to_str().unwrap()],
        vec![workflow.to_str().unwrap(), "--retry-uncertain"],
        vec![
            "check",
            workflow.to_str().unwrap(),
            "--checkpoint",
            "unused",
        ],
        vec!["run", workflow.to_str().unwrap(), "--retry-uncertain"],
    ] {
        let result = invoke(directory.path(), &manifests, &args);
        assert!(
            !result.status.success(),
            "unexpected success for args {args:?}"
        );
    }
}

#[test]
fn completed_checkpoint_resumes_without_repeating_output_and_fresh_run_wont_replace_it() {
    let (directory, manifests) = fixture();
    let workflow = directory.path().join("workflow.program");
    let checkpoint = directory.path().join("checkpoint");
    fs::write(&workflow, "run printf with message \"one time\"\n").unwrap();
    let fresh = invoke(
        directory.path(),
        &manifests,
        &[
            workflow.to_str().unwrap(),
            "--checkpoint",
            checkpoint.to_str().unwrap(),
        ],
    );
    assert!(
        fresh.status.success(),
        "{}",
        String::from_utf8_lossy(&fresh.stderr)
    );
    assert_eq!(fresh.stdout, b"one time\n");

    let repeated_fresh = invoke(
        directory.path(),
        &manifests,
        &[
            workflow.to_str().unwrap(),
            "--checkpoint",
            checkpoint.to_str().unwrap(),
        ],
    );
    assert!(!repeated_fresh.status.success());

    let resumed = invoke(
        directory.path(),
        &manifests,
        &[
            "resume",
            workflow.to_str().unwrap(),
            "--checkpoint",
            checkpoint.to_str().unwrap(),
            "--trace",
        ],
    );
    assert!(
        resumed.status.success(),
        "{}",
        String::from_utf8_lossy(&resumed.stderr)
    );
    assert_eq!(resumed.stdout, b"one time\n");
    assert!(
        !String::from_utf8_lossy(&resumed.stderr).contains("\"event\":\"spawn\""),
        "completed resume launched a child process: {}",
        String::from_utf8_lossy(&resumed.stderr)
    );
}

#[test]
fn checkpointed_fresh_runs_can_reuse_cached_transform_results() {
    let (directory, manifests) = fixture();
    let workflow = directory.path().join("workflow.program");
    let cache = directory.path().join("cache");
    let first_checkpoint = directory.path().join("first-checkpoint");
    let second_checkpoint = directory.path().join("second-checkpoint");
    fs::write(
        &workflow,
        "run printf with message \"cached checkpoint\"\nthen uppercase and cache\n",
    )
    .unwrap();

    let first = invoke(
        directory.path(),
        &manifests,
        &[
            workflow.to_str().unwrap(),
            "--checkpoint",
            first_checkpoint.to_str().unwrap(),
            "--cache-dir",
            cache.to_str().unwrap(),
            "--trace",
        ],
    );
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert_eq!(first.stdout, b"CACHED CHECKPOINT\n");

    let second = invoke(
        directory.path(),
        &manifests,
        &[
            workflow.to_str().unwrap(),
            "--checkpoint",
            second_checkpoint.to_str().unwrap(),
            "--cache-dir",
            cache.to_str().unwrap(),
            "--trace",
        ],
    );
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert_eq!(second.stdout, b"CACHED CHECKPOINT\n");
    assert!(
        String::from_utf8_lossy(&second.stderr).contains("cache_hit"),
        "expected cached stage to be reused: {}",
        String::from_utf8_lossy(&second.stderr)
    );
}

#[test]
fn resume_refuses_a_corrupted_checkpoint_blob() {
    let (directory, manifests) = fixture();
    let workflow = directory.path().join("workflow.program");
    let checkpoint = directory.path().join("checkpoint");
    fs::write(&workflow, "run printf with message \"durable bytes\"\n").unwrap();
    let fresh = invoke(
        directory.path(),
        &manifests,
        &[
            workflow.to_str().unwrap(),
            "--checkpoint",
            checkpoint.to_str().unwrap(),
        ],
    );
    assert!(
        fresh.status.success(),
        "{}",
        String::from_utf8_lossy(&fresh.stderr)
    );

    let journal: serde_json::Value =
        serde_json::from_slice(&fs::read(checkpoint.join("journal.json")).unwrap()).unwrap();
    let digest = journal["data"]["snapshot"]["output"]["sha256"]
        .as_str()
        .expect("journal missing snapshot output digest");
    let blob = checkpoint.join("blobs").join(digest);
    let mut bytes = fs::read(&blob).unwrap();
    assert!(!bytes.is_empty(), "expected non-empty checkpoint blob");
    bytes[0] ^= 0xff;
    fs::write(blob, bytes).unwrap();

    let resumed = invoke(
        directory.path(),
        &manifests,
        &[
            "resume",
            workflow.to_str().unwrap(),
            "--checkpoint",
            checkpoint.to_str().unwrap(),
        ],
    );
    assert!(!resumed.status.success());
    let diagnostic = String::from_utf8_lossy(&resumed.stderr).to_lowercase();
    assert!(
        diagnostic.contains("blob") || diagnostic.contains("checksum"),
        "{diagnostic}"
    );
}

#[test]
fn failed_native_pipe_group_restarts_from_frozen_read_input() {
    let (directory, standard_manifests) = fixture();
    let manifests = directory.path().join("custom-manifests");
    fs::create_dir(&manifests).unwrap();
    fs::copy(
        standard_manifests.join("upper.json"),
        manifests.join("upper.json"),
    )
    .unwrap();
    let applet = directory.path().join("partial-failure.sh");
    fs::write(
        &applet,
        "#!/bin/sh\ncat >/dev/null\nprintf partial\nexit 1\n",
    )
    .unwrap();
    fs::set_permissions(&applet, fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(
        manifests.join("failer.json"),
        serde_json::json!({
            "name": "failer",
            "binary": applet,
            "accepts_stdin": true,
            "arguments": []
        })
        .to_string(),
    )
    .unwrap();

    let workflow = directory.path().join("workflow.program");
    let checkpoint = directory.path().join("checkpoint");
    let input = directory.path().join("input.txt");
    fs::write(&input, "original input\n").unwrap();
    fs::write(
        &workflow,
        "read file \"input.txt\"\npipe to uppercase\npipe to failer\n",
    )
    .unwrap();
    let failed = invoke(
        directory.path(),
        &manifests,
        &[
            workflow.to_str().unwrap(),
            "--checkpoint",
            checkpoint.to_str().unwrap(),
        ],
    );
    assert!(!failed.status.success());
    assert!(
        failed.stdout.is_empty(),
        "partial pipe output escaped on failure"
    );

    // The completed read is restored from its content-addressed snapshot; the
    // failed suffix is one node, so uppercase must run again with those bytes.
    fs::write(&input, "changed input\n").unwrap();
    fs::write(&applet, "#!/bin/sh\nexec /bin/cat\n").unwrap();
    fs::set_permissions(&applet, fs::Permissions::from_mode(0o755)).unwrap();
    let resumed = invoke(
        directory.path(),
        &manifests,
        &[
            "resume",
            workflow.to_str().unwrap(),
            "--checkpoint",
            checkpoint.to_str().unwrap(),
        ],
    );
    assert!(
        resumed.status.success(),
        "{}",
        String::from_utf8_lossy(&resumed.stderr)
    );
    assert_eq!(resumed.stdout, b"ORIGINAL INPUT\n");
}

#[test]
fn active_checkpoint_is_exclusively_locked() {
    let (directory, manifests) = fixture();
    let workflow = directory.path().join("workflow.program");
    let checkpoint = directory.path().join("checkpoint");
    fs::write(
        &workflow,
        "every 60 seconds\n  run printf with message \"tick\"\n",
    )
    .unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(directory.path())
        .arg(&workflow)
        .arg("--manifest-dir")
        .arg(&manifests)
        .arg("--checkpoint")
        .arg(&checkpoint)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut child = ChildCleanup(child);

    let deadline = Instant::now() + Duration::from_secs(2);
    while !checkpoint.join("journal.json").is_file() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        checkpoint.join("journal.json").is_file(),
        "service never opened checkpoint"
    );

    let second = invoke(
        directory.path(),
        &manifests,
        &[
            workflow.to_str().unwrap(),
            "--checkpoint",
            checkpoint.to_str().unwrap(),
        ],
    );
    assert!(!second.status.success());
    assert!(
        String::from_utf8_lossy(&second.stderr)
            .to_lowercase()
            .contains("locked"),
        "expected exclusive lock error: {}",
        String::from_utf8_lossy(&second.stderr)
    );

    // Ensure the still-running scheduled service is cleaned up promptly.
    unsafe {
        libc::kill(child.0.id() as libc::pid_t, libc::SIGTERM);
    }
    let status = wait_bounded(&mut child.0, Duration::from_secs(3));
    assert!(status.is_some(), "checkpointed service ignored SIGTERM");
}

#[test]
fn uncertain_in_flight_invocation_requires_explicit_retry() {
    let (directory, _) = fixture();
    let manifest_dir = directory.path().join("custom-manifests");
    fs::create_dir(&manifest_dir).unwrap();
    let marker = directory.path().join("started.marker");
    let applet = directory.path().join("controlled-applet.sh");
    fs::write(
        &applet,
        format!(
            "#!/bin/sh\nif [ ! -f '{}' ]; then\n  : > '{}'\n  exec /bin/sleep 30\nfi\nprintf 'retried\\n'\n",
            marker.display(),
            marker.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&applet, fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(
        manifest_dir.join("controlled.json"),
        serde_json::json!({
            "name": "controlled",
            "binary": applet,
            "arguments": []
        })
        .to_string(),
    )
    .unwrap();
    let workflow = directory.path().join("workflow.program");
    let checkpoint = directory.path().join("checkpoint");
    fs::write(&workflow, "run controlled\n").unwrap();

    let child = Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(directory.path())
        .arg(&workflow)
        .arg("--manifest-dir")
        .arg(&manifest_dir)
        .arg("--checkpoint")
        .arg(&checkpoint)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut child = ChildCleanup(child);
    let deadline = Instant::now() + Duration::from_secs(3);
    while !marker.exists() && Instant::now() < deadline {
        if child.0.try_wait().unwrap().is_some() {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        marker.exists(),
        "controlled applet never reached its uncertain side effect"
    );

    unsafe {
        libc::kill(child.0.id() as libc::pid_t, libc::SIGTERM);
    }
    assert!(
        wait_bounded(&mut child.0, Duration::from_secs(3)).is_some(),
        "runner ignored cancellation during an applet invocation"
    );

    let automatic = invoke(
        directory.path(),
        &manifest_dir,
        &[
            "resume",
            workflow.to_str().unwrap(),
            "--checkpoint",
            checkpoint.to_str().unwrap(),
        ],
    );
    assert!(!automatic.status.success());
    assert!(
        String::from_utf8_lossy(&automatic.stderr)
            .to_lowercase()
            .contains("retry-uncertain"),
        "unexpected uncertain-resume diagnostic: {}",
        String::from_utf8_lossy(&automatic.stderr)
    );

    let retried = invoke(
        directory.path(),
        &manifest_dir,
        &[
            "resume",
            workflow.to_str().unwrap(),
            "--checkpoint",
            checkpoint.to_str().unwrap(),
            "--retry-uncertain",
        ],
    );
    assert!(
        retried.status.success(),
        "{}",
        String::from_utf8_lossy(&retried.stderr)
    );
    assert_eq!(retried.stdout, b"retried\n");
}

#[test]
fn scheduled_pending_tick_is_resumed_before_waiting_for_a_new_interval() {
    let (directory, _) = fixture();
    let manifest_dir = directory.path().join("custom-manifests");
    fs::create_dir(&manifest_dir).unwrap();
    let marker = directory.path().join("started.marker");
    let applet = directory.path().join("scheduled-applet.sh");
    fs::write(
        &applet,
        format!(
            "#!/bin/sh\nif [ ! -f '{}' ]; then\n  : > '{}'\n  exec /bin/sleep 30\nfi\nprintf 'resumed tick\\n'\n",
            marker.display(),
            marker.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&applet, fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(
        manifest_dir.join("scheduled.json"),
        serde_json::json!({
            "name": "scheduled",
            "binary": applet,
            "arguments": []
        })
        .to_string(),
    )
    .unwrap();
    let workflow = directory.path().join("scheduled.program");
    let checkpoint = directory.path().join("checkpoint");
    fs::write(&workflow, "every 2 seconds\n  run scheduled\n").unwrap();

    let child = Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(directory.path())
        .arg(&workflow)
        .arg("--manifest-dir")
        .arg(&manifest_dir)
        .arg("--checkpoint")
        .arg(&checkpoint)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut child = ChildCleanup(child);
    let deadline = Instant::now() + Duration::from_secs(4);
    while !marker.exists() && Instant::now() < deadline {
        if child.0.try_wait().unwrap().is_some() {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert!(marker.exists(), "scheduled tick never launched its applet");

    unsafe {
        libc::kill(child.0.id() as libc::pid_t, libc::SIGTERM);
    }
    assert!(
        wait_bounded(&mut child.0, Duration::from_secs(3)).is_some(),
        "scheduled runner ignored cancellation"
    );

    let started = Instant::now();
    let resumed = invoke(
        directory.path(),
        &manifest_dir,
        &[
            "resume",
            workflow.to_str().unwrap(),
            "--checkpoint",
            checkpoint.to_str().unwrap(),
            "--retry-uncertain",
            "--max-runs",
            "1",
        ],
    );
    let resume_elapsed = started.elapsed();
    assert!(
        resumed.status.success(),
        "{}",
        String::from_utf8_lossy(&resumed.stderr)
    );
    assert_eq!(resumed.stdout, b"resumed tick\n");
    assert!(
        resume_elapsed < Duration::from_millis(1500),
        "resume waited for a new two-second interval: {resume_elapsed:?}"
    );
}

#[test]
fn checkpointed_listener_replays_failed_record_before_reopening_stdin() {
    let (directory, manifests) = fixture();
    let workflow = directory.path().join("listener.program");
    let checkpoint = directory.path().join("checkpoint");
    fs::write(
        &workflow,
        "listen to stdin\nwhenever a line arrives as record\n  then fail\n",
    )
    .unwrap();

    let mut first = Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(directory.path())
        .arg(&workflow)
        .arg("--manifest-dir")
        .arg(&manifests)
        .arg("--checkpoint")
        .arg(&checkpoint)
        .arg("--max-runs")
        .arg("1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    first
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"original record\n")
        .unwrap();
    drop(first.stdin.take());
    let first_status = wait_bounded(&mut first, Duration::from_secs(5)).unwrap_or_else(|| {
        unsafe {
            libc::kill(first.id() as libc::pid_t, libc::SIGTERM);
        }
        if wait_bounded(&mut first, Duration::from_secs(1)).is_none() {
            let _ = first.kill();
            let _ = first.wait();
        }
        panic!("failed listener event exceeded five-second test deadline")
    });
    let first = collect_output(&mut first, first_status);
    assert!(
        !first.status.success(),
        "the first record handler must fail"
    );

    fs::write(
        &workflow,
        "listen to stdin\nwhenever a line arrives as record\n  then copy\n",
    )
    .unwrap();
    let resumed = invoke(
        directory.path(),
        &manifests,
        &[
            "resume",
            workflow.to_str().unwrap(),
            "--checkpoint",
            checkpoint.to_str().unwrap(),
            "--max-runs",
            "1",
        ],
    );
    assert!(
        resumed.status.success(),
        "{}",
        String::from_utf8_lossy(&resumed.stderr)
    );
    assert_eq!(resumed.stdout, b"original record\n");
}

#[test]
fn checkpoint_directory_symlinks_are_rejected_without_touching_the_target() {
    use std::os::unix::fs::symlink;
    for suffix in ["", "/", "/."] {
        let (directory, manifests) = fixture();
        let target = directory.path().join("owned-target");
        fs::create_dir(&target).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(target.join("sentinel"), "unchanged").unwrap();
        let checkpoint = directory.path().join("checkpoint-link");
        symlink(&target, &checkpoint).unwrap();
        let workflow = directory.path().join("workflow.program");
        fs::write(
            &workflow,
            "run \"printf\" with arguments [\"must not run\"]\n",
        )
        .unwrap();
        let path = format!("{}{suffix}", checkpoint.display());
        let result = invoke(
            directory.path(),
            &manifests,
            &[workflow.to_str().unwrap(), "--checkpoint", &path],
        );
        assert!(
            !result.status.success(),
            "accepted symlink suffix {suffix:?}"
        );
        assert!(result.stdout.is_empty());
        assert!(String::from_utf8_lossy(&result.stderr)
            .contains("checkpoint path must be a real directory"));
        assert_eq!(
            fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert_eq!(fs::read(target.join("sentinel")).unwrap(), b"unchanged");
        assert_eq!(fs::read_dir(&target).unwrap().count(), 1);
    }
}

#[test]
fn checkpoint_blob_directory_symlinks_are_rejected_for_fresh_and_resume() {
    use std::os::unix::fs::symlink;
    for resume in [false, true] {
        let (directory, manifests) = fixture();
        let checkpoint = directory.path().join("checkpoint");
        let target = directory.path().join("owned-target");
        let workflow = directory.path().join("workflow.program");
        fs::write(
            &workflow,
            "run \"printf\" with arguments [\"owned fixture\"]\n",
        )
        .unwrap();
        if resume {
            let first = invoke(
                directory.path(),
                &manifests,
                &[
                    workflow.to_str().unwrap(),
                    "--checkpoint",
                    checkpoint.to_str().unwrap(),
                ],
            );
            assert!(
                first.status.success(),
                "{}",
                String::from_utf8_lossy(&first.stderr)
            );
            fs::rename(checkpoint.join("blobs"), &target).unwrap();
        } else {
            fs::create_dir(&checkpoint).unwrap();
            fs::create_dir(&target).unwrap();
        }
        fs::write(target.join("sentinel"), "unchanged").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();
        symlink(&target, checkpoint.join("blobs")).unwrap();
        let mut before: Vec<_> = fs::read_dir(&target)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        before.sort();
        let mut args = vec![
            workflow.to_str().unwrap(),
            "--checkpoint",
            checkpoint.to_str().unwrap(),
        ];
        if resume {
            args.insert(0, "resume");
        }
        let result = invoke(directory.path(), &manifests, &args);
        assert!(!result.status.success());
        assert!(result.stdout.is_empty());
        assert!(String::from_utf8_lossy(&result.stderr)
            .contains("checkpoint blob path must be a real directory"));
        assert_eq!(
            fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o755
        );
        let mut after: Vec<_> = fs::read_dir(&target)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        after.sort();
        assert_eq!(before, after);
        assert_eq!(fs::read(target.join("sentinel")).unwrap(), b"unchanged");
    }
}
