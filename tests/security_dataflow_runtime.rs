#![cfg(feature = "remote")]
#![cfg(unix)]

use flow::contracts::{
    AppletBackend, AppletContract, AppletPort, AppletPorts, Lifecycle, OutputType, PortDelivery,
    RemoteEndpointConfig, Sensitivity, ValueSchema,
};
use flow::graph::{
    EdgeSpec, GraphNode, GraphPlan, GraphRunOptions, GraphSession, PortRef, PortSchema,
    RuntimeGraphExecutor,
};
use flow::manifest::Registry;
use flow::remote::{RemoteAppletDescriptor, RemoteAppletIdentity, PROTOCOL_VERSION};
use flow::{ast::Value, parser, runtime, security::DataFlowPolicy};
use serde_json::{json, Value as JsonValue};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::{atomic::AtomicBool, Arc};
use std::time::Duration;

fn options(cwd: &Path) -> runtime::RunOptions {
    runtime::RunOptions {
        max_output_bytes: 1024 * 1024,
        command_timeout: Some(Duration::from_secs(3)),
        total_timeout: Some(Duration::from_secs(5)),
        ..runtime::RunOptions::new(cwd.to_path_buf())
    }
}

fn text_port(sensitivity: Sensitivity) -> AppletPort {
    AppletPort {
        schema: ValueSchema {
            scalar_only: false,
            additional_properties: None,
            min_properties: None,
            max_properties: None,
            nonempty: false,
            kind: OutputType::Text,
            nullable: false,
            enum_values: None,
            items: None,
            fields: BTreeMap::new(),
            required: Vec::new(),
        },
        delivery: PortDelivery::Single,
        sensitivity,
        pointer: None,
        encoding: None,
    }
}

fn remote_manifest(
    endpoint: &str,
    input_sensitivity: Sensitivity,
    cwd: &Path,
) -> (String, AppletContract) {
    let mut endpoint_config = RemoteEndpointConfig::new(endpoint);
    endpoint_config.allow_loopback_http_fixture = true;
    endpoint_config.descriptor_cache = Some("descriptor.json".into());

    let mut contract = AppletContract {
        version: 1,
        backend: AppletBackend::Remote,
        lifecycle: Lifecycle::OneShot,
        program: None,
        remote: Some(endpoint_config),
        environment: None,
        output: Some(flow::contracts::OutputSchema {
            kind: OutputType::Text,
            encoding: flow::contracts::OutputEncoding::Raw,
            enum_values: None,
            same_as_parameter: None,
            items: None,
            fields: BTreeMap::new(),
            required: Vec::new(),
            trim_final_lf: false,
        }),
        outcomes: Vec::new(),
        capabilities: vec!["network.connect".into()],
        effects: Vec::new(),
        result_envelope: false,
        ports: Some(AppletPorts {
            inputs: BTreeMap::from([("message".into(), text_port(input_sensitivity))]),
            outputs: BTreeMap::from([("value".into(), text_port(input_sensitivity))]),
            events: BTreeMap::new(),
        }),
    };
    let mut advertised = contract.clone();
    advertised.backend = AppletBackend::InProcess;
    advertised.remote = None;
    let descriptor = RemoteAppletDescriptor {
        protocol_version: PROTOCOL_VERSION,
        identity: RemoteAppletIdentity {
            name: "fixture.remote".into(),
            version: "1".into(),
        },
        contract: advertised,
        events: Vec::new(),
        idempotency: None,
    };
    let descriptor_bytes = serde_json::to_vec(&descriptor).unwrap();
    fs::write(cwd.join("descriptor.json"), &descriptor_bytes).unwrap();
    fs::set_permissions(
        cwd.join("descriptor.json"),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    let digest = format!("{:x}", Sha256::digest(&descriptor_bytes));
    contract
        .remote
        .as_mut()
        .expect("remote config")
        .descriptor_sha256 = Some(digest);
    contract
        .remote
        .as_ref()
        .unwrap()
        .validate_declaration()
        .unwrap();
    let manifest_value = json!({
        "name": "fixture.remote",
        "binary": "@remote-applet",
        "parameters": {"message": {"type": "string", "required": true}},
        "arguments": [],
        "contract": contract,
    });
    (manifest_value.to_string(), contract)
}

fn policy_for(endpoint: &str, labels: &[&str]) -> DataFlowPolicy {
    DataFlowPolicy::from_json(
        &serde_json::to_vec(&json!({
            "version": 1,
            "remote_sinks": {endpoint: {"allow": labels}},
        }))
        .unwrap(),
    )
    .unwrap()
}

fn reply_to_one_remote_call(listener: TcpListener) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut request_line = String::new();
        reader.read_line(&mut request_line).unwrap();
        let mut content_length = 0usize;
        loop {
            let mut header = String::new();
            reader.read_line(&mut header).unwrap();
            if header == "\r\n" || header.is_empty() {
                break;
            }
            if let Some((name, value)) = header.trim_end().split_once(':') {
                if name.eq_ignore_ascii_case("content-length") {
                    content_length = value.trim().parse().unwrap();
                }
            }
        }
        let mut body = vec![0; content_length];
        reader.read_exact(&mut body).unwrap();
        let request: JsonValue = serde_json::from_slice(&body).unwrap();
        let response = serde_json::to_vec(&json!({
            "protocol_version": 1,
            "request_id": request["request_id"],
            "status": 0,
            "outputs": {"value": {"encoding": "json", "data": "ok"}},
            "error": null,
        }))
        .unwrap();
        let headers = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/vnd.execute.applet+json;version=1\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            response.len()
        );
        stream.write_all(headers.as_bytes()).unwrap();
        stream.write_all(&response).unwrap();
        stream.flush().unwrap();
    })
}

