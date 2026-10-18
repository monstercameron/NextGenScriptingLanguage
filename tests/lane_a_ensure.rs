//! Idempotent in-process ensure operations. Each test runs in an owned temporary
//! directory. Mode and symbolic-link checks are Unix-only.

use flow::applets::{execute_in_cwd, AppletError};
use serde_json::{json, Map, Value};
use std::fs;
use std::path::Path;

fn args(value: Value) -> Map<String, Value> {
    value.as_object().cloned().unwrap_or_default()
}

fn run(operation: &str, arguments: Value, cwd: &Path) -> Result<Value, AppletError> {
    execute_in_cwd(operation, &args(arguments), &[], cwd)
}

fn changed(value: &Value) -> bool {
    value["changed"].as_bool().expect("changed is a boolean")
}

#[test]
fn ensure_file_creates_then_is_a_no_op() {
    let dir = tempfile::tempdir().unwrap();
    let first = run(
        "fs.ensure_file",
        json!({"path": "conf.txt", "content": "alpha\n"}),
        dir.path(),
    )
    .unwrap();
    assert!(changed(&first));
    assert_eq!(
        fs::read_to_string(dir.path().join("conf.txt")).unwrap(),
        "alpha\n"
    );

    // An unchanged ensure takes no lock, so it creates no lock file.
    for entry in fs::read_dir(dir.path()).unwrap().filter_map(Result::ok) {
        if entry
            .file_name()
            .to_string_lossy()
            .contains("execute-applet")
        {
            fs::remove_file(entry.path()).unwrap();
        }
    }
    let second = run(
        "fs.ensure_file",
        json!({"path": "conf.txt", "content": "alpha\n"}),
        dir.path(),
    )
    .unwrap();
    assert!(!changed(&second));
    assert!(!fs::read_dir(dir.path())
        .unwrap()
        .filter_map(Result::ok)
        .any(|entry| entry
            .file_name()
            .to_string_lossy()
            .contains("execute-applet")));

    let third = run(
        "fs.ensure_file",
        json!({"path": "conf.txt", "content": "beta\n"}),
        dir.path(),
    )
    .unwrap();
    assert!(changed(&third));
    assert_eq!(
        fs::read_to_string(dir.path().join("conf.txt")).unwrap(),
        "beta\n"
    );
}

#[cfg(unix)]
#[test]
fn ensure_file_applies_and_keeps_an_octal_mode() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("secret.env");
    let created = run(
        "fs.ensure_file",
        json!({"path": "secret.env", "content": "K=1\n", "mode": "600"}),
        dir.path(),
    )
    .unwrap();
    assert!(changed(&created));
    assert_eq!(created["mode_applied"], json!(true));
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );

    // Same content, wrong mode: the file is rewritten to the requested mode.
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    let fixed = run(
        "fs.ensure_file",
        json!({"path": "secret.env", "content": "K=1\n", "mode": "0600"}),
        dir.path(),
    )
    .unwrap();
    assert!(changed(&fixed));
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );

    let settled = run(
        "fs.ensure_file",
        json!({"path": "secret.env", "content": "K=1\n", "mode": "600"}),
        dir.path(),
    )
    .unwrap();
    assert!(!changed(&settled));
}

#[test]
fn ensure_file_rejects_bad_modes_and_directories() {
    let dir = tempfile::tempdir().unwrap();
    for mode in ["", "8", "0779", "77777", "rw"] {
        assert!(
            run(
                "fs.ensure_file",
                json!({"path": "f", "content": "", "mode": mode}),
                dir.path()
            )
            .is_err(),
            "{mode:?}"
        );
    }
    fs::create_dir(dir.path().join("sub")).unwrap();
    let error = run(
        "fs.ensure_file",
        json!({"path": "sub", "content": "x"}),
        dir.path(),
    )
    .unwrap_err();
    assert_eq!(error.exit_status(), 2);
}

#[test]
fn ensure_line_adds_once_and_respects_line_endings() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rc");
    // No trailing newline: the line is appended after one is added.
    fs::write(&path, "export A=1").unwrap();
    assert!(changed(
        &run(
            "fs.ensure_line",
            json!({"path": "rc", "line": "export B=2"}),
            dir.path()
        )
        .unwrap()
    ));
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        "export A=1\nexport B=2\n"
    );
    assert!(!changed(
        &run(
            "fs.ensure_line",
            json!({"path": "rc", "line": "export B=2"}),
            dir.path()
        )
        .unwrap()
    ));

    // A CRLF file matches a line written without its carriage return.
    fs::write(&path, "one\r\ntwo\r\n").unwrap();
    assert!(!changed(
        &run(
            "fs.ensure_line",
            json!({"path": "rc", "line": "two"}),
            dir.path()
        )
        .unwrap()
    ));
    assert_eq!(fs::read(&path).unwrap(), b"one\r\ntwo\r\n");
}

