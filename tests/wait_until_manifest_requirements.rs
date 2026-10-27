//! `wait.until`'s implementation reads both probe and timeout on every call.
use serde_json::Value;
use std::{fs, path::Path, process::Command};

fn execute(root: &Path, command: &str, source: &str, disk_manifests: bool) -> std::process::Output {
    execute_with_json(root, command, source, disk_manifests, false)
}

fn execute_with_json(
    root: &Path,
    command: &str,
    source: &str,
    disk_manifests: bool,
    json: bool,
) -> std::process::Output {
    let program = root.join("wait.program");
    fs::write(&program, source).unwrap();
    let mut invocation = Command::new(env!("CARGO_BIN_EXE_execute"));
    invocation.arg(command).arg(&program).current_dir(root);
    if json {
        invocation.arg("--json");
    }
    if disk_manifests {
        invocation
            .arg("--manifest-dir")
            .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/manifests"));
    }
    invocation.output().unwrap()
}

fn execute_with_input(
    root: &Path,
    source: &str,
    input: &str,
    disk_manifests: bool,
) -> std::process::Output {
    let program = root.join("wait.program");
    fs::write(&program, source).unwrap();
    let mut invocation = Command::new(env!("CARGO_BIN_EXE_execute"));
    invocation
        .arg("run")
        .arg(&program)
        .arg("--input")
        .arg(input)
        .current_dir(root);
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

#[test]
fn check_rejects_only_literal_non_loopback_ip_hosts_for_tcp_probe() {
    let root = tempfile::tempdir().unwrap();
    for disk_manifests in [false, true] {
        let rejected = "run wait.until with probe \"tcp_port_open\" and host \"8.8.8.8\" and port 443 and timeout_ms 0\n";
        let output = execute(root.path(), "check", rejected, disk_manifests);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "check accepted non-loopback IP");
        assert!(
            stderr.contains("loopback IP"),
            "unexpected diagnostic: {stderr}"
        );
        let json_output = execute_with_json(root.path(), "check", rejected, disk_manifests, true);
        let report: Value = serde_json::from_slice(&json_output.stdout).unwrap();
        assert!(
            report["diagnostics"]
                .as_array()
                .unwrap()
                .iter()
                .any(|diagnostic| diagnostic["code"] == "applet.parameter.constraint"),
            "missing stable constraint diagnostic: {report}"
        );

        // The TCP applet accepts IPv4 loopback throughout 127/8, IPv6 loopback,
        // and IPv4-mapped loopback after canonicalization.
        for host in ["127.0.0.1", "127.255.255.255", "::1", "::ffff:127.0.0.1"] {
            let source = format!(
                "run wait.until with probe \"tcp_port_open\" and host \"{host}\" and port 443 and timeout_ms 0\n"
            );
            let output = execute(root.path(), "check", &source, disk_manifests);
            assert!(
                output.status.success(),
                "check rejected loopback IP {host}; disk_manifests={disk_manifests}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        // Names are not IP literals, so keep their validation to runtime. A
        // referenced value is also dynamic from the static checker's perspective.
        for source in [
            "run wait.until with probe \"tcp_port_open\" and host \"localhost\" and port 443 and timeout_ms 0\n",
            "use value \"8.8.8.8\" as address\nrun wait.until with probe \"tcp_port_open\" and host address and port 443 and timeout_ms 0\n",
        ] {
            let output = execute(root.path(), "check", source, disk_manifests);
            assert!(
                output.status.success(),
                "check statically rejected a name or referenced host; disk_manifests={disk_manifests}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        let dynamic_host = "use value \"8.8.8.8\" as address\nrun wait.until with probe \"tcp_port_open\" and host address and port 443 and timeout_ms 0\n";
        let output = execute(root.path(), "run", dynamic_host, disk_manifests);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !output.status.success() && stderr.contains("loopback address"),
            "runtime did not retain TCP loopback validation; disk_manifests={disk_manifests}: {stderr}"
        );

        let other_probe = "run wait.until with probe \"file_exists\" and host \"8.8.8.8\" and path \".\" and timeout_ms 0\n";
        let output = execute(root.path(), "check", other_probe, disk_manifests);
        assert!(
            output.status.success(),
            "check applied TCP constraint to another probe; disk_manifests={disk_manifests}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn timeout_and_interval_ranges_match_check_and_runtime() {
    let root = tempfile::tempdir().unwrap();
    let valid_literals = [
        "run wait.until with probe \"file_exists\" and path \".\" and timeout_ms 0\n",
        "run wait.until with probe \"file_exists\" and path \".\" and timeout_ms 86400000 and interval_ms 1\n",
        "run wait.until with probe \"file_exists\" and path \".\" and timeout_ms 0 and interval_ms 10000\n",
    ];
    let invalid_literals = [
        (
            "run wait.until with probe \"file_exists\" and path \".\" and timeout_ms -1\n",
            "timeout_ms must be an integer from 0 through 86400000",
        ),
        (
            "run wait.until with probe \"file_exists\" and path \".\" and timeout_ms 86400001\n",
            "timeout_ms must be an integer from 0 through 86400000",
        ),
        (
            "run wait.until with probe \"file_exists\" and path \".\" and timeout_ms 0 and interval_ms 0\n",
            "interval_ms must be an integer from 1 through 10000",
        ),
        (
            "run wait.until with probe \"file_exists\" and path \".\" and timeout_ms 0 and interval_ms 10001\n",
            "interval_ms must be an integer from 1 through 10000",
        ),
    ];

    for disk_manifests in [false, true] {
        for source in valid_literals {
            for command in ["check", "run"] {
                let output = execute(root.path(), command, source, disk_manifests);
                assert!(
                    output.status.success(),
                    "{command} rejected valid bound; disk_manifests={disk_manifests}: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
        }

        for (source, message) in invalid_literals {
            for command in ["check", "run"] {
                let output = execute(root.path(), command, source, disk_manifests);
                let stderr = String::from_utf8_lossy(&output.stderr);
                assert!(
                    !output.status.success() && stderr.contains(message),
                    "{command} did not reject out-of-range literal; disk_manifests={disk_manifests}: {stderr}"
                );
            }
        }

        for (parameter, valid_values, invalid_values, message) in [
            (
                "timeout_ms",
                [0, 86400000],
                [-1, 86400001],
                "timeout_ms must be an integer from 0 through 86400000",
            ),
            (
                "interval_ms",
                [1, 10000],
                [0, 10001],
                "interval_ms must be an integer from 1 through 10000",
            ),
        ] {
            let source = format!(
                "accept bound as integer\nrun wait.until with probe \"file_exists\" and path \".\" and timeout_ms {}\n",
                if parameter == "timeout_ms" { "bound" } else { "0" }
            );
            let source = if parameter == "interval_ms" {
                source.replace(
                    " and timeout_ms 0",
                    " and timeout_ms 0 and interval_ms bound",
                )
            } else {
                source
            };
            let checked = execute(root.path(), "check", &source, disk_manifests);
            assert!(
                checked.status.success(),
                "check rejected dynamic {parameter}; disk_manifests={disk_manifests}: {}",
                String::from_utf8_lossy(&checked.stderr)
            );

            for value in valid_values {
                let output = execute_with_input(
                    root.path(),
                    &source,
                    &format!("bound={value}"),
                    disk_manifests,
                );
                assert!(
                    output.status.success(),
                    "run rejected dynamic {parameter}={value}; disk_manifests={disk_manifests}: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }

            for value in invalid_values {
                let output = execute_with_input(
                    root.path(),
                    &source,
                    &format!("bound={value}"),
                    disk_manifests,
                );
                let stderr = String::from_utf8_lossy(&output.stderr);
                assert!(
                    !output.status.success() && stderr.contains(message),
                    "run accepted dynamic {parameter}={value}; disk_manifests={disk_manifests}: {stderr}"
                );
            }
        }
    }
}
