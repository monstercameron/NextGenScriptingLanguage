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
#[cfg(unix)]
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
fn source_result_binding_exposes_raw_output_and_all_pipeline_stages() {
    let directory = tempfile::tempdir().unwrap();
    let source = "run \"sh\" with arguments [\"-c\", \"printf first\"]\npipe to \"sh\" with arguments [\"-c\", \"cat >/dev/null; printf terminal\"] and keep result as execution\nthen encode execution as json\n";
    let result = runtime::run(
        &parser::parse(source).unwrap(),
        &Registry::default(),
        &options(directory.path()),
    )
    .unwrap();
    let record: JsonValue = serde_json::from_slice(&result.output).unwrap();

    assert_eq!(record["kind"], "success");
    assert_eq!(record["status"], 0);
    assert_eq!(record["effect_certainty"], "known_response");
    assert_eq!(record["stdout_captured"], true);
    assert_eq!(record["stdout"], json!(b"terminal"));
    assert_eq!(record["value_captured"], false);
    assert_eq!(record["stages"].as_array().unwrap().len(), 2);
    for stage in record["stages"].as_array().unwrap() {
        assert_eq!(stage["state"], "exited");
        assert_eq!(stage["status"], 0);
        assert_eq!(stage["effect_certainty"], "known_response");
        assert!(stage.get("stderr_captured").is_some());
        assert!(stage.get("stderr").is_none());
    }
}

#[cfg(unix)]
#[test]
fn source_result_binding_decodes_typed_output_without_an_output_binding() {
    let directory = tempfile::tempdir().unwrap();
    let catalog = registry(json!({
        "name": "typed.fixture",
        "binary": "printf",
        "arguments": [{"literal": "{\"count\":5}"}],
        "contract": {
            "version": 1,
            "backend": "command",
            "output": {
                "type": "record",
                "encoding": "json",
                "fields": {"count": {"type": "integer"}},
                "required": ["count"]
            }
        }
    }));
    let result = runtime::run(
        &parser::parse(
            "run typed.fixture and keep result as execution\nthen encode execution as json\n",
        )
        .unwrap(),
        &catalog,
        &options(directory.path()),
    )
    .unwrap();
    let record: JsonValue = serde_json::from_slice(&result.output).unwrap();
    assert_eq!(record["value_captured"], true);
    assert_eq!(record["value"]["count"], 5);
}

#[cfg(unix)]
#[test]
fn source_result_binding_keeps_the_value_alongside_a_named_output() {
    let directory = tempfile::tempdir().unwrap();
    let catalog = registry(json!({
        "name": "typed.fixture",
        "binary": "printf",
        "arguments": [{"literal": "{\"count\":5}"}],
        "contract": {
            "version": 1,
            "backend": "command",
            "output": {
                "type": "record",
                "encoding": "json",
                "fields": {"count": {"type": "integer"}},
                "required": ["count"]
            }
        }
    }));
    let result = runtime::run(
        &parser::parse(
            "run typed.fixture as decoded and keep result as execution\ntake integer at \"/count\" from decoded as count\nthen encode execution as json\n",
        )
        .unwrap(),
        &catalog,
        &options(directory.path()),
    )
    .unwrap();
    let record: JsonValue = serde_json::from_slice(&result.output).unwrap();

    assert_eq!(record["value_captured"], true);
    assert_eq!(record["value"]["count"], 5);
}

#[cfg(unix)]
#[test]
fn source_result_binding_preserves_missing_stdout_capture_and_does_not_swallow_failure() {
    let directory = tempfile::tempdir().unwrap();
    let catalog = registry(json!({
        "name": "discard.fixture",
        "binary": "printf",
        "arguments": [{"literal": "discarded"}],
        "stdio": {"stdout": "discard", "stderr": "capture"}
    }));
    let source =
        "run discard.fixture and keep result as execution\nthen encode execution as json\n";
    let result = runtime::run(
        &parser::parse(source).unwrap(),
        &catalog,
        &options(directory.path()),
    )
    .unwrap();
    let execution: JsonValue = serde_json::from_slice(&result.output).unwrap();
    assert_eq!(execution["stdout_captured"], false);
    assert_eq!(execution["stdout_bytes"], 0);
    assert_eq!(execution["stdout"], json!([]));
    assert_eq!(execution["stderr_captured"], true);
    assert_eq!(execution["stderr"], json!([]));

    let failure = runtime::run(
        &parser::parse(
            "run \"sh\" with arguments [\"-c\", \"printf partial; exit 7\"] and keep result as execution\n",
        )
        .unwrap(),
        &Registry::default(),
        &options(directory.path()),
    )
    .err()
    .expect("result capture must leave ordinary failures fail-fast");
    let caught = record(runtime::execution_result::error_value(&failure));
    assert_eq!(caught.get("status"), Some(&Value::Integer(7)));
    assert_eq!(caught.get("kind"), Some(&Value::String("failure".into())));
}

