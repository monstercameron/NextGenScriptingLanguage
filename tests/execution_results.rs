use flow::ast::Value;
use flow::contracts::{
    AppletBackend, AppletContract, AppletPort, AppletPorts, EffectCertainty, Lifecycle,
    OutcomeKind, OutcomeSpec, OutputEncoding, OutputSchema, OutputType, PortDelivery,
    RemoteEndpointConfig, ValueSchema,
};
use flow::manifest::Registry;
use flow::remote::{RemoteAppletDescriptor, RemoteAppletIdentity, PROTOCOL_VERSION};
use flow::{parser, runtime};
use serde_json::{json, Value as JsonValue};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read};
use std::net::TcpListener;
use std::path::Path;
use std::process::Command as ProcessCommand;
use std::thread;

fn options(cwd: &Path) -> runtime::RunOptions {
    runtime::RunOptions {
        inputs: BTreeMap::new(),
        working_directory: cwd.to_path_buf(),
        cache_directory: None,
        trace: false,
        max_output_bytes: 1024 * 1024,
        command_timeout: None,
        total_timeout: None,
    }
}

fn registry(manifest: JsonValue) -> Registry {
    Registry::from_sources(&[serde_json::to_string(&manifest).unwrap()]).unwrap()
}

fn record(value: Value) -> BTreeMap<String, Value> {
    let Value::Record(fields) = value else {
        panic!("expected an execution result record");
    };
    fields
}

#[cfg(unix)]
#[test]
fn native_fail_fast_result_keeps_actual_status_and_pipeline_stage() {
    let directory = tempfile::tempdir().unwrap();
    let error = runtime::run(
        &parser::parse(
            "run \"sh\" with arguments [\"-c\", \"printf partial\"]\npipe to \"sh\" with arguments [\"-c\", \"cat >/dev/null; exit 7\"]\n",
        )
        .unwrap(),
        &Registry::default(),
        &options(directory.path()),
    )
    .err()
    .expect("native command should fail");
    let result = record(runtime::execution_result::error_value(&error));

    assert_eq!(result.get("kind"), Some(&Value::String("failure".into())));
    assert_eq!(result.get("outcome"), Some(&Value::String("exit_7".into())));
    assert_eq!(result.get("status"), Some(&Value::Integer(7)));
    assert_eq!(
        result.get("effect_certainty"),
        Some(&Value::String("uncertain".into()))
    );
    // Fail-fast may observe the exit before collecting the terminal pipe.
    assert_eq!(result.get("stdout_captured"), Some(&Value::Bool(false)));
    let Some(Value::List(stages)) = result.get("stages") else {
        panic!("expected stage records");
    };
    assert_eq!(stages.len(), 2);
    let earlier_stage = record(stages[0].clone());
    assert_eq!(
        earlier_stage.get("state"),
        Some(&Value::String("exited".into()))
    );
    assert_eq!(earlier_stage.get("status"), Some(&Value::Integer(0)));
    let stage = record(stages[1].clone());
    assert_eq!(stage.get("state"), Some(&Value::String("exited".into())));
    assert_eq!(stage.get("status"), Some(&Value::Integer(7)));
    assert_eq!(
        stage.get("effect_certainty"),
        Some(&Value::String("uncertain".into()))
    );
}

#[cfg(unix)]
#[test]
fn native_timeout_keeps_uncertain_effects_without_inventing_an_exit_status() {
    let directory = tempfile::tempdir().unwrap();
    let mut run_options = options(directory.path());
    run_options.command_timeout = Some(std::time::Duration::from_millis(30));
    let error = runtime::run(
        &parser::parse("run \"sh\" with arguments [\"-c\", \"sleep 1\"]\n").unwrap(),
        &Registry::default(),
        &run_options,
    )
    .err()
    .expect("command should time out");
    let result = record(runtime::execution_result::error_value(&error));

    assert_eq!(result.get("kind"), Some(&Value::String("timeout".into())));
    assert_eq!(result.get("status"), Some(&Value::Null));
    assert_eq!(
        result.get("effect_certainty"),
        Some(&Value::String("uncertain".into()))
    );
}

#[cfg(unix)]
#[test]
fn native_output_limit_keeps_the_captured_prefix_without_a_synthetic_status() {
    let directory = tempfile::tempdir().unwrap();
    let mut run_options = options(directory.path());
    run_options.max_output_bytes = 4;
    let error = runtime::run(
        &parser::parse("run \"printf\" with arguments [\"12345678\"]\n").unwrap(),
        &Registry::default(),
        &run_options,
    )
    .err()
    .expect("command output should exceed the configured cap");
    let result = record(runtime::execution_result::error_value(&error));

    assert_eq!(
        result.get("kind"),
        Some(&Value::String("output_limit".into()))
    );
    assert_eq!(result.get("status"), Some(&Value::Null));
    assert_eq!(result.get("stdout_captured"), Some(&Value::Bool(true)));
    assert_eq!(
        result.get("stdout"),
        Some(&Value::List(vec![
            Value::Integer(b'1' as i64),
            Value::Integer(b'2' as i64),
            Value::Integer(b'3' as i64),
            Value::Integer(b'4' as i64),
        ]))
    );
    assert_eq!(
        result.get("effect_certainty"),
        Some(&Value::String("uncertain".into()))
    );
}

