//! #263: a program's `# execute-policy:` header is enforced by `run`, before any
//! effect, not only by `explain`.

use std::fs;
use std::path::Path;
use std::process::Command;

fn run(directory: &Path, program: &str) -> std::process::Output {
    fs::write(directory.join("p.program"), program).unwrap();
    Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(directory)
        .arg("run")
        .arg("p.program")
        .arg("--manifest-dir")
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests"))
        .output()
        .expect("execute starts")
}

#[test]
fn allow_none_header_refuses_a_file_write_before_it_happens() {
    let directory = tempfile::tempdir().unwrap();
    let output = run(
        directory.path(),
        "# execute-policy: allow none\nuse value \"x\"\nthen save to \"written.txt\"\n",
    );
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("execute-policy header denies effects"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!directory.path().join("written.txt").exists());
}

#[test]
fn a_program_without_a_header_is_unaffected() {
    let directory = tempfile::tempdir().unwrap();
    let output = run(
        directory.path(),
        "use value \"x\"\nthen save to \"written.txt\"\n",
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(directory.path().join("written.txt").exists());
}
