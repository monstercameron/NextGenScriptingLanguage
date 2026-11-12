#![cfg(feature = "graph")]
//! #561: a lossless subscriber that stops reading must not hold back its
//! siblings on the same output port. Each edge has its own queue, so the
//! producer waits only once the slow edge is full. These tests use in-process
//! executors only, so they need no external binary and no manifests folder.

use anyhow::Result;
use flow::ast::Value;
use flow::contracts::{OutputType, ValueSchema};
use flow::graph::{
    EdgeSpec, GraphContext, GraphExecutor, GraphNode, GraphNodeKind, GraphPlan, GraphRunOptions,
    GraphSession, NodeIo, PortRef, PortSchema, PortValue,
};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

fn text_schema() -> ValueSchema {
    ValueSchema {
        min_items: None,
        scalar_only: false,
        additional_properties: None,
        min_properties: None,
        max_properties: None,
        nonempty: false,
        kind: OutputType::Text,
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

fn node(id: &str, inputs: &[&str], outputs: &[&str]) -> GraphNode {
    GraphNode {
        id: id.into(),
        kind: GraphNodeKind::Renderer {
            backend: "test".into(),
        },
        inputs: inputs
            .iter()
            .map(|name| (name.to_string(), PortSchema::records(text_schema())))
            .collect(),
        outputs: outputs
            .iter()
            .map(|name| (name.to_string(), PortSchema::records(text_schema())))
            .collect(),
        events: BTreeMap::new(),
        line: None,
    }
}

/// What a scripted node does with its input and output.
type Behaviour = dyn Fn(&GraphNode, &mut NodeIo) -> Result<()> + Send + Sync;

/// An in-process executor whose behaviour is supplied by the test.
struct Scripted(Box<Behaviour>);

impl Scripted {
    fn new(
        behaviour: impl Fn(&GraphNode, &mut NodeIo) -> Result<()> + Send + Sync + 'static,
    ) -> Self {
        Self(Box::new(behaviour))
    }
}

impl GraphExecutor for Scripted {
    fn execute(&self, node: &GraphNode, io: &mut NodeIo, _context: &GraphContext) -> Result<()> {
        (self.0)(node, io)
    }
}

fn text(value: &str) -> PortValue {
    PortValue::Value(Value::String(value.into()))
}

#[test]
fn fast_subscriber_keeps_receiving_until_slow_lossless_edge_is_full() {
    const SLOW_CAPACITY: usize = 2;
    const EVENTS: usize = 5;

    let mut plan = GraphPlan::new("fanout_fast_and_slow");
    plan.add_node(node("producer", &[], &["feed"])).unwrap();
    plan.add_node(node("slow", &["value"], &[])).unwrap();
    plan.add_node(node("fast", &["value"], &[])).unwrap();
    // The slow edge is declared first, so it is the first sink of the port.
    plan.add_edge(EdgeSpec::lossless(
        PortRef::new("producer", "feed"),
        PortRef::new("slow", "value"),
        SLOW_CAPACITY,
    ));
    plan.add_edge(EdgeSpec::lossless(
        PortRef::new("producer", "feed"),
        PortRef::new("fast", "value"),
        8,
    ));

    let emitted = Arc::new(AtomicUsize::new(0));
    let fast_seen = Arc::new(AtomicUsize::new(0));
    let producer_emitted = Arc::clone(&emitted);
    let fast_counter = Arc::clone(&fast_seen);
    let executor = Scripted::new(move |node, io| match node.id.as_str() {
        "producer" => {
            for index in 0..EVENTS {
                io.emit("feed", text(&format!("event-{index}")))?;
                producer_emitted.fetch_add(1, Ordering::SeqCst);
            }
            Ok(())
        }
        "fast" => {
            while io.recv("value")?.is_some() {
                fast_counter.fetch_add(1, Ordering::SeqCst);
            }
            Ok(())
        }
        _ => {
            // The slow subscriber never reads. It stays alive until cancelled,
            // so its edge is full rather than closed.
            while !io.is_cancelled() {
                thread::sleep(Duration::from_millis(5));
            }
            Ok(())
        }
    });
    let mut session =
        GraphSession::start(plan, Arc::new(executor), GraphRunOptions::default()).unwrap();

    // Let the producer run until it blocks on the full slow edge.
    thread::sleep(Duration::from_millis(300));

    assert_eq!(
        fast_seen.load(Ordering::SeqCst),
        SLOW_CAPACITY + 1,
        "the fast subscriber stopped at the slow edge's capacity instead of receiving the \
         event the slow edge blocks on"
    );
    assert_eq!(
        emitted.load(Ordering::SeqCst),
        SLOW_CAPACITY,
        "the producer should block on the full lossless slow edge and not emit past it"
    );
    session.cancel_and_join().unwrap();
}
