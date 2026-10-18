//! Checkpoints for finite graphs (#71, phase 1).
//!
//! A checkpointed node holds its outputs back until it completes and its record is
//! stored. Downstream nodes therefore see an output only after the output's record
//! exists. A later run replays a recorded node's outputs instead of running it.
//! Storage sits behind [`GraphCheckpointStore`]. The on-disk journal implements it
//! once the v5 `graph` mode lands; [`MemoryGraphStore`] implements it for tests.

use super::plan::{GraphNode, GraphPlan, PortValue};
use super::transport::{EventEnvelope, GraphContext, GraphExecutor, NodeIo, RecordedEmit};
use crate::manifest::Registry;
use crate::security::SensitivitySet;
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

/// What a store holds: completed nodes in completion order, and nodes that started
/// and did not complete.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GraphCheckpointState {
    pub completed: Vec<CompletedNode>,
    pub interrupted: Vec<InterruptedNode>,
}

/// One node's completion record. Its position in [`GraphCheckpointState::completed`]
/// is its completion order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompletedNode {
    pub node: String,
    pub fingerprint: String,
    pub outputs: Vec<RecordedOutput>,
}

/// A node that started and did not complete. Its effects may have happened, so a
/// resume needs `--retry-uncertain` unless the node is retry-safe.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InterruptedNode {
    pub node: String,
    pub fingerprint: String,
    pub retry_safe: bool,
}

/// One delivered output of a completed node, with the sensitivity it carried.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecordedOutput {
    pub port: String,
    pub payload: PortValue,
    pub sensitivity: SensitivitySet,
}

/// Durable storage for finite graph nodes.
pub trait GraphCheckpointStore: Send + Sync {
    /// Everything recorded so far, including nodes left in flight by an earlier run.
    fn load(&self) -> Result<GraphCheckpointState>;
    /// Record that a node is starting. It stays interrupted until `complete` is called.
    fn begin(&self, node: &str, fingerprint: &str, retry_safe: bool) -> Result<()>;
    /// Record a node's completion before any of its outputs are delivered.
    fn complete(&self, record: CompletedNode) -> Result<()>;
}

/// An in-memory store, for tests. It keeps the same contract as the journal: a node
/// is interrupted from `begin` until `complete`.
#[derive(Default)]
pub struct MemoryGraphStore {
    completed: Mutex<Vec<CompletedNode>>,
    in_flight: Mutex<BTreeMap<String, InterruptedNode>>,
}

impl GraphCheckpointStore for MemoryGraphStore {
    fn load(&self) -> Result<GraphCheckpointState> {
        Ok(GraphCheckpointState {
            completed: lock(&self.completed).clone(),
            interrupted: lock(&self.in_flight).values().cloned().collect(),
        })
    }

    fn begin(&self, node: &str, fingerprint: &str, retry_safe: bool) -> Result<()> {
        lock(&self.in_flight).insert(
            node.to_owned(),
            InterruptedNode {
                node: node.to_owned(),
                fingerprint: fingerprint.to_owned(),
                retry_safe,
            },
        );
        Ok(())
    }

