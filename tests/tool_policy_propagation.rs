#![cfg(all(feature = "tooling", feature = "records"))]
use std::{
    fs,
    io::Write,
    process::{Command, Stdio},
};

#[test]
fn policies_reach_children_and_repl_and_legacy_behavior_remains() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("policy.json"),
        r#"{"version":1,"applets":{}}"#,
    )
    .unwrap();
    fs::write(dir.path().join("flow.json"), r#"{"version":1}"#).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(
            dir.path().join("flow.json"),
            fs::Permissions::from_mode(0o600),
        )
        .unwrap();
    }
    fs::create_dir(dir.path().join("parts")).unwrap();
    fs::write(
        dir.path().join("parts/deny.program"),
        "run \"printf\" with arguments [\"ok\"]\n",
    )
    .unwrap();
    fs::write(
        dir.path().join("deny.test.program"),
        "run \"printf\" with arguments [\"ok\"]\n",
    )
    .unwrap();
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_execute"))
            .current_dir(dir.path())
            .args(args)
            .output()
            .unwrap()
    };
    let legacy = run(&["run-parts", "parts"]);
    assert!(
        legacy.status.success(),
        "{}",
        String::from_utf8_lossy(&legacy.stderr)
    );
    let legacy_test = run(&["test", "deny.test.program"]);
    assert!(
        legacy_test.status.success(),
        "{}",
        String::from_utf8_lossy(&legacy_test.stdout)
    );
    let parts = run(&[
        "run-parts",
        "parts",
        "--capability-policy",
        "policy.json",
        "--data-flow-policy",
        "flow.json",
        "--execution-mode",
        "untrusted",
        "--json",
    ]);
    assert!(!parts.status.success());
    assert!(String::from_utf8_lossy(&parts.stderr)
        .contains("capability policy denies direct quoted commands"));
    let report = String::from_utf8_lossy(&parts.stdout);
    assert!(
        report.contains("deny.program") && report.contains("failed"),
        "{report}"
    );
    let test = run(&[
        "test",
        "deny.test.program",
        "--capability-policy",
        "policy.json",
        "--data-flow-policy",
        "flow.json",
        "--execution-mode",
        "untrusted",
    ]);
    assert!(
        !test.status.success(),
        "{}",
        String::from_utf8_lossy(&test.stdout)
    );
    assert!(String::from_utf8_lossy(&test.stdout)
        .contains("capability policy denies direct quoted commands"));
    let mut repl = Command::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(dir.path())
        .args([
            "repl",
            "--capability-policy",
            "policy.json",
            "--execution-mode",
            "untrusted",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    repl.stdin
        .take()
        .unwrap()
        .write_all(b"run \"printf\" with arguments [\"ok\"]\n:quit\n")
        .unwrap();
    let output = repl.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("capability policy denies direct quoted commands"),
        "{stdout}"
    );
    for cmd in ["run-parts", "test", "repl"] {
        let output = run(&[cmd, "--allow", "filesystem.read"]);
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("--allow"));
    }
}

