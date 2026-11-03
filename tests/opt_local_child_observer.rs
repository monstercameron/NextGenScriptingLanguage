use flow::manifest::Registry;
use std::collections::BTreeMap;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

#[path = "common/portable_shell.rs"]
mod portable_shell;

/// Manifest for a script worker: direct execution on Unix, through the given
/// interpreter on Windows (which cannot execute the script file itself).
fn script_registry(directory: &Path, script: &Path, interpreter: &str) -> Registry {
    if cfg!(windows) {
        let manifest = serde_json::json!({
            "name": "worker",
            "binary": interpreter,
            "accepts_stdin": false,
            "arguments": [{"literal": script.to_str().unwrap()}]
        });
        fs::write(directory.join("worker.json"), manifest.to_string()).unwrap();
    } else {
        let manifest = serde_json::json!({
            "name": "worker",
            "binary": script.to_string_lossy(),
            "accepts_stdin": false,
            "arguments": []
        });
        fs::write(directory.join("worker.json"), manifest.to_string()).unwrap();
    }
    Registry::load(directory).unwrap()
}

fn executable(directory: &Path, name: &str, source: &str) {
    let path = directory.join(name);
    fs::write(&path, source).unwrap();
    #[cfg(unix)]
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
}

fn run(directory: &Path, registry: &Registry, timeout: Duration) -> anyhow::Result<Vec<u8>> {
    run_with_cancellation(
        directory,
        registry,
        timeout,
        Arc::new(AtomicBool::new(false)),
    )
}

fn run_with_cancellation(
    directory: &Path,
    registry: &Registry,
    timeout: Duration,
    cancelled: Arc<AtomicBool>,
) -> anyhow::Result<Vec<u8>> {
    let program = flow::parser::parse("run worker\n")?;
    let result = flow::runtime::run_with_context(
        &program,
        registry,
        &flow::runtime::RunOptions {
            max_output_bytes: 1024 * 1024,
            command_timeout: Some(timeout),
            ..flow::runtime::RunOptions::new(directory.to_path_buf())
        },
        BTreeMap::new(),
        Vec::new(),
        cancelled,
    )?;
    Ok(result.output)
}

// Unix-only: signal 0 probes whether the reaped worker leaked. `pid` is the
// worker shell's PID, written to the `pid` file; `exec sleep` keeps it.
#[cfg(unix)]
fn assert_worker_reaped(directory: &Path) {
    let pid: libc::pid_t = fs::read_to_string(directory.join("pid"))
        .unwrap()
        .parse()
        .unwrap();
    // SAFETY: signal 0 only probes whether `pid` exists; it sends no signal and touches no memory.
    assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH)
    );
}

#[test]
fn fast_exit_is_collected_after_immediate_registration() {
    let directory = tempfile::tempdir().unwrap();
    executable(directory.path(), "worker", "#!/bin/sh\nprintf 'fast\\n'\n");
    let registry = script_registry(
        directory.path(),
        &directory.path().join("worker"),
        &portable_shell::shell_binary("sh"),
    );

    for _ in 0..8 {
        assert_eq!(
            run(directory.path(), &registry, Duration::from_secs(2)).unwrap(),
            b"fast\n"
        );
    }
}

#[test]
fn closed_stdout_does_not_hide_later_child_failure() {
    let directory = tempfile::tempdir().unwrap();
    executable(
        directory.path(),
        "worker",
        "#!/bin/sh\nexec 1>&-\nsleep 0.05\nexit 7\n",
    );
    let registry = script_registry(
        directory.path(),
        &directory.path().join("worker"),
        &portable_shell::shell_binary("sh"),
    );

    let error = match run(directory.path(), &registry, Duration::from_secs(2)) {
        Err(error) => error,
        Ok(_) => panic!("closed-stdout worker unexpectedly succeeded"),
    };
    let error = format!("{error:#}");
    assert!(error.contains("failed"), "{error}");
    assert!(error.contains("exit status"), "{error}");
}

#[test]
fn closed_stdout_live_child_still_times_out_and_is_reaped() {
    let directory = tempfile::tempdir().unwrap();
    executable(
        directory.path(),
        "worker",
        "#!/bin/sh\nprintf '%s' \"$$\" > pid\nexec 1>&-\nexec sleep 30\n",
    );
    let registry = script_registry(
        directory.path(),
        &directory.path().join("worker"),
        &portable_shell::shell_binary("sh"),
    );

    // Allow worker startup under concurrent CI load; this checks cleanup, not latency.
    let error = match run(directory.path(), &registry, Duration::from_secs(2)) {
        Err(error) => error,
        Ok(_) => panic!("timed-out closed-stdout worker unexpectedly succeeded"),
    };
    assert!(format!("{error:#}").contains("timed out"));
    #[cfg(unix)]
    assert_worker_reaped(directory.path());
}

#[test]
fn closed_stdout_live_child_observes_external_cancellation_and_is_reaped() {
    let directory = tempfile::tempdir().unwrap();
    executable(
        directory.path(),
        "worker",
        "#!/bin/sh\nprintf '%s' \"$$\" > pid\nexec 1>&-\n: > ready\nexec sleep 30\n",
    );
    let directory_path = directory.path().to_path_buf();
    let registry = script_registry(
        directory.path(),
        &directory.path().join("worker"),
        &portable_shell::shell_binary("sh"),
    );
    let cancelled = Arc::new(AtomicBool::new(false));
    let worker_cancelled = Arc::clone(&cancelled);
    let worker_path = directory_path.clone();
    let worker = thread::spawn(move || {
        let result = run_with_cancellation(
            &worker_path,
            &registry,
            Duration::from_secs(2),
            worker_cancelled,
        );
        (result, directory)
    });

    let ready_deadline = std::time::Instant::now() + Duration::from_secs(1);
    while !directory_path.join("ready").exists() && std::time::Instant::now() < ready_deadline {
        thread::sleep(Duration::from_millis(2));
    }
    let child_closed_stdout = directory_path.join("ready").exists();
    if child_closed_stdout {
        // Let the runner consume EOF and enter its no-stdout wait before canceling.
        thread::sleep(Duration::from_millis(50));
    }
    cancelled.store(true, std::sync::atomic::Ordering::Relaxed);
    let (result, _directory) = worker.join().unwrap();
    let error = match result {
        Err(error) => error,
        Ok(_) => panic!("cancelled closed-stdout worker unexpectedly succeeded"),
    };
    assert!(
        child_closed_stdout,
        "worker did not reach the closed-stdout state"
    );
    assert!(format!("{error:#}").contains("cancelled"), "{error:#}");
    #[cfg(unix)]
    assert_worker_reaped(&directory_path);
}

#[test]
fn concurrent_groups_use_independent_observers() {
    let mut runners = Vec::new();
    for label in ["one", "two"] {
        let directory = tempfile::tempdir().unwrap();
        executable(
            directory.path(),
            "worker",
            &format!("#!/usr/bin/env python3\nimport time\ntime.sleep(0.03)\nprint({label:?})\n"),
        );
        let registry = script_registry(
            directory.path(),
            &directory.path().join("worker"),
            &portable_shell::python(),
        );
        runners.push(thread::spawn(move || {
            run(directory.path(), &registry, Duration::from_secs(2))
        }));
    }

    let output_one = runners.remove(0).join().unwrap().unwrap();
    let output_two = runners.remove(0).join().unwrap().unwrap();
    assert_eq!(output_one, b"one\n");
    assert_eq!(output_two, b"two\n");
}
