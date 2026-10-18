use flow::applets::execute_in_cwd;
use serde_json::{json, Value};
use std::fs;
use std::path::Path;

fn run(operation: &str, arguments: Value, cwd: &Path) -> Result<Value, flow::applets::AppletError> {
    let arguments = arguments.as_object().cloned().unwrap_or_default();
    execute_in_cwd(operation, &arguments, &[], cwd)
}

#[test]
fn posix_dirname_and_basename_edges() {
    for (input, expected) in [
        ("/usr/lib", "/usr"),
        ("/usr/", "/"),
        ("usr", "."),
        ("usr/lib/", "usr"),
        ("usr//lib", "usr"),
        ("/", "/"),
        ("//", "/"),
        ("a/.", "a"),
        ("a//b", "a"),
    ] {
        assert_eq!(
            run("path.dirname", json!({"path": input}), Path::new(".")).unwrap(),
            json!(expected)
        );
    }
    for (path, suffix, expected) in [
        ("a/b.txt", "b.txt", "b.txt"),
        ("a/b.txt", "/b.txt", "b.txt"),
        (".txt", ".txt", ".txt"),
        ("dir.txt/", ".txt", "dir"),
        ("/", ".", "/"),
        ("..", "", ".."),
        ("a/..", "", ".."),
        (".", "", "."),
        ("a/.", "", "."),
    ] {
        assert_eq!(
            run(
                "path.basename",
                json!({"path": path, "suffix": suffix}),
                Path::new(".")
            )
            .unwrap(),
            json!(expected)
        );
    }
}

#[test]
fn adjacent_glob_depth_components_are_collapsed() {
    let dir = tempfile::tempdir().unwrap();
    let mut nested = dir.path().to_path_buf();
    for _ in 0..25 {
        nested.push("a");
    }
    fs::create_dir_all(&nested).unwrap();
    fs::write(nested.join("hit"), b"").unwrap();
    let one = run("fs.glob", json!({"pattern":"**/hit"}), dir.path()).unwrap();
    let repeated_pattern = format!("{}hit", "**/".repeat(8));
    let repeated = run("fs.glob", json!({"pattern": repeated_pattern}), dir.path()).unwrap();
    assert_eq!(one, repeated);
    assert_eq!(repeated.as_array().unwrap().len(), 1);
}

#[test]
fn write_atomic_identical_bytes_and_long_names() {
    let dir = tempfile::tempdir().unwrap();
    let name = format!("{}.txt", "n".repeat(235));
    fs::write(dir.path().join(&name), b"same").unwrap();
    use sha2::Digest;
    let digest = format!("{:x}", sha2::Sha256::digest(b"same"));
    #[cfg(windows)]
    let target = format!("\\\\?\\{}", dir.path().join(&name).display());
    #[cfg(not(windows))]
    let target = name;
    let result = run(
        "fs.write_atomic",
        json!({
            "path": target, "content": "same", "expected_content_hash": digest,
            "symlink_policy": "reject", "permissions_policy": "preserve"
        }),
        dir.path(),
    )
    .unwrap();
    assert_eq!(result["changed"], false);
}

#[test]
fn split_uses_left_to_right_overlapping_separator_tokens() {
    for (text, separator, expected) in [
        ("a|||", "||", json!(["a", "|"])),
        ("a|||b", "||", json!(["a", "|b"])),
        ("aaa", "aa", json!(["", "a"])),
    ] {
        assert_eq!(
            run(
                "text.split",
                json!({"text": text, "separator": separator}),
                Path::new(".")
            )
            .unwrap(),
            expected
        );
    }
}

#[test]
fn recovery_does_not_create_a_lock_in_a_non_journal_directory() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("victim")).unwrap();
    assert!(run(
        "fs.transaction.recover",
        json!({"journal":"victim", "decision":"rollback"}),
        dir.path()
    )
    .is_err());
    assert!(!dir.path().join("victim/transaction.lock").exists());
}

#[test]
fn ensure_line_preserves_crlf_and_matches_after_a_bom() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("lines"), b"x\r\ny\r\n").unwrap();
    run(
        "fs.ensure_line",
        json!({"path":"lines", "line":"z"}),
        dir.path(),
    )
    .unwrap();
    assert_eq!(
        fs::read(dir.path().join("lines")).unwrap(),
        b"x\r\ny\r\nz\r\n"
    );
    fs::write(dir.path().join("bom"), b"\xef\xbb\xbfkey=1\n").unwrap();
    let result = run(
        "fs.ensure_line",
        json!({"path":"bom", "line":"key=1"}),
        dir.path(),
    )
    .unwrap();
    assert_eq!(result["changed"], false);
    assert_eq!(
        fs::read(dir.path().join("bom")).unwrap(),
        b"\xef\xbb\xbfkey=1\n"
    );
    let removed = run(
        "fs.ensure_line",
        json!({"path":"bom", "line":"key=1", "state":"absent"}),
        dir.path(),
    )
    .unwrap();
    assert_eq!(removed["changed"], true);
    assert_eq!(fs::read(dir.path().join("bom")).unwrap(), b"\xef\xbb\xbf");
}
