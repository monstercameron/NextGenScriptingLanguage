//! Regression tests for issue #558: streamed graph terminal events.
//!
//! A graph with persistent nodes must stream one `execute.graph-event/1`
//! JSON line per terminal event instead of collecting a closing document,
//! so a timer graph stays live with flat memory and a closed reader stops
//! it. These tests pin the persistent classification, the streamed line
//! shape (including its single-line rendering), the published schema, the
//! bounded per-event serialization, and the rule that a worker failure at
//! EOF surfaces instead of a clean end of stream.

#![cfg(feature = "graph")]

use anyhow::{anyhow, Result};
use flow::ast::Value;
use flow::contracts::{OutputType, ValueSchema};
use flow::graph::{
    graph_event_line, graph_event_line_json, stream_event_byte_limit, EventEnvelope, GraphContext,
    GraphExecutor, GraphNode, GraphNodeKind, GraphPlan, GraphRunOptions, GraphSession, NodeIo,
    PortRef, PortSchema, PortValue,
};
use flow::manifest::Registry;
use flow::security::SensitivitySet;
use std::collections::BTreeMap;
use std::sync::Arc;

fn timer_plan() -> GraphPlan {
    let registry = Registry::from_sources(&[]).expect("empty registry parses");
    flow::graph::parse(
        "graph ticks\nevery 10 milliseconds as ticker\nend graph\n",
        &registry,
    )
    .expect("timer graph parses without applet manifests")
}

fn sample_envelope() -> EventEnvelope {
    EventEnvelope {
        port: PortRef::new("ticker", "tick"),
        input: None,
        generation: 1,
        revision: 1,
        sequence: 42,
        sensitivity: SensitivitySet::empty(),
        remote: None,
        payload: PortValue::Value(Value::Integer(42)),
    }
}

#[test]
fn timer_graph_counts_as_persistent_while_an_empty_plan_does_not() {
    assert!(
        timer_plan().has_persistent_nodes(),
        "a timer node runs indefinitely, so the graph must stream"
    );
    assert!(
        !GraphPlan::new("empty").has_persistent_nodes(),
        "a graph without persistent nodes keeps the closing document"
    );
}

#[test]
fn streamed_line_carries_the_graph_event_protocol_and_arrival_metadata() {
    let event = sample_envelope();
    let line = graph_event_line("ticks", 1, 1, &event);
    assert_eq!(line["protocol"], "execute.graph-event/1");
    assert_eq!(line["graph"], "ticks");
    assert_eq!(line["generation"], 1);
    assert_eq!(line["revision"], 1);
    assert_eq!(line["sequence"], 42);
    assert_eq!(line["port"]["node"], "ticker");
    assert_eq!(line["port"]["port"], "tick");
    assert!(line.get("payload").is_some(), "payload must travel with the event");
}

#[test]
fn streamed_line_renders_as_one_line_for_stdout_piping() {
    let event = sample_envelope();
    let line = graph_event_line("ticks", 2, 3, &event);
    let mut text = serde_json::to_string(&line).expect("event line serializes");
    assert!(!text.contains('\n'), "one event is one JSON line");
    text.push('\n');
    let parsed: serde_json::Value = serde_json::from_str(text.trim_end())
        .expect("piped line parses back to the same event");
    assert_eq!(parsed["generation"], 2);
    assert_eq!(parsed["revision"], 3);
}

#[test]
fn streamed_line_conforms_to_the_published_schema() {
    let schema_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("docs/schemas/graph-event.v1.schema.json");
    let schema_text = std::fs::read_to_string(&schema_path).expect("schema file exists");
    let schema: serde_json::Value = serde_json::from_str(&schema_text).expect("schema parses");
    let required = schema["required"].as_array().expect("schema lists required fields");
    let names: Vec<&str> = required.iter().filter_map(|name| name.as_str()).collect();
    for field in [
        "protocol",
        "graph",
        "generation",
        "revision",
        "port",
        "sequence",
        "sensitivity",
        "payload",
    ] {
        assert!(names.contains(&field), "schema requires {field}");
    }
    assert_eq!(schema["properties"]["protocol"]["const"], "execute.graph-event/1");
    let line = graph_event_line("ticks", 1, 1, &sample_envelope());
    let object = line.as_object().expect("event line is an object");
    for field in &names {
        assert!(object.contains_key(*field), "event line carries {field}");
    }
}

