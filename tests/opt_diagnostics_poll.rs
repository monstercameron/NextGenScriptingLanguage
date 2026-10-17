#![cfg(unix)]
use flow::manifest::Registry;
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

fn registry(directory: &Path, name: &str, binary: &str, accepts_stdin: bool) -> Registry {
    let manifest = serde_json::json!({
        "name": name,
        "binary": binary,
        "accepts_stdin": accepts_stdin,
        "arguments": []
    });
    fs::write(directory.join(format!("{name}.json")), manifest.to_string()).unwrap();
    Registry::load(directory).unwrap()
}

fn executable(directory: &Path, name: &str, source: &str) -> std::path::PathBuf {
    let path = directory.join(name);
    fs::write(&path, source).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    path
}

fn run(
    directory: &Path,
    registry: &Registry,
    source: &str,
    timeout: Duration,
    cancelled: Arc<AtomicBool>,
) -> anyhow::Result<flow::runtime::RunResult> {
    let program = flow::parser::parse(source)?;
    flow::runtime::run_with_context(
        &program,
        registry,
        &flow::runtime::RunOptions {
            inputs: Default::default(),
            working_directory: directory.to_path_buf(),
            cache_directory: None,
            trace: false,
            max_output_bytes: 1024 * 1024,
            command_timeout: Some(timeout),
            total_timeout: None,
        },
        BTreeMap::new(),
        Vec::new(),
        cancelled,
    )
}

fn make_stderr_worker(directory: &Path, exits_nonzero: bool) {
    let body = if exits_nonzero {
        r#"sys.stderr.buffer.write(b'diagnostic-begin\n')
sys.stderr.buffer.write(blob)
sys.stderr.buffer.flush()
sys.exit(7)
"#
    } else {
        r#"sys.stderr.buffer.write(blob)
sys.stderr.buffer.flush()
data = sys.stdin.buffer.read()
sys.stdout.buffer.write(data if data else b'from-first\n')
"#
    };
    let script = format!(
        r#"#!/usr/bin/env python3
import sys
blob = open('stderr-data', 'rb').read()
{body}"#
    );
    executable(directory, "worker", &script);
}

fn stderr_blob() -> Vec<u8> {
    vec![b'e'; 1024 * 1024]
}

#[test]
fn large_owned_stderr_is_drained_before_stdout_completion() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(directory.path().join("stderr-data"), stderr_blob()).unwrap();
    make_stderr_worker(directory.path(), false);
    let registry = registry(directory.path(), "worker", "./worker", false);

    let result = run(
        directory.path(),
        &registry,
        "run worker\n",
        Duration::from_secs(10),
        Arc::new(AtomicBool::new(false)),
    )
    .unwrap();

    assert_eq!(result.output, b"from-first\n");
}

#[test]
fn every_pipeline_stage_can_fill_stderr_before_writing_stdout() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(directory.path().join("stderr-data"), stderr_blob()).unwrap();
    make_stderr_worker(directory.path(), false);
    let registry = registry(directory.path(), "worker", "./worker", true);

    let result = run(
        directory.path(),
        &registry,
        "run worker\npipe to worker\n",
        Duration::from_secs(10),
        Arc::new(AtomicBool::new(false)),
    )
    .unwrap();

    assert_eq!(result.output, b"from-first\n");
}

#[test]
fn large_failure_stderr_keeps_the_bounded_diagnostic_prefix() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(directory.path().join("stderr-data"), stderr_blob()).unwrap();
    make_stderr_worker(directory.path(), true);
    let registry = registry(directory.path(), "worker", "./worker", false);

    let error = run(
        directory.path(),
        &registry,
        "run worker\n",
        Duration::from_secs(10),
        Arc::new(AtomicBool::new(false)),
    )
    .err()
    .expect("worker should fail");
    let error = format!("{error:#}");
    assert!(error.contains("failed"), "{error}");
    assert!(error.contains("diagnostic-begin"), "{error}");
    assert!(
        error.len() < 10_000,
        "diagnostic was not truncated: {}",
        error.len()
    );
}

fn closed_stderr_worker(directory: &Path) {
    executable(
        directory,
        "worker",
        "#!/bin/sh\nprintf '%s' \"$$\" > pid\nexec 2>&-\nprintf ready > ready\nexec sleep 30\n",
    );
}

