#![cfg(unix)]
use flow::manifest::Registry;
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

fn registry(directory: &Path, binary: &str) -> Registry {
    fs::write(
        directory.join("worker.json"),
        serde_json::json!({
            "name": "worker",
            "binary": binary,
            "accepts_stdin": false,
            "arguments": []
        })
        .to_string(),
    )
    .unwrap();
    Registry::load(directory).unwrap()
}

fn executable(directory: &Path, name: &str, source: &str) {
    let path = directory.join(name);
    fs::write(&path, source).unwrap();
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
    )?;
    Ok(result.output)
}

fn assert_worker_reaped(directory: &Path) {
    let pid: libc::pid_t = fs::read_to_string(directory.join("pid"))
        .unwrap()
        .parse()
        .unwrap();
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
    let registry = registry(directory.path(), "./worker");

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
    let registry = registry(directory.path(), "./worker");

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
    let registry = registry(directory.path(), "./worker");

    // Allow worker startup under concurrent CI load; this checks cleanup, not latency.
    let error = match run(directory.path(), &registry, Duration::from_secs(2)) {
        Err(error) => error,
        Ok(_) => panic!("timed-out closed-stdout worker unexpectedly succeeded"),
    };
    assert!(format!("{error:#}").contains("timed out"));
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
    let registry = registry(directory.path(), "./worker");
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
        let registry = registry(directory.path(), "./worker");
        runners.push(thread::spawn(move || {
            run(directory.path(), &registry, Duration::from_secs(2))
        }));
    }

    let output_one = runners.remove(0).join().unwrap().unwrap();
    let output_two = runners.remove(0).join().unwrap().unwrap();
    assert_eq!(output_one, b"one\n");
    assert_eq!(output_two, b"two\n");
}
