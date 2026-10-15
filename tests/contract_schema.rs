use flow::{
    ast::{Invocation, Value},
    contracts::{AppletOutput, OutcomeKind},
    manifest::Registry,
};
use serde_json::{json, Value as JsonValue};
use std::collections::BTreeMap;

fn load(source: JsonValue) -> flow::manifest::Manifest {
    Registry::from_sources(&[source.to_string()])
        .unwrap()
        .resolve("typed")
        .unwrap()
        .clone()
}

fn base(contract: JsonValue) -> JsonValue {
    json!({"name":"typed", "binary":"fixture", "arguments":[], "contract":contract})
}

#[test]
fn old_manifests_keep_their_serialized_identity_and_discovery_projection() {
    let manifest = load(json!({"name":"typed", "binary":"fixture", "arguments":[]}));
    assert_eq!(
        serde_json::to_string(&manifest).unwrap(),
        r#"{"name":"typed","binary":"fixture","builtin":null,"accepts_stdin":false,"cacheable":false,"phrases":[],"parameters":{},"arguments":[]}"#
    );
    let projection = manifest.contract();
    assert!(projection.get("contract_version").is_none());
    assert!(projection["io"]["output_schema"].is_null());
    assert!(projection["execution"].get("contract_backend").is_none());
    assert!(manifest.execution_contract().is_none());
    assert_eq!(manifest.output_kind(), None);
    assert_eq!(
        manifest.decode_output(b"\xff\n", 0).unwrap(),
        AppletOutput::Bytes(b"\xff\n".to_vec())
    );
    assert_eq!(
        manifest.decode_output(b"failed", 1).unwrap(),
        AppletOutput::Bytes(b"failed".to_vec())
    );
}

#[test]
fn raw_text_newline_normalization_is_opt_in_and_removes_exactly_one_lf() {
    let preserve = load(base(json!({
        "version":1,
        "output":{"type":"text","encoding":"raw"}
    })));
    assert_eq!(
        preserve.decode_output(b"one\n\n", 0).unwrap(),
        AppletOutput::Value(Value::String("one\n\n".into()))
    );

    let trim = load(base(json!({
        "version":1,
        "output":{"type":"text","encoding":"raw","trim_final_lf":true}
    })));
    assert_eq!(
        trim.decode_output(b"one\r\n\n", 0).unwrap(),
        AppletOutput::Value(Value::String("one\r\n".into()))
    );
    assert!(trim.decode_output(b"\xff", 0).is_err());
    assert_eq!(trim.output_kind(), Some(flow::ast::InputType::Text));
}

