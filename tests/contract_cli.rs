use serde_json::{json, Value};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Command, Output, Stdio};

fn cli(root: &Path, arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_execute"))
        .args(arguments)
        .current_dir(root)
        .output()
        .expect("start execute")
}

fn write_manifest(directory: &Path, name: &str, binary: &str) {
    let value = json!({
        "name": name,
        "binary": binary,
        "parameters": {"path": {"type": "string", "required": true}},
        "arguments": [{"parameter": "path"}],
        "contract": {"version": 1}
    });
    fs::write(
        directory.join(format!("{name}.json")),
        serde_json::to_vec_pretty(&value).unwrap(),
    )
    .unwrap();
}

fn decoded(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap()
}

#[cfg(unix)]
fn live_request(socket: &Path, request: Value) -> Value {
    use std::os::unix::net::UnixStream;

    let mut stream = UnixStream::connect(socket).unwrap();
    writeln!(stream, "{}", request).unwrap();
    let mut response = String::new();
    BufReader::new(stream).read_line(&mut response).unwrap();
    serde_json::from_str(&response).unwrap()
}

#[cfg(unix)]
struct ChildCleanup(std::process::Child);

#[cfg(unix)]
impl Drop for ChildCleanup {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

#[test]
fn applet_inspection_projects_typed_contract_and_keeps_legacy_fields() {
    let root = tempfile::tempdir().unwrap();
    let manifests = root.path().join("manifests");
    fs::create_dir(&manifests).unwrap();
    let manifest = json!({
        "name": "fixture.contract",
        "phrases": ["contract fixture"],
        "binary": "fixture-command-that-is-not-run",
        "parameters": {
            "path": {"type": "string", "required": true}
        },
        "arguments": [{"parameter": "path"}],
        "stdio": {"stdout": "capture", "stderr": "inherit"},
        "contract": {
            "version": 1,
            "backend": "command",
            "output": {"type": "text", "encoding": "raw", "trim_final_lf": true},
            "outcomes": [
                {"status": 0, "name": "ok", "kind": "success"},
                {"status": 1, "name": "missing", "kind": "absence"}
            ],
            "capabilities": ["filesystem.read"],
            "effects": ["filesystem.read"]
        }
    });
    fs::write(
        manifests.join("fixture.json"),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();

    let output = cli(
        root.path(),
        &[
            "applets",
            "inspect",
            "fixture.contract",
            "--manifest-dir",
            manifests.to_str().unwrap(),
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report = decoded(&output);
    let contract = &report["applet"];
    assert_eq!(report["protocol"], "execute.applet/1");
    assert_eq!(contract["contract_version"], 1);
    assert_eq!(contract["backend"], "native_process");
    assert_eq!(
        contract["parameters_schema"]["properties"]["path"]["type"],
        "string"
    );
    assert_eq!(contract["argv_template"][0]["parameter"], "path");
    assert_eq!(contract["io"]["output_schema"]["type"], "string");
    assert_eq!(contract["io"]["output_schema"]["encoding"], "raw");
    assert_eq!(contract["io"]["output_schema"]["trim_final_lf"], true);
    assert_eq!(contract["io"]["stdio"]["stdout"], "capture");
    assert_eq!(contract["execution"]["contract_backend"], "command");
    assert_eq!(
        contract["execution"]["capabilities"],
        json!(["filesystem.read"])
    );
    assert_eq!(contract["execution"]["effects"], json!(["filesystem.read"]));
    assert_eq!(contract["execution"]["outcomes"][1]["name"], "missing");
    assert_eq!(contract["execution"]["lifecycle"]["kind"], "oneshot");
    assert_eq!(contract["execution"]["result_envelope"], false);
    // Existing discovery fields are kept alongside the versioned projection.
    assert_eq!(contract["name"], "fixture.contract");
    assert_eq!(contract["execution"]["shell_expansion"], false);
    assert_eq!(contract["execution"]["availability"], "not_checked");
}

#[cfg(unix)]
#[test]
fn strict_policy_preflights_every_invocation_before_the_first_command_runs() {
    let root = tempfile::tempdir().unwrap();
    let manifests = root.path().join("manifests");
    fs::create_dir(&manifests).unwrap();
    write_manifest(&manifests, "before", "touch");
    write_manifest(&manifests, "denied", "touch");
    let marker = root.path().join("must-not-exist");
    let later = root.path().join("also-not-exist");
    let program = root.path().join("main.program");
    fs::write(
        &program,
        format!(
            "run before with path {:?}\nrun denied with path {:?}\n",
            marker.to_string_lossy(),
            later.to_string_lossy()
        ),
    )
    .unwrap();
    let policy = root.path().join("policy.json");
    fs::write(
        &policy,
        r#"{"version":1,"applets":{"before":{"capabilities":["process.exec"]}}}"#,
    )
    .unwrap();

    let output = cli(
        root.path(),
        &[
            "run",
            program.to_str().unwrap(),
            "--manifest-dir",
            manifests.to_str().unwrap(),
            "--capability-policy",
            policy.to_str().unwrap(),
        ],
    );
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("no allowlist entry"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !marker.exists(),
        "preflight must precede every native effect"
    );
    assert!(!later.exists());
}

#[cfg(unix)]
#[test]
fn live_policy_rejects_a_source_patch_before_committing_it() {
    let root = tempfile::tempdir().unwrap();
    let manifests = root.path().join("manifests");
    fs::create_dir(&manifests).unwrap();
    write_manifest(&manifests, "before", "touch");
    write_manifest(&manifests, "denied", "touch");
    let policy = root.path().join("policy.json");
    fs::write(
        &policy,
        r#"{"version":1,"applets":{"before":{"capabilities":["process.exec"]}}}"#,
    )
    .unwrap();
    let program = root.path().join("live.program");
    let source = "run before with path \"never-created\"\n";
    fs::write(&program, source).unwrap();
    let socket = root.path().join("control.sock");
    let child = Command::new(env!("CARGO_BIN_EXE_execute"))
        .args([
            "live",
            "serve",
            program.to_str().unwrap(),
            "--socket",
            socket.to_str().unwrap(),
            "--manifest-dir",
            manifests.to_str().unwrap(),
            "--capability-policy",
            policy.to_str().unwrap(),
            "--paused",
        ])
        .current_dir(root.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut child = ChildCleanup(child);
    let mut stdout = BufReader::new(child.0.stdout.take().unwrap());
    let mut ready = String::new();
    stdout.read_line(&mut ready).unwrap();
    assert!(serde_json::from_str::<Value>(&ready).unwrap()["event"] == "ready");

    let replacement = format!(
        "run before with path \"never-created\"\nrun denied with path {:?}\n",
        root.path().join("denied-effect").to_string_lossy()
    );
    let rejected = live_request(
        &socket,
        json!({
            "op": "patch",
            "operations": [{"op": "replace", "path": "/source", "value": replacement}]
        }),
    );
    assert_eq!(rejected["ok"], false);
    assert!(rejected["error"]["message"]
        .as_str()
        .unwrap()
        .contains("no allowlist entry"));
    let state = live_request(&socket, json!({"op":"read"}));
    assert_eq!(state["result"]["state"]["source"], source);

    let stopped = live_request(&socket, json!({"op":"control","action":"stop"}));
    assert_eq!(stopped["ok"], true);
    assert!(child.0.wait().unwrap().success());
}

#[cfg(unix)]
#[test]
fn live_agent_policy_requires_credentials_and_never_returns_them() {
    use std::os::unix::fs::PermissionsExt;

    let root = tempfile::tempdir().unwrap();
    let program = root.path().join("live.program");
    fs::write(&program, "run printf with message \"ready\"\n").unwrap();
    let token_file = root.path().join("reader.token");
    let reader_created = cli(
        root.path(),
        &[
            "live",
            "token-create",
            "--agent",
            "reader",
            "--token-file",
            token_file.to_str().unwrap(),
        ],
    );
    assert!(reader_created.status.success());
    let reader_created = decoded(&reader_created);
    let token = reader_created["token"].as_str().unwrap().to_owned();
    let digest = reader_created["token_sha256"].as_str().unwrap().to_owned();
    assert_eq!(
        fs::metadata(&token_file).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let operator_file = root.path().join("operator.token");
    let operator_created = cli(
        root.path(),
        &[
            "live",
            "token-create",
            "--agent",
            "operator",
            "--token-file",
            operator_file.to_str().unwrap(),
        ],
    );
    assert!(operator_created.status.success());
    let operator_created = decoded(&operator_created);
    let operator_token = operator_created["token"].as_str().unwrap().to_owned();
    let operator_hash = operator_created["token_sha256"]
        .as_str()
        .unwrap()
        .to_owned();
    let policy_file = root.path().join("agents.json");
    fs::write(
        &policy_file,
        serde_json::to_vec(&json!({
            "version": 1,
            "agents": {
                "reader": {"token_sha256": digest, "permissions": ["read"]},
                "operator": {"token_sha256": operator_hash, "permissions": ["stop"]}
            }
        }))
        .unwrap(),
    )
    .unwrap();
    fs::set_permissions(&policy_file, fs::Permissions::from_mode(0o600)).unwrap();
    let socket = root.path().join("control.sock");
    let child = Command::new(env!("CARGO_BIN_EXE_execute"))
        .args([
            "live",
            "serve",
            program.to_str().unwrap(),
            "--socket",
            socket.to_str().unwrap(),
            "--agent-policy",
            policy_file.to_str().unwrap(),
            "--paused",
        ])
        .current_dir(root.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut child = ChildCleanup(child);
    let mut stdout = BufReader::new(child.0.stdout.take().unwrap());
    let mut ready = String::new();
    stdout.read_line(&mut ready).unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&ready).unwrap()["event"],
        "ready"
    );

    let denied = live_request(&socket, json!({"op":"read"}));
    assert_eq!(denied["ok"], false);
    let allowed = Command::new(env!("CARGO_BIN_EXE_execute"))
        .args([
            "live",
            "read",
            "--socket",
            socket.to_str().unwrap(),
            "--agent",
            "reader",
            "--token-file",
            token_file.to_str().unwrap(),
        ])
        .current_dir(root.path())
        .output()
        .unwrap();
    assert!(
        allowed.status.success(),
        "{}",
        String::from_utf8_lossy(&allowed.stderr)
    );
    assert!(!String::from_utf8_lossy(&allowed.stdout).contains(token.as_str()));

    let stopped = live_request(
        &socket,
        json!({"op":"control","action":"stop","auth":{"agent":"operator","token":operator_token}}),
    );
    assert_eq!(stopped["ok"], true);
    assert!(child.0.wait().unwrap().success());
}

#[test]
fn policy_input_is_bounded_and_check_json_reports_parse_failures() {
    let root = tempfile::tempdir().unwrap();
    let program = root.path().join("main.program");
    fs::write(&program, "run \"true\"\n").unwrap();
    let policy = root.path().join("policy.json");
    fs::write(&policy, "{\"version\":1,\"unexpected\":true}").unwrap();

    let invalid = cli(
        root.path(),
        &[
            "check",
            program.to_str().unwrap(),
            "--json",
            "--capability-policy",
            policy.to_str().unwrap(),
        ],
    );
    assert!(!invalid.status.success());
    let report = decoded(&invalid);
    assert_eq!(report["protocol"], "execute.check/1");
    assert_eq!(report["ok"], false);
    assert!(report["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .any(|diagnostic| {
            diagnostic["code"] == "check_failed"
                && diagnostic["message"]
                    .as_str()
                    .is_some_and(|message| message.contains("capability policy"))
        }));

    fs::write(&policy, vec![b' '; 1024 * 1024 + 1]).unwrap();
    let oversized = cli(
        root.path(),
        &[
            "inspect",
            program.to_str().unwrap(),
            "--capability-policy",
            policy.to_str().unwrap(),
        ],
    );
    assert!(!oversized.status.success());
    assert!(String::from_utf8_lossy(&oversized.stderr).contains("exceeds 1048576 bytes"));

    fs::write(&policy, r#"{"version":1,"applets":{}}"#).unwrap();
    let warning = cli(
        root.path(),
        &[
            "check",
            program.to_str().unwrap(),
            "--json",
            "--capability-policy",
            policy.to_str().unwrap(),
        ],
    );
    let report = decoded(&warning);
    assert!(report["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| {
            item["code"] == "data_flow_policy_not_configured"
                && item["message"]
                    .as_str()
                    .unwrap()
                    .contains("remote egress and persistence")
        }));
}

#[test]
fn help_lists_the_opt_in_policy_and_contract_inspection_details() {
    let root = tempfile::tempdir().unwrap();
    let output = cli(root.path(), &["--help"]);
    let help = String::from_utf8_lossy(&output.stdout);
    assert!(help.contains("--capability-policy FILE"));
    assert!(help.contains("remote egress and persistence need --data-flow-policy"));
    assert!(help.contains("--total-timeout-ms N"));

    let applets = cli(root.path(), &["applets", "--help"]);
    let help = String::from_utf8_lossy(&applets.stdout);
    assert!(help.contains("typed I/O, outcomes, effects, backends, and lifecycle"));
}

#[cfg(unix)]
#[test]
fn total_timeout_is_enforced_for_finite_run() {
    let root = tempfile::tempdir().unwrap();
    let workflow = root.path().join("slow.program");
    fs::write(&workflow, "run \"sleep\" with arguments [\"1\"]\n").unwrap();
    let output = cli(
        root.path(),
        &[
            "run",
            workflow.to_str().unwrap(),
            "--total-timeout-ms",
            "40",
        ],
    );
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("timed out"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn applet_discovery_and_typed_matching_are_machine_readable_without_execution() {
    let root = tempfile::tempdir().unwrap();
    let discovered = cli(root.path(), &["applets", "discover"]);
    assert!(
        discovered.status.success(),
        "{}",
        String::from_utf8_lossy(&discovered.stderr)
    );
    assert_eq!(
        decoded(&discovered)["protocol"],
        "execute.applets-discovery/1"
    );

    let manifests = root.path().join("manifests");
    fs::create_dir(&manifests).unwrap();
    for (name, ports) in [
        (
            "producer",
            json!({"outputs":{"value":{"schema":{"type":"text"},"delivery":"single"}}}),
        ),
        (
            "consumer",
            json!({"inputs":{"value":{"schema":{"type":"text"},"delivery":"single"}}}),
        ),
    ] {
        fs::write(
            manifests.join(format!("{name}.json")),
            serde_json::to_vec(&json!({
                "name": name,
                "binary": "must-not-be-executed",
                "parameters": if name == "consumer" {
                    json!({"value":{"type":"string","required":true}})
                } else {
                    json!({})
                },
                "arguments": if name == "consumer" {
                    json!([{"parameter":"value"}])
                } else {
                    json!([])
                },
                "contract": {
                    "version": 1,
                    "output": {"type":"text","encoding":"raw"},
                    "ports": ports
                }
            }))
            .unwrap(),
        )
        .unwrap();
    }
    let request = json!({
        "source": {"applet":"producer", "port":"value"},
        "target": {"applet":"consumer", "port":"value"}
    });
    let matched = cli(
        root.path(),
        &[
            "applets",
            "match",
            "--json",
            &request.to_string(),
            "--manifest-dir",
            manifests.to_str().unwrap(),
        ],
    );
    assert!(
        matched.status.success(),
        "{}",
        String::from_utf8_lossy(&matched.stderr)
    );
    let report = decoded(&matched);
    assert_eq!(report["protocol"], "execute.applet-matches/1");
    assert_eq!(report["matches"].as_array().unwrap().len(), 1);
    assert_eq!(report["matches"][0]["compatible"], true);
}

#[test]
fn hidden_remote_resolver_emits_only_numeric_socket_addresses() {
    let root = tempfile::tempdir().unwrap();
    let resolved = cli(root.path(), &["__remote_resolve", "127.0.0.1", "443"]);
    assert!(
        resolved.status.success(),
        "{}",
        String::from_utf8_lossy(&resolved.stderr)
    );
    let output = String::from_utf8(resolved.stdout).unwrap();
    let addresses = output.lines().collect::<Vec<_>>();
    assert_eq!(addresses, vec!["127.0.0.1:443"]);
    for address in addresses {
        assert_eq!(address.parse::<std::net::SocketAddr>().unwrap().port(), 443);
    }
}

#[test]
fn modules_verify_and_service_generate_are_explicit_read_or_render_operations() {
    let root = tempfile::tempdir().unwrap();
    let examples = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/modules");
    let package = examples.join("package");
    let manifest = package.join("program.modules.json");
    let lock = package.join("program.modules.lock.json");
    let verify = cli(
        root.path(),
        &[
            "modules",
            "verify",
            manifest.to_str().unwrap(),
            "--root",
            examples.to_str().unwrap(),
            "--lock",
            lock.to_str().unwrap(),
        ],
    );
    assert!(
        verify.status.success(),
        "{}",
        String::from_utf8_lossy(&verify.stderr)
    );
    assert_eq!(decoded(&verify)["validation"], "package_and_lock");

    let workflow = root.path().join("scheduled.program");
    fs::write(
        &workflow,
        "every 1 hour\n  run \"printf\" with arguments [\"tick\"]\n",
    )
    .unwrap();
    let executable = env!("CARGO_BIN_EXE_execute");
    let rendered = cli(
        root.path(),
        &[
            "service",
            "generate",
            "systemd",
            workflow.to_str().unwrap(),
            "--working-dir",
            root.path().to_str().unwrap(),
        ],
    );
    assert!(
        rendered.status.success(),
        "{}",
        String::from_utf8_lossy(&rendered.stderr)
    );
    let rendered = String::from_utf8(rendered.stdout).unwrap();
    assert!(rendered.contains("[Service]"));
    assert!(rendered.contains(executable));
    assert!(rendered.contains("scheduled.program"));
    assert!(!root.path().join("scheduled.service").exists());
}

#[test]
fn schedule_flags_are_bounded_and_rejected_for_finite_workflows() {
    let root = tempfile::tempdir().unwrap();
    let workflow = root.path().join("finite.program");
    fs::write(&workflow, "run printf with message \"unused\"\n").unwrap();
    let finite = cli(
        root.path(),
        &[
            "run",
            workflow.to_str().unwrap(),
            "--missed-ticks",
            "coalesce",
        ],
    );
    assert!(!finite.status.success());
    assert!(String::from_utf8_lossy(&finite.stderr).contains("only to triggered"));
    let unbounded = cli(
        root.path(),
        &[
            "run",
            workflow.to_str().unwrap(),
            "--missed-ticks",
            "catch-up",
        ],
    );
    assert!(!unbounded.status.success());
    assert!(String::from_utf8_lossy(&unbounded.stderr).contains("requires --max-catch-up"));
    let too_many = cli(
        root.path(),
        &[
            "run",
            workflow.to_str().unwrap(),
            "--missed-ticks",
            "catch-up",
            "--max-catch-up",
            "65",
        ],
    );
    assert!(!too_many.status.success());
    assert!(String::from_utf8_lossy(&too_many.stderr).contains("between 1 and 64"));
}
