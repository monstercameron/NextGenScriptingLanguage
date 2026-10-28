//! Regression tests for issue #558: streamed graph terminal events.
//!
//! A graph with persistent nodes must stream one `execute.graph-event/1`
//! JSON line per terminal event instead of collecting a closing document,
//! so a timer graph stays live with flat memory and a closed reader stops
//! it. These tests pin the persistent classification, the streamed line
//! shape (including its single-line rendering), full validation of emitted
//! lines against the published schema, the bounded per-event serialization
//! for bytes and escaped string/value payloads alike, the rule that
//! streaming bypasses the collection cap (past the old 100,000-event
//! failure point), the rule that a tick is visible within milliseconds
//! while its producer still runs and an early close reports cancellation
//! instead of EOF, and the rule that queued events are delivered before a
//! mid-stream or end-of-stream worker failure surfaces instead of a clean
//! end of stream.

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
use std::time::{Duration, Instant};

fn timer_plan() -> GraphPlan {
    let registry = Registry::from_sources(&[] as &[&str]).expect("empty registry parses");
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
    assert!(
        line.get("payload").is_some(),
        "payload must travel with the event"
    );
}

#[test]
fn streamed_line_renders_as_one_line_for_stdout_piping() {
    let event = sample_envelope();
    let line = graph_event_line("ticks", 2, 3, &event);
    let mut text = serde_json::to_string(&line).expect("event line serializes");
    assert!(!text.contains('\n'), "one event is one JSON line");
    text.push('\n');
    let parsed: serde_json::Value =
        serde_json::from_str(text.trim_end()).expect("piped line parses back to the same event");
    assert_eq!(parsed["generation"], 2);
    assert_eq!(parsed["revision"], 3);
}

#[test]
fn streamed_line_conforms_to_the_published_schema() {
    let schema_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("docs/schemas/graph-event.v1.schema.json");
    let schema_text = std::fs::read_to_string(&schema_path).expect("schema file exists");
    let schema: serde_json::Value = serde_json::from_str(&schema_text).expect("schema parses");
    let required = schema["required"]
        .as_array()
        .expect("schema lists required fields");
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
    assert_eq!(
        schema["properties"]["protocol"]["const"],
        "execute.graph-event/1"
    );
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
    mock_node_with_events(id, outputs, BTreeMap::new())
}

