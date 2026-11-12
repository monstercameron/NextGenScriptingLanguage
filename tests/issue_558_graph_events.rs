#![cfg(feature = "graph")]
//! #558: a persistent graph's terminal events reach the reader as they arrive,
//! and the default for an unflagged `run` follows the plan's persistent nodes.
//! The library tests use in-process executors only. The command-line tests run
//! the binary with its default manifests, so they need no `--manifest-dir`.

use anyhow::Result;
use flow::ast::Value;
use flow::contracts::{OutputType, ValueSchema};
use flow::graph::{
    GraphContext, GraphExecutor, GraphNode, GraphNodeKind, GraphPlan, GraphRunOptions,
    GraphSession, NodeIo, PortRef, PortSchema, PortValue,
};
use serde_json::Value as JsonValue;
use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc;
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

fn source_node() -> GraphNode {
    GraphNode {
        id: "source".into(),
        kind: GraphNodeKind::Renderer {
            backend: "test".into(),
        },
        inputs: BTreeMap::new(),
        outputs: BTreeMap::from([("out".into(), PortSchema::records(text_schema()))]),
        events: BTreeMap::new(),
        line: None,
    }
}

/// Emits one terminal event, then keeps running until cancelled. Receiving the
/// event before cancellation proves the reader sees it while the node is alive.
struct EmitThenRun;

impl GraphExecutor for EmitThenRun {
    fn execute(&self, node: &GraphNode, io: &mut NodeIo, _: &GraphContext) -> Result<()> {
        assert_eq!(node.id, "source");
        io.emit("out", PortValue::Value(Value::String("tick".into())))?;
        while !io.is_cancelled() {
            thread::sleep(Duration::from_millis(1));
        }
        Ok(())
    }
}

#[test]
fn persistent_node_event_is_observable_before_the_session_ends() {
    let mut plan = GraphPlan::new("live");
    plan.add_node(source_node()).unwrap();
    let mut session =
        GraphSession::start(plan, Arc::new(EmitThenRun), GraphRunOptions::default()).unwrap();

    // The node never finishes on its own, so this receive returns only if the
    // terminal event was delivered while the session was still running.
    let event = session
        .recv_output()
        .expect("receive works")
        .expect("the event is observable before the session ends");
    assert_eq!(event.port, PortRef::new("source", "out"));
    assert_eq!(
        event.payload,
        PortValue::Value(Value::String("tick".into()))
    );

    session.cancel();
    session
        .cancel_and_join()
        .expect("cancellation fallout is not a worker failure");
}

const TIMER_GRAPH: &str = "graph ticks\nevery 10 milliseconds as ticker\nend graph\n";
const FINITE_GRAPH: &str =
    "graph legacy\n  run text.concat with parts [\"hello\"] as message\nend graph\n";

/// A program in its own temporary directory. The binary runs there with its
/// default manifests: no `--manifest-dir` and no `--manifest-catalog`.
struct Program {
    directory: tempfile::TempDir,
    file: PathBuf,
}

impl Program {
    fn new(source: &str) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("graph.program");
        fs::write(&file, source).unwrap();
        Self { directory, file }
    }

    fn run(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_execute"));
        command
            .current_dir(self.directory.path())
            .arg("run")
            .arg(&self.file);
        command
    }
}

#[test]
fn unflagged_persistent_graph_streams_while_it_is_still_running() {
    let program = Program::new(TIMER_GRAPH);
    let mut child = program
        .run()
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run spawns");
    let stdout = child.stdout.take().expect("stdout is piped");
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut line = String::new();
        let result = BufReader::new(stdout).read_line(&mut line).map(|_| line);
        let _ = sender.send(result);
    });

    // The timer never stops, so a document-style run would never print. A
    // missing line within the window means the default did not stream.
    let line = match receiver.recv_timeout(Duration::from_secs(20)) {
        Ok(Ok(line)) if !line.is_empty() => line,
        outcome => {
            let _ = child.kill();
            let _ = child.wait();
            let mut stderr = String::new();
            if let Some(mut pipe) = child.stderr.take() {
                let _ = pipe.read_to_string(&mut stderr);
            }
            panic!("no streamed line within 20 s: {outcome:?}\nstderr: {stderr}");
        }
    };
    let still_running = child
        .try_wait()
        .expect("child status is readable")
        .is_none();
    let _ = child.kill();
    let _ = child.wait();

    let event: JsonValue = serde_json::from_str(&line).expect("one JSON line per event");
    assert_eq!(event["protocol"], "execute.graph-event/1");
    assert_eq!(event["graph"], "ticks");
    assert_eq!(event["port"]["node"], "ticker");
    assert!(
        still_running,
        "the first event must arrive while the graph is still running"
    );
}

#[test]
fn unflagged_finite_graph_keeps_the_closing_document() {
    let program = Program::new(FINITE_GRAPH);
    let output = program.run().output().expect("run executes");
    assert!(
        output.status.success(),
        "finite run must succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let document: JsonValue = serde_json::from_slice(&output.stdout)
        .expect("a finite graph prints exactly one JSON document");
    assert_eq!(document["protocol"], "execute.graph-run/1");
    let outputs = document["outputs"]
        .as_array()
        .expect("the document lists its outputs");
    assert_eq!(
        outputs.len(),
        2,
        "the value and status terminals: {document}"
    );
}
