use anyhow::{anyhow, Result};
use flow::ast::Value;
use flow::contracts::{OutputType, ValueSchema};
use flow::graph::{
    self, BufferPolicy, ChannelStdinSource, EdgeSpec, GraphContext, GraphExecutor, GraphNode,
    GraphNodeKind, GraphPlan, GraphRunOptions, GraphSession, NodeIo, PortRef, PortSchema,
    PortValue, RuntimeGraphExecutor,
};
use flow::manifest::Registry;
use flow::runtime::RunOptions;
use flow::security::{DataFlowPolicy, SensitivitySet};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

fn options(cwd: &Path) -> RunOptions {
    RunOptions {
        inputs: BTreeMap::new(),
        working_directory: cwd.to_path_buf(),
        cache_directory: None,
        trace: false,
        max_output_bytes: 1024 * 1024,
        command_timeout: Some(Duration::from_secs(5)),
        total_timeout: None,
    }
}

fn bytes() -> ValueSchema {
    ValueSchema {
        kind: OutputType::Bytes,
        nullable: false,
        fields: BTreeMap::new(),
        required: Vec::new(),
        items: None,
    }
}

fn int() -> ValueSchema {
    ValueSchema {
        kind: OutputType::Integer,
        nullable: false,
        fields: BTreeMap::new(),
        required: Vec::new(),
        items: None,
    }
}

fn record_node(
    id: &str,
    inputs: BTreeMap<String, PortSchema>,
    outputs: BTreeMap<String, PortSchema>,
) -> GraphNode {
    GraphNode {
        id: id.into(),
        kind: GraphNodeKind::Renderer {
            backend: "mock".into(),
        },
        inputs,
        outputs,
        events: BTreeMap::new(),
        line: None,
    }
}

struct MergeExecutor;

impl GraphExecutor for MergeExecutor {
    fn execute(&self, node: &GraphNode, io: &mut NodeIo, _: &GraphContext) -> Result<()> {
        match node.id.as_str() {
            "source" => {
                for (port, value) in [("left", 1), ("right", 2), ("left", 3), ("right", 4)] {
                    io.emit(port, PortValue::Value(Value::Integer(value)))?;
                }
            }
            "merge" => {
                let mut values = Vec::new();
                while let Some(envelope) = io.recv_any()? {
                    let PortValue::Value(value) = envelope.payload else {
                        return Err(anyhow!("unexpected bytes"));
                    };
                    values.push(value);
                }
                io.emit("joined", PortValue::Value(Value::List(values)))?;
            }
            _ => return Err(anyhow!("unknown test node")),
        }
        Ok(())
    }
}

#[test]
fn fanout_merges_record_ports_in_order_with_lossless_bounded_queues() {
    let mut plan = GraphPlan::new("merge");
    plan.add_node(record_node(
        "source",
        BTreeMap::new(),
        BTreeMap::from([
            ("left".into(), PortSchema::records(int())),
            ("right".into(), PortSchema::records(int())),
        ]),
    ))
    .unwrap();
    plan.add_node(record_node(
        "merge",
        BTreeMap::from([
            ("left".into(), PortSchema::records(int())),
            ("right".into(), PortSchema::records(int())),
        ]),
        BTreeMap::from([(
            "joined".into(),
            PortSchema::single(ValueSchema {
                kind: OutputType::List,
                nullable: false,
                fields: BTreeMap::new(),
                required: Vec::new(),
                items: Some(Box::new(int())),
            }),
        )]),
    ))
    .unwrap();
    for (source, target) in [("left", "left"), ("right", "right")] {
        plan.add_edge(EdgeSpec {
            from: PortRef::new("source", source),
            to: PortRef::new("merge", target),
            capacity: 1,
            policy: BufferPolicy::Lossless,
            line: None,
        });
    }
    let result = GraphSession::start(plan, Arc::new(MergeExecutor), GraphRunOptions::default())
        .unwrap()
        .run_to_completion()
        .unwrap();
    let joined = result
        .outputs
        .iter()
        .find(|event| event.port.port == "joined")
        .unwrap();
    assert_eq!(
        joined.payload,
        PortValue::Value(Value::List(
            vec![1, 2, 3, 4].into_iter().map(Value::Integer).collect()
        ))
    );
    assert_eq!(result.dropped_messages, 0);
}

struct OverflowExecutor;