fn capture_one_remote_call(listener: TcpListener) -> std::thread::JoinHandle<Option<Vec<u8>>> {
    std::thread::spawn(move || {
        for _ in 0..200 {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream
                        .set_read_timeout(Some(Duration::from_secs(2)))
                        .unwrap();
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut request_line = String::new();
                    reader.read_line(&mut request_line).unwrap();
                    let mut content_length = 0usize;
                    loop {
                        let mut header = String::new();
                        reader.read_line(&mut header).unwrap();
                        if header == "\r\n" || header.is_empty() {
                            break;
                        }
                        if let Some((name, value)) = header.trim_end().split_once(':') {
                            if name.eq_ignore_ascii_case("content-length") {
                                content_length = value.trim().parse().unwrap();
                            }
                        }
                    }
                    let mut body = vec![0; content_length];
                    reader.read_exact(&mut body).unwrap();
                    let request: JsonValue = serde_json::from_slice(&body).unwrap();
                    let response = serde_json::to_vec(&json!({
                        "protocol_version": 1,
                        "request_id": request["request_id"],
                        "status": 0,
                        "outputs": {"value": {"encoding": "json", "data": "ok"}},
                        "error": null,
                    }))
                    .unwrap();
                    let headers = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/vnd.execute.applet+json;version=1\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        response.len()
                    );
                    stream.write_all(headers.as_bytes()).unwrap();
                    stream.write_all(&response).unwrap();
                    stream.flush().unwrap();
                    return Some(body);
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("checking remote fixture socket: {error}"),
            }
        }
        None
    })
}

fn endpoint_listener() -> (TcpListener, String) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    (listener, format!("http://{address}"))
}

fn invoke_remote(
    registry: &Registry,
    cwd: &Path,
) -> anyhow::Result<runtime::AppletInvocationResult> {
    runtime::invoke_applet(
        registry,
        "fixture.remote",
        BTreeMap::from([("message".into(), Value::String("payload".into()))]),
        Vec::new(),
        &options(cwd),
        Arc::new(AtomicBool::new(false)),
    )
}

