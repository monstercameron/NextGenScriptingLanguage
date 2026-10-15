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

fn registry(directory: &Path, binary: &str) -> Registry {
    let manifest = serde_json::json!({
        "name": "worker",
        "binary": binary,
        "accepts_stdin": true,
        "arguments": []
    });
    fs::write(directory.join("worker.json"), manifest.to_string()).unwrap();
    Registry::load(directory).unwrap()
}

fn run_with_input(
    directory: &Path,
    registry: &Registry,
    source: &str,
    input: Vec<u8>,
    cancelled: Arc<AtomicBool>,
    command_timeout: Duration,
) -> anyhow::Result<Vec<u8>> {
    run_with_input_and_limit(
        directory,
        registry,
        source,
        input,
        cancelled,
        command_timeout,
        16 * 1024 * 1024,
    )
}

#[allow(clippy::too_many_arguments)]
fn run_with_input_and_limit(
    directory: &Path,
    registry: &Registry,
    source: &str,
    input: Vec<u8>,
    cancelled: Arc<AtomicBool>,
    command_timeout: Duration,
    max_output_bytes: usize,
) -> anyhow::Result<Vec<u8>> {
    let program = flow::parser::parse(source)?;
    let result = flow::runtime::run_with_context(
        &program,
        registry,
        &flow::runtime::RunOptions {
            inputs: Default::default(),
            working_directory: directory.to_path_buf(),
            cache_directory: None,
            trace: false,
            max_output_bytes,
            command_timeout: Some(command_timeout),
            total_timeout: None,
        },
        BTreeMap::new(),
        input,
        cancelled,
    )?;
    Ok(result.output)
}