#[cfg(unix)]
#[test]
fn native_signal_keeps_signal_separate_from_an_exit_status() {
    let directory = tempfile::tempdir().unwrap();
    let error = runtime::run(
        &parser::parse("run \"sh\" with arguments [\"-c\", \"kill -TERM $$\"]\n").unwrap(),
        &Registry::default(),
        &options(directory.path()),
    )
    .err()
    .expect("self-signaled command should fail");
    let result = record(runtime::execution_result::error_value(&error));

    assert_eq!(result.get("kind"), Some(&Value::String("signaled".into())));
    assert_eq!(result.get("status"), Some(&Value::Null));
    assert_eq!(result.get("signal"), Some(&Value::Integer(15)));
    let Some(Value::List(stages)) = result.get("stages") else {
        panic!("expected stage records");
    };
    let stage = record(stages[0].clone());
    assert_eq!(stage.get("state"), Some(&Value::String("signaled".into())));
    assert_eq!(stage.get("status"), Some(&Value::Null));
    assert_eq!(stage.get("signal"), Some(&Value::Integer(15)));
}

#[test]
fn in_process_failure_keeps_helper_diagnostic_and_declared_outcome() {
    let directory = tempfile::tempdir().unwrap();
    let catalog = registry(json!({
        "name": "unknown.fixture",
        "binary": "@execute-applet",
        "arguments": [],
        "contract": {
            "version": 1,
            "backend": "in_process",
            "outcomes": [
                {"status": 0, "name": "success", "kind": "success"},
                {"status": 2, "name": "failure", "kind": "failure"}
            ]
        }
    }));
    let error = runtime::run(
        &parser::parse("run unknown.fixture\n").unwrap(),
        &catalog,
        &options(directory.path()),
    )
    .err()
    .expect("in-process applet should fail");
    let result = record(runtime::execution_result::error_value(&error));

    assert_eq!(result.get("kind"), Some(&Value::String("failure".into())));
    assert_eq!(
        result.get("outcome"),
        Some(&Value::String("failure".into()))
    );
    assert_eq!(result.get("status"), Some(&Value::Integer(2)));
    assert_eq!(
        result.get("effect_certainty"),
        Some(&Value::String("uncertain".into()))
    );
    assert_eq!(result.get("stderr_captured"), Some(&Value::Bool(true)));
    let Some(Value::List(stderr)) = result.get("stderr") else {
        panic!("expected captured stderr bytes");
    };
    let diagnostic = stderr
        .iter()
        .map(|byte| match byte {
            Value::Integer(byte) => u8::try_from(*byte).unwrap(),
            _ => panic!("expected stderr byte"),
        })
        .collect::<Vec<_>>();
    assert!(String::from_utf8(diagnostic)
        .unwrap()
        .contains("unknown_operation"));
}

#[cfg(unix)]
#[test]
fn successful_native_pipeline_exposes_each_actual_stage_status() {
    let directory = tempfile::tempdir().unwrap();
    let result = runtime::run(
        &parser::parse(
            "run \"sh\" with arguments [\"-c\", \"printf stage-output\"]\npipe to \"sh\" with arguments [\"-c\", \"cat >/dev/null; exit 0\"]\n",
        )
        .unwrap(),
        &Registry::default(),
        &options(directory.path()),
    )
    .unwrap();

    assert_eq!(result.output, b"");
    assert_eq!(result.executions.len(), 1);
    assert_eq!(result.dropped_executions, 0);
    assert_eq!(result.dropped_stages, 0);
    let execution = &result.executions[0];
    assert_eq!(
        execution.kind,
        runtime::execution_result::ExecutionKind::Success
    );
    assert_eq!(execution.status, Some(0));
    assert!(execution.stdout_captured);
    assert_eq!(execution.stdout_bytes, 0);
    assert_eq!(execution.stages.len(), 2);

    for (index, stage) in execution.stages.iter().enumerate() {
        assert_eq!(stage.index, index);
        assert_eq!(stage.state, runtime::execution_result::StageState::Exited);
        assert_eq!(stage.status, Some(0));
        assert_eq!(stage.signal, None);
        assert_eq!(stage.outcome.as_deref(), Some("success"));
    }
}

