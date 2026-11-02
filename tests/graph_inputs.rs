#![cfg(feature = "graph")]
use flow::ast::Value;
#[cfg(unix)]
use flow::graph::PortValue;
use flow::graph::{self, GraphPlan};
use flow::manifest::Registry;
#[cfg(unix)]
use flow::runtime::RunOptions;
use std::collections::BTreeMap;
#[cfg(unix)]
use std::path::Path;
#[cfg(unix)]
use std::sync::atomic::AtomicBool;
#[cfg(unix)]
use std::sync::Arc;
#[cfg(unix)]
use std::time::Duration;

fn echo_registry() -> Registry {
    Registry::from_sources(&[r#"{"name":"demo.echo","binary":"/usr/bin/printf","parameters":{"message":{"type":"string","required":true}},"arguments":[{"literal":"%s"},{"parameter":"message"}],"contract":{"version":1,"output":{"type":"bytes","encoding":"raw"}}}"#]).unwrap()
}

#[cfg(unix)]
fn options(cwd: &Path, inputs: BTreeMap<String, Value>) -> RunOptions {
    RunOptions {
        inputs,
        max_output_bytes: 1024 * 1024,
        command_timeout: Some(Duration::from_secs(5)),
        ..RunOptions::new(cwd.to_path_buf())
    }
}

const INPUT_GRAPH: &str = "accept message as text\ngraph input_demo\nrun demo.echo with message message as echoed\nend graph\n";

#[test]
fn graph_inputs_keep_the_legacy_entry_point_and_are_omitted_for_old_plans() {
    let registry = echo_registry();
    assert!(graph::is_graph(
        "# header\naccept message as text\ngraph input_demo\nend graph\n"
    ));
    assert!(!graph::is_graph(
        "accept message as text\nrun demo.echo with message message\n"
    ));

    let plan = graph::parse(
        "# interface\naccept message as text from argument 1\ngraph input_demo\nrun demo.echo with message message as echoed\nend graph\n",
        &registry,
    )
    .unwrap();
    assert_eq!(plan.input_declarations().len(), 1);
    assert_eq!(plan.input_declarations()[0].name, "message");
    assert_eq!(plan.input_declarations()[0].line, 2);
    let json: serde_json::Value = serde_json::from_str(&plan.to_json().unwrap()).unwrap();
    assert_eq!(json["inputs"][0]["source"]["Argument"], 1);

    let legacy = GraphPlan::new("legacy");
    let legacy_json: serde_json::Value = serde_json::from_str(&legacy.to_json().unwrap()).unwrap();
    assert!(legacy_json.get("inputs").is_none());
    let decoded: GraphPlan = serde_json::from_value(serde_json::json!({
        "name": "legacy",
        "nodes": [],
        "edges": []
    }))
    .unwrap();
    assert!(decoded.input_declarations().is_empty());
}

#[test]
fn graph_parameter_references_are_type_checked_and_support_partial_inspection() {
    let registry = echo_registry();
    let plan = graph::parse(INPUT_GRAPH, &registry).unwrap();
    let partial = plan.with_input_values(&BTreeMap::new(), false).unwrap();
    partial.validate_with_registry(&registry).unwrap();
    assert!(plan
        .with_input_values(&BTreeMap::new(), true)
        .unwrap_err()
        .to_string()
        .contains("missing required input"));

    let known = BTreeMap::from([("message".into(), Value::String("a b $HOME".into()))]);
    let resolved = plan.with_input_values(&known, false).unwrap();
    assert_eq!(
        resolved.nodes()[0].invocation().unwrap().parameters["message"],
        Value::String("a b $HOME".into())
    );
    assert!(plan
        .with_input_values(
            &BTreeMap::from([("extra".into(), Value::String("x".into()))]),
            false
        )
        .unwrap_err()
        .to_string()
        .contains("unknown program input"));
    assert!(plan
        .with_input_values(
            &BTreeMap::from([("message".into(), Value::Integer(3))]),
            false
        )
        .unwrap_err()
        .to_string()
        .contains("expects text"));

    let unused_input = graph::parse(
        "accept message as text\naccept unused as integer\ngraph unused\nrun demo.echo with message message as echoed\nend graph\n",
        &registry,
    )
    .unwrap();
    let invalid_unused_values = BTreeMap::from([
        ("message".into(), Value::String("valid".into())),
        ("unused".into(), Value::String("invalid".into())),
    ]);
    for require_complete in [false, true] {
        assert!(unused_input
            .with_input_values(&invalid_unused_values, require_complete)
            .unwrap_err()
            .to_string()
            .contains("expects integer"));
    }

    let wrong_type = "accept message as integer\ngraph typed\nrun demo.echo with message message as echoed\nend graph\n";
    let error = graph::parse(wrong_type, &registry).unwrap_err().to_string();
    assert!(error.contains("expects string"), "{error}");

    let undefined = "graph undefined\nrun demo.echo with message missing as echoed\nend graph\n";
    let error = graph::parse(undefined, &registry).unwrap_err().to_string();
    assert!(error.contains("undeclared graph input"), "{error}");
}

#[cfg(unix)]
#[test]
fn typed_nested_references_and_deferred_json_lists_validate_before_effects() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().unwrap();
    let script = directory.path().join("effect.sh");
    let marker = directory.path().join("effect.marker");
    let quoted_marker = marker.display().to_string().replace('\'', "'\\''");
    std::fs::write(
        &script,
        format!("#!/bin/sh\nprintf x >> '{quoted_marker}'\nexec /usr/bin/printf \"$@\"\n"),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

    let record_manifest = serde_json::json!({
        "name": "demo.record",
        "binary": script,
        "parameters": {
            "config": {
                "type": "record",
                "required": true,
                "fields": {
                    "label": {"type": "string", "required": true},
                    "settings": {
                        "type": "record",
                        "required": true,
                        "fields": {"retry": {"type": "integer", "required": true}}
                    }
                }
            }
        },
        "arguments": [{"literal": "%s"}, {"json_parameter": "config"}],
        "contract": {
            "version": 1,
            "output": {"type": "bytes", "encoding": "raw"},
            "ports": {
                "inputs": {
                    "config": {
                        "schema": {
                            "type": "record",
                            "fields": {
                                "label": {"type": "text", "enum": ["Ada"]},
                                "settings": {
                                    "type": "record",
                                    "fields": {"retry": {"type": "integer"}},
                                    "required": ["retry"]
                                }
                            },
                            "required": ["label", "settings"]
                        },
                        "delivery": "single"
                    }
                },
                "outputs": {
                    "stdout": {
                        "schema": {"type": "bytes"},
                        "delivery": "single"
                    }
                }
            }
        }
    })
    .to_string();
    let list_manifest = serde_json::json!({
        "name": "demo.integers",
        "binary": script,
        "parameters": {
            "values": {"type": "list", "items": "integer", "required": true}
        },
        "arguments": [{"literal": "%d,%d"}, {"spread": "values"}],
        "contract": {"version": 1, "output": {"type": "bytes", "encoding": "raw"}}
    })
    .to_string();
    let registry = Registry::from_sources(&[&record_manifest, &list_manifest]).unwrap();

    let nested_source = "accept label as text\naccept retry as integer\ngraph typed\nrun demo.record with config {\"label\": label, \"settings\": {\"retry\": retry}} as record\nrun demo.integers with values [retry, 7] as integers\nend graph\n";
    let plan = graph::parse(nested_source, &registry).unwrap();
    let partial = plan.with_input_values(&BTreeMap::new(), false).unwrap();
    partial.validate_with_registry(&registry).unwrap();
    let description = flow::inputs::describe(plan.input_declarations(), &BTreeMap::new()).unwrap();
    assert_eq!(
        description["missing_inputs"],
        serde_json::json!(["label", "retry"])
    );
    assert_eq!(
        partial.nodes()[0].invocation().unwrap().parameters["config"],
        Value::Record(BTreeMap::from([
            ("label".into(), Value::Reference("label".into())),
            (
                "settings".into(),
                Value::Record(BTreeMap::from([(
                    "retry".into(),
                    Value::Reference("retry".into()),
                )])),
            ),
        ]))
    );

    let bad_record_type = nested_source.replace("accept retry as integer", "accept retry as text");
    let error = graph::parse(&bad_record_type, &registry)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("record field \"config.settings.retry\" expects integer"),
        "{error}"
    );

    let bad_list_type = nested_source.replace("values [retry, 7]", "values [label, 7]");
    let error = graph::parse(&bad_list_type, &registry)
        .unwrap_err()
        .to_string();
    assert!(error.contains("requires integer list items"), "{error}");

    let nested_list = "accept words as list of text\ngraph nested_list\nrun demo.integers with values [words] as integers\nend graph\n";
    let error = graph::parse(nested_list, &registry)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("contains list of text"),
        "list-valued references cannot be nested as scalar items: {error}"
    );
    let json_scalar_item = "accept payload as json\ngraph json_item\nrun demo.integers with values [payload] as integers\nend graph\n";
    let error = graph::parse(json_scalar_item, &registry)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("requires integer list items") && error.contains("contains json"),
        "aggregate JSON references must not stand in for scalar list items: {error}"
    );

    let json_source = "accept values as json\ngraph deferred\nrun demo.integers with values values as integers\nend graph\n";
    let json_plan = graph::parse(json_source, &registry).unwrap();
    json_plan
        .with_input_values(&BTreeMap::new(), false)
        .unwrap()
        .validate_with_registry(&registry)
        .unwrap();

    for (invalid, expected) in [
        (Value::String("not a list".into()), "expects list"),
        (
            Value::List(vec![Value::Integer(1), Value::String("wrong item".into())]),
            "not integer",
        ),
    ] {
        let error = graph::run(
            json_plan.clone(),
            &registry,
            &options(
                directory.path(),
                BTreeMap::from([("values".into(), invalid)]),
            ),
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains(expected), "{error}");
        assert!(!marker.exists(), "an invalid JSON list started its applet");
    }

    let result = graph::run(
        json_plan,
        &registry,
        &options(
            directory.path(),
            BTreeMap::from([(
                "values".into(),
                Value::List(vec![Value::Integer(3), Value::Integer(8)]),
            )]),
        ),
        Arc::new(AtomicBool::new(false)),
    )
    .unwrap();
    let output = result
        .outputs
        .iter()
        .find(|event| event.port.node == "integers" && event.port.port == "stdout")
        .unwrap();
    assert_eq!(output.payload, PortValue::Bytes(b"3,8".to_vec()));
    assert_eq!(std::fs::read(&marker).unwrap(), b"x");

    let error = graph::run(
        plan.clone(),
        &registry,
        &options(
            directory.path(),
            BTreeMap::from([
                ("label".into(), Value::String("Grace".into())),
                ("retry".into(), Value::Integer(4)),
            ]),
        ),
        Arc::new(AtomicBool::new(false)),
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("enum"), "{error}");
    assert_eq!(
        std::fs::read(&marker).unwrap(),
        b"x",
        "a nested input-port constraint must fail before any later node starts"
    );

    let result = graph::run(
        plan,
        &registry,
        &options(
            directory.path(),
            BTreeMap::from([
                ("label".into(), Value::String("Ada".into())),
                ("retry".into(), Value::Integer(4)),
            ]),
        ),
        Arc::new(AtomicBool::new(false)),
    )
    .unwrap();
    let record = result
        .outputs
        .iter()
        .find(|event| event.port.node == "record" && event.port.port == "stdout")
        .unwrap();
    assert_eq!(
        record.payload,
        PortValue::Bytes(br#"{"label":"Ada","settings":{"retry":4}}"#.to_vec())
    );
    let integers = result
        .outputs
        .iter()
        .find(|event| event.port.node == "integers" && event.port.port == "stdout")
        .unwrap();
    assert_eq!(integers.payload, PortValue::Bytes(b"4,7".to_vec()));
    assert_eq!(std::fs::read(&marker).unwrap(), b"xxx");

    std::fs::remove_file(&marker).unwrap();
    let invalid_default_source =
        "accept values as json with default [1, \"wrong item\"]\ngraph bad_default\nrun demo.integers with values values as integers\nend graph\n";
    let invalid_default_plan = graph::parse(invalid_default_source, &registry).unwrap();
    let error = graph::run(
        invalid_default_plan,
        &registry,
        &options(directory.path(), BTreeMap::new()),
        Arc::new(AtomicBool::new(false)),
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("not integer"), "{error}");
    assert!(
        !marker.exists(),
        "an invalid default JSON list started its applet"
    );

    let default_source = "accept values as json with default [1, 2]\ngraph defaulted\nrun demo.integers with values values as integers\nend graph\n";
    let default_plan = graph::parse(default_source, &registry).unwrap();
    let result = graph::run(
        default_plan,
        &registry,
        &options(directory.path(), BTreeMap::new()),
        Arc::new(AtomicBool::new(false)),
    )
    .unwrap();
    let output = result
        .outputs
        .iter()
        .find(|event| event.port.node == "integers" && event.port.port == "stdout")
        .unwrap();
    assert_eq!(output.payload, PortValue::Bytes(b"1,2".to_vec()));
    assert_eq!(std::fs::read(&marker).unwrap(), b"x");
}

