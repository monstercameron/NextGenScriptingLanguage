//! Regression tests for issue #558: streamed graph terminal events.
//!
//! A graph with persistent nodes must stream one `execute.graph-event/1`
//! JSON line per terminal event instead of collecting a closing document,
//! so a timer graph stays live with flat memory and a closed reader stops
//! it. These tests pin the persistent classification, the streamed line
//! shape (including its single-line rendering), and the published schema.

#![cfg(feature = "graph")]

use flow::ast::Value;
use flow::graph::{graph_event_line, EventEnvelope, GraphPlan, PortRef, PortValue};
use flow::manifest::Registry;
use flow::security::SensitivitySet;

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
