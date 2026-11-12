#![cfg(feature = "graph")]
use flow::ast::Value;
use flow::contracts::{OutputType, ValueSchema};
use flow::graph::{
    GraphExecutor, GraphNode, GraphNodeKind, GraphPlan, GraphRunOptions, GraphSession, PortSchema,
    PortValue, RuntimeGraphExecutor,
};
use flow::manifest::Registry;
use flow::runtime::RunOptions;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

fn leaf(kind: OutputType) -> ValueSchema {
    ValueSchema {
        min_items: None,
        scalar_only: false,
        additional_properties: None,
        min_properties: None,
        max_properties: None,
        nonempty: false,
        kind,
        nullable: false,
        fields: BTreeMap::new(),
        required: Vec::new(),
        items: None,
        enum_values: None,
        single_line: false,
        minimum: None,
        pattern: None,
    }
}

fn timer_plan(events: BTreeMap<String, PortSchema>) -> GraphPlan {
    let mut plan = GraphPlan::new("timer_ports");
    plan.add_node(GraphNode {
        id: "clock".into(),
        kind: GraphNodeKind::Timer { interval_ms: 1 },
        inputs: BTreeMap::new(),
        outputs: BTreeMap::new(),
        events,
        line: None,
    })
    .unwrap();
    plan
}

fn diagnostic_messages(plan: &GraphPlan) -> Vec<String> {
    plan.diagnostics()
        .into_iter()
        .filter(|diagnostic| diagnostic.code == "invalid_timer_event_port")
        .map(|diagnostic| diagnostic.message)
        .collect()
}

#[test]
fn timer_without_event_port_is_rejected_with_actionable_diagnostic() {
    let plan = timer_plan(BTreeMap::new());
    let messages = diagnostic_messages(&plan);
    assert_eq!(messages.len(), 1, "{messages:?}");
    assert!(messages[0].contains("no event port"), "{messages:?}");
    assert!(messages[0].contains("integer event port"), "{messages:?}");
    assert!(plan.validate().is_err());
}

#[test]
fn timer_with_text_event_port_is_rejected() {
    let plan = timer_plan(BTreeMap::from([(
        "tick".into(),
        PortSchema::events(leaf(OutputType::Bytes)),
    )]));
    let messages = diagnostic_messages(&plan);
    assert_eq!(messages.len(), 1, "{messages:?}");
    assert!(messages[0].contains("\"tick\""), "{messages:?}");
    assert!(messages[0].contains("integer schema"), "{messages:?}");
    assert!(plan.validate().is_err());
}

#[test]
fn timer_with_multiple_event_ports_is_rejected() {
    let plan = timer_plan(BTreeMap::from([
        (
            "first".into(),
            PortSchema::events(leaf(OutputType::Integer)),
        ),
        (
            "second".into(),
            PortSchema::events(leaf(OutputType::Integer)),
        ),
    ]));
    let messages = diagnostic_messages(&plan);
    assert_eq!(messages.len(), 1, "{messages:?}");
    assert!(messages[0].contains("2 event ports"), "{messages:?}");
    assert!(plan.validate().is_err());
}

#[test]
fn invalid_timer_is_refused_before_the_graph_starts() {
    let plan = timer_plan(BTreeMap::new());
    let executor: Arc<dyn GraphExecutor> = Arc::new(RuntimeGraphExecutor::new(
        Registry::default(),
        RunOptions::new(Path::new(".").to_path_buf()),
    ));
    assert!(GraphSession::start(plan, executor, GraphRunOptions::default()).is_err());
}

#[test]
fn dsl_generated_timer_validates_and_emits_integer_ticks() {
    let registry = Registry::from_sources(&[] as &[&str]).expect("empty registry parses");
    let plan = flow::graph::parse(
        "graph ticks\nevery 1 millisecond as ticker\nend graph\n",
        &registry,
    )
    .expect("timer graph parses");
    assert!(diagnostic_messages(&plan).is_empty());
    plan.validate().expect("DSL timer validates");

    let executor: Arc<dyn GraphExecutor> = Arc::new(RuntimeGraphExecutor::new(
        Registry::default(),
        RunOptions {
            command_timeout: Some(Duration::from_secs(5)),
            ..RunOptions::new(Path::new(".").to_path_buf())
        },
    ));
    let mut session = GraphSession::start(plan, executor, GraphRunOptions::default())
        .expect("DSL timer graph starts");
    let envelope = session
        .recv_output()
        .expect("receive tick")
        .expect("tick output");
    assert_eq!(envelope.port.node, "ticker");
    assert_eq!(envelope.port.port, "tick");
    assert!(matches!(
        envelope.payload,
        PortValue::Value(Value::Integer(1))
    ));
    session.cancel_and_join().unwrap();
}
