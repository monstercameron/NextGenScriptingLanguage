//! Two concurrent runs sharing one `--cache-dir` must both succeed.
//!
//! The cache maintenance lock is taken on every cached store and lookup. On Windows it is
//! an exclusive byte-range lock, so a second process that reaches it while the first holds
//! it must wait and retry rather than fail with a sharing violation (os error 32) or a
//! lock violation (os error 33). This test starts two processes against one cache
//! directory and checks that neither reports that failure.

use serde_json::json;
use std::fs;
use std::path::Path;
use std::process::{Command, Output};

#[path = "common/portable_shell.rs"]
mod portable_shell;

const RUNS_PER_PROCESS: usize = 12;

/// Writes a cacheable `stamp` applet that wraps the same printf binary and arguments as
/// `manifests/printf.json`. That repo manifest omits `cacheable`, and the cache-call check in
/// `src/runtime/shared.rs` rejects `and cache` for any applet that is not declared cacheable
/// ("applet ... is not declared cacheable"). Declaring the flag here is the only requirement:
/// the purity gate in `src/security.rs` (`authorize_cache_purity`) only applies when a
/// data-flow policy is active, and this program configures none.
fn write_manifest(manifests: &Path) {
    fs::create_dir_all(manifests).unwrap();
    fs::write(
        manifests.join("stamp.json"),
        serde_json::to_vec(&json!({
            "name": "stamp",
            "binary": portable_shell::shell_binary("printf"),
            "cacheable": true,
            "parameters": {"message": {"type": "string", "required": true}},
            "arguments": [{"literal": "%s\\n"}, {"parameter": "message"}]
        }))
        .unwrap(),
    )
    .unwrap();
}

fn write_program(path: &Path, tag: &str) {
    let mut script = String::new();
    for index in 0..RUNS_PER_PROCESS {
        script.push_str(&format!(
            "run stamp with message \"{tag}-{index}\" and cache\n"
        ));
    }
    fs::write(path, script).unwrap();
}

fn run_program(directory: &Path, program: &Path, manifests: &Path, cache: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(directory)
        .arg(program)
        .arg("--manifest-dir")
        .arg(manifests)
        .arg("--cache-dir")
        .arg(cache)
        .output()
        .unwrap()
}

fn is_sharing_failure(stderr: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    lower.contains("sharing violation")
        || lower.contains("used by another process")
        || lower.contains("another process has locked")
        || lower.contains("os error 32")
        || lower.contains("os error 33")
}

#[test]
fn two_processes_sharing_one_cache_dir_both_complete() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let manifests = root.join("manifests");
    let cache = root.join("cache");
    let first_program = root.join("first.program");
    let second_program = root.join("second.program");
    write_manifest(&manifests);
    write_program(&first_program, "first");
    write_program(&second_program, "second");

    let (first, second) = std::thread::scope(|scope| {
        let first = scope.spawn(|| run_program(root, &first_program, &manifests, &cache));
        let second = scope.spawn(|| run_program(root, &second_program, &manifests, &cache));
        (first.join().unwrap(), second.join().unwrap())
    });

    for (name, output) in [("first", &first), ("second", &second)] {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let one_line = stderr.replace('\n', " | ");
        assert!(
            output.status.success(),
            "expected success: {name} process exit {:?}; stderr: {}",
            output.status,
            one_line.chars().take(600).collect::<String>()
        );
        assert!(
            !is_sharing_failure(&stderr),
            "expected no cache lock contention: {name} stderr: {}",
            stderr.replace('\n', " | ")
        );
    }
}