#[cfg(unix)]
#[test]
fn data_flow_policy_is_enforced_by_each_execution_entry_point() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::write(root.join("flow.json"), r#"{"version":1}"#).unwrap();
    fs::set_permissions(root.join("flow.json"), fs::Permissions::from_mode(0o600)).unwrap();
    fs::create_dir(root.join("parts")).unwrap();
    let source = "run \"printf\" with arguments [\"ok\"]\nthen save to \"out.txt\"\n";
    fs::write(root.join("parts/a.program"), source).unwrap();
    fs::write(root.join("a.test.program"), source).unwrap();
    for (command, target) in [("run-parts", "parts"), ("test", "a.test.program")] {
        let invoke = |policy: bool| {
            let mut child = Command::new(env!("CARGO_BIN_EXE_execute"));
            child.current_dir(root).args([command, target]);
            if policy {
                child.args(["--data-flow-policy", "flow.json"]);
            }
            child.output().unwrap()
        };
        let allowed = invoke(false);
        assert!(
            allowed.status.success(),
            "{} {}",
            String::from_utf8_lossy(&allowed.stdout),
            String::from_utf8_lossy(&allowed.stderr)
        );
        let denied = invoke(true);
        assert!(!denied.status.success());
        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&denied.stdout),
            String::from_utf8_lossy(&denied.stderr)
        );
        assert!(
            combined.contains("data persistence is denied by policy"),
            "{combined}"
        );
    }
    let repl = |policy: bool| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_execute"));
        command
            .current_dir(root)
            .arg("repl")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped());
        if policy {
            command.args(["--data-flow-policy", "flow.json"]);
        }
        let mut child = command.spawn().unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(format!("{source}:quit\n").as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    };
    fs::remove_file(root.join("out.txt")).unwrap();
    let allowed = repl(false);
    assert!(allowed.status.success());
    assert!(root.join("out.txt").exists());
    fs::remove_file(root.join("out.txt")).unwrap();
    let denied = repl(true);
    assert!(
        String::from_utf8_lossy(&denied.stdout).contains("data persistence is denied by policy")
    );
    assert!(!root.join("out.txt").exists());
}

#[cfg(unix)]
#[test]
fn execution_modes_are_forwarded_and_untrusted_without_policy_fails_early() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir(root.join("parts")).unwrap();
    fs::write(root.join("parts/a.program"), "run fixture.native\n").unwrap();
    fs::write(root.join("a.test.program"), "run fixture.native\n").unwrap();
    fs::write(root.join("manifest.json"), r#"{"name":"fixture.native","binary":"printf","parameters":{},"arguments":[{"literal":"ok"}],"contract":{"version":1,"capabilities":["process.exec","clock.read"],"effects":["clock.read"]}}"#).unwrap();
    fs::write(root.join("cap.json"), r#"{"version":1,"applets":{"fixture.native":{"capabilities":["process.exec","clock.read"]}}}"#).unwrap();
    for (command, target) in [
        ("run-parts", Some("parts")),
        ("test", Some("a.test.program")),
        ("repl", None),
    ] {
        let invoke = |mode: &str, policy: bool| {
            let mut cmd = Command::new(env!("CARGO_BIN_EXE_execute"));
            cmd.current_dir(root).arg(command).args([
                "--manifest-dir",
                "manifest.json",
                "--execution-mode",
                mode,
            ]);
            if let Some(target) = target {
                cmd.arg(target);
            }
            if policy {
                cmd.args(["--capability-policy", "cap.json"]);
            }
            if target.is_none() {
                cmd.stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped());
                let mut child = cmd.spawn().unwrap();
                child
                    .stdin
                    .take()
                    .unwrap()
                    .write_all(b"run fixture.native\n:quit\n")
                    .unwrap();
                child.wait_with_output().unwrap()
            } else {
                cmd.output().unwrap()
            }
        };
        let missing = invoke("untrusted", false);
        assert!(!missing.status.success(), "{command}");
        assert!(String::from_utf8_lossy(&missing.stderr)
            .contains("untrusted execution requires a capability policy"));
        let trusted = invoke("trusted", true);
        assert!(
            trusted.status.success(),
            "{command}: {} {}",
            String::from_utf8_lossy(&trusted.stdout),
            String::from_utf8_lossy(&trusted.stderr)
        );
        assert!(String::from_utf8_lossy(&trusted.stdout).contains("ok"));
        let untrusted = invoke("untrusted", true);
        let output = format!(
            "{}{}",
            String::from_utf8_lossy(&untrusted.stdout),
            String::from_utf8_lossy(&untrusted.stderr)
        );
        assert!(
            output.contains("cannot enforce declared native effect"),
            "{command}: {output}"
        );
        if target.is_some() {
            assert!(!untrusted.status.success());
        }
    }
}