fn int_schema() -> ValueSchema {
    ValueSchema {
        min_items: None,
        scalar_only: false,
        additional_properties: None,
        min_properties: None,
        max_properties: None,
        nonempty: false,
        kind: OutputType::Integer,
        nullable: false,
        items: None,
        fields: BTreeMap::new(),
        required: Vec::new(),
        enum_values: None,
        single_line: false,
        minimum: None,
        pattern: None,
    }
}

fn mock_node(id: &str, outputs: BTreeMap<String, PortSchema>) -> GraphNode {
    GraphNode {
        id: id.into(),
        kind: GraphNodeKind::Renderer {
            backend: "mock".into(),
        },
        inputs: BTreeMap::new(),
        outputs,
        events: BTreeMap::new(),
        line: None,
    }
}

fn bytes_envelope(bytes: Vec<u8>) -> EventEnvelope {
    EventEnvelope {
        port: PortRef::new("ticker", "tick"),
        input: None,
        generation: 1,
        revision: 1,
        sequence: 1,
        sensitivity: SensitivitySet::empty(),
        remote: None,
        payload: PortValue::Bytes(bytes),
    }
}

/// A worker failure that lands after the last terminal event was drained
/// must surface as an error, never as a clean end of stream.
const LATE_FAILURE_MARKER: &str = "late worker failure at EOF marker";

struct FailAfterEmitExecutor;

impl GraphExecutor for FailAfterEmitExecutor {
    fn execute(&self, node: &GraphNode, io: &mut NodeIo, _: &GraphContext) -> Result<()> {
        assert_eq!(node.id, "source");
        io.emit("out", PortValue::Value(Value::Integer(7)))?;
        Err(anyhow!(LATE_FAILURE_MARKER))
    }
}

fn failing_plan() -> GraphPlan {
    let mut plan = GraphPlan::new("failing");
    plan.add_node(mock_node(
        "source",
        BTreeMap::from([("out".into(), PortSchema::records(int_schema()))]),
    ))
    .expect("mock node is admitted");
    plan
}

#[test]
fn late_worker_failure_after_last_event_surfaces_instead_of_eof() {
    let mut session = GraphSession::start(
        failing_plan(),
        Arc::new(FailAfterEmitExecutor),
        GraphRunOptions::default(),
    )
    .expect("failing session starts");
    let first = session
        .recv_output()
        .expect("receive works")
        .expect("one terminal event arrives before the failure");
    assert_eq!(first.payload, PortValue::Value(Value::Integer(7)));
    let error = session
        .recv_output()
        .expect_err("a late worker failure must surface, not a clean EOF");
    assert!(
        error.to_string().contains(LATE_FAILURE_MARKER),
        "unexpected stream error: {error:#}"
    );
    // The recorded failure also survives the final join, so a caller that
    // already saw EOF still learns about the late failure.
    let joined = session
        .cancel_and_join()
        .expect_err("cancel_and_join must propagate the late failure");
    assert!(
        joined.to_string().contains(LATE_FAILURE_MARKER),
        "unexpected join error: {joined:#}"
    );
}

struct WaitForCancelExecutor;

