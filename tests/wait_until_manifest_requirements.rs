//! `wait.until`'s implementation reads both probe and timeout on every call.
use serde_json::Value;
use std::{fs, path::Path, process::Command};

fn execute(root: &Path, command: &str, source: &str, disk_manifests: bool) -> std::process::Output {
    let program = root.join("wait.program");
    fs::write(&program, source).unwrap();
    let mut invocation = Command::new(env!("CARGO_BIN_EXE_execute"));
    invocation.arg(command).arg(&program).current_dir(root);
    if disk_manifests {
        invocation
            .arg("--manifest-dir")
            .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/manifests"));
    }
    invocation.output().unwrap()
}

#[test]
fn required_probe_and_timeout_are_checked_for_disk_and_bundled_manifests() {
    let cases = [
        (
            "run wait.until with probe \"file_exists\" and path \".\"\n",
            "timeout_ms",
        ),
        ("run wait.until with timeout_ms 0\n", "probe"),
        ("run wait.until\n", "probe"),
    ];
    let root = tempfile::tempdir().unwrap();
    for disk_manifests in [false, true] {
        for (source, missing) in cases {
            for command in ["check", "run"] {
                let output = execute(root.path(), command, source, disk_manifests);
                let diagnostic = String::from_utf8_lossy(&output.stderr);
                assert!(
                    !output.status.success(),
                    "{command} accepted {source:?}; disk_manifests={disk_manifests}"
                );
                assert!(
                    diagnostic.contains(missing),
                    "{command} {source:?}; disk_manifests={disk_manifests}: {diagnostic}"
                );
            }
        }
    }
}

#[test]
fn valid_probe_checks_and_runs_and_conditional_inputs_stay_runtime_validated() {
    let root = tempfile::tempdir().unwrap();
    let valid = "run wait.until with probe \"file_exists\" and path \".\" and timeout_ms 0\n";
    for disk_manifests in [false, true] {
        for command in ["check", "run"] {
            let output = execute(root.path(), command, valid, disk_manifests);
            assert!(
                output.status.success(),
                "{command} valid probe; disk_manifests={disk_manifests}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            if command == "run" {
                let result: Value = serde_json::from_slice(&output.stdout).unwrap();
                assert_eq!(result["ready"], true);
            }
        }

        // Probe-specific values remain conditional: the manifest cannot require
        // path/host/port globally, so runtime still validates each selected probe.
        for (source, missing) in [
            (
                "run wait.until with probe \"file_exists\" and timeout_ms 0\n",
                "path",
            ),
            (
                "run wait.until with probe \"tcp_port_open\" and timeout_ms 0\n",
                "host",
            ),
            (
                "run wait.until with probe \"tcp_port_open\" and host \"127.0.0.1\" and timeout_ms 0\n",
                "port",
            ),
        ] {
            let checked = execute(root.path(), "check", source, disk_manifests);
            assert!(
                checked.status.success(),
                "check rejected conditional {missing}; disk_manifests={disk_manifests}: {}",
                String::from_utf8_lossy(&checked.stderr)
            );
            let ran = execute(root.path(), "run", source, disk_manifests);
            let diagnostic = String::from_utf8_lossy(&ran.stderr);
            assert!(
                !ran.status.success() && diagnostic.contains(missing),
                "run did not validate conditional {missing}; disk_manifests={disk_manifests}: {diagnostic}"
            );
        }
    }
}