#[test]
fn graph_input_resolution_keeps_environment_precedence_over_defaults() {
    let key = format!("EXECUTE_GRAPH_INPUT_API_{}", std::process::id());
    std::env::set_var(&key, "from environment");
    let source = format!(
        "accept message as text from environment \"{key}\" with default \"fallback\"\n\
         graph configured\n\
           run demo.echo with message message as echoed\n\
         end graph\n"
    );
    let registry = echo_registry();
    let plan = graph::parse(&source, &registry).unwrap();
    assert_eq!(
        plan.nodes()[0].invocation().unwrap().parameters["message"],
        Value::Reference("message".into()),
        "parsing must preserve the source reference until runtime resolution"
    );

    let resolved = plan.with_input_values(&BTreeMap::new(), true).unwrap();
    assert_eq!(
        resolved.nodes()[0].invocation().unwrap().parameters["message"],
        Value::String("from environment".into())
    );
    let supplied = BTreeMap::from([("message".into(), Value::String("named".into()))]);
    let overridden = plan.with_input_values(&supplied, true).unwrap();
    assert_eq!(
        overridden.nodes()[0].invocation().unwrap().parameters["message"],
        Value::String("named".into())
    );
    std::env::remove_var(key);
}

#[cfg(unix)]
#[test]
fn graph_run_preserves_input_bytes_as_a_single_literal_argv_value() {
    let directory = tempfile::tempdir().unwrap();
    let registry = echo_registry();
    let plan = graph::parse(INPUT_GRAPH, &registry).unwrap();
    let literal = "two words, $HOME, 'quotes', and café";
    let inputs = BTreeMap::from([("message".into(), Value::String(literal.into()))]);
    let result = graph::run(
        plan,
        &registry,
        &options(directory.path(), inputs),
        Arc::new(AtomicBool::new(false)),
    )
    .unwrap();
    let output = result
        .outputs
        .iter()
        .find(|event| event.port.node == "echoed" && event.port.port == "stdout")
        .expect("stdout is a terminal graph port");
    assert_eq!(
        output.payload,
        PortValue::Bytes(literal.as_bytes().to_vec())
    );
    assert_eq!(result.launched_processes, 1);
}