#[test]
fn unknown_remote_data_is_allowed_by_exact_unknown_grant_but_secret_is_denied_pre_send() {
    let directory = tempfile::tempdir().unwrap();
    let (listener, endpoint) = endpoint_listener();
    let (manifest, _) = remote_manifest(&endpoint, Sensitivity::Unknown, directory.path());
    let allowed = Registry::from_sources(&[manifest]).unwrap();
    allowed.set_host_state(policy_for(&endpoint, &["unknown"]));
    let responder = reply_to_one_remote_call(listener);
    let result = invoke_remote(&allowed, directory.path()).unwrap();
    assert_eq!(result.status, 0);
    responder.join().unwrap();

    let directory = tempfile::tempdir().unwrap();
    let (listener, endpoint) = endpoint_listener();
    listener.set_nonblocking(true).unwrap();
    let (manifest, _) = remote_manifest(&endpoint, Sensitivity::Secret, directory.path());
    let denied = Registry::from_sources(&[manifest]).unwrap();
    // The grant deliberately contains Unknown but omits Secret.
    denied.set_host_state(policy_for(&endpoint, &["unknown"]));
    assert!(invoke_remote(&denied, directory.path()).is_err());
    assert!(matches!(
        listener.accept(),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock
    ));
}

fn executable(path: &Path, body: &str) {
    fs::write(path, body).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn secret_producer(path: &Path) -> String {
    executable(path, "#!/bin/sh\nprintf 'TOP-SECRET-MARKER'\n");
    json!({
        "name": "fixture.secret",
        "binary": path,
        "parameters": {},
        "arguments": [],
        "contract": {
            "version": 1,
            "output": {"type": "text", "encoding": "raw"},
            "ports": {
                "inputs": {},
                "outputs": {
                    "value": {
                        "schema": {"type": "text"},
                        "delivery": "single",
                        "sensitivity": "secret"
                    }
                },
                "events": {}
            }
        }
    })
    .to_string()
}

fn stateful_worker(directory: &Path) -> String {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/json_lines_worker.py");
    json!({
        "name": "fixture.worker",
        "binary": "python3",
        "arguments": [],
        "accepts_stdin": true,
        "contract": {
            "version": 1,
            "backend": "json_lines_worker",
            "lifecycle": {
                "kind": "json_lines",
                "startup_arguments": [
                    fixture.to_string_lossy(),
                    "--worker",
                    "remember_first",
                    directory.join("worker-launches").to_string_lossy(),
                    directory.join("worker-effects").to_string_lossy()
                ]
            },
            "output": {"type": "text", "encoding": "raw"},
            "ports": {
                "inputs": {},
                "outputs": {"value": {
                    "schema": {"type": "text"},
                    "delivery": "single",
                    "sensitivity": "public"
                }},
                "events": {}
            }
        }
    })
    .to_string()
}

#[test]
fn retained_worker_secret_is_included_in_the_next_run_egress_floor() {
    let directory = tempfile::tempdir().unwrap();
    let producer = directory.path().join("secret-producer");
    let producer_manifest = secret_producer(&producer);
    let worker_manifest = stateful_worker(directory.path());
    let (listener, endpoint) = endpoint_listener();
    listener.set_nonblocking(true).unwrap();
    let (remote, _) = remote_manifest(&endpoint, Sensitivity::Public, directory.path());
    let registry = Registry::from_sources(&[producer_manifest, worker_manifest, remote]).unwrap();
    registry.set_host_state(policy_for(&endpoint, &["public", "unknown"]));

    let seed = parser::parse("run fixture.secret\nthen fixture.worker\n").unwrap();
    runtime::run(&seed, &registry, &options(directory.path())).unwrap();

    let read_and_send =
        parser::parse("run fixture.worker as token\nthen fixture.remote with message token\n")
            .unwrap();
    let responder = capture_one_remote_call(listener);
    let result = runtime::run(&read_and_send, &registry, &options(directory.path()));
    let remote_request = responder.join().unwrap();
    match result {
        Err(error) => {
            assert!(format!("{error:#}").contains("data flow"), "{error:#}");
            assert!(remote_request.is_none());
        }
        Ok(_) => {
            let body = remote_request.expect("remote applet received a request");
            assert!(
                body.windows(b"TOP-SECRET-MARKER".len())
                    .any(|window| window == b"TOP-SECRET-MARKER"),
                "remote request did not carry the retained secret: {}",
                String::from_utf8_lossy(&body)
            );
            panic!("retained secret unexpectedly reached the remote applet");
        }
    }
}

#[test]
fn retained_worker_secret_flows_through_a_later_graph_to_remote_authorization() {
    use flow::ast::StepKind;

    let directory = tempfile::tempdir().unwrap();
    let producer = directory.path().join("secret-producer");
    let producer_manifest = secret_producer(&producer);
    let worker_manifest = stateful_worker(directory.path());
    let (listener, endpoint) = endpoint_listener();
    listener.set_nonblocking(true).unwrap();
    let (remote, _) = remote_manifest(&endpoint, Sensitivity::Public, directory.path());
    let registry = Registry::from_sources(&[producer_manifest, worker_manifest, remote]).unwrap();
    registry.set_host_state(policy_for(&endpoint, &["public", "unknown"]));

    let seed = parser::parse("run fixture.secret\nthen fixture.worker\n").unwrap();
    runtime::run(&seed, &registry, &options(directory.path())).unwrap();

    let worker_call = match parser::parse("run fixture.worker\n").unwrap().steps[0]
        .kind
        .clone()
    {
        StepKind::Invoke { call, .. } => call,
        _ => panic!("worker invocation expected"),
    };
    let remote_call = match parser::parse("run fixture.remote\n").unwrap().steps[0]
        .kind
        .clone()
    {
        StepKind::Invoke { call, .. } => call,
        _ => panic!("remote invocation expected"),
    };

    let mut worker_output = PortSchema::single(ValueSchema {
        scalar_only: false,
        additional_properties: None,
        min_properties: None,
        max_properties: None,
        nonempty: false,
        kind: OutputType::Text,
        nullable: false,
        enum_values: None,
        fields: BTreeMap::new(),
        required: Vec::new(),
        items: None,
    });
    worker_output.sensitivity = Sensitivity::Public;
    let mut remote_input = worker_output.clone();
    remote_input.required = true;
    let mut plan = GraphPlan::new("retained-worker-secret-egress");
    plan.add_node(GraphNode::applet(
        "worker",
        worker_call,
        BTreeMap::new(),
        BTreeMap::from([("value".into(), worker_output)]),
    ))
    .unwrap();
    plan.add_node(GraphNode::applet(
        "remote",
        remote_call,
        BTreeMap::from([("message".into(), remote_input)]),
        BTreeMap::new(),
    ))
    .unwrap();
    plan.add_edge(EdgeSpec::lossless(
        PortRef::new("worker", "value"),
        PortRef::new("remote", "message"),
        1,
    ));
    plan.validate_with_registry(&registry).unwrap();

    let executor = Arc::new(RuntimeGraphExecutor::new(
        registry,
        options(directory.path()),
    ));
    let error = GraphSession::start(plan, executor, GraphRunOptions::default())
        .unwrap()
        .run_to_completion()
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("runtime data flow into remote applet"),
        "{error:#}"
    );
    assert!(matches!(
        listener.accept(),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock
    ));
}

