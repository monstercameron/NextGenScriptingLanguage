use std::process::Command;

fn fixture() -> (tempfile::TempDir, std::path::PathBuf) {
    let directory = tempfile::tempdir().unwrap();
    let program = directory.path().join("workflow.program");
    std::fs::write(&program, "run fixture.native\n").unwrap();
    std::fs::write(
        directory.path().join("fixture.json"),
        serde_json::json!({
            "name":"fixture.native", "binary":env!("CARGO_BIN_EXE_execute"), "arguments":[],
            "contract":{"version":1}
        })
        .to_string(),
    )
    .unwrap();
    (directory, program)
}

#[test]
fn untrusted_without_policy_is_refused_before_execution() {
    let (_directory, program) = fixture();
    let output = Command::new(env!("CARGO_BIN_EXE_execute"))
        .args(["run", "--execution-mode", "untrusted"])
        .arg(program)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr)
        .contains("untrusted execution requires a capability policy"));
    assert!(output.stdout.is_empty());
}

#[test]
fn explain_reports_trusted_mode_and_declaration_only_steps() {
    let (directory, program) = fixture();
    let output = Command::new(env!("CARGO_BIN_EXE_execute"))
        .args(["explain", "--json", "--execution-mode", "trusted"])
        .arg(program)
        .arg("--manifest-dir")
        .arg(directory.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let plan: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(plan["execution_mode"], "trusted");
    assert!(plan["steps"]
        .as_array()
        .unwrap()
        .iter()
        .all(|step| step["enforcement"].is_string()));
    assert_eq!(plan["steps"][0]["enforcement"], "declared_only");
}

#[test]
fn explain_with_policy_reports_untrusted_and_never_probes_or_launches() {
    let (directory, program) = fixture();
    let policy = directory.path().join("policy.json");
    std::fs::write(
        &policy,
        r#"{"version":1,"applets":{"fixture.native":{"capabilities":["process.exec"]}}}"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&policy, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    // Keep the policy outside the manifest directory's JSON scan.
    let manifests = directory.path().join("manifests");
    std::fs::create_dir(&manifests).unwrap();
    std::fs::rename(
        directory.path().join("fixture.json"),
        manifests.join("fixture.json"),
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_execute"))
        .args(["explain", "--json"])
        .arg(program)
        .arg("--manifest-dir")
        .arg(manifests)
        .arg("--capability-policy")
        .arg(policy)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let plan: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(plan["execution_mode"], "untrusted");
    assert_eq!(
        plan["steps"][0]["enforcement"],
        if cfg!(any(target_os = "linux", target_os = "macos")) {
            "os_sandbox_required"
        } else {
            "refused"
        }
    );
}

#[cfg(windows)]
#[test]
fn policy_native_launch_defaults_to_refusal_without_os_confinement() {
    use flow::capabilities::{command_for, CapabilityPolicy};
    let binary = std::env::current_exe().unwrap();
    let registry = flow::manifest::Registry::from_sources(&[serde_json::json!({
        "name":"fixture.native", "binary":binary, "arguments":[],
        "contract":{"version":1}
    })
    .to_string()])
    .unwrap();
    let policy = CapabilityPolicy::from_json(
        r#"{"version":1,"applets":{"fixture.native":{"capabilities":["process.exec"]}}}"#,
    )
    .unwrap();
    registry.set_host_state(Some(policy));
    let cwd = flow::working_dir::WorkingDirectory::plain(std::env::current_dir().unwrap());
    let result = command_for(
        &binary,
        std::iter::empty::<&str>(),
        &registry,
        "fixture.native",
        &cwd,
    );
    assert!(
        result.is_err(),
        "a policy must never silently launch an unconfined native child"
    );
    assert!(result
        .unwrap_err()
        .downcast_ref::<flow::capabilities::SandboxRefused>()
        .is_some());
}

#[cfg(feature = "agent-advisory")]
#[test]
fn what_if_fingerprint_changes_when_execution_mode_changes() {
    let (directory, program) = fixture();
    let policy = directory.path().join("policy.json");
    std::fs::write(
        &policy,
        r#"{"version":1,"applets":{"fixture.native":{"capabilities":["process.exec"]}}}"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&policy, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let manifests = directory.path().join("manifests");
    std::fs::create_dir(&manifests).unwrap();
    std::fs::rename(
        directory.path().join("fixture.json"),
        manifests.join("fixture.json"),
    )
    .unwrap();
    let mut fingerprints = Vec::new();
    for mode in ["trusted", "untrusted"] {
        let output = Command::new(env!("CARGO_BIN_EXE_execute"))
            .args(["run", "--what-if", "--execution-mode", mode])
            .arg(&program)
            .arg("--manifest-dir")
            .arg(&manifests)
            .arg("--capability-policy")
            .arg(&policy)
            .env("LOCALAPPDATA", directory.path())
            .env("XDG_STATE_HOME", directory.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(report["plan"]["execution_mode"], mode);
        fingerprints.push(report["fingerprint"].clone());
    }
    assert_ne!(fingerprints[0], fingerprints[1]);
}
