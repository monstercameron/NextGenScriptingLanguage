//! Per-element checkpoints for top-level `for each` and `parallel for each`. An
//! interrupted loop resumes after its committed elements, and the elements that
//! already ran do not run again. A changed committed element refuses resume.
use std::fs;
use std::path::Path;
use std::process::{Command, Output};
use tempfile::tempdir;

fn execute(args: &[&str], directory: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_execute"))
        .args(args)
        .current_dir(directory)
        .output()
        .unwrap()
}

/// The element body: this binary runs `step.program` with the element as its one
/// argument. `step.program` reads the file named by the element and appends its
/// text to `log.txt`, so a missing file fails exactly that element.
fn write_step(directory: &Path) {
    fs::write(
        directory.join("step.program"),
        "accept word as text from argument 1\nread word\nthen append to \"log.txt\"\n",
    )
    .unwrap();
}

fn outer_program(list: &str) -> String {
    let execute_path = env!("CARGO_BIN_EXE_execute").replace('\\', "/");
    format!(
        "for each item in {list}\n  run \"{execute_path}\" with arguments [\"run\", \"step.program\", \"--\", item]\n"
    )
}

fn log_lines(directory: &Path) -> Vec<String> {
    fs::read_to_string(directory.join("log.txt"))
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

#[test]
fn interrupted_for_each_resumes_after_its_committed_elements() {
    let directory = tempdir().unwrap();
    write_step(directory.path());
    // The element named "b" has no file yet, so its run fails after "a" committed.
    fs::write(directory.path().join("a"), "A\n").unwrap();
    fs::write(directory.path().join("c"), "C\n").unwrap();
    fs::write(
        directory.path().join("outer.program"),
        outer_program("[\"a\", \"b\", \"c\"]"),
    )
    .unwrap();

    let first = execute(
        &["run", "outer.program", "--checkpoint", "ck"],
        directory.path(),
    );
    assert_ne!(first.status.code(), Some(0));
    assert_eq!(log_lines(directory.path()), vec!["A"]);

    // The failed element may have started, so resume needs the explicit override.
    fs::write(directory.path().join("b"), "B\n").unwrap();
    let refused = execute(
        &["resume", "outer.program", "--checkpoint", "ck"],
        directory.path(),
    );
    assert_ne!(refused.status.code(), Some(0));
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("interrupted side effect"),
        "stderr: {}",
        String::from_utf8_lossy(&refused.stderr)
    );

    let resumed = execute(
        &[
            "resume",
            "outer.program",
            "--checkpoint",
            "ck",
            "--retry-uncertain",
        ],
        directory.path(),
    );
    assert_eq!(
        resumed.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&resumed.stderr)
    );
    // "a" ran once: the resume restored its record instead of running it again.
    assert_eq!(log_lines(directory.path()), vec!["A", "B", "C"]);
}

#[test]
fn a_changed_committed_element_refuses_resume() {
    let directory = tempdir().unwrap();
    write_step(directory.path());
    fs::write(directory.path().join("a"), "A\n").unwrap();
    fs::write(
        directory.path().join("outer.program"),
        outer_program("[\"a\", \"b\"]"),
    )
    .unwrap();
    let first = execute(
        &["run", "outer.program", "--checkpoint", "ck"],
        directory.path(),
    );
    assert_ne!(first.status.code(), Some(0));

    // Changing the list changes the node, which refuses resume before any element runs.
    fs::write(
        directory.path().join("outer.program"),
        outer_program("[\"z\", \"b\"]"),
    )
    .unwrap();
    let refused = execute(
        &[
            "resume",
            "outer.program",
            "--checkpoint",
            "ck",
            "--retry-uncertain",
        ],
        directory.path(),
    );
    assert_ne!(refused.status.code(), Some(0));
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("changed"),
        "stderr: {}",
        String::from_utf8_lossy(&refused.stderr)
    );
}

