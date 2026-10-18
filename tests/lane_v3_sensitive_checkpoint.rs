use std::fs;
use std::process::{Command, Stdio};

#[test]
fn environment_checkpoint_refused_before_any_artifact_is_written() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(
        directory.path().join("secret.program"),
        "accept key as text from environment \"API_KEY\"\nuse value key\n",
    )
    .unwrap();
    for command in ["run", "resume"] {
        let output = Command::new(env!("CARGO_BIN_EXE_execute"))
            .args([command, "secret.program", "--checkpoint", "ck"])
            .env("API_KEY", "CANARY-V3-112")
            .current_dir(directory.path())
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(!output.status.success());
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.contains("sensitive input"), "{error}");
        assert!(!error.contains("CANARY-V3-112"));
        assert!(!directory.path().join("ck").exists());
    }
    let plain = Command::new(env!("CARGO_BIN_EXE_execute"))
        .args(["run", "secret.program"])
        .env("API_KEY", "CANARY-V3-112")
        .current_dir(directory.path())
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        plain.status.success(),
        "{}",
        String::from_utf8_lossy(&plain.stderr)
    );
    assert!(String::from_utf8_lossy(&plain.stdout).contains("CANARY-V3-112"));
}