#[cfg(unix)]
#[test]
fn trace_execution_event_omits_captured_payload_and_reports_metadata() {
    let directory = tempfile::tempdir().unwrap();
    let script_path = directory.path().join("trace.program");
    let secret = "TRACE_SECRET_PAYLOAD";
    std::fs::write(
        &script_path,
        format!("run \"printf\" with arguments [{secret:?}]\n"),
    )
    .unwrap();

    let output = ProcessCommand::new(env!("CARGO_BIN_EXE_execute"))
        .current_dir(directory.path())
        .arg("run")
        .arg(&script_path)
        .arg("--trace")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output
        .stdout
        .windows(secret.len())
        .any(|window| window == secret.as_bytes()));

    let trace = String::from_utf8(output.stderr).unwrap();
    assert!(!trace.contains(secret));
    let event = trace
        .lines()
        .filter_map(|line| serde_json::from_str::<JsonValue>(line).ok())
        .find(|event| event.get("event").and_then(JsonValue::as_str) == Some("execution_result"))
        .expect("trace should contain an execution_result event");
    let execution = &event["execution"];
    assert_eq!(execution["stdout_captured"], true);
    assert_eq!(execution["stdout_bytes"], secret.len());
    assert!(execution.get("stdout").is_none());
    assert!(execution.get("stderr").is_none());
    assert!(execution.get("value").is_none());
    assert_eq!(execution["stages"].as_array().unwrap().len(), 1);
}

#[test]
fn remote_transport_failure_through_runtime_keeps_unknown_status_and_certainty() {
    let directory = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream);
        let mut request_line = String::new();
        reader.read_line(&mut request_line).unwrap();
        let mut content_length = 0;
        loop {
            let mut header = String::new();
            reader.read_line(&mut header).unwrap();
            if header == "\r\n" || header.is_empty() {
                break;
            }
            if let Some((name, value)) = header.trim_end().split_once(':') {
                if name.eq_ignore_ascii_case("content-length") {
                    content_length = value.trim().parse::<usize>().unwrap();
                }
            }
        }
        let mut request_body = vec![0; content_length];
        reader.read_exact(&mut request_body).unwrap();
        assert!(request_line.starts_with("POST /invoke "));
        // Closing after the complete request simulates a lost response after
        // transmission, where the remote effect cannot be safely retried.
    });

    let descriptor_path = directory.path().join("remote-descriptor.json");
    let output_schema = OutputSchema {
        kind: OutputType::Json,
        encoding: OutputEncoding::Json,
        items: None,
        fields: BTreeMap::new(),
        required: Vec::new(),
        trim_final_lf: false,
    };
    let outcomes = vec![OutcomeSpec {
        status: 0,
        name: "success".into(),
        kind: OutcomeKind::Success,
        effect_certainty: Some(EffectCertainty::KnownResponse),
    }];
    let ports = AppletPorts {
        inputs: BTreeMap::new(),
        outputs: BTreeMap::from([(
            "result".into(),
            AppletPort {
                schema: ValueSchema {
                    kind: OutputType::Json,
                    nullable: false,
                    items: None,
                    fields: BTreeMap::new(),
                    required: Vec::new(),
                },
                delivery: PortDelivery::Single,
                sensitivity: Default::default(),
                pointer: None,
                encoding: None,
            },
        )]),
        events: BTreeMap::new(),
    };
    let mut remote = RemoteEndpointConfig::new(endpoint);
    remote.allow_loopback_http_fixture = true;
    remote.descriptor_cache = Some("remote-descriptor.json".into());
    remote.timeout_ms = 500;
    let contract = AppletContract {
        version: 1,
        backend: AppletBackend::Remote,
        lifecycle: Lifecycle::OneShot,
        program: None,
        remote: Some(remote.clone()),
        environment: None,
        output: Some(output_schema),
        outcomes: outcomes.clone(),
        capabilities: Vec::new(),
        effects: Vec::new(),
        result_envelope: false,
        ports: Some(ports),
    };
    let mut advertised_contract = contract.clone();
    advertised_contract.remote = None;
    let descriptor = RemoteAppletDescriptor {
        protocol_version: PROTOCOL_VERSION,
        identity: RemoteAppletIdentity {
            name: "remote.fixture".into(),
            version: "1.0.0".into(),
        },
        contract: advertised_contract,
        events: Vec::new(),
        idempotency: None,
    };
    let descriptor_bytes = serde_json::to_vec(&descriptor).unwrap();
    std::fs::write(&descriptor_path, &descriptor_bytes).unwrap();
    remote.descriptor_sha256 = Some(format!("{:x}", Sha256::digest(&descriptor_bytes)));
    let mut contract = contract;
    contract.remote = Some(remote);
    let catalog = registry(json!({
        "name": "remote.fixture",
        "binary": "@remote-applet",
        "arguments": [],
        "contract": contract,
    }));

    let error = runtime::run(
        &parser::parse("run remote.fixture\n").unwrap(),
        &catalog,
        &options(directory.path()),
    )
    .err()
    .expect("dropped remote response should fail");
    server.join().unwrap();

    let result = record(runtime::execution_result::error_value(&error));
    assert_eq!(
        result.get("kind"),
        Some(&Value::String("transport_error".into()))
    );
    assert_eq!(result.get("status"), Some(&Value::Null));
    assert_eq!(
        result.get("effect_certainty"),
        Some(&Value::String("uncertain".into()))
    );
    let Some(Value::List(stages)) = result.get("stages") else {
        panic!("expected remote stage record");
    };
    let stage = record(stages[0].clone());
    assert_eq!(stage.get("state"), Some(&Value::String("unknown".into())));
    assert_eq!(stage.get("status"), Some(&Value::Null));
}