#[test]
fn ensure_line_absent_removes_only_exact_matches() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hosts");
    fs::write(&path, "keep\ndrop\nkeep drop\ndrop\n").unwrap();
    let removed = run(
        "fs.ensure_line",
        json!({"path": "hosts", "line": "drop", "state": "absent"}),
        dir.path(),
    )
    .unwrap();
    assert!(changed(&removed));
    assert_eq!(fs::read_to_string(&path).unwrap(), "keep\nkeep drop\n");
    assert!(!changed(
        &run(
            "fs.ensure_line",
            json!({"path": "hosts", "line": "drop", "state": "absent"}),
            dir.path()
        )
        .unwrap()
    ));
    // Absent on a missing file is a no-op and creates nothing.
    assert!(!changed(
        &run(
            "fs.ensure_line",
            json!({"path": "missing", "line": "x", "state": "absent"}),
            dir.path()
        )
        .unwrap()
    ));
    assert!(!dir.path().join("missing").exists());
}

#[test]
fn ensure_line_creates_missing_files_and_rejects_multi_line_text() {
    let dir = tempfile::tempdir().unwrap();
    assert!(changed(
        &run(
            "fs.ensure_line",
            json!({"path": "new", "line": "first"}),
            dir.path()
        )
        .unwrap()
    ));
    assert_eq!(
        fs::read_to_string(dir.path().join("new")).unwrap(),
        "first\n"
    );
    assert!(run(
        "fs.ensure_line",
        json!({"path": "new", "line": "a\nb"}),
        dir.path()
    )
    .is_err());
    assert!(run(
        "fs.ensure_line",
        json!({"path": "new", "line": "x", "state": "maybe"}),
        dir.path()
    )
    .is_err());
}

#[cfg(unix)]
#[test]
fn ensure_symlink_creates_keeps_and_replaces_only_links() {
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("real-a"), "a").unwrap();
    fs::write(dir.path().join("real-b"), "b").unwrap();

    let created = run(
        "fs.ensure_symlink",
        json!({"path": "link", "target": "real-a"}),
        dir.path(),
    )
    .unwrap();
    assert!(changed(&created));
    assert_eq!(
        fs::read_link(dir.path().join("link")).unwrap(),
        Path::new("real-a")
    );

    assert!(!changed(
        &run(
            "fs.ensure_symlink",
            json!({"path": "link", "target": "real-a"}),
            dir.path()
        )
        .unwrap()
    ));

    assert!(changed(
        &run(
            "fs.ensure_symlink",
            json!({"path": "link", "target": "real-b"}),
            dir.path()
        )
        .unwrap()
    ));
    assert_eq!(
        fs::read_link(dir.path().join("link")).unwrap(),
        Path::new("real-b")
    );
    assert_eq!(fs::read_to_string(dir.path().join("link")).unwrap(), "b");

    // A regular file at the link path is never replaced.
    fs::write(dir.path().join("plain"), "keep me").unwrap();
    let error = run(
        "fs.ensure_symlink",
        json!({"path": "plain", "target": "real-a"}),
        dir.path(),
    )
    .unwrap_err();
    assert_eq!(error.exit_status(), 2);
    assert_eq!(
        fs::read_to_string(dir.path().join("plain")).unwrap(),
        "keep me"
    );

    // A dangling existing link is still a link, and is replaced.
    symlink("nowhere", dir.path().join("dangling")).unwrap();
    assert!(changed(
        &run(
            "fs.ensure_symlink",
            json!({"path": "dangling", "target": "real-a"}),
            dir.path()
        )
        .unwrap()
    ));
}

#[test]
fn ensure_dir_creates_parents_and_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    assert!(changed(
        &run("fs.ensure_dir", json!({"path": "a/b/c"}), dir.path()).unwrap()
    ));
    assert!(dir.path().join("a/b/c").is_dir());
    assert!(!changed(
        &run("fs.ensure_dir", json!({"path": "a/b/c"}), dir.path()).unwrap()
    ));
    fs::write(dir.path().join("file"), "x").unwrap();
    let error = run("fs.ensure_dir", json!({"path": "file"}), dir.path()).unwrap_err();
    assert_eq!(error.exit_status(), 2);
}

#[cfg(unix)]
#[test]
fn ensure_dir_sets_and_keeps_its_mode() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let created = run(
        "fs.ensure_dir",
        json!({"path": "private", "mode": "700"}),
        dir.path(),
    )
    .unwrap();
    assert!(changed(&created));
    assert_eq!(created["mode_applied"], json!(true));
    let target = dir.path().join("private");
    assert_eq!(
        fs::metadata(&target).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert!(!changed(
        &run(
            "fs.ensure_dir",
            json!({"path": "private", "mode": "0700"}),
            dir.path()
        )
        .unwrap()
    ));
    fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(changed(
        &run(
            "fs.ensure_dir",
            json!({"path": "private", "mode": "700"}),
            dir.path()
        )
        .unwrap()
    ));
}

#[test]
fn ensure_operations_reject_missing_arguments() {
    let dir = tempfile::tempdir().unwrap();
    assert!(run(
        "fs.ensure_file",
        json!({"path": "", "content": ""}),
        dir.path()
    )
    .is_err());
    assert!(run("fs.ensure_file", json!({"path": "f"}), dir.path()).is_err());
    assert!(run("fs.ensure_symlink", json!({"path": "l"}), dir.path()).is_err());
    assert!(run("fs.ensure_dir", json!({}), dir.path()).is_err());
}