impl GraphExecutor for OverflowExecutor {
    fn execute(&self, node: &GraphNode, io: &mut NodeIo, _: &GraphContext) -> Result<()> {
        if node.id == "source" {
            for value in 1..=3 {
                io.emit("records", PortValue::Value(Value::Integer(value)))?;
            }
        } else {
            thread::sleep(Duration::from_millis(50));
            while let Some(event) = io.recv("records")? {
                io.emit("seen", event.payload)?;
            }
        }
        Ok(())
    }
}

#[test]
fn explicit_latest_overflow_drops_old_backlog_without_blocking() {
    let mut plan = GraphPlan::new("overflow");
    plan.add_node(record_node(
        "source",
        BTreeMap::new(),
        BTreeMap::from([("records".into(), PortSchema::records(int()))]),
    ))
    .unwrap();
    plan.add_node(record_node(
        "sink",
        BTreeMap::from([("records".into(), PortSchema::records(int()))]),
        BTreeMap::from([("seen".into(), PortSchema::records(int()))]),
    ))
    .unwrap();
    plan.add_edge(EdgeSpec {
        from: PortRef::new("source", "records"),
        to: PortRef::new("sink", "records"),
        capacity: 1,
        policy: BufferPolicy::Latest,
        line: None,
    });
    let result = GraphSession::start(plan, Arc::new(OverflowExecutor), GraphRunOptions::default())
        .unwrap()
        .run_to_completion()
        .unwrap();
    let values = result
        .outputs
        .iter()
        .filter_map(|event| match event.payload {
            PortValue::Value(Value::Integer(value)) => Some(value),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(values, vec![3]);
    assert_eq!(result.dropped_messages, 2);
}

#[test]
fn cancellation_reconfigure_advances_revision_and_reaps_timer_workers() {
    let plan = GraphPlan::new("timer");
    let mut plan = plan;
    plan.add_node(GraphNode {
        id: "clock".into(),
        kind: GraphNodeKind::Timer { interval_ms: 2 },
        inputs: BTreeMap::new(),
        outputs: BTreeMap::new(),
        events: BTreeMap::from([("tick".into(), PortSchema::events(int()))]),
        line: None,
    })
    .unwrap();
    let graph_options = GraphRunOptions {
        generation: 9,
        revision: 4,
        ..GraphRunOptions::default()
    };
    let executor: Arc<dyn GraphExecutor> = Arc::new(flow::graph::RuntimeGraphExecutor::new(
        Registry::default(),
        options(Path::new(".")),
    ));
    let mut session =
        GraphSession::start(plan.clone(), executor.clone(), graph_options.clone()).unwrap();
    let first = session.recv_output().unwrap().unwrap();
    assert_eq!(first.generation, 9);
    assert_eq!(first.revision, 4);
    let next = session
        .reconfigure(plan.clone(), executor, graph_options)
        .unwrap();
    assert_eq!(next.context().generation, 10);
    assert_eq!(next.context().revision, 5);
    let mut next = next;
    let fresh = next.recv_output().unwrap().unwrap();
    assert_eq!(fresh.generation, 10);
    assert_eq!(fresh.revision, 5);
    next.cancel_and_join().unwrap();
}

struct BackendExecutor(&'static [u8]);

impl GraphExecutor for BackendExecutor {
    fn execute(&self, node: &GraphNode, io: &mut NodeIo, _: &GraphContext) -> Result<()> {
        if node.id == "producer" {
            io.emit("data", PortValue::Bytes(self.0.to_vec()))?;
        } else if let Some(event) = io.recv("data")? {
            io.emit("rendered", event.payload)?;
        }
        Ok(())
    }
}

#[test]
fn graph_logic_is_reused_when_backend_adapter_changes() {
    let mut plan = GraphPlan::new("backend_swap");
    plan.add_node(record_node(
        "producer",
        BTreeMap::new(),
        BTreeMap::from([("data".into(), PortSchema::single(bytes()))]),
    ))
    .unwrap();
    plan.add_node(record_node(
        "renderer",
        BTreeMap::from([("data".into(), PortSchema::single(bytes()))]),
        BTreeMap::from([("rendered".into(), PortSchema::single(bytes()))]),
    ))
    .unwrap();
    plan.add_edge(EdgeSpec::lossless(
        PortRef::new("producer", "data"),
        PortRef::new("renderer", "data"),
        1,
    ));
    let one = GraphSession::start(
        plan.clone(),
        Arc::new(BackendExecutor(b"native")),
        GraphRunOptions::default(),
    )
    .unwrap()
    .run_to_completion()
    .unwrap();
    let two = GraphSession::start(
        plan,
        Arc::new(BackendExecutor(b"remote")),
        GraphRunOptions::default(),
    )
    .unwrap()
    .run_to_completion()
    .unwrap();
    assert!(one
        .outputs
        .iter()
        .any(|event| event.payload == PortValue::Bytes(b"native".to_vec())));
    assert!(two
        .outputs
        .iter()
        .any(|event| event.payload == PortValue::Bytes(b"remote".to_vec())));
}

#[test]
fn injected_stdin_listener_is_supervised_and_reconfigure_drops_old_generation() {
    let registry = Registry::from_sources(&[] as &[&str]).unwrap();
    let plan = graph::parse(
        "graph stdin_feed\nlisten to stdin as source\nend graph\n",
        &registry,
    )
    .unwrap();

    let (old_source, old_sender) = ChannelStdinSource::channel(1).unwrap();
    let old_executor = Arc::new(
        RuntimeGraphExecutor::new(registry.clone(), options(Path::new(".")))
            .with_stdin_source(old_source),
    );
    let session =
        GraphSession::start(plan.clone(), old_executor, GraphRunOptions::default()).unwrap();
    let (new_source, new_sender) = ChannelStdinSource::channel(1).unwrap();
    let new_executor = Arc::new(
        RuntimeGraphExecutor::new(registry, options(Path::new("."))).with_stdin_source(new_source),
    );
    let next = session
        .reconfigure(plan, new_executor, GraphRunOptions::default())
        .unwrap();

    // The prior generation has been cancelled and joined before new workers
    // start. A late record on its now-unobserved source cannot appear in the
    // replacement session.
    assert!(old_sender.send(b"stale".to_vec()).is_err());
    new_sender.send(b"current".to_vec()).unwrap();
    drop(new_sender);
    let output = next.run_to_completion().unwrap();
    assert_eq!(output.generation, 2);
    assert_eq!(output.revision, 2);
    assert_eq!(output.outputs.len(), 1);
    assert_eq!(output.outputs[0].generation, 2);
    assert_eq!(output.outputs[0].revision, 2);
    assert_eq!(output.outputs[0].port.node, "source");
    assert_eq!(
        output.outputs[0].payload,
        PortValue::Bytes(b"current".to_vec())
    );
}

struct SecretFlowExecutor;

impl GraphExecutor for SecretFlowExecutor {
    fn execute(&self, node: &GraphNode, io: &mut NodeIo, _: &GraphContext) -> Result<()> {
        if node.id == "source" {
            io.emit(
                "secret",
                PortValue::Value(Value::String("classified".into())),
            )?;
        } else if let Some(event) = io.recv("input")? {
            io.emit("output", event.payload)?;
        }
        Ok(())
    }
}

#[test]
fn emitted_labels_preserve_secret_unknown_and_public_provenance() {
    let mut secret = PortSchema::single(ValueSchema {
        kind: OutputType::Text,
        nullable: false,
        fields: BTreeMap::new(),
        required: Vec::new(),
        items: None,
    });
    secret.sensitivity = flow::contracts::Sensitivity::Secret;
    let mut declared_public_in = PortSchema::single(ValueSchema {
        kind: OutputType::Text,
        nullable: false,
        fields: BTreeMap::new(),
        required: Vec::new(),
        items: None,
    });
    declared_public_in.sensitivity = flow::contracts::Sensitivity::Unknown;
    let mut declared_public_out = declared_public_in.clone();
    declared_public_out.sensitivity = flow::contracts::Sensitivity::Public;
    let mut plan = GraphPlan::new("runtime_taint");
    plan.add_node(record_node(
        "source",
        BTreeMap::new(),
        BTreeMap::from([("secret".into(), secret)]),
    ))
    .unwrap();
    plan.add_node(record_node(
        "formatter",
        BTreeMap::from([("input".into(), declared_public_in)]),
        BTreeMap::from([("output".into(), declared_public_out)]),
    ))
    .unwrap();
    plan.add_edge(EdgeSpec::lossless(
        PortRef::new("source", "secret"),
        PortRef::new("formatter", "input"),
        1,
    ));
    let result = GraphSession::start(
        plan,
        Arc::new(SecretFlowExecutor),
        GraphRunOptions::default(),
    )
    .unwrap()
    .run_to_completion()
    .unwrap();
    let output = result
        .outputs
        .iter()
        .find(|event| event.port.port == "output")
        .unwrap();
    assert!(output
        .sensitivity
        .contains(&flow::contracts::Sensitivity::Secret));
    assert!(output
        .sensitivity
        .contains(&flow::contracts::Sensitivity::Unknown));
    assert!(output
        .sensitivity
        .contains(&flow::contracts::Sensitivity::Public));
}

struct SecretProvenanceExecutor {
    applets: flow::graph::RuntimeGraphExecutor,
}

impl GraphExecutor for SecretProvenanceExecutor {
    fn execute(&self, node: &GraphNode, io: &mut NodeIo, context: &GraphContext) -> Result<()> {
        if node.id == "source" {
            io.include_sensitivity_provenance(&SensitivitySet::from_labels([
                flow::contracts::Sensitivity::Secret,
                flow::contracts::Sensitivity::Unknown,
            ]));
            io.emit("out", PortValue::Value(Value::String("classified".into())))?;
            return Ok(());
        }
        self.applets.execute(node, io, context)
    }
}

#[test]
fn runtime_provenance_union_denies_remote_before_invocation() {
    use flow::ast::StepKind;
    use flow::graph::{PortRef, PortSchema};
    use flow::parser;

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
    let parsed = parser::parse("run remote.sink\n").unwrap();
    let StepKind::Invoke { call, .. } = parsed.steps[0].kind.clone() else {
        panic!("remote invocation expected")
    };

    let mut source = PortSchema::single(ValueSchema {
        kind: OutputType::Text,
        nullable: false,
        fields: BTreeMap::new(),
        required: Vec::new(),
        items: None,
    });
    source.sensitivity = flow::contracts::Sensitivity::Unknown;
    let mut remote_input = source.clone();
    remote_input.sensitivity = flow::contracts::Sensitivity::Public;
    remote_input.required = true;
    let mut plan = GraphPlan::new("runtime_provenance_guard");
    plan.add_node(GraphNode {
        id: "source".into(),
        kind: GraphNodeKind::Renderer {
            backend: "fixture".into(),
        },
        inputs: BTreeMap::new(),
        outputs: BTreeMap::from([("out".into(), source)]),
        events: BTreeMap::new(),
        line: None,
    })
    .unwrap();
    plan.add_node(GraphNode::applet(
        "remote",
        call,
        BTreeMap::from([("payload".into(), remote_input)]),
        BTreeMap::new(),
    ))
    .unwrap();
    plan.add_edge(EdgeSpec::lossless(
        PortRef::new("source", "out"),
        PortRef::new("remote", "payload"),
        1,
    ));

    // Static declarations fit this policy; the fixture then reports an
    // additional Secret label at runtime, which must not be masked by Unknown.
    plan.validate_with_registry(&registry).unwrap();
    let runtime = flow::graph::RuntimeGraphExecutor::new(registry, options(Path::new(".")));
    let executor = Arc::new(SecretProvenanceExecutor { applets: runtime });
    let error = GraphSession::start(plan, executor, GraphRunOptions::default())
        .unwrap()
        .run_to_completion()
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("runtime data flow into remote applet"),
        "{error:#}"
    );
}

struct PanicExecutor;

impl GraphExecutor for PanicExecutor {
    fn execute(&self, _: &GraphNode, _: &mut NodeIo, _: &GraphContext) -> Result<()> {
        panic!("intentional graph worker panic")
    }
}

#[test]
fn panicking_worker_closes_terminal_and_recv_output_returns_promptly() {
    let mut plan = GraphPlan::new("panic");
    plan.add_node(record_node(
        "source",
        BTreeMap::new(),
        BTreeMap::from([("out".into(), PortSchema::single(bytes()))]),
    ))
    .unwrap();
    let mut session =
        GraphSession::start(plan, Arc::new(PanicExecutor), GraphRunOptions::default()).unwrap();
    let started = std::time::Instant::now();
    let error = session.recv_output().unwrap_err().to_string();
    assert!(
        error.contains("panicked") || error.contains("worker failed"),
        "{error}"
    );
    assert!(started.elapsed() < Duration::from_secs(1));
}

struct OversizeExecutor;

impl GraphExecutor for OversizeExecutor {
    fn execute(&self, _: &GraphNode, io: &mut NodeIo, _: &GraphContext) -> Result<()> {
        io.emit("out", PortValue::Bytes(vec![0; 8]))?;
        Ok(())
    }
}

#[test]
fn emitted_message_bytes_are_checked_before_queueing() {
    let mut plan = GraphPlan::new("message_limit");
    plan.add_node(record_node(
        "source",
        BTreeMap::new(),
        BTreeMap::from([("out".into(), PortSchema::single(bytes()))]),
    ))
    .unwrap();
    let options = GraphRunOptions {
        max_message_bytes: 4,
        ..GraphRunOptions::default()
    };
    let error = GraphSession::start(plan, Arc::new(OversizeExecutor), options)
        .unwrap()
        .run_to_completion()
        .unwrap_err()
        .to_string();
    assert!(error.contains("exceeds graph message limit"), "{error}");
}

struct EmitManyExecutor;

impl GraphExecutor for EmitManyExecutor {
    fn execute(&self, _: &GraphNode, io: &mut NodeIo, _: &GraphContext) -> Result<()> {
        for value in 1..=4 {
            io.emit("out", PortValue::Value(Value::Integer(value)))?;
        }
        Ok(())
    }
}

#[test]
fn finite_collection_has_total_event_cap() {
    let mut plan = GraphPlan::new("collection_limit");
    plan.add_node(record_node(
        "source",
        BTreeMap::new(),
        BTreeMap::from([("out".into(), PortSchema::records(int()))]),
    ))
    .unwrap();
    let options = GraphRunOptions {
        terminal_capacity: 1,
        max_collected_events: 2,
        ..GraphRunOptions::default()
    };
    let error = GraphSession::start(plan, Arc::new(EmitManyExecutor), options)
        .unwrap()
        .run_to_completion()
        .unwrap_err()
        .to_string();
    assert!(error.contains("collected output exceeded"), "{error}");
}

#[test]
fn actual_manifest_applets_connect_through_shared_invocation_runtime() {
    let cwd = tempfile::tempdir().unwrap();
    let registry = Registry::from_sources(&[
        r#"{"name":"mock.source","binary":"/bin/echo","arguments":[{"literal":"hello"}],"contract":{"version":1,"output":{"type":"bytes","encoding":"raw"}}}"#,
        r#"{"name":"mock.sink","binary":"/bin/cat","accepts_stdin":true,"arguments":[],"contract":{"version":1,"output":{"type":"bytes","encoding":"raw"}}}"#,
    ]).unwrap();
    let plan = graph::parse("graph actual\nrun mock.source as producer\nrun mock.sink as consumer\nbind producer.stdout to consumer.stdin with capacity 1 lossless\nend graph\n", &registry).unwrap();
    let result = graph::run(
        plan,
        &registry,
        &options(cwd.path()),
        Arc::new(AtomicBool::new(false)),
    )
    .unwrap();
    assert!(result
        .outputs
        .iter()
        .any(|event| event.port.node == "consumer"
            && event.port.port == "stdout"
            && event.payload == PortValue::Bytes(b"hello\n".to_vec())));
}

#[cfg(unix)]
#[test]
fn typed_native_jsonl_stream_emits_decoded_records() {
    let cwd = tempfile::tempdir().unwrap();
    let registry = Registry::from_sources(&[r#"{"name":"json.records","binary":"/usr/bin/printf","arguments":[{"literal":"{\"text\":\"tick\"}\\n"}],"contract":{"version":1,"output":{"type":"record","encoding":"json","fields":{"text":{"type":"text"}},"required":["text"]}}}"#]).unwrap();
    let plan = graph::parse(
        "graph typed_stream\nstream from json.records as records\nshow each line\nend graph\n",
        &registry,
    )
    .unwrap();
    let result = graph::run(
        plan,
        &registry,
        &options(cwd.path()),
        Arc::new(AtomicBool::new(false)),
    )
    .unwrap();
    assert!(result.outputs.iter().any(|event| {
        event.port.node == "records"
            && event.port.port == "stdout"
            && event.payload
                == PortValue::Value(Value::Record(BTreeMap::from([(
                    "text".into(),
                    Value::String("tick".into()),
                )])))
    }));
}