#[test]
fn path_contracts_use_checked_nonempty_nul_free_strings() {
    let manifest = load(base(json!({
        "version":1,
        "output":{"type":"path","encoding":"json"}
    })));
    assert_eq!(
        manifest.decode_output(br#""/tmp/result""#, 0).unwrap(),
        AppletOutput::Value(Value::String("/tmp/result".into()))
    );
    assert!(manifest.decode_output(br#""""#, 0).is_err());
    assert!(manifest.decode_output(br#""/tmp/a\u0000b""#, 0).is_err());
    let projection = manifest.contract();
    assert_eq!(projection["io"]["output_schema"]["format"], "path");
}

#[test]
fn json_outputs_validate_types_records_and_duplicate_keys() {
    let manifest = load(base(json!({
        "version":1,
        "output":{
            "type":"record", "encoding":"json",
            "fields":{"count":{"type":"integer"},"label":{"type":"text"}},
            "required":["count"]
        }
    })));
    assert_eq!(
        manifest
            .decode_output(br#"{"count":4,"label":"ok"}"#, 0)
            .unwrap(),
        AppletOutput::Value(Value::Record(BTreeMap::from([
            ("count".into(), Value::Integer(4)),
            ("label".into(), Value::String("ok".into())),
        ])))
    );
    assert!(manifest.decode_output(br#"{"count":"4"}"#, 0).is_err());
    assert!(manifest
        .decode_output(br#"{"count":4,"extra":true}"#, 0)
        .is_err());
    assert!(manifest
        .decode_output(br#"{"count":1,"count":2}"#, 0)
        .is_err());
    assert_eq!(manifest.output_kind(), Some(flow::ast::InputType::Json));
}

#[test]
fn recursive_list_outputs_use_typed_items_and_reject_nested_bytes() {
    let manifest = load(base(json!({
        "version":1,
        "output":{
            "type":"list", "encoding":"json",
            "items":{"type":"record","fields":{
                "path":{"type":"path"},"ok":{"type":"boolean"}
            },"required":["path"]}
        }
    })));
    assert_eq!(
        manifest
            .decode_output(br#"[{"path":"/tmp/a","ok":true}]"#, 0)
            .unwrap(),
        AppletOutput::Value(Value::List(vec![Value::Record(BTreeMap::from([
            ("path".into(), Value::String("/tmp/a".into())),
            ("ok".into(), Value::Bool(true)),
        ]))]))
    );
    assert!(manifest
        .decode_output(br#"[{"path":"","ok":true}]"#, 0)
        .is_err());
    assert_eq!(manifest.output_kind(), Some(flow::ast::InputType::Json));
    assert_eq!(manifest.contract()["io"]["output_schema"]["type"], "array");

    for invalid in [
        json!({"version":1,"output":{"type":"list","encoding":"json"}}),
        json!({"version":1,"output":{"type":"text","encoding":"raw","items":{"type":"text"}}}),
        json!({"version":1,"output":{"type":"list","encoding":"json","items":{"type":"bytes"}}}),
    ] {
        assert!(Registry::from_sources(&[base(invalid).to_string()]).is_err());
    }
}

#[test]
fn manifest_ports_match_parameters_and_map_typed_output_pointers() {
    let manifest = load(json!({
        "name":"typed",
        "binary":"fixture",
        "accepts_stdin":true,
        "parameters":{
            "file":{"type":"path","required":true},
            "tags":{"type":"list","items":"string","required":true}
        },
        "arguments":[{"parameter":"file"},{"spread":"tags"}],
        "contract":{
            "version":1,
            "output":{"type":"record","encoding":"json","fields":{
                "text":{"type":"text"},"count":{"type":"integer"},
                "labels":{"type":"list","items":{"type":"text"}}
            },"required":["text","count","labels"]},
            "ports":{
                "inputs":{
                    "file":{"schema":{"type":"path"},"delivery":"single"},
                    "tags":{"schema":{"type":"list","items":{"type":"text"}},"delivery":"single"},
                    "stdin":{"schema":{"type":"bytes"},"delivery":"records","encoding":"raw"}
                },
                "outputs":{
                    "text":{"schema":{"type":"text"},"delivery":"single","pointer":"/text"},
                    "count":{"schema":{"type":"integer"},"delivery":"single","pointer":"/count"},
                    "labels":{"schema":{"type":"text"},"delivery":"records","pointer":"/labels"}
                },
                "events":{"progress":{"schema":{"type":"json"},"delivery":"events"}}
            }
        }
    }));
    let ports = manifest.ports().unwrap();
    assert_eq!(ports.inputs.len(), 3);
    assert_eq!(ports.outputs["text"].pointer.as_deref(), Some("/text"));
    assert_eq!(
        ports.outputs["labels"].delivery,
        flow::contracts::PortDelivery::Records
    );
    assert_eq!(
        ports.events["progress"].delivery,
        flow::contracts::PortDelivery::Events
    );
    assert_eq!(
        manifest.contract()["ports"]["inputs"]["stdin"]["schema"]["type"],
        "bytes"
    );

    for invalid in [
        json!({
            "name":"typed","binary":"fixture","parameters":{"file":{"type":"path"}},
            "arguments":[{"parameter":"file"}],
            "contract":{"version":1,"ports":{"inputs":{}}}
        }),
        json!({
            "name":"typed","binary":"fixture","parameters":{"file":{"type":"path"}},
            "arguments":[{"parameter":"file"}],
            "contract":{"version":1,"output":{"type":"record","encoding":"json","fields":{"count":{"type":"integer"}},"required":["count"]},
                "ports":{"inputs":{"file":{"schema":{"type":"text"},"delivery":"single"}},
                    "outputs":{"count":{"schema":{"type":"text"},"delivery":"single","pointer":"/count"}}}}
        }),
        json!({
            "name":"typed","binary":"fixture","accepts_stdin":true,"arguments":[],
            "contract":{"version":1,"ports":{"inputs":{"stdin":{"schema":{"type":"record","fields":{"x":{"type":"integer"}}},"delivery":"records"}}}}
        }),
    ] {
        assert!(Registry::from_sources(&[invalid.to_string()]).is_err());
    }
}

#[test]
fn outcomes_are_named_and_closed_when_declared() {
    let manifest = load(base(json!({
        "version":1,
        "output":{"type":"text","encoding":"raw"},
        "outcomes":[
            {"status":0,"name":"success","kind":"success"},
            {"status":4,"name":"not_found","kind":"absence"},
            {"status":75,"name":"try_again","kind":"retryable"}
        ]
    })));
    assert_eq!(manifest.outcome_for(4).kind, OutcomeKind::Absence);
    assert_eq!(
        manifest.outcome_for(4).effect_certainty,
        flow::contracts::EffectCertainty::KnownResponse
    );
    assert_eq!(manifest.outcome_for(75).kind, OutcomeKind::Retryable);
    assert_eq!(
        manifest.outcome_for(75).effect_certainty,
        flow::contracts::EffectCertainty::Uncertain
    );
    assert_eq!(manifest.outcome_for(9).kind, OutcomeKind::Failure);
    assert_eq!(
        manifest.outcome_for(9).effect_certainty,
        flow::contracts::EffectCertainty::Uncertain
    );
    assert!(manifest.decode_output(b"none", 4).is_err());
    assert_eq!(manifest.success_status(), Some(0));

    let no_success = load(base(json!({
        "version":1,
        "outcomes":[{"status":4,"name":"not_found","kind":"absence"}]
    })));
    assert_eq!(no_success.success_status(), None);

    let envelope = load(base(json!({
        "version":1,
        "output":{"type":"json","encoding":"json"},
        "outcomes":[
            {"status":0,"name":"success","kind":"success"},
            {"status":4,"name":"not_found","kind":"absence"}
        ],
        "result_envelope":true,
        "ports":{"outputs":{"certainty":{
            "schema":{"type":"text"},"delivery":"single","pointer":"/effect_certainty"
        }}}
    })));
    let AppletOutput::Value(Value::Record(success)) =
        envelope.decode_output(br#"{"ok":true}"#, 0).unwrap()
    else {
        panic!("expected result envelope");
    };
    assert_eq!(
        success["value"],
        Value::Record(BTreeMap::from([("ok".into(), Value::Bool(true))]))
    );
    assert_eq!(success["kind"], Value::String("success".into()));
    assert_eq!(
        success["effect_certainty"],
        Value::String("known_response".into())
    );
    let AppletOutput::Value(Value::Record(absence)) = envelope.decode_output(b"", 4).unwrap()
    else {
        panic!("expected absence envelope");
    };
    assert_eq!(absence["value"], Value::Null);
    assert_eq!(absence["outcome"], Value::String("not_found".into()));
    assert_eq!(
        absence["effect_certainty"],
        Value::String("known_response".into())
    );
    assert_eq!(absence["status"], Value::Integer(4));
    assert_eq!(
        envelope.contract()["execution"]["result_envelope_schema"]["properties"]
            ["effect_certainty"]["type"],
        "string"
    );
}

#[test]
fn explicit_outcome_certainty_is_machine_readable_and_bytes_cannot_be_bound() {
    let manifest = load(base(json!({
        "version":1,
        "output":{"type":"bytes","encoding":"raw"},
        "outcomes":[
            {"status":0,"name":"success","kind":"success"},
            {"status":75,"name":"retry_before_effect","kind":"retryable","effect_certainty":"not_started"}
        ]
    })));
    let outcome = manifest.outcome_for(75);
    assert_eq!(outcome.kind, OutcomeKind::Retryable);
    assert_eq!(
        outcome.effect_certainty,
        flow::contracts::EffectCertainty::NotStarted
    );
    assert_eq!(outcome.effect_certainty.as_str(), "not_started");
    assert!(manifest
        .validate(&Invocation {
            operation: "typed".into(),
            direct: false,
            parameters: BTreeMap::new(),
            binding: Some("raw".into()),
            cache: false,
            status_binding: None,
        })
        .is_err());
    assert!(manifest
        .validate(&Invocation {
            operation: "typed".into(),
            direct: false,
            parameters: BTreeMap::new(),
            binding: None,
            cache: false,
            status_binding: None,
        })
        .is_ok());
    let implicit_bytes = load(base(json!({"version":1})));
    assert!(implicit_bytes
        .validate(&Invocation {
            operation: "typed".into(),
            direct: false,
            parameters: BTreeMap::new(),
            binding: Some("raw".into()),
            cache: false,
            status_binding: None,
        })
        .is_err());

    let serialized = serde_json::to_value(manifest.execution_contract().unwrap()).unwrap();
    assert_eq!(serialized["outcomes"][1]["effect_certainty"], "not_started");
    let omitted = load(base(json!({
        "version":1,
        "outcomes":[{"status":0,"name":"success","kind":"success"}]
    })));
    let omitted_serialized = serde_json::to_value(omitted.execution_contract().unwrap()).unwrap();
    assert!(omitted_serialized["outcomes"][0]
        .get("effect_certainty")
        .is_none());
}

#[test]
fn typed_parameters_require_explicit_json_argv_mapping_and_validate_fields() {
    let mut source = json!({
        "name":"typed",
        "binary":"@execute-applet",
        "contract":{"version":1,"backend":"in_process"},
        "parameters":{
            "path":{"type":"path","required":true},
            "payload":{"type":"json"},
            "options":{"type":"record","required":true,"fields":{
                "mode":{"type":"string","required":true},
                "limit":{"type":"integer"}
            }}
        },
        "arguments":[{"literal":"path.inspect"},{"json_arguments":true}]
    });
    let manifest = load(source.clone());
    let values = BTreeMap::from([
        ("path".into(), Value::String("/tmp/file".into())),
        (
            "payload".into(),
            Value::Record(BTreeMap::from([("ok".into(), Value::Bool(true))])),
        ),
        (
            "options".into(),
            Value::Record(BTreeMap::from([
                ("mode".into(), Value::String("compact".into())),
                ("limit".into(), Value::Integer(3)),
            ])),
        ),
    ]);
    assert_eq!(
        manifest.build_arguments(&values).unwrap(),
        [
            "path.inspect",
            r#"{"options":{"limit":3,"mode":"compact"},"path":"/tmp/file","payload":{"ok":true}}"#
        ]
    );
    assert_eq!(
        manifest.parameters_json(&values).unwrap()["path"],
        "/tmp/file"
    );
    assert_eq!(
        manifest.execution_backend(),
        flow::contracts::AppletBackend::InProcess
    );
    let bad_record = BTreeMap::from([
        ("path".into(), Value::String("/tmp/file".into())),
        (
            "options".into(),
            Value::Record(BTreeMap::from([("wrong".into(), Value::Bool(true))])),
        ),
    ]);
    assert!(manifest.build_arguments(&bad_record).is_err());
    let bad_path = BTreeMap::from([
        ("path".into(), Value::String(String::new())),
        (
            "options".into(),
            Value::Record(BTreeMap::from([("mode".into(), Value::String("x".into()))])),
        ),
    ]);
    assert!(manifest.build_arguments(&bad_path).is_err());

    source["arguments"] = json!([{"parameter":"payload"}]);
    assert!(Registry::from_sources(&[source.to_string()]).is_err());
    source["parameters"]["payload"]["required"] = json!(true);
    source["arguments"] = json!([{"json_parameter":"payload"}]);
    let single = load(source);
    assert_eq!(single.build_arguments(&values).unwrap(), [r#"{"ok":true}"#]);
}

#[test]
fn unsupported_versions_invalid_encodings_and_unenforceable_lifecycles_fail_closed() {
    for contract in [
        json!({"version":2}),
        json!({"version":1,"output":{"type":"integer","encoding":"raw"}}),
        json!({"version":1,"output":{"type":"text","encoding":"raw","trim_final_lf":true,"surprise":true}}),
        json!({"version":1,"backend":"json_lines_worker"}),
        json!({"version":1,"backend":"in_process","lifecycle":{"kind":"json_lines"}}),
        json!({"version":1,"result_envelope":true}),
        json!({"version":1,"capabilities":["Filesystem.Read"]}),
        json!({"version":1,"capabilities":["filesystem.read"],"effects":["filesystem.write"]}),
    ] {
        assert!(Registry::from_sources(&[base(contract).to_string()]).is_err());
    }
    let worker = load(base(json!({
        "version":1,
        "backend":"json_lines_worker",
        "lifecycle":{"kind":"json_lines","max_frame_bytes":1024}
    })));
    let contract = worker.execution_contract().unwrap();
    assert!(matches!(
        contract.lifecycle,
        flow::contracts::Lifecycle::JsonLines { .. }
    ));
}

#[test]
fn stdio_and_registry_host_state_are_shared_across_clones() {
    let registry = Registry::from_sources(&[json!({
        "name":"typed","binary":"fixture","arguments":[],
        "stdio":{"stdout":"discard","stderr":"capture"},
        "contract":{"version":1}
    })
    .to_string()])
    .unwrap();
    let clone = registry.clone();
    assert_eq!(
        registry.resolve("typed").unwrap().stdio().unwrap().stdout,
        Some(flow::contracts::StdioDisposition::Discard)
    );
    let installed = registry.set_host_state::<String>("policy".into());
    assert_eq!(clone.host_state::<String>().as_str(), "policy");
    assert!(std::sync::Arc::ptr_eq(
        &installed,
        &clone.host_state::<String>()
    ));
}

#[test]
fn manifest_relative_module_paths_and_cache_dependencies_are_validated() {
    let module: flow::contracts::ProgramModule = serde_json::from_value(json!({
        "source":"../modules/utility.program",
        "entry":"utility",
        "interface_version":1,
        "package_manifest":"../modules/package.json",
        "package_lock":"../modules/package.lock.json"
    }))
    .unwrap();
    module.validate().unwrap();
    let unpaired: flow::contracts::ProgramModule = serde_json::from_value(json!({
        "source":"module.program",
        "entry":"utility",
        "interface_version":1,
        "package_manifest":"package.json"
    }))
    .unwrap();
    assert!(unpaired.validate().is_err());

    let registry = Registry::from_sources(&[json!({
        "name":"typed","binary":"fixture","cacheable":true,
        "cache_dependencies":["inputs/config.json"],"arguments":[]
    })
    .to_string()])
    .unwrap();
    let manifest = registry.resolve("typed").unwrap();
    assert_eq!(manifest.cache_dependencies(), ["inputs/config.json"]);
    assert_eq!(
        serde_json::to_value(manifest).unwrap()["cache_dependencies"],
        json!(["inputs/config.json"])
    );
    for invalid in [
        json!({
            "name":"typed","binary":"fixture","arguments":[],
            "cache_dependencies":["inputs/config.json"]
        }),
        json!({
            "name":"typed","binary":"fixture","cacheable":true,
            "cache_dependencies":["../outside.json"],"arguments":[]
        }),
    ] {
        assert!(Registry::from_sources(&[invalid.to_string()]).is_err());
    }
}

#[test]
fn applet_environment_is_validated_and_redacted_from_debug_and_discovery() {
    let manifest = load(base(json!({
        "version":1,
        "environment":{
            "set":{"TOKEN":"secret-value","MODE":"safe"},
            "unset":["OLD_TOKEN"]
        }
    })));
    let environment = manifest.environment().unwrap();
    assert_eq!(environment.set["MODE"], "safe");
    assert!(environment.unset.contains("OLD_TOKEN"));
    assert!(!format!("{environment:?}").contains("secret-value"));
    assert!(!format!("{:?}", manifest.execution_contract()).contains("secret-value"));
    let discovery = manifest.contract().to_string();
    assert!(discovery.contains("TOKEN"));
    assert!(discovery.contains("OLD_TOKEN"));
    assert!(!discovery.contains("secret-value"));
    assert_eq!(
        serde_json::to_value(environment).unwrap()["set"]["TOKEN"],
        "secret-value"
    );

    for environment in [
        json!({"set":{"BAD-NAME":"value"}}),
        json!({"set":{"TOKEN":format!("bad{}value", '\0')}}),
        json!({"set":{"TOKEN":"one"},"unset":["TOKEN"]}),
    ] {
        assert!(Registry::from_sources(&[base(json!({
            "version":1,
            "environment":environment
        }))
        .to_string()])
        .is_err());
    }
}

#[test]
fn scoped_manifests_shadow_canonical_names_without_mutating_the_parent_registry() {
    let directory = tempfile::tempdir().unwrap();
    let origin = directory.path().join("package.json");
    std::fs::write(&origin, "{}").unwrap();
    let parent = Registry::from_sources(&[
        json!({"name":"shared.operation","binary":"ambient","arguments":[]}).to_string(),
    ])
    .unwrap();
    let parent_state = parent.set_host_state::<String>("policy".into());
    let scoped = parent
        .with_scoped_manifest_sources(
            &[json!({
                "name":"shared.operation","binary":"scoped","arguments":[]
            })
            .to_string()],
            &origin,
        )
        .unwrap();
    assert!(parent.scope_identity().is_none());
    assert!(scoped.scope_identity().is_some());
    assert_eq!(
        parent.resolve("shared.operation").unwrap().binary,
        "ambient"
    );
    let scoped_manifest = scoped.resolve("shared.operation").unwrap();
    assert_eq!(scoped_manifest.binary, "scoped");
    assert_eq!(
        scoped_manifest.origin(),
        Some(std::fs::canonicalize(origin).unwrap().as_path())
    );
    assert!(std::sync::Arc::ptr_eq(
        &parent_state,
        &scoped.host_state::<String>()
    ));
}
