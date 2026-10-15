use flow::contracts::{OutputType, PortDelivery, Sensitivity, ValueSchema};
use flow::graph::{
    self, BufferPolicy, EdgeSpec, GraphNode, GraphNodeKind, GraphPlan, PortRef, PortSchema,
};
use flow::manifest::Registry;
use std::collections::BTreeMap;

fn registry() -> Registry {
    Registry::from_sources(&[
        r#"{"name":"text.source","binary":"/bin/echo","arguments":[{"literal":"hello"}],"contract":{"version":1,"output":{"type":"bytes","encoding":"raw"}}}"#,
        r#"{"name":"bytes.sink","binary":"/bin/cat","accepts_stdin":true,"arguments":[],"contract":{"version":1,"output":{"type":"bytes","encoding":"raw"}}}"#,
    ])
    .unwrap()
}

fn leaf(kind: OutputType) -> ValueSchema {
    ValueSchema {
        kind,
        nullable: false,
        fields: BTreeMap::new(),
        required: Vec::new(),
        items: None,
    }
}

#[test]
fn parser_builds_typed_graph_and_preserves_explicit_queue_policy() {
    let plan = graph::parse(
        "graph demo\n  run text.source as source\n  run bytes.sink as sink\n  bind source.stdout to sink.stdin with capacity 3 drop oldest\nend graph\n",
        &registry(),
    )
    .unwrap();
    assert_eq!(plan.name, "demo");
    assert_eq!(plan.nodes().len(), 2);
    assert_eq!(plan.edges()[0].capacity, 3);
    assert_eq!(plan.edges()[0].policy, BufferPolicy::DropOldest);
    assert_eq!(
        plan.nodes()[0].outputs["stdout"].schema.kind,
        OutputType::Bytes
    );
    assert_eq!(
        plan.nodes()[1].inputs["stdin"].schema.kind,
        OutputType::Bytes
    );
}

#[test]
fn schema_mismatch_is_reported_before_any_node_can_start() {
    let mut plan = GraphPlan::new("bad_types");
    let mut source = GraphNode {
        id: "source".into(),
        kind: GraphNodeKind::Renderer {
            backend: "mock".into(),
        },
        inputs: BTreeMap::new(),
        outputs: BTreeMap::from([("out".into(), PortSchema::single(leaf(OutputType::Text)))]),
        events: BTreeMap::new(),
        line: None,
    };
    source.line = Some(2);
    let sink = GraphNode {
        id: "sink".into(),
        kind: GraphNodeKind::Renderer {
            backend: "mock".into(),
        },
        inputs: BTreeMap::from([(
            "in".into(),
            PortSchema {
                schema: leaf(OutputType::Bytes),
                delivery: PortDelivery::Single,
                sensitivity: Sensitivity::Unknown,
                required: true,
                pointer: None,
                encoding: None,
            },
        )]),
        outputs: BTreeMap::new(),
        events: BTreeMap::new(),
        line: Some(3),
    };
    plan.add_node(source).unwrap();
    plan.add_node(sink).unwrap();
    plan.add_edge(EdgeSpec::lossless(
        PortRef::new("source", "out"),
        PortRef::new("sink", "in"),
        1,
    ));
    let error = plan.validate().unwrap_err().to_string();
    assert!(error.contains("port_schema_mismatch"), "{error}");
}

#[test]
fn required_inputs_and_record_items_are_validated_recursively() {
    let item = leaf(OutputType::Integer);
    let list = ValueSchema {
        kind: OutputType::List,
        nullable: false,
        fields: BTreeMap::new(),
        required: Vec::new(),
        items: Some(Box::new(item)),
    };
    assert!(list.validate().is_ok());
    let invalid = ValueSchema {
        kind: OutputType::List,
        nullable: false,
        fields: BTreeMap::new(),
        required: Vec::new(),
        items: None,
    };
    assert!(invalid.validate().is_err());

    let mut plan = GraphPlan::new("required");
    plan.add_node(GraphNode {
        id: "sink".into(),
        kind: GraphNodeKind::Renderer {
            backend: "mock".into(),
        },
        inputs: BTreeMap::from([(
            "record".into(),
            PortSchema {
                schema: leaf(OutputType::Record),
                delivery: PortDelivery::Single,
                sensitivity: Sensitivity::Unknown,
                required: true,
                pointer: None,
                encoding: None,
            },
        )]),
        outputs: BTreeMap::new(),
        events: BTreeMap::new(),
        line: None,
    })
    .unwrap();
    assert!(plan
        .validate()
        .unwrap_err()
        .to_string()
        .contains("required_port_unbound"));
}