    fn complete(&self, record: CompletedNode) -> Result<()> {
        lock(&self.in_flight).remove(&record.node);
        lock(&self.completed).push(record);
        Ok(())
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// A validated checkpoint for one run: the store, the current fingerprints, and the
/// completed records the run may replay.
pub(crate) struct Checkpointing {
    store: Arc<dyn GraphCheckpointStore>,
    fingerprints: BTreeMap<String, String>,
    retry_safe: BTreeMap<String, bool>,
    completed: BTreeMap<String, CompletedNode>,
}

impl Checkpointing {
    pub(crate) fn store(&self) -> Arc<dyn GraphCheckpointStore> {
        Arc::clone(&self.store)
    }
}

/// Check that this graph can be checkpointed, and that the store's records still
/// match it. Runs before any node starts, so a refusal has no effects.
pub(crate) fn prepare(
    plan: &GraphPlan,
    registry: &Registry,
    cwd: &Path,
    modules: &crate::modules::ProgramModuleSession,
    store: Arc<dyn GraphCheckpointStore>,
    retry_uncertain: bool,
) -> Result<Checkpointing> {
    plan.checkpoint_refusal(registry, modules)?;
    let fingerprints = plan.checkpoint_fingerprints(registry, cwd, modules.fingerprints())?;
    let state = store.load()?;
    let mut completed = BTreeMap::new();
    for record in state.completed {
        let Some(fingerprint) = fingerprints.get(&record.node) else {
            bail!(
                "the checkpoint records node {:?}, which this graph does not have",
                record.node
            );
        };
        if *fingerprint != record.fingerprint {
            bail!(
                "node {:?} changed since the checkpoint was written; start a fresh run",
                record.node
            );
        }
        completed.insert(record.node.clone(), record);
    }
    if !retry_uncertain {
        if let Some(node) = state.interrupted.iter().find(|node| !node.retry_safe) {
            bail!(
                "checkpoint may contain an interrupted side effect at node {:?}; pass --retry-uncertain to retry",
                node.node
            );
        }
    }
    let mut retry_safe = BTreeMap::new();
    for node in plan.nodes() {
        retry_safe.insert(node.id.clone(), plan.checkpoint_retry_safe(node, registry)?);
    }
    Ok(Checkpointing {
        store,
        fingerprints,
        retry_safe,
        completed,
    })
}

/// Wraps the graph's executor. A recorded node replays its outputs; any other node
/// runs with its outputs held back, records them, and then delivers them.
pub(crate) struct CheckpointedExecutor {
    inner: Arc<dyn GraphExecutor>,
    checkpointing: Checkpointing,
}

impl CheckpointedExecutor {
    pub(crate) fn new(inner: Arc<dyn GraphExecutor>, checkpointing: Checkpointing) -> Self {
        Self {
            inner,
            checkpointing,
        }
    }
}

impl GraphExecutor for CheckpointedExecutor {
    fn execute(&self, node: &GraphNode, io: &mut NodeIo, context: &GraphContext) -> Result<()> {
        let state = &self.checkpointing;
        if let Some(record) = state.completed.get(&node.id) {
            for output in &record.outputs {
                io.emit_recorded(
                    &output.port,
                    output.payload.clone(),
                    output.sensitivity.clone(),
                )?;
            }
            return Ok(());
        }
        let fingerprint = state
            .fingerprints
            .get(&node.id)
            .ok_or_else(|| anyhow!("node {:?} has no checkpoint fingerprint", node.id))?;
        let retry_safe = state.retry_safe.get(&node.id).copied().unwrap_or(false);
        // The node is in flight only from its first applet invocation. A node still
        // waiting for its inputs has no effects to make uncertain.
        let begun = AtomicBool::new(false);
        let store = Arc::clone(&state.store);
        let (node_id, fingerprint_text) = (node.id.clone(), fingerprint.clone());
        io.begin_recording(Arc::new(move || {
            if begun.swap(true, Ordering::SeqCst) {
                return Ok(());
            }
            store.begin(&node_id, &fingerprint_text, retry_safe)
        }));
        let result = self.inner.execute(node, io, context);
        // Outputs held back by a failed node are dropped with its interrupted record.
        let held: Vec<RecordedEmit> = io.finish_recording();
        result?;
        let outputs = held
            .iter()
            .map(|emit| RecordedOutput {
                port: emit.port.clone(),
                payload: emit.payload.clone(),
                sensitivity: emit.sensitivity.clone(),
            })
            .collect();
        state.store.complete(CompletedNode {
            node: node.id.clone(),
            fingerprint: fingerprint.clone(),
            outputs,
        })?;
        for emit in held {
            io.emit_recorded(&emit.port, emit.payload, emit.sensitivity)?;
        }
        Ok(())
    }
}

/// Under checkpoints, terminal outputs are ordered by the completion order of the
/// node that produced them. Order within one node is its emission order.
pub(crate) fn order_by_completion(
    outputs: &mut [EventEnvelope],
    store: &dyn GraphCheckpointStore,
) -> Result<()> {
    let order: BTreeMap<String, usize> = store
        .load()?
        .completed
        .iter()
        .enumerate()
        .map(|(index, record)| (record.node.clone(), index))
        .collect();
    outputs.sort_by_key(|event| order.get(&event.port.node).copied().unwrap_or(usize::MAX));
    Ok(())
}

/// Open the graph checkpoint in `directory` as the store for `run_checkpointed`.
/// `resume` says whether the checkpoint must already exist; a fresh run refuses an
/// existing one. Completed payloads are written only after the program journal's
/// persistence policy accepts their sensitivity labels.
pub fn open_checkpoint_directory(
    directory: &Path,
    resume: bool,
    identity: &str,
    registry: &Registry,
) -> Result<Arc<dyn GraphCheckpointStore>> {
    let registry = registry.clone();
    let authorize: crate::runtime::PersistenceAuthorizer =
        Arc::new(move |floor: &SensitivitySet| {
            crate::runtime::authorize_checkpoint_persistence(&registry, floor)
        });
    crate::runtime::open_graph_checkpoint(directory, resume, identity, authorize)
}

/// The identity a graph checkpoint is written with: the canonical plan (inputs
/// included) and the working directory. A resume must present the same identity.
pub fn checkpoint_identity(plan: &GraphPlan, cwd: &Path) -> Result<String> {
    let key = crate::agent::what_if_key::load_or_create()
        .context("a graph checkpoint needs the per-user input key")?;
    let mut canonical = serde_json::to_value(plan)?;
    remove_source_lines(&mut canonical);
    let mut message = b"execute-graph-checkpoint-identity/v6\n".to_vec();
    message.extend_from_slice(&serde_json::to_vec(&canonical)?);
    message.push(b'\n');
    message.extend_from_slice(cwd.to_string_lossy().as_bytes());
    Ok(crate::agent::what_if_key::hmac_sha256(&key, &message)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

fn remove_source_lines(value: &mut serde_json::Value) {
    let Some(fields) = value.as_object_mut() else {
        return;
    };
    for name in ["nodes", "edges"] {
        if let Some(items) = fields
            .get_mut(name)
            .and_then(serde_json::Value::as_array_mut)
        {
            for item in items {
                if let Some(fields) = item.as_object_mut() {
                    fields.remove("line");
                }
            }
        }
    }
}