#[test]
fn small_and_threaded_inputs_preserve_bytes_and_eof() {
    let directory = tempfile::tempdir().unwrap();
    let registry = registry(directory.path(), "/bin/cat");
    let threshold = libc::PIPE_BUF;

    for length in [1, threshold, threshold + 1, 2 * 1024 * 1024] {
        let input: Vec<_> = (0..length).map(|index| (index % 251) as u8).collect();
        let output = run_with_input(
            directory.path(),
            &registry,
            "then worker\n",
            input.clone(),
            Arc::new(AtomicBool::new(false)),
            Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(output, input, "input length {length}");
    }
}

#[test]
fn prefetched_input_flows_through_a_live_pipe_group() {
    let directory = tempfile::tempdir().unwrap();
    let registry = registry(directory.path(), "/bin/cat");
    let input: Vec<_> = (0..=libc::PIPE_BUF)
        .map(|index| (index % 239) as u8)
        .collect();

    let output = run_with_input(
        directory.path(),
        &registry,
        "then worker\npipe to worker\n",
        input.clone(),
        Arc::new(AtomicBool::new(false)),
        Duration::from_secs(5),
    )
    .unwrap();

    assert_eq!(output, input);
}

#[test]
fn empty_input_stays_null_and_ignored_input_does_not_block() {
    let directory = tempfile::tempdir().unwrap();
    let input_registry = registry(directory.path(), "/bin/cat");
    assert!(run_with_input(
        directory.path(),
        &input_registry,
        "run worker\n",
        Vec::new(),
        Arc::new(AtomicBool::new(false)),
        Duration::from_secs(5),
    )
    .unwrap()
    .is_empty());

    let worker = directory.path().join("ignore-input");
    fs::write(&worker, "#!/bin/sh\nprintf 'done\\n'\n").unwrap();
    fs::set_permissions(&worker, fs::Permissions::from_mode(0o700)).unwrap();
    let ignore_registry = registry(directory.path(), "./ignore-input");

    for length in [32, 2 * 1024 * 1024] {
        let output = run_with_input(
            directory.path(),
            &ignore_registry,
            "then worker\n",
            vec![b'x'; length],
            Arc::new(AtomicBool::new(false)),
            Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(output, b"done\n", "input length {length}");
    }
}

#[test]
fn cancellation_still_kills_a_child_after_small_input_prefill() {
    let directory = tempfile::tempdir().unwrap();
    let worker = directory.path().join("wait-after-closing-stdout");
    fs::write(
        &worker,
        "#!/bin/sh\nprintf '%s' \"$$\" > pid\nexec 1>&-\nexec sleep 30\n",
    )
    .unwrap();
    fs::set_permissions(&worker, fs::Permissions::from_mode(0o700)).unwrap();
    let registry = registry(directory.path(), "./wait-after-closing-stdout");
    let cancelled = Arc::new(AtomicBool::new(false));
    let cancel_for_thread = cancelled.clone();
    let pid_file = directory.path().join("pid");
    let cancellation_thread = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !pid_file.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        cancel_for_thread.store(true, Ordering::Relaxed);
    });

    let result = run_with_input(
        directory.path(),
        &registry,
        "then worker\n",
        b"small input".to_vec(),
        cancelled,
        Duration::from_secs(3),
    );
    cancellation_thread.join().unwrap();
    let error = match result {
        Ok(_) => panic!("cancelled command unexpectedly succeeded"),
        Err(error) => error,
    };
    assert!(format!("{error:#}").contains("workflow cancelled"));

    let pid: libc::pid_t = fs::read_to_string(directory.path().join("pid"))
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
fn stdout_eof_does_not_finish_a_still_running_child() {
    let directory = tempfile::tempdir().unwrap();
    let worker = directory.path().join("close-stdout-then-finish");
    fs::write(
        &worker,
        "#!/bin/sh\nexec 1>&-\nsleep 0.2\nprintf finished > finished\n",
    )
    .unwrap();
    fs::set_permissions(&worker, fs::Permissions::from_mode(0o700)).unwrap();
    let registry = registry(directory.path(), "./close-stdout-then-finish");

    let output = run_with_input(
        directory.path(),
        &registry,
        "then worker\n",
        b"small input".to_vec(),
        Arc::new(AtomicBool::new(false)),
        Duration::from_secs(3),
    )
    .unwrap();

    assert!(output.is_empty());
    assert_eq!(
        fs::read_to_string(directory.path().join("finished")).unwrap(),
        "finished"
    );
}

#[test]
fn nonzero_exit_after_stdout_eof_is_reported() {
    let directory = tempfile::tempdir().unwrap();
    let worker = directory.path().join("close-stdout-then-fail");
    fs::write(
        &worker,
        "#!/bin/sh\nexec 1>&-\nsleep 0.2\nprintf 'later failure\n' >&2\nexit 7\n",
    )
    .unwrap();
    fs::set_permissions(&worker, fs::Permissions::from_mode(0o700)).unwrap();
    let registry = registry(directory.path(), "./close-stdout-then-fail");

    let error = run_with_input(
        directory.path(),
        &registry,
        "then worker\n",
        b"small input".to_vec(),
        Arc::new(AtomicBool::new(false)),
        Duration::from_secs(3),
    )
    .unwrap_err();
    let error = format!("{error:#}");
    assert!(error.contains("failed"), "{error}");
    assert!(error.contains("later failure"), "{error}");
}

#[test]
fn output_limit_error_precedes_a_later_nonzero_exit_after_stdout_eof() {
    let directory = tempfile::tempdir().unwrap();
    let worker = directory.path().join("overflow-then-fail");
    fs::write(
        &worker,
        "#!/bin/sh\nprintf xx\nexec 1>&-\nsleep 0.2\nprintf 'later failure\n' >&2\nexit 7\n",
    )
    .unwrap();
    fs::set_permissions(&worker, fs::Permissions::from_mode(0o700)).unwrap();
    let registry = registry(directory.path(), "./overflow-then-fail");

    let error = run_with_input_and_limit(
        directory.path(),
        &registry,
        "then worker\n",
        b"small input".to_vec(),
        Arc::new(AtomicBool::new(false)),
        Duration::from_secs(3),
        1,
    )
    .unwrap_err();
    let error = format!("{error:#}");
    assert!(
        error.contains("output exceeds maximum of 1 bytes"),
        "{error}"
    );
    assert!(!error.contains("later failure"), "{error}");
}