#[test]
fn topology_inspection_exports_typed_ports_and_queue_policy() {
    let plan = graph::parse(
        "graph inspect\nrun text.source as source\nrun bytes.sink as sink\nbind source.stdout to sink.stdin with capacity 2 latest\nend graph\n",
        &registry(),
    )
    .unwrap();
    let json: serde_json::Value = serde_json::from_str(&plan.to_json().unwrap()).unwrap();
    assert_eq!(json["nodes"][0]["outputs"]["stdout"]["delivery"], "single");
    let dot = plan.to_dot();
    assert!(dot.contains("capacity=2"));
    assert!(dot.to_lowercase().contains("latest"));
    assert!(dot.contains("bytes") || dot.contains("text"));
}

#[test]
fn native_jsonl_stream_uses_the_final_applet_output_schema() {
    let registry = Registry::from_sources(&[r#"{"name":"json.records","binary":"/usr/bin/printf","arguments":[{"literal":"{\"text\":\"tick\"}\\n"}],"contract":{"version":1,"output":{"type":"record","encoding":"json","fields":{"text":{"type":"text"}},"required":["text"]}}}"#]).unwrap();
    let plan = graph::parse(
        "graph typed_stream\nstream from json.records as records\nshow each line\nend graph\n",
        &registry,
    )
    .unwrap();
    let output = &plan.nodes()[0].outputs["stdout"];
    assert_eq!(output.delivery, PortDelivery::Records);
    assert_eq!(output.schema.kind, OutputType::Record);
    assert_eq!(output.schema.fields["text"].kind, OutputType::Text);
}

#[test]
fn renderer_backends_share_typed_component_ports() {
    let registry = registry();
    let quickshell = graph::parse(
        "graph qml\nrender quickshell:button as action\nend graph\n",
        &registry,
    )
    .unwrap();
    let tui = graph::parse(
        "graph tui\nrender tui:button as action\nend graph\n",
        &registry,
    )
    .unwrap();
    assert_eq!(quickshell.nodes()[0].inputs, tui.nodes()[0].inputs);
    assert_eq!(quickshell.nodes()[0].events, tui.nodes()[0].events);
    assert_eq!(
        tui.nodes()[0].inputs["properties"].schema.fields["text"].kind,
        OutputType::Text
    );
    assert_eq!(
        tui.nodes()[0].inputs["properties"].schema.fields["enabled"].kind,
        OutputType::Boolean
    );
    assert!(!tui.nodes()[0].inputs["properties"].required);
    assert_eq!(
        tui.nodes()[0].inputs["property_updates"].delivery,
        PortDelivery::Records
    );
    assert!(tui.nodes()[0].inputs["property_updates"]
        .schema
        .required
        .is_empty());
    assert_eq!(
        tui.nodes()[0].events["clicked"].delivery,
        PortDelivery::Events
    );
}

#[test]
fn renderer_record_literal_seeds_the_typed_properties_port() {
    let plan = graph::parse(
        "graph seeded\nrender tui:text with properties {\"text\": \"Hello\"} as label\nend graph\n",
        &registry(),
    )
    .unwrap();
    assert_eq!(
        plan.initial_inputs()["label"]["properties"],
        flow::ast::Value::Record(BTreeMap::from([(
            "text".into(),
            flow::ast::Value::String("Hello".into()),
        )]))
    );
}

fn streamed_formatter_registry(output_delivery: &str, event_output: bool) -> Registry {
    let mut manifest = serde_json::json!({
        "name": "demo.format_integer",
        "binary": "/bin/echo",
        "parameters": {
            "value": {"type": "integer", "required": true}
        },
        "arguments": [{"parameter": "value"}],
        "contract": {
            "version": 1,
            "output": {"type": "text", "encoding": "json"},
            "ports": {
                "inputs": {
                    "value": {"schema": {"type": "integer"}, "delivery": "events"}
                },
                "outputs": {
                    "value": {"schema": {"type": "text"}, "delivery": output_delivery}
                }
            }
        }
    });
    if event_output {
        manifest["contract"]["ports"]["events"] = serde_json::json!({
            "updated": {
                "schema": {"type": "record", "fields": {}, "required": []},
                "delivery": "events"
            }
        });
    }
    let source = manifest.to_string();
    Registry::from_sources(&[&source]).unwrap()
}

const STREAM_FORMATTER_GRAPH: &str = "graph clocked\nevery 1 second as clock\nrun demo.format_integer as formatter\nbind clock.tick to formatter.value with capacity 2 lossless\nend graph\n";

#[test]
fn stream_triggered_applet_requires_records_outputs_before_effects() {
    let registry = streamed_formatter_registry("single", false);
    let error = graph::parse(STREAM_FORMATTER_GRAPH, &registry)
        .unwrap_err()
        .to_string();
    assert!(error.contains("streaming input"), "{error}");
    assert!(error.contains("single-valued outputs"), "{error}");

    let registry = streamed_formatter_registry("records", false);
    let plan = graph::parse(STREAM_FORMATTER_GRAPH, &registry).unwrap();
    assert_eq!(
        plan.nodes()[1].outputs["value"].delivery,
        PortDelivery::Records
    );
    assert_eq!(
        plan.nodes()[1].outputs["status"].delivery,
        PortDelivery::Records
    );
}

#[test]
fn generic_one_shot_applet_event_outputs_are_rejected_during_preflight() {
    let registry = streamed_formatter_registry("records", true);
    let error = graph::parse(STREAM_FORMATTER_GRAPH, &registry)
        .unwrap_err()
        .to_string();
    assert!(error.contains("cannot publish backend events"), "{error}");
}

#[test]
fn cycles_are_rejected_during_preflight() {
    let mut plan = GraphPlan::new("cycle");
    for (id, input, output) in [("left", "right", "left"), ("right", "left", "right")] {
        plan.add_node(GraphNode {
            id: id.into(),
            kind: GraphNodeKind::Renderer {
                backend: "mock".into(),
            },
            inputs: BTreeMap::from([(
                input.into(),
                PortSchema::records(leaf(OutputType::Integer)),
            )]),
            outputs: BTreeMap::from([(
                output.into(),
                PortSchema::records(leaf(OutputType::Integer)),
            )]),
            events: BTreeMap::new(),
            line: None,
        })
        .unwrap();
    }
    plan.add_edge(EdgeSpec::lossless(
        PortRef::new("left", "left"),
        PortRef::new("right", "left"),
        1,
    ));
    plan.add_edge(EdgeSpec::lossless(
        PortRef::new("right", "right"),
        PortRef::new("left", "right"),
        1,
    ));
    let error = plan.validate().unwrap_err().to_string();
    assert!(error.contains("graph_cycle_requires_seed"), "{error}");
}

#[test]
fn secret_provenance_survives_public_transformer_before_remote_egress() {
    use flow::ast::StepKind;
    use flow::security::DataFlowPolicy;

    let remote = r#"{
      "name":"remote.sink",
      "binary":"@remote-applet",
      "parameters":{"payload":{"type":"string","required":true}},
      "arguments":[{"parameter":"payload"}],
      "contract":{
        "version":1,
        "backend":"remote",
        "remote":{"endpoint":"https://api.example.test/v1"},
        "ports":{"inputs":{"payload":{"schema":{"type":"text"},"delivery":"single","sensitivity":"public"}}}
      }
    }"#;
    let registry = Registry::from_sources(&[remote]).unwrap();
    registry.set_host_state(
        DataFlowPolicy::from_json(
            br#"{"version":1,"remote_sinks":{"https://api.example.test":{"allow":["public"]}}}"#,
        )
        .unwrap(),
    );
    let parsed = flow::parser::parse("run remote.sink\n").unwrap();
    let StepKind::Invoke { call, .. } = parsed.steps[0].kind.clone() else {
        panic!("invocation expected")
    };

    let mut source_schema = leaf(OutputType::Text);
    let mut transform_input = leaf(OutputType::Text);
    let mut transform_output = leaf(OutputType::Text);
    source_schema.nullable = false;
    let mut source_port = PortSchema::single(source_schema);
    source_port.sensitivity = Sensitivity::Secret;
    let mut transform_input_port = PortSchema::single(transform_input.clone());
    transform_input_port.sensitivity = Sensitivity::Public;
    let mut transform_output_port = PortSchema::single(transform_output.clone());
    transform_output_port.sensitivity = Sensitivity::Public;
    transform_input.nullable = false;
    transform_output.nullable = false;

    let mut plan = GraphPlan::new("taint");
    plan.add_node(GraphNode {
        id: "source".into(),
        kind: GraphNodeKind::Renderer {
            backend: "mock".into(),
        },
        inputs: BTreeMap::new(),
        outputs: BTreeMap::from([("out".into(), source_port)]),
        events: BTreeMap::new(),
        line: None,
    })
    .unwrap();
    plan.add_node(GraphNode {
        id: "formatter".into(),
        kind: GraphNodeKind::Renderer {
            backend: "mock".into(),
        },
        inputs: BTreeMap::from([("in".into(), transform_input_port)]),
        outputs: BTreeMap::from([("out".into(), transform_output_port)]),
        events: BTreeMap::new(),
        line: None,
    })
    .unwrap();
    plan.add_node(GraphNode::applet(
        "remote",
        call,
        BTreeMap::from([(
            "payload".into(),
            PortSchema {
                schema: leaf(OutputType::Text),
                delivery: PortDelivery::Single,
                sensitivity: Sensitivity::Public,
                required: true,
                pointer: None,
                encoding: None,
            },
        )]),
        BTreeMap::new(),
    ))
    .unwrap();
    plan.add_edge(EdgeSpec::lossless(
        PortRef::new("source", "out"),
        PortRef::new("formatter", "in"),
        1,
    ));
    plan.add_edge(EdgeSpec::lossless(
        PortRef::new("formatter", "out"),
        PortRef::new("remote", "payload"),
        1,
    ));

    let error = plan
        .validate_with_registry(&registry)
        .unwrap_err()
        .to_string();
    assert!(error.contains("data flow into remote node"), "{error}");
    let labels = plan.effective_sensitivity_labels(&registry).unwrap()
        [&("formatter".into(), "out".into())]
        .clone();
    assert!(labels.contains(&Sensitivity::Secret));
    assert!(labels.contains(&Sensitivity::Public));
}

#[test]
fn unknown_label_cannot_mask_secret_before_remote_preflight() {
    use flow::ast::StepKind;
    use flow::security::DataFlowPolicy;

    let remote = r#"{
      "name":"remote.sink",
      "binary":"@remote-applet",
      "parameters":{"payload":{"type":"string","required":true}},
      "arguments":[{"parameter":"payload"}],
      "contract":{
        "version":1,
        "backend":"remote",
        "remote":{"endpoint":"https://api.example.test/v1"},
        "ports":{"inputs":{"payload":{"schema":{"type":"text"},"delivery":"single","sensitivity":"public"}}}
      }
    }"#;
    let registry = Registry::from_sources(&[remote]).unwrap();
    registry.set_host_state(
        DataFlowPolicy::from_json(
            br#"{"version":1,"remote_sinks":{"https://api.example.test":{"allow":["public","unknown"]}}}"#,
        )
        .unwrap(),
    );
    let parsed = flow::parser::parse("run remote.sink\n").unwrap();
    let StepKind::Invoke { call, .. } = parsed.steps[0].kind.clone() else {
        panic!("invocation expected")
    };

    let mut source_port = PortSchema::single(leaf(OutputType::Text));
    source_port.sensitivity = Sensitivity::Secret;
    let mut formatter_input = PortSchema::single(leaf(OutputType::Text));
    formatter_input.sensitivity = Sensitivity::Unknown;
    let mut formatter_output = PortSchema::single(leaf(OutputType::Text));
    formatter_output.sensitivity = Sensitivity::Public;

    let mut plan = GraphPlan::new("unknown_does_not_declassify");
    plan.add_node(GraphNode {
        id: "source".into(),
        kind: GraphNodeKind::Renderer {
            backend: "mock".into(),
        },
        inputs: BTreeMap::new(),
        outputs: BTreeMap::from([("out".into(), source_port)]),
        events: BTreeMap::new(),
        line: None,
    })
    .unwrap();
    plan.add_node(GraphNode {
        id: "formatter".into(),
        kind: GraphNodeKind::Renderer {
            backend: "mock".into(),
        },
        inputs: BTreeMap::from([("in".into(), formatter_input)]),
        outputs: BTreeMap::from([("out".into(), formatter_output)]),
        events: BTreeMap::new(),
        line: None,
    })
    .unwrap();
    plan.add_node(GraphNode::applet(
        "remote",
        call,
        BTreeMap::from([(
            "payload".into(),
            PortSchema {
                schema: leaf(OutputType::Text),
                delivery: PortDelivery::Single,
                sensitivity: Sensitivity::Public,
                required: true,
                pointer: None,
                encoding: None,
            },
        )]),
        BTreeMap::new(),
    ))
    .unwrap();
    plan.add_edge(EdgeSpec::lossless(
        PortRef::new("source", "out"),
        PortRef::new("formatter", "in"),
        1,
    ));
    plan.add_edge(EdgeSpec::lossless(
        PortRef::new("formatter", "out"),
        PortRef::new("remote", "payload"),
        1,
    ));

    let labels = plan.effective_sensitivity_labels(&registry).unwrap()
        [&("formatter".into(), "out".into())]
        .clone();
    assert!(labels.contains(&Sensitivity::Secret));
    assert!(labels.contains(&Sensitivity::Unknown));
    assert!(labels.contains(&Sensitivity::Public));
    // This exact policy accepts the formatter's Public+Unknown declarations;
    // the graph is denied only because the known Secret contributor survives.
    let public_unknown =
        flow::security::SensitivitySet::from_labels([Sensitivity::Public, Sensitivity::Unknown]);
    assert!(flow::security::authorize_data_flow_set(
        &registry,
        "remote.sink",
        "https://api.example.test/v1",
        &public_unknown,
    )
    .is_ok());
    let error = plan.validate_with_registry(&registry).unwrap_err();
    assert!(
        format!("{error:#}").contains("data flow into remote node"),
        "{error:#}"
    );
    assert_eq!(
        serde_json::to_value(&labels).unwrap(),
        serde_json::json!(["public", "secret", "unknown"])
    );
}