#[cfg(unix)]
fn tree_contains(path: &Path, needle: &[u8]) -> bool {
    let Ok(entries) = fs::read_dir(path) else {
        return fs::read(path).is_ok_and(|bytes| bytes.windows(needle.len()).any(|w| w == needle));
    };
    entries
        .flatten()
        .any(|entry| tree_contains(&entry.path(), needle))
}

/// A sensitive element is refused at checkpoint persistence, as a node is, and its
/// output never reaches the element log or any other checkpoint file.
#[cfg(unix)]
#[test]
fn a_secret_element_is_refused_at_persistence_and_never_reaches_the_log() {
    use std::os::unix::fs::PermissionsExt;
    let directory = tempdir().unwrap();
    let root = directory.path();
    let producer = root.join("secret-producer");
    fs::write(&producer, "#!/bin/sh\nprintf 'TOP-SECRET-MARKER'\n").unwrap();
    fs::set_permissions(&producer, fs::Permissions::from_mode(0o755)).unwrap();
    fs::create_dir(root.join("manifests")).unwrap();
    let manifest = serde_json::json!({
        "name": "fixture.secret",
        "binary": producer,
        "parameters": {},
        "arguments": [],
        "contract": {
            "version": 1,
            "output": {"type": "text", "encoding": "raw"},
            "ports": {
                "inputs": {},
                "outputs": {
                    "value": {
                        "schema": {"type": "text"},
                        "delivery": "single",
                        "sensitivity": "secret"
                    }
                },
                "events": {}
            }
        }
    });
    fs::write(
        root.join("manifests").join("fixture.secret.json"),
        manifest.to_string(),
    )
    .unwrap();
    fs::write(
        root.join("secret.program"),
        "for each item in [\"a\", \"b\"]\n  run fixture.secret\n",
    )
    .unwrap();

    let output = execute(
        &[
            "run",
            "secret.program",
            "--checkpoint",
            "ck",
            "--manifest-dir",
            "manifests",
        ],
        root,
    );
    assert_ne!(output.status.code(), Some(0), "a secret element persisted");
    assert!(
        !tree_contains(&root.join("ck"), b"TOP-SECRET-MARKER"),
        "the secret reached the checkpoint directory"
    );
}

#[test]
fn resumed_for_each_counts_committed_elements_against_new_limit() {
    let directory = tempdir().unwrap();
    write_step(directory.path());
    fs::write(directory.path().join("a"), "A\n").unwrap();
    fs::write(directory.path().join("c"), "C\n").unwrap();
    fs::write(
        directory.path().join("outer.program"),
        outer_program("[\"a\", \"b\", \"c\"]"),
    )
    .unwrap();
    let first = execute(
        &[
            "run",
            "outer.program",
            "--checkpoint",
            "ck",
            "--max-loop-iterations",
            "3",
        ],
        directory.path(),
    );
    assert!(!first.status.success());
    assert_eq!(log_lines(directory.path()), ["A"]);
    fs::write(directory.path().join("b"), "B\n").unwrap();
    let limited = execute(
        &[
            "resume",
            "outer.program",
            "--checkpoint",
            "ck",
            "--retry-uncertain",
            "--max-loop-iterations",
            "1",
        ],
        directory.path(),
    );
    assert!(!limited.status.success());
    let message = String::from_utf8_lossy(&limited.stderr);
    assert!(
        message.contains("line 1") && message.contains("budget of 1"),
        "{message}"
    );
    assert_eq!(log_lines(directory.path()), ["A"]);
    let resumed = execute(
        &[
            "resume",
            "outer.program",
            "--checkpoint",
            "ck",
            "--retry-uncertain",
            "--max-loop-iterations",
            "3",
        ],
        directory.path(),
    );
    assert!(
        resumed.status.success(),
        "{}",
        String::from_utf8_lossy(&resumed.stderr)
    );
    assert_eq!(log_lines(directory.path()), ["A", "B", "C"]);
}
