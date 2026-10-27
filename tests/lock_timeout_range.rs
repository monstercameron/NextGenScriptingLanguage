use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use flow::applets::execute_in_cwd;
use serde_json::json;

const MESSAGE: &str = "lock_timeout_ms must be an integer from 0 through 300000";

fn invoke(dir: &Path, source: &str, command: &str, input: Option<&str>) -> Output {
    let program = dir.join("lock-timeout.program");
    fs::write(&program, source).unwrap();
    let mut process = Command::new(env!("CARGO_BIN_EXE_execute"));
    process
        .current_dir(dir)
        .arg(command)
        .arg(&program)
        .arg("--manifest-dir")
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests"));
    if let Some(input) = input {
        process.args(["--input", input]);
    }
    process.output().unwrap()
}

fn cases() -> [(&'static str, &'static str); 6] {
    [
        (
            "fs.write_atomic",
            "run fs.write_atomic with path \"out\" and expected_content_hash \"missing\" and symlink_policy \"reject\" and permissions_policy \"preserve\" and content \"x\"",
        ),
        (
            "fs.unlink_checked",
            "run fs.unlink_checked with path \"absent\" and expected_snapshot \"none\"",
        ),
        (
            "fs.transaction",
            "run fs.transaction with journal \"journal\" and operations []",
        ),
        (
            "fs.transaction.recover",
            "run fs.transaction.recover with journal \"journal\" and decision \"rollback\"",
        ),
        (
            "jsonc.set",
            "run jsonc.set with path \"out.jsonc\" and expected_content_hash \"missing\" and symlink_policy \"reject\" and permissions_policy \"preserve\" and key \"k\" and value null",
        ),
        (
            "jsonc.remove",
            "run jsonc.remove with path \"out.jsonc\" and expected_content_hash \"missing\" and symlink_policy \"reject\" and permissions_policy \"preserve\" and key \"k\"",
        ),
    ]
}

#[test]
fn all_filesystem_lock_timeouts_are_optional_and_inclusive() {
    let dir = tempfile::tempdir().unwrap();
    for (operation, prefix) in cases() {
        for source in [
            format!("{prefix}\n"),
            format!("{prefix} and lock_timeout_ms 0\n"),
            format!("{prefix} and lock_timeout_ms 300000\n"),
        ] {
            let output = invoke(dir.path(), &source, "check", None);
            assert!(
                output.status.success(),
                "check rejected valid {operation}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        for value in [-1, 300001] {
            let source = format!("{prefix} and lock_timeout_ms {value}\n");
            let output = invoke(dir.path(), &source, "check", None);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                !output.status.success() && stderr.contains(MESSAGE),
                "check accepted invalid {operation} timeout {value}: {stderr}"
            );
        }
    }
}

#[test]
fn resolved_lock_timeouts_are_rechecked_before_applet_side_effects() {
    let dir = tempfile::tempdir().unwrap();
    let sentinel = dir.path().join("must-survive");
    fs::write(&sentinel, "unchanged").unwrap();

    for (_, prefix) in cases() {
        let source = format!("accept timeout as integer\n{prefix} and lock_timeout_ms timeout\n");
        let checked = invoke(dir.path(), &source, "check", None);
        assert!(
            checked.status.success(),
            "check should defer a referenced timeout: {}",
            String::from_utf8_lossy(&checked.stderr)
        );

        for value in ["-1", "300001"] {
            let output = invoke(
                dir.path(),
                &source,
                "run",
                Some(&format!("timeout={value}")),
            );
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(!output.status.success(), "runtime accepted timeout {value}");
            assert!(stderr.contains(MESSAGE), "unexpected error: {stderr}");
            assert_eq!(fs::read_to_string(&sentinel).unwrap(), "unchanged");
        }
    }

    // Exercise both runtime validators with dynamically supplied inclusive
    // endpoints. These applets fail later because their fixture paths do not
    // exist, so success here means the timeout itself was accepted.
    for source in [
        "accept timeout as integer\nrun fs.unlink_checked with path \"missing\" and expected_snapshot \"none\" and lock_timeout_ms timeout\n",
        "accept timeout as integer\nrun fs.transaction.recover with journal \"missing-journal\" and decision \"rollback\" and lock_timeout_ms timeout\n",
    ] {
        assert!(invoke(dir.path(), source, "check", None).status.success());
        for value in ["0", "300000"] {
            let output = invoke(dir.path(), source, "run", Some(&format!("timeout={value}")));
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(!output.status.success(), "fixture unexpectedly succeeded");
            assert!(
                !stderr.contains(MESSAGE),
                "runtime rejected inclusive timeout {value}: {stderr}"
            );
        }
    }
    assert_eq!(fs::read_to_string(&sentinel).unwrap(), "unchanged");
}

#[test]
fn omitted_timeout_uses_the_filesystem_runtime_default() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("victim"), "content").unwrap();
    let snapshot = execute_in_cwd(
        "fs.snapshot",
        json!({"path":"victim"}).as_object().unwrap(),
        &[],
        dir.path(),
    )
    .unwrap()["snapshot"]
        .as_str()
        .unwrap()
        .to_owned();

    let removed = execute_in_cwd(
        "fs.unlink_checked",
        json!({"path":"victim", "expected_snapshot":snapshot})
            .as_object()
            .unwrap(),
        &[],
        dir.path(),
    )
    .unwrap();
    assert_eq!(removed["removed"], true);
    assert!(!dir.path().join("victim").exists());
}