impl GraphExecutor for WaitForCancelExecutor {
    fn execute(&self, _: &GraphNode, io: &mut NodeIo, _: &GraphContext) -> Result<()> {
        while !io.is_cancelled() {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        Ok(())
    }
}

#[test]
fn head_close_cancels_session_promptly_without_worker_error() {
    let mut plan = GraphPlan::new("pausable");
    plan.add_node(mock_node(
        "source",
        BTreeMap::from([("tick".into(), PortSchema::events(int_schema()))]),
    ))
    .expect("mock node is admitted");
    let mut session = GraphSession::start(
        plan,
        Arc::new(WaitForCancelExecutor),
        GraphRunOptions::default(),
    )
    .expect("pausable session starts");
    assert_eq!(
        session.max_stream_event_bytes(),
        stream_event_byte_limit(GraphRunOptions::default().max_message_bytes),
        "the session limit is the derived per-event bound"
    );
    // A closed pipe reader cancels the session; the next receive reports the
    // cancellation immediately instead of hanging or reporting EOF.
    session.cancel();
    let error = session
        .recv_output()
        .expect_err("a cancelled session must not report EOF");
    assert!(
        error.to_string().contains("cancelled"),
        "unexpected cancel error: {error:#}"
    );
    session
        .cancel_and_join()
        .expect("cancellation fallout is not a worker failure");
}

#[test]
fn stream_event_byte_limit_derives_from_the_message_bound() {
    assert_eq!(stream_event_byte_limit(1024), 1024 * 4 + 4096);
    assert_eq!(
        stream_event_byte_limit(8 * 1024 * 1024),
        8 * 1024 * 1024 * 4 + 4096
    );
    assert_eq!(stream_event_byte_limit(usize::MAX), usize::MAX);
}

#[test]
fn oversize_event_line_is_rejected_before_unbounded_allocation() {
    let event = bytes_envelope(vec![255; 300]);
    let error = graph_event_line_json("ticks", 1, 1, &event, 64)
        .expect_err("a 300-byte payload cannot fit a 64-byte line");
    assert!(
        error.to_string().contains("exceeds stream limit of 64 bytes"),
        "unexpected bound error: {error:#}"
    );
    // Every payload admitted by `emit` still fits its derived bound: the
    // bound is not an arbitrary weakening of the message limit.
    let admitted = bytes_envelope(vec![0xAB; 1024]);
    let limit = stream_event_byte_limit(1024);
    let text = graph_event_line_json("ticks", 1, 1, &admitted, limit)
        .expect("an admitted payload must fit its derived bound");
    assert!(text.len() <= limit, "rendered line stays within its bound");
    assert!(!text.contains('\n'), "one event is one JSON line");
}

#[test]
fn streamed_payload_variants_match_the_schema_bytes_and_value_shapes() {
    let schema_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("docs/schemas/graph-event.v1.schema.json");
    let schema_text = std::fs::read_to_string(&schema_path).expect("schema file exists");
    let schema: serde_json::Value = serde_json::from_str(&schema_text).expect("schema parses");
    let variants = schema["properties"]["payload"]["oneOf"]
        .as_array()
        .expect("payload schema lists its bytes/value variants");
    assert_eq!(variants.len(), 2, "payload stays a two-variant union");
    assert_eq!(variants[0]["properties"]["kind"]["const"], "bytes");
    assert_eq!(variants[0]["properties"]["value"]["items"]["minimum"], 0);
    assert_eq!(variants[0]["properties"]["value"]["items"]["maximum"], 255);
    assert_eq!(variants[1]["properties"]["kind"]["const"], "value");

    let bytes_line = graph_event_line("ticks", 1, 1, &bytes_envelope(vec![0, 255]));
    assert_eq!(bytes_line["payload"]["kind"], "bytes");
    let items = bytes_line["payload"]["value"]
        .as_array()
        .expect("bytes payload renders as an integer array");
    assert_eq!(items.len(), 2);
    for item in items {
        let byte = item.as_u64().expect("bytes items are integers");
        assert!(byte <= 255, "bytes items fit the schema range");
    }
    let value_line = graph_event_line("ticks", 1, 1, &sample_envelope());
    assert_eq!(value_line["payload"]["kind"], "value");
    assert_eq!(value_line["payload"]["value"], 42);
}