#[test]
fn task_public_consumer_cannot_lower_secret_producer_provenance() {
    let directory = tempfile::tempdir().unwrap();
    let producer = directory.path().join("secret-producer");
    let producer_manifest = secret_producer(&producer);
    let (listener, endpoint) = endpoint_listener();
    listener.set_nonblocking(true).unwrap();
    let (remote, _) = remote_manifest(&endpoint, Sensitivity::Public, directory.path());
    let registry = Registry::from_sources(&[producer_manifest, remote]).unwrap();
    registry.set_host_state(policy_for(&endpoint, &["public", "unknown"]));
    let program = parser::parse(
        "run fixture.secret as token\nparallel at most 1 as results\n  run fixture.remote with message token as submitted\n",
    )
    .unwrap();
    let error = match runtime::run(&program, &registry, &options(directory.path())) {
        Err(error) => error,
        Ok(_) => panic!("secret producer data unexpectedly reached the remote task"),
    };
    assert!(format!("{error:#}").contains("data flow"), "{error:#}");
    assert!(matches!(
        listener.accept(),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock
    ));
}

#[test]
fn secret_listener_event_label_survives_into_service_handler_authorization() {
    let directory = tempfile::tempdir().unwrap();
    let listener_script = directory.path().join("secret-listener");
    executable(
        &listener_script,
        "#!/bin/sh\nprintf 'TOP-SECRET-EVENT\\n'\n",
    );
    let listener_manifest = json!({
        "name": "fixture.listener",
        "binary": listener_script,
        "parameters": {},
        "arguments": [],
        "contract": {
            "version": 1,
            "ports": {
                "inputs": {},
                "outputs": {},
                "events": {
                    "records": {
                        "schema": {"type": "text"},
                        "delivery": "events",
                        "sensitivity": "secret"
                    }
                }
            }
        }
    })
    .to_string();
    let (listener, endpoint) = endpoint_listener();
    listener.set_nonblocking(true).unwrap();
    let (remote, _) = remote_manifest(&endpoint, Sensitivity::Public, directory.path());
    let registry = Registry::from_sources(&[listener_manifest, remote]).unwrap();
    registry.set_host_state(policy_for(&endpoint, &["public", "unknown"]));
    let program = parser::parse(
        "listen to fixture.listener\nwhenever a line arrives as record\n  run fixture.remote with message record as result\n",
    )
    .unwrap();
    let remote_contacted = std::thread::spawn(move || {
        for _ in 0..100 {
            match listener.accept() {
                Ok(_) => return true,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("checking listener socket: {error}"),
            }
        }
        false
    });
    let error = match runtime::run_service(
        &program,
        &registry,
        &options(directory.path()),
        &runtime::ServiceLimits {
            max_runs: Some(1),
            max_record_bytes: 1024,
        },
        |_| Ok(()),
    ) {
        Err(error) => error,
        Ok(_) => panic!("secret event data unexpectedly reached the remote handler"),
    };
    assert!(format!("{error:#}").contains("data flow"), "{error:#}");
    assert!(!remote_contacted.join().unwrap());
}

