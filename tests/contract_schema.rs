use flow::{
    ast::{Invocation, Value},
    contracts::{AppletOutput, OutcomeKind, OutputType, ValueSchema},
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
fn open_records_validate_dynamic_values_and_keep_closed_records_closed() {
    let open: ValueSchema = serde_json::from_value(json!({
        "type":"record",
        "fields":{},
        "additionalProperties":{"type":"json"},
        "min_properties":1
    }))
    .unwrap();
    let dynamic = Value::Record(BTreeMap::from([
        ("user-key".into(), Value::List(vec![Value::Integer(3)])),
        ("another.key".into(), Value::Bool(true)),
    ]));
    assert!(open.validate_value(&dynamic).is_ok());
    assert!(open
        .validate_value(&Value::Record(BTreeMap::new()))
        .is_err());
    assert!(open.validate_value(&Value::List(Vec::new())).is_err());
    assert_eq!(open.json_schema()["additionalProperties"], json!({}));
    assert_eq!(open.json_schema()["minProperties"], 1);

    let closed: ValueSchema = serde_json::from_value(json!({
        "type":"record","fields":{"known":{"type":"integer"}}
    }))
    .unwrap();
    assert!(closed
        .validate_value(&Value::Record(BTreeMap::from([(
            "arbitrary".into(),
            Value::Integer(1)
        )])))
        .is_err());
}

#[test]
fn open_record_value_constraints_validate_scalars_and_nonempty_text() {
    let scalar_values: ValueSchema = serde_json::from_value(json!({
        "type":"record","fields":{},
        "additionalProperties":{"type":"json","scalar_only":true}
    }))
    .unwrap();
    for value in [
        Value::String("x".into()),
        Value::Integer(2),
        Value::Number(serde_json::Number::from_f64(2.5).unwrap()),
        Value::Bool(false),
    ] {
        assert!(scalar_values
            .validate_value(&Value::Record(BTreeMap::from([("key".into(), value)])))
            .is_ok());
    }
    assert!(scalar_values
        .validate_value(&Value::Record(BTreeMap::from([(
            "key".into(),
            Value::Null
        )])))
        .is_err());

    let descriptions: ValueSchema = serde_json::from_value(json!({
        "type":"record","fields":{},"min_properties":2,"max_properties":128,
        "additionalProperties":{"type":"text","nonempty":true}
    }))
    .unwrap();
    assert!(descriptions
        .validate_value(&Value::Record(BTreeMap::from([
            ("yes".into(), Value::String(" description ".into())),
            ("no".into(), Value::String("\t ".into()))
        ])))
        .is_err());
    assert!(descriptions
        .validate_value(&Value::Record(BTreeMap::from([
            ("yes".into(), Value::String("description".into())),
            ("no".into(), Value::String("other".into()))
        ])))
        .is_ok());
    assert!(descriptions
        .validate_value(&Value::Record(BTreeMap::from([
            ("yes".into(), Value::String("\u{0085}".into())),
            ("no".into(), Value::String("other".into()))
        ])))
        .is_err());
    assert!(descriptions
        .validate_value(&Value::Record(BTreeMap::from([
            ("yes".into(), Value::String("\u{FEFF}".into())),
            ("no".into(), Value::String("other".into()))
        ])))
        .is_ok());
}

#[test]
fn open_record_assignability_accounts_for_dynamic_keys_and_named_overrides() {
    let open_json: ValueSchema = serde_json::from_value(json!({
        "type":"record","fields":{},
        "additionalProperties":{"type":"json"}
    }))
    .unwrap();
    let named_text: ValueSchema = serde_json::from_value(json!({
        "type":"record","fields":{"special":{"type":"text"}}
    }))
    .unwrap();
    assert!(!open_json.is_assignable_to(&named_text));

    let source_with_override: ValueSchema = serde_json::from_value(json!({
        "type":"record",
        "fields":{"special":{"type":"text"}},
        "additionalProperties":{"type":"json"}
    }))
    .unwrap();
    let target_with_override: ValueSchema = serde_json::from_value(json!({
        "type":"record",
        "fields":{"special":{"type":"text"}},
        "additionalProperties":{"type":"json"}
    }))
    .unwrap();
    assert!(source_with_override.is_assignable_to(&target_with_override));
}

#[test]
fn manifest_check_accepts_dynamic_scalar_maps_and_rejects_wrong_roots() {
    let manifest = load(json!({
        "name":"typed",
        "binary":"fixture",
        "parameters":{"values":{
            "type":"record","required":true,"fields":{},
            "additional_properties":{"type":"scalar"}
        }},
        "arguments":[],
        "contract":{"version":1,"ports":{"inputs":{"values":{
            "schema":{"type":"record","fields":{},
                "additionalProperties":{"type":"json","scalar_only":true}},
            "delivery":"single"
        }}}}
    }));
    let invocation = |value| Invocation {
        operation: "typed".into(),
        direct: false,
        discard_stderr: false,
        parameters: BTreeMap::from([("values".into(), value)]),
        binding: None,
        cache: false,
        status_binding: None,
        result_binding: None,
        retry: None,
    };
    let valid = Value::Record(BTreeMap::from([
        ("arbitrary key".into(), Value::String("hello".into())),
        (
            "another".into(),
            Value::Number(serde_json::Number::from_f64(2.5).unwrap()),
        ),
        ("flag".into(), Value::Bool(true)),
    ]));
    assert!(manifest.validate(&invocation(valid)).is_ok());
    assert!(manifest
        .validate(&invocation(Value::List(Vec::new())))
        .is_err());
    assert!(manifest
        .validate(&invocation(Value::Record(BTreeMap::from([(
            "null".into(),
            Value::Null
        )]))))
        .is_err());
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
            discard_stderr: false,
            parameters: BTreeMap::new(),
            binding: Some("raw".into()),
            cache: false,
            status_binding: None,
            result_binding: None,
            retry: None,
        })
        .is_err());
    assert!(manifest
        .validate(&Invocation {
            operation: "typed".into(),
            direct: false,
            discard_stderr: false,
            parameters: BTreeMap::new(),
            binding: None,
            cache: false,
            status_binding: None,
            result_binding: None,
            retry: None,
        })
        .is_ok());
    let implicit_bytes = load(base(json!({"version":1})));
    assert!(implicit_bytes
        .validate(&Invocation {
            operation: "typed".into(),
            direct: false,
            discard_stderr: false,
            parameters: BTreeMap::new(),
            binding: Some("raw".into()),
            cache: false,
            status_binding: None,
            result_binding: None,
            retry: None,
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

#[test]
fn number_output_is_finite_json_numeric_and_not_generic_json() {
    let manifest = load(base(json!({
        "version":1,
        "output":{"type":"number","encoding":"json"}
    })));
    assert_eq!(manifest.output_kind(), Some(flow::ast::InputType::Json));
    assert_eq!(
        manifest.decode_output(b"12", 0).unwrap(),
        AppletOutput::Value(Value::Integer(12))
    );
    assert_eq!(
        manifest.decode_output(b"12.5", 0).unwrap(),
        AppletOutput::Value(Value::Number(serde_json::Number::from_f64(12.5).unwrap()))
    );
    for invalid in [b"\"12\"".as_slice(), b"true", b"null", b"[]", b"NaN"] {
        assert!(manifest.decode_output(invalid, 0).is_err(), "{invalid:?}");
    }
    assert_eq!(manifest.contract()["io"]["output_schema"]["type"], "number");
}

#[test]
fn number_ports_refine_json_parameters_before_invocation_and_dispatch() {
    let manifest = load(json!({
        "name":"typed",
        "binary":"fixture",
        "parameters":{
            "left":{"type":"json","required":true},
            "right":{"type":"json","required":true}
        },
        "arguments":[],
        "contract":{
            "version":1,
            "output":{"type":"number","encoding":"json"},
            "ports":{
                "inputs":{
                    "left":{"schema":{"type":"number"},"delivery":"single"},
                    "right":{"schema":{"type":"number"},"delivery":"single"}
                },
                "outputs":{"value":{"schema":{"type":"number"},"delivery":"single"}}
            }
        }
    }));
    let invocation = |left, right| Invocation {
        operation: "typed".into(),
        direct: false,
        discard_stderr: false,
        parameters: BTreeMap::from([("left".into(), left), ("right".into(), right)]),
        binding: None,
        cache: false,
        status_binding: None,
        result_binding: None,
        retry: None,
    };
    assert!(manifest
        .validate(&invocation(
            Value::Integer(2),
            Value::Number("2.5".parse().unwrap())
        ))
        .is_ok());
    assert!(manifest
        .validate(&invocation(Value::String("2".into()), Value::Integer(1)))
        .is_err());
    assert!(manifest
        .validate(&invocation(Value::Bool(false), Value::Integer(1)))
        .is_err());

    let resolved_bad = BTreeMap::from([
        ("left".into(), Value::Null),
        ("right".into(), Value::Integer(1)),
    ]);
    assert!(manifest.parameters_json(&resolved_bad).is_err());
}

#[test]
fn text_enums_validate_values_project_as_json_schema_and_allow_nullable_null() {
    let manifest = load(base(json!({
        "version":1,
        "output":{"type":"text","encoding":"raw","enum":["red","blue"]}
    })));
    assert_eq!(
        manifest.decode_output(b"red", 0).unwrap(),
        AppletOutput::Value(Value::String("red".into()))
    );
    assert!(manifest.decode_output(b"green", 0).is_err());
    assert_eq!(
        manifest.contract()["io"]["output_schema"]["enum"],
        json!(["red", "blue"])
    );

    let nullable = ValueSchema {
        min_items: None,
        scalar_only: false,
        additional_properties: None,
        min_properties: None,
        max_properties: None,
        nonempty: false,
        kind: OutputType::Text,
        nullable: true,
        items: None,
        fields: BTreeMap::new(),
        required: Vec::new(),
        enum_values: Some(vec!["red".into(), "blue".into()]),
        single_line: false,
        minimum: None,
        pattern: None,
    };
    assert!(nullable.validate_value(&Value::Null).is_ok());
    assert!(nullable
        .validate_value(&Value::String("red".into()))
        .is_ok());
    assert!(nullable
        .validate_value(&Value::String("green".into()))
        .is_err());
    let json_schema = nullable.json_schema();
    assert_eq!(json_schema["anyOf"][0]["enum"], json!(["red", "blue"]));
    assert_eq!(json_schema["anyOf"][1]["type"], "null");
}

#[test]
fn single_line_text_schemas_reject_cr_and_lf_but_allow_empty_text() {
    let single_line = ValueSchema {
        min_items: None,
        kind: OutputType::Text,
        scalar_only: false,
        additional_properties: None,
        min_properties: None,
        max_properties: None,
        nonempty: false,
        nullable: false,
        items: None,
        fields: BTreeMap::new(),
        required: Vec::new(),
        enum_values: None,
        single_line: true,
        minimum: None,
        pattern: None,
    };
    let unrestricted = ValueSchema {
        min_items: None,
        single_line: false,
        minimum: None,
        ..single_line.clone()
    };

    assert!(single_line
        .validate_value(&Value::String(String::new()))
        .is_ok());
    assert!(single_line
        .validate_value(&Value::String("plain text".into()))
        .is_ok());
    assert!(single_line
        .validate_value(&Value::String("a\nb".into()))
        .is_err());
    assert!(single_line
        .validate_value(&Value::String("a\rb".into()))
        .is_err());
    assert!(single_line.is_assignable_to(&unrestricted));
    assert!(!unrestricted.is_assignable_to(&single_line));
    assert_eq!(single_line.json_schema()["pattern"], json!("^[^\\r\\n]*$"));
}

#[test]
fn manifest_single_line_ports_check_literals_but_defer_references() {
    let manifest = json!({
        "name":"fixture.line",
        "binary":"fixture",
        "parameters":{"line":{"type":"string","required":true}},
        "arguments":[],
        "contract":{
            "version":1,
            "backend":"command",
            "lifecycle":{"kind":"oneshot"},
            "ports":{"inputs":{
                "line":{"schema":{"type":"text","single_line":true},"delivery":"single"}
            }}
        }
    });
    let registry = Registry::from_sources(&[manifest.to_string()]).unwrap();
    let manifest = registry.resolve("fixture.line").unwrap();
    let invocation = |line| Invocation {
        operation: "fixture.line".into(),
        direct: false,
        discard_stderr: false,
        parameters: BTreeMap::from([("line".into(), line)]),
        binding: None,
        cache: false,
        status_binding: None,
        result_binding: None,
        retry: None,
    };

    assert!(manifest
        .validate(&invocation(Value::String("left\nright".into())))
        .is_err());
    assert!(manifest
        .validate(&invocation(Value::String(String::new())))
        .is_ok());
    assert!(manifest
        .validate(&invocation(Value::Reference("runtime_line".into())))
        .is_ok());
}

#[test]
fn new_optional_schema_fields_do_not_change_legacy_serialized_shapes() {
    let value_schema: ValueSchema = serde_json::from_value(json!({"type":"text"})).unwrap();
    assert_eq!(
        serde_json::to_value(value_schema).unwrap(),
        json!({"type":"text"})
    );

    let manifest = load(base(json!({
        "version":1,
        "output":{"type":"text","encoding":"raw"}
    })));
    let serialized = serde_json::to_value(manifest).unwrap();
    assert!(serialized["contract"]["output"].get("enum").is_none());
}

#[test]
fn numeric_list_schemas_enforce_minimum_length_without_rejecting_floats() {
    let schema: ValueSchema = serde_json::from_value(json!({
        "type":"list",
        "items":{"type":"number"},
        "min_items":1
    }))
    .unwrap();
    assert_eq!(schema.json_schema()["minItems"], 1);
    assert!(schema
        .validate_value(&Value::List(vec![
            Value::Integer(1),
            Value::Number("2.5".parse().unwrap())
        ]))
        .is_ok());
    assert!(schema.validate_value(&Value::List(Vec::new())).is_err());
    assert!(schema
        .validate_value(&Value::List(vec![Value::String("bad".into())]))
        .is_err());

    let unconstrained: ValueSchema = serde_json::from_value(json!({
        "type":"list", "items":{"type":"number"}
    }))
    .unwrap();
    assert!(!unconstrained.is_assignable_to(&schema));
    assert!(schema.is_assignable_to(&unconstrained));
}

#[cfg(unix)]
#[test]
fn native_pipes_validate_wire_encoding_before_launching_and_accept_json_text() {
    use std::{fs, os::unix::fs::PermissionsExt, path::Path, time::Duration};
    use tempfile::tempdir;

    fn shell_quote(value: &str) -> String {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
    fn write_script(path: &Path, body: &str) {
        fs::write(path, format!("#!/bin/sh\nset -eu\n{body}\n")).unwrap();
        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).unwrap();
    }
    fn options(cwd: &Path) -> flow::runtime::RunOptions {
        flow::runtime::RunOptions {
            max_output_bytes: 1024,
            command_timeout: Some(Duration::from_secs(3)),
            total_timeout: Some(Duration::from_secs(5)),
            ..flow::runtime::RunOptions::new(cwd.to_path_buf())
        }
    }

    let directory = tempdir().unwrap();
    let raw_producer = directory.path().join("raw-producer");
    let json_producer = directory.path().join("json-producer");
    let marker = directory.path().join("producer-ran");
    write_script(
        &raw_producer,
        &format!(
            ": > {}\nprintf '%s' 'hello'",
            shell_quote(&marker.to_string_lossy())
        ),
    );
    write_script(&json_producer, "printf '%s' '\"hello\"'");

    let raw_manifest = json!({
        "name":"fixture.raw_text",
        "binary":raw_producer,
        "parameters":{},
        "arguments":[],
        "contract":{"version":1,"output":{"type":"text","encoding":"raw"}}
    });
    let json_manifest = json!({
        "name":"fixture.json_text",
        "binary":json_producer,
        "parameters":{},
        "arguments":[],
        "contract":{"version":1,"output":{"type":"text","encoding":"json"}}
    });
    let consumer_manifest = json!({
        "name":"fixture.json_sink",
        "binary":"/bin/cat",
        "accepts_stdin":true,
        "parameters":{},
        "arguments":[],
        "contract":{"version":1,"ports":{"inputs":{
            "stdin":{"schema":{"type":"json"},"delivery":"single","encoding":"json"}
        }}}
    });
    let registry = Registry::from_sources(&[
        raw_manifest.to_string(),
        json_manifest.to_string(),
        consumer_manifest.to_string(),
    ])
    .unwrap();

    let raw_source = "run fixture.raw_text\npipe to fixture.json_sink\n";
    let diagnostic = flow::diagnostics::analyze_source(raw_source, &registry)
        .into_iter()
        .find(|diagnostic| diagnostic.code == "applet.pipeline.incompatible")
        .expect("raw text cannot be consumed as JSON framing");
    assert!(diagnostic.message.contains("stdout encoding is Raw"));
    let raw_program = flow::parser::parse(raw_source).unwrap();
    let error = match flow::runtime::run(&raw_program, &registry, &options(directory.path())) {
        Ok(_) => panic!("raw text unexpectedly reached a JSON stdin consumer"),
        Err(error) => error,
    };
    assert!(
        format!("{error:#}").contains("stdout encoding is Raw, expected stdin encoding Json"),
        "{error:#}"
    );
    assert!(
        !marker.exists(),
        "pipe validation must run before producer effects"
    );

    let json_program =
        flow::parser::parse("run fixture.json_text\npipe to fixture.json_sink\n").unwrap();
    let result = flow::runtime::run(&json_program, &registry, &options(directory.path())).unwrap();
    assert_eq!(result.output, br#""hello""#);
    assert_eq!(result.launched_processes, 2);
}

#[cfg(unix)]
#[test]
fn native_pipe_rejects_decode_only_path_normalization_but_keeps_raw_bytes() {
    use std::{fs, os::unix::fs::PermissionsExt, path::Path, time::Duration};
    use tempfile::tempdir;

    fn shell_quote(value: &str) -> String {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
    fn write_script(path: &Path, body: &str) {
        fs::write(path, format!("#!/bin/sh\nset -eu\n{body}\n")).unwrap();
        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).unwrap();
    }
    fn options(cwd: &Path) -> flow::runtime::RunOptions {
        flow::runtime::RunOptions {
            max_output_bytes: 1024,
            command_timeout: Some(Duration::from_secs(3)),
            total_timeout: Some(Duration::from_secs(5)),
            ..flow::runtime::RunOptions::new(cwd.to_path_buf())
        }
    }

    let directory = tempdir().unwrap();
    let path_producer = directory.path().join("path-producer");
    let marker = directory.path().join("path-producer-ran");
    write_script(
        &path_producer,
        &format!(
            ": > {}\nprintf '%s\\n' '/tmp/result'",
            shell_quote(&marker.to_string_lossy())
        ),
    );
    let producer = json!({
        "name":"fixture.trimmed_path",
        "binary":path_producer,
        "parameters":{},
        "arguments":[],
        "contract":{"version":1,"output":{
            "type":"path","encoding":"raw","trim_final_lf":true
        }}
    });
    let path_consumer = json!({
        "name":"fixture.path_sink",
        "binary":"/bin/cat",
        "accepts_stdin":true,
        "parameters":{},
        "arguments":[],
        "contract":{"version":1,"ports":{"inputs":{
            "stdin":{"schema":{"type":"path"},"delivery":"single","encoding":"raw"}
        }}}
    });
    let bytes_consumer = json!({
        "name":"fixture.bytes_sink",
        "binary":"/bin/cat",
        "accepts_stdin":true,
        "parameters":{},
        "arguments":[],
        "contract":{"version":1,"ports":{"inputs":{
            "stdin":{"schema":{"type":"bytes"},"delivery":"single","encoding":"raw"}
        }}}
    });
    let registry = Registry::from_sources(&[
        producer.to_string(),
        path_consumer.to_string(),
        bytes_consumer.to_string(),
    ])
    .unwrap();

    let typed_pipe = "run fixture.trimmed_path\npipe to fixture.path_sink\n";
    let diagnostic = flow::diagnostics::analyze_source(typed_pipe, &registry)
        .into_iter()
        .find(|diagnostic| diagnostic.code == "applet.pipeline.incompatible")
        .expect("typed path stdin cannot apply the producer's binding-only trim");
    assert!(diagnostic.message.contains("trim_final_lf"));
    let typed_program = flow::parser::parse(typed_pipe).unwrap();
    let error = match flow::runtime::run(&typed_program, &registry, &options(directory.path())) {
        Ok(_) => panic!("typed path stdin unexpectedly received unnormalized raw bytes"),
        Err(error) => error,
    };
    assert!(format!("{error:#}").contains("trim_final_lf"), "{error:#}");
    assert!(
        !marker.exists(),
        "pipe validation must reject before producer effects"
    );

    let bytes_pipe =
        flow::parser::parse("run fixture.trimmed_path\npipe to fixture.bytes_sink\n").unwrap();
    let result = flow::runtime::run(&bytes_pipe, &registry, &options(directory.path())).unwrap();
    assert_eq!(result.output, b"/tmp/result\n");
    assert_eq!(result.launched_processes, 2);
    assert!(marker.exists());
}
