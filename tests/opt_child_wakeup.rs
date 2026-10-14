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

fn run(
    directory: &Path,
    registry: &Registry,
    source: &str,
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
            max_output_bytes: 1024,
            command_timeout: Some(Duration::from_secs(5)),
        },
        BTreeMap::new(),
        Vec::new(),
        Arc::new(AtomicBool::new(false)),
    )
}

#[test]
fn independent_contexts_receive_their_own_child_exit_wakeups() {
    let workers: Vec<_> = (0..2)
        .map(|_| {
            thread::spawn(|| {
                let directory = tempfile::tempdir().unwrap();
                let worker = directory.path().join("worker");
                fs::write(
                    &worker,
                    "#!/bin/sh\nexec 1>&-\nsleep 0.1\nprintf done > done\n",
                )
                .unwrap();
                fs::set_permissions(&worker, fs::Permissions::from_mode(0o700)).unwrap();
                let registry = registry(directory.path(), "./worker");

                let result = run(directory.path(), &registry, "run worker\n").unwrap();
                assert!(result.output.is_empty());
                fs::read_to_string(directory.path().join("done")).unwrap()
            })
        })
        .collect();

    for worker in workers {
        assert_eq!(worker.join().unwrap(), "done");
    }
}

#[test]
fn rapid_child_exits_do_not_lose_notifications_or_wait_forever() {
    let directory = tempfile::tempdir().unwrap();
    let worker = directory.path().join("worker");
    fs::write(&worker, "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&worker, fs::Permissions::from_mode(0o700)).unwrap();
    let registry = registry(directory.path(), "./worker");
    let source = "run worker\n".repeat(64);

    let result = run(directory.path(), &registry, &source).unwrap();

    assert_eq!(result.launched_processes, 64);
    assert!(result.output.is_empty());
}