fn mock_node_with_events(
    id: &str,
    outputs: BTreeMap<String, PortSchema>,
    events: BTreeMap<String, PortSchema>,
) -> GraphNode {
    GraphNode {
        id: id.into(),
        kind: GraphNodeKind::Renderer {
            backend: "mock".into(),
        },
        inputs: BTreeMap::new(),
        outputs,
        events,
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

fn terminal_plan(name: &str) -> GraphPlan {
    let mut plan = GraphPlan::new(name);
    plan.add_node(mock_node(
        "source",
        BTreeMap::from([("out".into(), PortSchema::records(int_schema()))]),
    ))
    .expect("mock node is admitted");
    plan
}

/// Emits `count` terminal events and finishes cleanly. Used with a tiny
/// `max_collected_events` to prove the streaming path never consults the
/// collection cap that bounds the closing-document path.
struct EmitManyExecutor {
    count: usize,
}

impl GraphExecutor for EmitManyExecutor {
    fn execute(&self, node: &GraphNode, io: &mut NodeIo, _: &GraphContext) -> Result<()> {
        assert_eq!(node.id, "source");
        for index in 0..self.count {
            io.emit("out", PortValue::Value(Value::Integer(index as i64)))?;
        }
        Ok(())
    }
}

/// Emits one terminal event, then runs until cancelled. Receiving that event
/// while the worker is still alive proves ticks are visible without waiting
/// for the graph to finish.
struct EmitThenWaitExecutor;

impl GraphExecutor for EmitThenWaitExecutor {
    fn execute(&self, node: &GraphNode, io: &mut NodeIo, _: &GraphContext) -> Result<()> {
        assert_eq!(node.id, "source");
        io.emit("out", PortValue::Value(Value::Integer(1)))?;
        while !io.is_cancelled() {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        Ok(())
    }
}

/// A worker failure that lands mid-stream, after events the consumer has not
/// drained yet, must deliver those events first and then report the failure.
const MID_STREAM_FAILURE_MARKER: &str = "mid-stream worker failure marker";

struct BurstThenFailExecutor;

impl GraphExecutor for BurstThenFailExecutor {
    fn execute(&self, node: &GraphNode, io: &mut NodeIo, _: &GraphContext) -> Result<()> {
        assert_eq!(node.id, "source");
        io.emit("out", PortValue::Value(Value::Integer(0)))?;
        io.emit("out", PortValue::Value(Value::Integer(1)))?;
        Err(anyhow!(MID_STREAM_FAILURE_MARKER))
    }
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
    // Event-delivery ports live in the events namespace: declaring `tick` as
    // an output is rejected as `invalid_output_delivery`.
    plan.add_node(mock_node_with_events(
        "source",
        BTreeMap::new(),
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
    // cancellation immediately instead of hanging or reporting EOF. The
    // elapsed bound pins the head-close promptness acceptance: a close must
    // surface as a cancellation on a millisecond scale, not after a hang.
    let closed = Instant::now();
    session.cancel();
    let error = session
        .recv_output()
        .expect_err("a cancelled session must not report EOF");
    assert!(
        error.to_string().contains("cancelled"),
        "unexpected cancel error: {error:#}"
    );
    assert!(
        closed.elapsed() < Duration::from_secs(1),
        "head close took {:?}, expected a prompt cancellation",
        closed.elapsed()
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
        error
            .to_string()
            .contains("exceeds stream limit of 64 bytes"),
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

#[test]
fn streaming_is_not_bound_by_the_collection_cap() {
    // The closing-document path refuses more than `max_collected_events`
    // outputs; the streaming path must deliver them all without collecting.
    let mut options = GraphRunOptions::default();
    options.max_collected_events = 8;
    let mut session = GraphSession::start(
        terminal_plan("uncapped"),
        Arc::new(EmitManyExecutor { count: 32 }),
        options,
    )
    .expect("streaming session starts");
    let mut seen = 0;
    loop {
        match session
            .recv_output()
            .expect("streaming stays below no collection cap")
        {
            Some(event) => {
                assert_eq!(
                    event.payload,
                    PortValue::Value(Value::Integer(seen as i64)),
                    "streamed events arrive in emission order"
                );
                seen += 1;
            }
            None => break,
        }
    }
    assert_eq!(seen, 32, "every emitted event streams, cap or no cap");
    session
        .cancel_and_join()
        .expect("a clean finish joins quietly");
}

#[test]
fn tick_is_visible_while_the_producer_is_still_running() {
    let mut session = GraphSession::start(
        terminal_plan("live"),
        Arc::new(EmitThenWaitExecutor),
        GraphRunOptions::default(),
    )
    .expect("live session starts");
    // The producer never finishes on its own, so receiving here proves the
    // event was visible mid-run rather than at shutdown.
    let first = session
        .recv_output()
        .expect("receive works")
        .expect("tick arrives before the producer finishes");
    assert_eq!(
        first.payload,
        PortValue::Value(Value::Integer(1)),
        "the live tick carries its payload"
    );
    // An early close then stops the graph: the next receive reports the
    // cancellation instead of hanging or reporting EOF.
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
fn mid_stream_failure_delivers_queued_events_then_surfaces() {
    let mut session = GraphSession::start(
        terminal_plan("burst"),
        Arc::new(BurstThenFailExecutor),
        GraphRunOptions::default(),
    )
    .expect("bursting session starts");
    // The worker may already have failed by the time the first receive runs;
    // either way the two queued events must arrive before the failure does.
    for expected in [0i64, 1] {
        let event = session
            .recv_output()
            .expect("receive works")
            .expect("queued event arrives before the failure");
        assert_eq!(
            event.payload,
            PortValue::Value(Value::Integer(expected)),
            "queued events are not discarded by the failure"
        );
    }
    let error = session
        .recv_output()
        .expect_err("the mid-stream failure must surface, not a clean EOF");
    assert!(
        error.to_string().contains(MID_STREAM_FAILURE_MARKER),
        "unexpected stream error: {error:#}"
    );
    let joined = session
        .cancel_and_join()
        .expect_err("cancel_and_join must propagate the mid-stream failure");
    assert!(
        joined.to_string().contains(MID_STREAM_FAILURE_MARKER),
        "unexpected join error: {joined:#}"
    );
}

fn value_envelope(payload: Value) -> EventEnvelope {
    EventEnvelope {
        port: PortRef::new("ticker", "tick"),
        input: None,
        generation: 1,
        revision: 1,
        sequence: 1,
        sensitivity: SensitivitySet::empty(),
        remote: None,
        payload: PortValue::Value(payload),
    }
}

fn load_event_schema() -> serde_json::Value {
    let schema_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("docs/schemas/graph-event.v1.schema.json");
    let schema_text = std::fs::read_to_string(&schema_path).expect("schema file exists");
    serde_json::from_str(&schema_text).expect("schema parses")
}

/// Validate one emitted line against the published schema, deriving every
/// expectation from the schema file itself so the check tracks schema edits.
/// Returns the reason when the line does not validate.
fn validate_graph_event_line(
    schema: &serde_json::Value,
    line: &serde_json::Value,
) -> Result<(), String> {
    let object = line
        .as_object()
        .ok_or_else(|| "event line is not an object".to_owned())?;
    let properties = schema
        .get("properties")
        .and_then(|properties| properties.as_object())
        .ok_or_else(|| "schema has no properties".to_owned())?;
    for key in object.keys() {
        if !properties.contains_key(key) {
            return Err(format!("event line carries undeclared field {key:?}"));
        }
    }
    let required = schema
        .get("required")
        .and_then(|required| required.as_array())
        .ok_or_else(|| "schema lists no required fields".to_owned())?;
    for field in required {
        let name = field
            .as_str()
            .ok_or_else(|| "required entry is not a string".to_owned())?;
        if !object.contains_key(name) {
            return Err(format!("event line misses required field {name:?}"));
        }
    }
    let constant = |field: &str| schema["properties"][field]["const"].clone();
    if line["protocol"] != constant("protocol") {
        return Err("event line names the wrong protocol".to_owned());
    }
    if !line["graph"].is_string() {
        return Err("event graph is not a string".to_owned());
    }
    for field in ["generation", "revision", "sequence"] {
        let minimum = schema["properties"][field]["minimum"]
            .as_u64()
            .ok_or_else(|| format!("schema sets no minimum for {field}"))?;
        let value = line[field]
            .as_u64()
            .ok_or_else(|| format!("event {field} is not an integer"))?;
        if value < minimum {
            return Err(format!("event {field} {value} is below the schema minimum"));
        }
    }
    let port = line["port"]
        .as_object()
        .ok_or_else(|| "event port is not an object".to_owned())?;
    for field in ["node", "port"] {
        if !port.get(field).is_some_and(|name| name.is_string()) {
            return Err(format!("event port misses string field {field:?}"));
        }
    }
    if port.len() != 2 {
        return Err("event port carries undeclared fields".to_owned());
    }
    let allowed: Vec<&str> = schema["properties"]["sensitivity"]["items"]["enum"]
        .as_array()
        .ok_or_else(|| "schema lists no sensitivity labels".to_owned())?
        .iter()
        .filter_map(|label| label.as_str())
        .collect();
    let sensitivity = line["sensitivity"]
        .as_array()
        .ok_or_else(|| "event sensitivity is not an array".to_owned())?;
    for label in sensitivity {
        let name = label
            .as_str()
            .ok_or_else(|| "sensitivity label is not a string".to_owned())?;
        if !allowed.contains(&name) {
            return Err(format!(
                "sensitivity label {name:?} is outside the schema enum"
            ));
        }
    }
    fn matches_bytes(payload: &serde_json::Value) -> bool {
        payload.as_object().is_some_and(|object| {
            object.len() == 2
                && object.get("kind").is_some_and(|kind| kind == "bytes")
                && object.get("value").is_some_and(|value| {
                    value.as_array().is_some_and(|items| {
                        items
                            .iter()
                            .all(|item| item.as_u64().is_some_and(|byte| byte <= 255))
                    })
                })
        })
    }
    fn matches_value(payload: &serde_json::Value) -> bool {
        payload.as_object().is_some_and(|object| {
            object.len() == 2
                && object.get("kind").is_some_and(|kind| kind == "value")
                && object.contains_key("value")
        })
    }
    let variants = [
        &matches_bytes as &dyn Fn(&serde_json::Value) -> bool,
        &matches_value,
    ];
    if variants
        .iter()
        .filter(|match_one| match_one(&line["payload"]))
        .count()
        != 1
    {
        return Err("event payload matches no schema variant exactly once".to_owned());
    }
    Ok(())
}

#[test]
fn emitted_lines_validate_against_the_full_schema() {
    let schema = load_event_schema();
    // Every payload shape the stream can carry must validate: an integer
    // value, a bytes payload, a string with characters JSON must escape, and
    // a nested record mixing lists, strings, and nulls.
    let nested = Value::Record(BTreeMap::from([
        (
            "items".into(),
            Value::List(vec![Value::Integer(1), Value::Null]),
        ),
        (
            "note".into(),
            Value::String("line\nfeed \"quoted\" \\ backslash".into()),
        ),
    ]));
    let cases = [
        sample_envelope(),
        bytes_envelope(vec![0, 255]),
        value_envelope(Value::String("é🦀\n\t\0\"\\".into())),
        value_envelope(nested),
    ];
    for event in &cases {
        let line = graph_event_line("ticks", 1, 1, event);
        validate_graph_event_line(&schema, &line)
            .expect("emitted line validates against the published schema");
        let text = serde_json::to_string(&line).expect("event line serializes");
        let reparsed: serde_json::Value =
            serde_json::from_str(&text).expect("piped line parses back");
        validate_graph_event_line(&schema, &reparsed).expect("the piped rendering validates too");
        assert!(!text.contains('\n'), "one event is one JSON line");
    }
}

#[test]
fn schema_validation_rejects_misshapen_lines() {
    let schema = load_event_schema();
    let valid = graph_event_line("ticks", 1, 1, &sample_envelope());
    // The validator is not vacuous: a missing field, an extra field, an
    // out-of-range byte, a wrong protocol, an unknown sensitivity label, and
    // a payload matching neither variant must each fail with a reason.
    let mut missing = valid.clone();
    missing.as_object_mut().unwrap().remove("payload");
    assert!(
        validate_graph_event_line(&schema, &missing).is_err(),
        "a line without its payload must not validate"
    );
    let mut extra = valid.clone();
    extra.as_object_mut().unwrap().insert(
        "tomorrow".to_owned(),
        serde_json::Value::String("never".into()),
    );
    assert!(
        validate_graph_event_line(&schema, &extra).is_err(),
        "a line with an undeclared field must not validate"
    );
    let mut bad_payload = valid.clone();
    bad_payload["payload"] = serde_json::json!({"kind": "bytes", "value": [256]});
    assert!(
        validate_graph_event_line(&schema, &bad_payload).is_err(),
        "a byte outside 0..=255 must not validate"
    );
    let mut bad_protocol = valid.clone();
    bad_protocol["protocol"] = serde_json::json!("execute.graph-run/1");
    assert!(
        validate_graph_event_line(&schema, &bad_protocol).is_err(),
        "a closing-document protocol must not validate as a stream event"
    );
    let mut bad_label = valid.clone();
    bad_label["sensitivity"] = serde_json::json!(["public", "no-such-label"]);
    assert!(
        validate_graph_event_line(&schema, &bad_label).is_err(),
        "a sensitivity label outside the schema enum must not validate"
    );
    let mut bad_kind = valid.clone();
    bad_kind["payload"] = serde_json::json!({"kind": "text", "value": "hello"});
    assert!(
        validate_graph_event_line(&schema, &bad_kind).is_err(),
        "a payload matching neither the bytes nor the value variant must not validate"
    );
}

#[test]
fn large_string_and_nested_value_payloads_fit_the_derived_bound() {
    // JSON escaping expands control characters to six bytes each
    // (`\u0001`), so a string payload's serialized form can far exceed its
    // raw length. The per-event bound derives from the admitted serialized
    // size, not the raw length, so escaping can never overflow it.
    let heavy = "A\u{1}é🦀\"\\\n\t\0".repeat(2000);
    assert!(
        heavy.len() < 64 * 1024,
        "the raw string stays small while its escaping does not"
    );
    let string_event = value_envelope(Value::String(heavy));
    let max_message_bytes = 128 * 1024;
    let limit = stream_event_byte_limit(max_message_bytes);
    let text = graph_event_line_json("ticks", 1, 1, &string_event, limit)
        .expect("an admitted string payload fits its derived bound");
    assert!(text.len() <= limit, "rendered line stays within its bound");
    assert!(!text.contains('\n'), "escaped newlines stay on one line");
    // A nested value mixes every JSON shape; it fits for the same reason.
    let nested = Value::Record(BTreeMap::from([
        (
            "runs".into(),
            Value::List((0..500).map(Value::Integer).collect()),
        ),
        (
            "note".into(),
            Value::String("tab\there \"quoted\" \\ end".repeat(50)),
        ),
    ]));
    let nested_event = value_envelope(nested);
    let text = graph_event_line_json("ticks", 1, 1, &nested_event, limit)
        .expect("an admitted nested payload fits its derived bound");
    assert!(text.len() <= limit, "rendered line stays within its bound");
    assert!(!text.contains('\n'), "one event is one JSON line");
    // And a payload that cannot fit is still refused before unbounded
    // allocation, for value payloads just as for bytes payloads.
    let error = graph_event_line_json("ticks", 1, 1, &string_event, 64)
        .expect_err("a large string cannot fit a 64-byte line");
    assert!(
        error
            .to_string()
            .contains("exceeds stream limit of 64 bytes"),
        "unexpected bound error: {error:#}"
    );
}

#[test]
fn first_tick_arrives_with_millisecond_latency() {
    // The producer never finishes on its own, so receiving here proves the
    // tick was visible mid-run; the 50 ms bound is the issue #558 acceptance
    // for first-tick visibility (production ticks land in ~2 ms poll steps).
    let started = Instant::now();
    let mut session = GraphSession::start(
        terminal_plan("live-latency"),
        Arc::new(EmitThenWaitExecutor),
        GraphRunOptions::default(),
    )
    .expect("live session starts");
    let first = session
        .recv_output()
        .expect("receive works")
        .expect("tick arrives before the producer finishes");
    let elapsed = started.elapsed();
    assert_eq!(
        first.payload,
        PortValue::Value(Value::Integer(1)),
        "the live tick carries its payload"
    );
    assert!(
        elapsed < Duration::from_millis(50),
        "tick took {elapsed:?}, expected visibility within 50 ms"
    );
    session.cancel();
    session
        .cancel_and_join()
        .expect("cancellation fallout is not a worker failure");
}

#[test]
fn streaming_outlives_the_old_collection_cap() {
    // The closing-document path fails past 100,000 collected events; the
    // streaming path holds nothing and must flow past that point with a tiny
    // collection cap configured. Each event is small, so this stays fast
    // while proving the old limit no longer applies to a live stream.
    let mut options = GraphRunOptions::default();
    options.max_collected_events = 8;
    let mut session = GraphSession::start(
        terminal_plan("sustained"),
        Arc::new(EmitManyExecutor { count: 100_001 }),
        options,
    )
    .expect("streaming session starts");
    let mut seen = 0;
    loop {
        match session
            .recv_output()
            .expect("streaming stays below no collection cap")
        {
            Some(event) => {
                assert_eq!(
                    event.payload,
                    PortValue::Value(Value::Integer(seen as i64)),
                    "streamed events arrive in emission order"
                );
                seen += 1;
            }
            None => break,
        }
    }
    assert_eq!(seen, 100_001, "every event streams past the old cap");
    session
        .cancel_and_join()
        .expect("a clean finish joins quietly");
}

/// Resident set size in bytes, when the platform exposes it. Linux parses it
/// from `/proc/self/status`; other platforms skip the flat-RSS assertion but
/// still stream the million events.
#[cfg(target_os = "linux")]
fn rss_bytes() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            let kibibytes: u64 = rest.split_whitespace().next()?.parse().ok()?;
            return Some(kibibytes * 1024);
        }
    }
    None
}

#[cfg(not(target_os = "linux"))]
fn rss_bytes() -> Option<u64> {
    None
}

#[test]
fn streaming_holds_flat_rss_past_one_million_events() {
    // Issue #558 acceptance: the stream holds nothing, so one million small
    // events must flow past the old 100,000-event cap with flat memory. A
    // collected document for this stream would retain every envelope (well
    // over 100 MB); stream growth must stay well below that.
    let mut options = GraphRunOptions::default();
    options.max_collected_events = 8;
    let before = rss_bytes();
    let mut session = GraphSession::start(
        terminal_plan("million"),
        Arc::new(EmitManyExecutor { count: 1_000_000 }),
        options,
    )
    .expect("streaming session starts");
    let mut seen = 0usize;
    loop {
        match session.recv_output().expect("streaming holds nothing back") {
            Some(event) => {
                assert_eq!(
                    event.payload,
                    PortValue::Value(Value::Integer(seen as i64)),
                    "streamed events arrive in emission order"
                );
                seen += 1;
            }
            None => break,
        }
    }
    assert_eq!(seen, 1_000_000, "every event streams past the old cap");
    session
        .cancel_and_join()
        .expect("a clean finish joins quietly");
    if let (Some(before), Some(after)) = (before, rss_bytes()) {
        let growth = after.saturating_sub(before);
        assert!(
            growth < 64 * 1024 * 1024,
            "streaming 1M events grew RSS by {growth} bytes, expected flat memory"
        );
    }
}
