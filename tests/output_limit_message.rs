//! A foreground command whose output exceeds `--max-output-bytes` is reported as a
//! foreground command, with the way out: pipe the output, or raise the limit.

use std::fs;
use std::process::Command;

#[test]
fn a_foreground_command_over_the_limit_is_named_as_one() {
    let work = tempfile::tempdir().unwrap();
    let executable = env!("CARGO_BIN_EXE_execute").replace('\\', "/");
    let program = work.path().join("big.program");
    // The binary's help text is far longer than 16 bytes. The output is consumed by the
    // save, so it is captured and the limit applies; unconsumed output streams instead.
    fs::write(
        &program,
        format!("run \"{executable}\" with arguments [\"--help\"]\nthen save to \"out.txt\"\n"),
    )
    .unwrap();
    let manifests = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests");
    let output = Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(work.path())
        .arg("run")
        .arg(&program)
        .arg("--max-output-bytes")
        .arg("16")
        .arg("--manifest-dir")
        .arg(&manifests)
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("foreground command output exceeds the limit of 16 bytes"),
        "{stderr}"
    );
    assert!(!stderr.contains("background job"), "{stderr}");
    assert!(stderr.contains("--max-output-bytes"), "{stderr}");
    assert!(stderr.contains("pipe the output"), "{stderr}");
}