#[cfg(unix)]
#[test]
fn explicit_status_and_result_capture_retains_a_non_success_without_faking_success() {
    let directory = tempfile::tempdir().unwrap();
    let result = runtime::run(
        &parser::parse(
            "run \"false\" and keep status as code and keep result as execution\nif code is 1\n  then encode execution as json\notherwise\n  run \"false\"\n",
        )
        .unwrap(),
        &Registry::default(),
        &options(directory.path()),
    )
    .unwrap();
    let execution: JsonValue = serde_json::from_slice(&result.output).unwrap();

    assert_eq!(execution["kind"], "failure");
    assert_eq!(execution["outcome"], "exit_1");
    assert_eq!(execution["status"], 1);
    assert_eq!(execution["effect_certainty"], "uncertain");
    assert_eq!(execution["value_captured"], false);
    assert_eq!(execution["stages"].as_array().unwrap().len(), 1);
    assert_eq!(execution["stages"][0]["state"], "exited");
    assert_eq!(execution["stages"][0]["status"], 1);
    assert_eq!(execution["stages"][0]["kind"], "failure");
}

#[cfg(unix)]
#[test]
fn captured_absence_decodes_only_the_contract_declared_result_envelope() {
    let directory = tempfile::tempdir().unwrap();
    let catalog = registry(json!({
        "name": "absence.fixture",
        "binary": "sh",
        "arguments": [{"literal": "-c"}, {"literal": "exit 1"}],
        "contract": {
            "version": 1,
            "backend": "command",
            "output": {
                "type": "record",
                "encoding": "json",
                "fields": {"count": {"type": "integer"}},
                "required": ["count"]
            },
            "result_envelope": true,
            "outcomes": [
                {"status": 0, "name": "success", "kind": "success"},
                {"status": 1, "name": "absence", "kind": "absence"}
            ]
        }
    }));
    let result = runtime::run(
        &parser::parse(
            "run absence.fixture and keep status as code and keep result as execution\nif code is 1\n  then encode execution as json\notherwise\n  run \"false\"\n",
        )
        .unwrap(),
        &catalog,
        &options(directory.path()),
    )
    .unwrap();
    let execution: JsonValue = serde_json::from_slice(&result.output).unwrap();

    assert_eq!(execution["kind"], "absence");
    assert_eq!(execution["status"], 1);
    assert_eq!(execution["value_captured"], true);
    assert_eq!(execution["value"]["kind"], "absence");
    assert_eq!(execution["value"]["value"], JsonValue::Null);
    assert_eq!(execution["stages"][0]["status"], 1);
}

#[test]
fn result_suffix_round_trips_and_absent_field_keeps_legacy_ast_bytes() {
    let program = parser::parse("run printf\n").unwrap();
    let flow::ast::StepKind::Invoke { call, .. } = &program.steps[0].kind else {
        panic!("expected invocation");
    };
    assert_eq!(
        serde_json::to_vec(call).unwrap(),
        br#"{"operation":"printf","parameters":{},"binding":null,"cache":false}"#
    );

    let program =
        parser::parse("run \"printf\" with arguments [\"payload\"] and keep result as execution\n")
            .unwrap();
    let flow::ast::StepKind::Invoke { call, .. } = &program.steps[0].kind else {
        panic!("expected invocation");
    };
    assert_eq!(call.result_binding.as_deref(), Some("execution"));
    let encoded = serde_json::to_vec(call).unwrap();
    let decoded: flow::ast::Invocation = serde_json::from_slice(&encoded).unwrap();
    assert_eq!(decoded, *call);
}

#[test]
fn result_binding_rejects_cache_intermediate_stage_and_name_conflicts() {
    let directory = tempfile::tempdir().unwrap();
    let registry = Registry::default();
    for source in [
        "run \"printf\" with arguments [\"x\"] and keep result as execution and cache\n",
        "run \"printf\" with arguments [\"x\"] and keep result as execution\npipe to \"cat\"\n",
        "run \"printf\" with arguments [\"x\"] as same and keep result as same\n",
    ] {
        let program = parser::parse(source).unwrap();
        assert!(
            runtime::run(&program, &registry, &options(directory.path()),).is_err(),
            "accepted invalid result capture: {source}"
        );
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
        enum_values: None,
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
                    enum_values: None,
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
