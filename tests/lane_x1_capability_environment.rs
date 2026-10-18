use std::fs;
use std::process::Command;

#[test]
fn capability_policy_denies_program_environment_reads_by_default() {
    let root = tempfile::tempdir().unwrap();
    let program = root.path().join("environment.program");
    let policy = root.path().join("policy.json");
    fs::write(
        &program,
        "accept key as text from environment \"LANE_X1_CANARY\"\nfail with message \"completed\"\n",
    )
    .unwrap();
    fs::write(&policy, r#"{"version":1,"applets":{}}"#).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_execute"))
        .args([
            "run",
            program.to_str().unwrap(),
            "--capability-policy",
            policy.to_str().unwrap(),
        ])
        .env("LANE_X1_CANARY", "secret-value")
        .output()
        .unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("capability policy denies reading environment variable"));
    assert!(!stderr.contains("secret-value"));
}
