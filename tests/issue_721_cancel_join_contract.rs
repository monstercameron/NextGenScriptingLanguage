#![cfg(feature = "graph")]
use anyhow::{anyhow, Result};
use flow::contracts::{OutputType, ValueSchema};
use flow::graph::{
    GraphContext, GraphExecutor, GraphNode, GraphNodeKind, GraphPlan, GraphRunOptions,
    GraphSession, NodeIo, PortSchema,
};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

fn bytes() -> ValueSchema {
    ValueSchema {
        min_items: None,
        scalar_only: false,
        additional_properties: None,
        min_properties: None,
        max_properties: None,
        nonempty: false,
        kind: OutputType::Bytes,
        nullable: false,
        enum_values: None,
        single_line: false,
        minimum: None,
        fields: BTreeMap::new(),
        required: Vec::new(),
        items: None,
        pattern: None,
    }
}

/// A worker that blocks until the test releases it, then records that it
/// stopped and optionally fails.
struct GatedExecutor {
    gate: Mutex<mpsc::Receiver<()>>,
    stopped: Arc<AtomicBool>,
    fail: bool,
}

impl GraphExecutor for GatedExecutor {
    fn execute(&self, _: &GraphNode, _: &mut NodeIo, _: &GraphContext) -> Result<()> {
        let gate = self.gate.lock().unwrap();
        let _ = gate.recv();
        drop(gate);
        self.stopped.store(true, Ordering::Release);
        if self.fail {
            return Err(anyhow!("boom from worker"));
        }
        Ok(())
    }
}

fn gated_plan(name: &str) -> GraphPlan {
    let mut plan = GraphPlan::new(name);
    plan.add_node(GraphNode {
        id: "source".into(),
        kind: GraphNodeKind::Renderer {
            backend: "mock".into(),
        },
        inputs: BTreeMap::new(),
        outputs: BTreeMap::from([("out".into(), PortSchema::single(bytes()))]),
        events: BTreeMap::new(),
        line: None,
    })
    .unwrap();
    plan
}

#[test]
fn cancel_returns_before_blocked_worker_stops_and_cancel_and_join_waits() {
    let (release, gate) = mpsc::channel();
    let stopped = Arc::new(AtomicBool::new(false));
    let executor = Arc::new(GatedExecutor {
        gate: Mutex::new(gate),
        stopped: Arc::clone(&stopped),
        fail: false,
    });
    let mut session = GraphSession::start(
        gated_plan("cancel_contract"),
        executor,
        GraphRunOptions::default(),
    )
    .unwrap();

    session.cancel();
    assert!(
        !stopped.load(Ordering::Acquire),
        "cancel must only request cancellation and not wait for the worker"
    );

    release.send(()).unwrap();
    session.cancel_and_join().unwrap();
    assert!(
        stopped.load(Ordering::Acquire),
        "cancel_and_join must wait until the worker has stopped"
    );
}

#[test]
fn cancel_and_join_reports_worker_failure() {
    let (release, gate) = mpsc::channel();
    let stopped = Arc::new(AtomicBool::new(false));
    let executor = Arc::new(GatedExecutor {
        gate: Mutex::new(gate),
        stopped: Arc::clone(&stopped),
        fail: true,
    });
    let mut session = GraphSession::start(
        gated_plan("join_contract"),
        executor,
        GraphRunOptions::default(),
    )
    .unwrap();

    release.send(()).unwrap();
    let started = Instant::now();
    let error = loop {
        match session.recv_output() {
            Err(error) => break error.to_string(),
            Ok(None) => {
                assert!(
                    started.elapsed() < Duration::from_secs(5),
                    "worker failure never surfaced"
                );
                thread::sleep(Duration::from_millis(5));
            }
            Ok(Some(_)) => panic!("failing worker must not emit output"),
        }
    };
    assert!(error.contains("boom from worker"), "{error}");

    let join_error = session
        .cancel_and_join()
        .expect_err("cancel_and_join must report the recorded worker failure")
        .to_string();
    assert!(join_error.contains("boom from worker"), "{join_error}");
    assert!(stopped.load(Ordering::Acquire));
}
