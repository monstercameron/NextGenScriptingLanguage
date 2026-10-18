#![cfg(feature = "tooling")]

use std::process::Command;

#[test]
fn generated_long_lived_services_request_restart_after_abnormal_exit() {
    let directory = tempfile::tempdir().unwrap();
    let program = directory.path().join("service.program");
    std::fs::write(&program, "every 1 hour\n  run fixture.native\n").unwrap();
    std::fs::write(
        directory.path().join("fixture.json"),
        serde_json::json!({
            "name":"fixture.native", "binary":env!("CARGO_BIN_EXE_execute"),
            "arguments":[{"literal":"--version"}], "contract":{"version":1}
        })
        .to_string(),
    )
    .unwrap();
    for (format, restart) in [
        ("systemd", "Restart=on-failure"),
        ("launchd", "<key>SuccessfulExit</key>\n<false/>"),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_execute"))
            .args(["service", "generate", format])
            .arg(&program)
            .args(["--name", "com.example.v8-panic-host"])
            .arg("--manifest-dir")
            .arg(directory.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let rendered = String::from_utf8(output.stdout).unwrap();
        // Ignore indentation, which is presentation rather than supervision.
        let normalized = rendered
            .lines()
            .map(str::trim)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(normalized.contains(restart), "{rendered}");
    }
}