fn assert_worker_reaped(directory: &Path) {
    let pid: libc::pid_t = fs::read_to_string(directory.join("pid"))
        .unwrap()
        .parse()
        .unwrap();
    // SAFETY: signal 0 delivers nothing. `pid` is the worker shell's PID, written to the `pid`
    // file before the worker runs `exec sleep 30`; exec keeps the PID, so the probe names the
    // worker process itself.
    assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH)
    );
}

#[test]
fn closed_stderr_on_live_stdout_remains_cancellable() {
    let directory = tempfile::tempdir().unwrap();
    closed_stderr_worker(directory.path());
    let registry = registry(directory.path(), "worker", "./worker", false);
    let cancelled = Arc::new(AtomicBool::new(false));
    let cancelled_thread = cancelled.clone();
    let ready = directory.path().join("ready");
    let cancellation_thread = thread::spawn(move || {
        let until = Instant::now() + Duration::from_secs(3);
        while !ready.exists() && Instant::now() < until {
            thread::sleep(Duration::from_millis(2));
        }
        cancelled_thread.store(true, Ordering::Relaxed);
    });

    let result = run(
        directory.path(),
        &registry,
        "run worker\n",
        Duration::from_secs(5),
        cancelled,
    );
    cancellation_thread.join().unwrap();
    let error = match result {
        Ok(_) => panic!("cancelled worker unexpectedly succeeded"),
        Err(error) => error,
    };
    assert!(format!("{error:#}").contains("workflow cancelled"));
    assert_worker_reaped(directory.path());
}

#[test]
fn closed_stderr_on_live_stdout_remains_subject_to_timeout() {
    let directory = tempfile::tempdir().unwrap();
    closed_stderr_worker(directory.path());
    let registry = registry(directory.path(), "worker", "./worker", false);

    let error = run(
        directory.path(),
        &registry,
        "run worker\n",
        // Allow the owned shell to start under parallel suite load so this
        // exercises closed-stderr supervision, rather than spawn scheduling.
        Duration::from_secs(2),
        Arc::new(AtomicBool::new(false)),
    )
    .err()
    .expect("worker should fail");

    assert!(format!("{error:#}").contains("timed out"));
    assert!(directory.path().join("ready").exists());
    assert_worker_reaped(directory.path());
}

#[test]
#[ignore = "manual headless run_with_context timing profile"]
fn profile_captured_one_mib_stderr_against_quiet_worker() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(directory.path().join("stderr-data"), stderr_blob()).unwrap();
    let noisy_script = "#!/usr/bin/env python3\nimport sys\nsys.stderr.buffer.write(open('stderr-data', 'rb').read())\nsys.stderr.buffer.flush()\nsys.stdout.buffer.write(b'ok\\n')\n";
    let quiet_script = "#!/usr/bin/env python3\nimport sys\nsys.stdout.buffer.write(b'ok\\n')\n";
    executable(directory.path(), "noisy", noisy_script);
    executable(directory.path(), "quiet", quiet_script);
    let noisy = registry(directory.path(), "noisy", "./noisy", false);
    let quiet = registry(directory.path(), "quiet", "./quiet", false);

    // Warm both paths, then alternate so startup/cache effects are shared.
    assert_eq!(
        run(
            directory.path(),
            &noisy,
            "run noisy\n",
            Duration::from_secs(10),
            Arc::new(AtomicBool::new(false))
        )
        .unwrap()
        .output,
        b"ok\n"
    );
    assert_eq!(
        run(
            directory.path(),
            &quiet,
            "run quiet\n",
            Duration::from_secs(10),
            Arc::new(AtomicBool::new(false))
        )
        .unwrap()
        .output,
        b"ok\n"
    );
    let mut noisy_elapsed = Duration::ZERO;
    let mut quiet_elapsed = Duration::ZERO;
    for _ in 0..3 {
        let started = Instant::now();
        let result = run(
            directory.path(),
            &noisy,
            "run noisy\n",
            Duration::from_secs(10),
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
        noisy_elapsed += started.elapsed();
        assert_eq!(result.output, b"ok\n");

        let started = Instant::now();
        let result = run(
            directory.path(),
            &quiet,
            "run quiet\n",
            Duration::from_secs(10),
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
        quiet_elapsed += started.elapsed();
        assert_eq!(result.output, b"ok\n");
    }
    eprintln!("three runs: 1 MiB stderr captured={noisy_elapsed:?}; quiet={quiet_elapsed:?}");
}