#[cfg(unix)]
#[test]
fn graph_execution_apis_authorize_environment_inputs_before_resolution() {
    use flow::capabilities::{AppletGrant, CapabilityPolicy, FilesystemGrant};
    use flow::graph::MemoryGraphStore;
    use flow::security::{policy_decisions, PolicyDecisionOutcome, PolicyDecisionReason};
    use std::collections::BTreeSet;

    #[derive(Clone, Copy)]
    enum Api {
        Finite,
        Streaming,
        Checkpointed,
    }

    fn execute(
        api: Api,
        plan: GraphPlan,
        registry: &Registry,
        run_options: &RunOptions,
    ) -> anyhow::Result<Vec<flow::graph::EventEnvelope>> {
        let cancelled = Arc::new(AtomicBool::new(false));
        match api {
            Api::Finite => Ok(graph::run(plan, registry, run_options, cancelled)?.outputs),
            Api::Streaming => {
                let (_, mut session) =
                    graph::start_session(plan, registry, run_options, cancelled)?;
                let mut events = Vec::new();
                while let Some(event) = session.recv_output()? {
                    events.push(event);
                }
                Ok(events)
            }
            Api::Checkpointed => Ok(graph::run_checkpointed(
                plan,
                registry,
                run_options,
                cancelled,
                Arc::new(MemoryGraphStore::default()),
                false,
            )?
            .outputs),
        }
    }

    fn registry(environment: Option<&str>) -> Registry {
        let registry = echo_registry();
        registry.set_host_state(Some(CapabilityPolicy {
            version: 1,
            applets: BTreeMap::from([(
                "demo.echo".into(),
                AppletGrant {
                    capabilities: BTreeSet::from(["process.exec".into()]),
                    binary_sha256: None,
                    environment: BTreeSet::new(),
                    filesystem: FilesystemGrant::default(),
                    network: false,
                    network_origins: BTreeSet::new(),
                    sandbox_required: false,
                },
            )]),
            host: None,
            host_environment: environment.into_iter().map(str::to_owned).collect(),
            deny_privileged: false,
            allow_wrappers: BTreeSet::new(),
        }));
        registry
    }

    fn plan(registry: &Registry, environment: &str, public: bool) -> GraphPlan {
        graph::parse(
            &format!(
                "accept message as text from {}environment \"{environment}\"\n\
                 graph env_api\n\
                 run demo.echo with message message as echoed\n\
                 end graph\n",
                if public { "public " } else { "" }
            ),
            registry,
        )
        .unwrap()
    }

    fn outputs_text(events: &[flow::graph::EventEnvelope]) -> String {
        let event = events
            .iter()
            .find(|event| event.port.node == "echoed" && event.port.port == "stdout")
            .expect("graph should emit the echo result");
        let flow::graph::PortValue::Bytes(bytes) = &event.payload else {
            panic!("printf output should be bytes");
        };
        String::from_utf8(bytes.clone()).unwrap()
    }

    let key = format!("EXECUTE_GRAPH_API_AUTH_{}", std::process::id());
    let secret = format!("graph-env-canary-{}", std::process::id());
    std::env::set_var(&key, &secret);
    let directory = tempfile::tempdir().unwrap();

    // A denied declaration is rejected at the same point for every public
    // execution API, before resolving even an explicitly supplied override.
    for api in [Api::Finite, Api::Streaming, Api::Checkpointed] {
        let registry = registry(None);
        let denied = execute(
            api,
            plan(&registry, &key, false),
            &registry,
            &options(directory.path(), BTreeMap::new()),
        )
        .unwrap_err();
        let message = format!("{denied:#}");
        assert!(
            message.contains("denies reading environment variable"),
            "{message}"
        );
        assert!(message.contains(&key), "{message}");
        assert!(
            !message.contains(&secret),
            "environment value leaked: {message}"
        );
        let decision = policy_decisions(&registry)
            .pop()
            .expect("denial is audited");
        assert_eq!(decision.action, "environment.read");
        assert_eq!(decision.outcome, PolicyDecisionOutcome::Denied);
        assert_eq!(decision.reason, PolicyDecisionReason::CapabilityNotGranted);
        assert!(!format!("{decision:?}").contains(&secret));

        let supplied = BTreeMap::from([("message".into(), Value::String("explicit".into()))]);
        let overridden = execute(
            api,
            plan(&registry, &key, false),
            &registry,
            &options(directory.path(), supplied),
        )
        .unwrap_err();
        assert!(
            format!("{overridden:#}").contains("denies reading environment variable"),
            "explicit values do not override the documented environment grant"
        );
    }

    // A matching host grant allows each execution style to resolve and use
    // the declared value. Checkpointed graphs use a public declaration because
    // the checkpoint contract separately refuses sensitive inputs.
    for api in [Api::Finite, Api::Streaming, Api::Checkpointed] {
        let registry = registry(Some(&key));
        let events = execute(
            api,
            plan(&registry, &key, true),
            &registry,
            &options(directory.path(), BTreeMap::new()),
        )
        .unwrap();
        assert_eq!(outputs_text(&events), secret);
        let supplied = BTreeMap::from([("message".into(), Value::String("explicit".into()))]);
        let events = execute(
            api,
            plan(&registry, &key, true),
            &registry,
            &options(directory.path(), supplied),
        )
        .unwrap();
        assert_eq!(outputs_text(&events), "explicit");
        let decision = policy_decisions(&registry)
            .into_iter()
            .find(|decision| decision.action == "environment.read")
            .expect("grant is audited");
        assert_eq!(decision.action, "environment.read");
        assert_eq!(decision.outcome, PolicyDecisionOutcome::Approved);
        assert_eq!(decision.reason, PolicyDecisionReason::PolicyMatched);
    }

    // No-policy resolution remains compatible and is explicitly audited.
    let registry = echo_registry();
    let events = execute(
        Api::Finite,
        plan(&registry, &key, true),
        &registry,
        &options(directory.path(), BTreeMap::new()),
    )
    .unwrap();
    assert_eq!(outputs_text(&events), secret);
    let decision = policy_decisions(&registry)
        .into_iter()
        .find(|decision| decision.action == "environment.read")
        .expect("baseline is audited");
    assert_eq!(decision.outcome, PolicyDecisionOutcome::Approved);
    assert_eq!(decision.reason, PolicyDecisionReason::NoPolicyConfigured);
    std::env::remove_var(key);
}

