//! Manifest discovery must not depend on the working directory. A script's
//! cwd is caller-controlled, so a `manifests/` folder there must never shadow
//! a bundled applet. Explicit `--manifest-dir` selection still applies.
use std::fs;
use std::process::Command;
use tempfile::tempdir;

#[test]
fn cwd_manifests_directory_does_not_override_a_bundled_applet() {
    let temp = tempdir().unwrap();
    let shadow = temp.path().join("manifests");
    fs::create_dir(&shadow).unwrap();
    // Would replace `cat` with `echo HIJACKED` if the cwd were searched.
    fs::write(
        shadow.join("cat.json"),
        r#"{"name":"cat","binary":"echo","parameters":{},"arguments":[{"literal":"HIJACKED"}]}"#,
    )
    .unwrap();
    fs::write(temp.path().join("file.txt"), "hello\n").unwrap();
    let program = temp.path().join("main.program");
    fs::write(&program, "run cat with path \"file.txt\"\n").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_execute"))
        .arg("run")
        .arg(&program)
        .current_dir(temp.path())
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout), "hello\n");
    assert!(!String::from_utf8_lossy(&output.stdout).contains("HIJACKED"));
}

#[test]
fn explicit_manifest_dir_still_selects_manifests() {
    let temp = tempdir().unwrap();
    let explicit = temp.path().join("explicit");
    fs::create_dir(&explicit).unwrap();
    fs::write(
        explicit.join("lane.probe.json"),
        r#"{"name":"lane.probe","binary":"echo","parameters":{},"arguments":[{"literal":"SELECTED"}]}"#,
    )
    .unwrap();
    let program = temp.path().join("probe.program");
    fs::write(&program, "run lane.probe\n").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_execute"))
        .arg("run")
        .arg(&program)
        .arg("--manifest-dir")
        .arg(&explicit)
        .current_dir(temp.path())
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "SELECTED");
}
