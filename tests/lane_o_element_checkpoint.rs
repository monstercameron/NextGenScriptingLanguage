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
