//! `parallel for each` with checkpoints: a group killed while one element is still
//! running resumes without running the elements that already committed. Each element
//! counts its own runs in a file, so a re-run shows up as a count above one.
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};
use tempfile::tempdir;

fn fixture() -> String {
    let name = if cfg!(windows) {
        "native-fixture.exe"
    } else {
        "native-fixture"
    };
    let path = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("examples")
        .join(name);
    assert!(
        path.is_file(),
        "build cargo build --example native-fixture before these tests"
    );
    // Forward slashes keep the path free of escapes in the program's string literal.
    path.to_str().unwrap().replace('\\', "/")
}

fn commit_lines(elements: &Path) -> usize {
    fs::read_to_string(elements)
        .unwrap_or_default()
        .lines()
        .filter(|line| line.contains("\"kind\":\"commit\""))
        .count()
}

fn count(root: &Path, element: &str) -> u32 {
    fs::read_to_string(root.join("counters").join(element))
        .unwrap_or_else(|_| "0".into())
        .trim()
        .parse()
        .unwrap()
}

fn execute(args: &[&str], directory: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_execute"))
        .args(args)
        .current_dir(directory)
        .output()
        .unwrap()
}

fn wait_until(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(90);
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_killed_parallel_group_resumes_without_rerunning_committed_elements() {
    let directory = tempdir().unwrap();
    let root: PathBuf = directory.path().to_path_buf();
    fs::create_dir(root.join("counters")).unwrap();
    fs::create_dir(root.join("gates")).unwrap();
    // Element "c" waits on its gate, so it is still running when the runner dies.
    fs::write(root.join("gates").join("c.hold"), b"").unwrap();
    let program = format!(
        "parallel for each item in [\"a\", \"b\", \"c\", \"d\", \"e\"] at most 2 as results\n  run \"{}\" with arguments [\"count-wait\", \"counters\", item, \"gates\"]\n",
        fixture()
    );
    fs::write(root.join("group.program"), program).unwrap();

    let elements = root.join("ck").join("elements.jsonl");
    let mut first = Command::new(env!("CARGO_BIN_EXE_execute"))
        .args(["run", "group.program", "--checkpoint", "ck"])
        .current_dir(&root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    // With two slots, a, b, then d and e commit while c waits.
    wait_until("four committed elements", || commit_lines(&elements) >= 4);
    first.kill().unwrap();
    first.wait().unwrap();

    assert_eq!(count(&root, "a"), 1);
    assert_eq!(count(&root, "b"), 1);
    assert_eq!(count(&root, "c"), 1, "c must have started before the kill");
    assert_eq!(count(&root, "d"), 1);
    assert_eq!(count(&root, "e"), 1);

    // Release c. Its begin record is uncertain, so resume needs the explicit override.
    fs::remove_file(root.join("gates").join("c.hold")).unwrap();
    let resumed = execute(
        &[
            "resume",
            "group.program",
            "--checkpoint",
            "ck",
            "--retry-uncertain",
        ],
        &root,
    );
    assert_eq!(
        resumed.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&resumed.stderr)
    );

    // Committed elements were restored from the log, not run again.
    for element in ["a", "b", "d", "e"] {
        assert_eq!(count(&root, element), 1, "element {element} re-ran");
    }
    // The element that never committed runs again.
    assert_eq!(count(&root, "c"), 2);
}