#[cfg(unix)]
#[test]
fn invalid_resolved_graph_input_fails_before_starting_any_applet() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().unwrap();
    let script = directory.path().join("effect.sh");
    let marker = directory.path().join("effect.marker");
    let quoted_marker = marker.display().to_string().replace('\'', "'\\''");
    std::fs::write(
        &script,
        format!("#!/bin/sh\nprintf x >> '{quoted_marker}'\n"),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let manifest = serde_json::json!({
        "name": "demo.effect",
        "binary": script,
        "parameters": {"value": {"type": "string", "required": true}},
        "arguments": [{"parameter": "value"}],
        "contract": {
            "version": 1,
            "output": {"type": "bytes", "encoding": "raw"},
            "ports": {
                "inputs": {
                    "value": {
                        "schema": {"type": "text", "enum": ["allowed"]},
                        "delivery": "single"
                    }
                }
            }
        }
    })
    .to_string();
    let registry = Registry::from_sources(&[&manifest]).unwrap();
    let plan = graph::parse(
        "accept value as text\ngraph guarded\nrun demo.effect with value value as effect\nend graph\n",
        &registry,
    )
    .unwrap();
    let supplied = BTreeMap::from([("value".into(), Value::String("denied".into()))]);
    let error = graph::run(
        plan,
        &registry,
        &options(directory.path(), supplied),
        Arc::new(AtomicBool::new(false)),
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("enum"), "{error}");
    assert!(
        !marker.exists(),
        "the applet started before graph input validation"
    );
}

#[test]
fn graph_inputs_are_rejected_in_static_declarations() {
    let registry = echo_registry();
    let timer =
        "accept interval as integer\ngraph timer\nevery interval seconds as ticker\nend graph\n";
    let error = graph::parse(timer, &registry).unwrap_err().to_string();
    assert!(error.contains("timer intervals must be literal"), "{error}");

    let renderer = "accept title as text\ngraph renderer\nrender tui:text with properties {\"text\": title} as label\nend graph\n";
    let error = graph::parse(renderer, &registry).unwrap_err().to_string();
    assert!(
        error.contains("renderer properties must be literal"),
        "{error}"
    );
}
