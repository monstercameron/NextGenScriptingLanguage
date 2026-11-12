#![cfg(feature = "graph")]
//! #684: stream end of file must not stop a graph while its workers are still
//! active. These tests use in-process executors only, so they need no external
//! binary and no manifests folder.

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
use std::time::{Duration, Instant};

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

/// An in-process executor whose behaviour is supplied by the test.
struct Scripted(Box<dyn Fn(&GraphNode, &mut NodeIo) -> Result<()> + Send + Sync>);

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
fn zero_terminal_graph_stays_open_while_its_worker_is_active() {
    // A persistent source with no terminal ports and no internal consumer.
    // Its worker runs until cancellation, so the stream must not report end of
    // stream before the explicit cancel below.
    let mut plan = GraphPlan::new("zero_terminal_persistent");
    plan.add_node(node("source", &[], &[])).unwrap();
    let options = GraphRunOptions::default();
    let external_cancel = Arc::clone(&options.cancelled);
    let executor = Scripted::new(|_node, io| {
        while !io.is_cancelled() {
            thread::sleep(Duration::from_millis(5));
        }
        Ok(())
    });
    let mut session = GraphSession::start(plan, Arc::new(executor), options).unwrap();

    let canceller = thread::spawn(move || {
        thread::sleep(Duration::from_millis(300));
        external_cancel.store(true, Ordering::Release);
    });
    let started = Instant::now();
    let outcome = session.recv_output();
    let waited = started.elapsed();
    canceller.join().unwrap();

    assert!(
        outcome.is_err(),
        "recv_output reported end of stream while the source worker was still running"
    );
    assert!(
        waited >= Duration::from_millis(250),
        "recv_output returned after {waited:?}, before the explicit cancellation"
    );
    session.cancel_and_join().unwrap();
}

#[test]
fn slow_internal_sink_drains_before_terminal_eof() {
    // The terminal producer finishes at once and closes its terminal queue,
    // while the internal sink still has three queued events to process.
    let mut plan = GraphPlan::new("slow_internal_sink");
    plan.add_node(node("producer", &[], &["feed", "result"]))
        .unwrap();
    plan.add_node(node("sink", &["value"], &[])).unwrap();
    plan.add_edge(EdgeSpec::lossless(
        PortRef::new("producer", "feed"),
        PortRef::new("sink", "value"),
        8,
    ));

    let processed = Arc::new(AtomicUsize::new(0));
    let sink_processed = Arc::clone(&processed);
    let executor = Scripted::new(move |node, io| match node.id.as_str() {
        "producer" => {
            for index in 0..3 {
                io.emit("feed", text(&format!("feed-{index}")))?;
            }
            io.emit("result", text("done"))?;
            Ok(())
        }
        "sink" => {
            while io.recv("value")?.is_some() {
                thread::sleep(Duration::from_millis(100));
                sink_processed.fetch_add(1, Ordering::SeqCst);
            }
            Ok(())
        }
        _ => Ok(()),
    });
    let mut session =
        GraphSession::start(plan, Arc::new(executor), GraphRunOptions::default()).unwrap();

    let mut terminal = Vec::new();
    while let Some(event) = session.recv_output().unwrap() {
        terminal.push(event);
    }

    assert_eq!(
        terminal.len(),
        1,
        "the terminal event is delivered before EOF"
    );
    assert_eq!(
        processed.load(Ordering::SeqCst),
        3,
        "end of stream arrived before the internal sink drained its queued work"
    );
    session.cancel_and_join().unwrap();
}