#[test]
fn explicit_secret_blocks_save_checkpoint_and_declared_filesystem_write_without_policy() {
    let directory = tempfile::tempdir().unwrap();
    let producer = directory.path().join("secret-producer");
    let producer_manifest = secret_producer(&producer);
    let registry = Registry::from_sources(&[producer_manifest]).unwrap();

    let save_path = directory.path().join("saved-secret");
    let save_program = parser::parse(&format!(
        "run fixture.secret as token\nthen save to file {:?}\n",
        save_path
    ))
    .unwrap();
    assert!(runtime::run(&save_program, &registry, &options(directory.path())).is_err());
    assert!(!save_path.exists());

    let checkpoint_program_path = directory.path().join("checkpoint.program");
    fs::write(&checkpoint_program_path, "run fixture.secret\n").unwrap();
    let checkpoint_program = parser::parse("run fixture.secret\n").unwrap();
    let checkpoint_directory = directory.path().join("secret-journal");
    let checkpoint = runtime::CheckpointOptions {
        directory: checkpoint_directory.clone(),
        program_path: checkpoint_program_path,
        resume: false,
        retry_uncertain: false,
    };
    assert!(runtime::run_checkpointed(
        &checkpoint_program,
        &registry,
        &options(directory.path()),
        &checkpoint,
    )
    .is_err());
    assert!(!checkpoint_directory.exists());

    let write_marker = directory.path().join("write-ran");
    let writer = directory.path().join("writer");
    executable(&writer, "#!/bin/sh\nprintf ran > \"$1\"\n");
    let writer_manifest = json!({
        "name": "fixture.writer",
        "binary": writer,
        "parameters": {"path": {"type": "path", "required": true}},
        "arguments": [{"parameter": "path"}],
        "contract": {
            "version": 1,
            "capabilities": ["filesystem.write"],
            "effects": ["filesystem.write"]
        }
    })
    .to_string();
    let registry = Registry::from_sources(&[secret_producer(&producer), writer_manifest]).unwrap();
    let write_program = parser::parse(&format!(
        "run fixture.secret as token\nrun fixture.writer with path {:?}\n",
        write_marker
    ))
    .unwrap();
    assert!(runtime::run(&write_program, &registry, &options(directory.path())).is_err());
    assert!(!write_marker.exists());
}
