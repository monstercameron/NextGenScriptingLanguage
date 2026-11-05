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
    let sensitivity_floor = super::transport::graph_module_sensitivity_floor(plan, registry, cwd)?;
    crate::runtime::authorize_checkpoint_persistence(registry, &sensitivity_floor)?;
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
            // This node replays instead of consuming its inputs, so detach
            // them first: an upstream replay must not block on a full
            // lossless edge that nobody will drain (#818).
            io.detach_inputs();
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
    checkpoint_identity_for(plan, cwd, false)
}

/// Like [`checkpoint_identity`], but for a resume (`resume` is true) the
/// per-user key must already exist: a fresh key cannot reproduce the identity
/// a journal was written with, so a missing key is refused with its path and
/// recovery guidance instead of being silently recreated.
pub fn checkpoint_identity_for(plan: &GraphPlan, cwd: &Path, resume: bool) -> Result<String> {
    let key_path = crate::agent::what_if_key::key_path()?;
    checkpoint_identity_at(plan, cwd, &key_path, resume)
}

/// The cwd bytes mixed into the checkpoint identity. A UTF-8 cwd keeps its
/// plain bytes, so identities written before #795 are unchanged. A non-UTF-8
/// cwd is encoded losslessly from its exact platform form (Unix `OsStr`
/// bytes, Windows wide characters) behind a 0xff marker, which never occurs
/// in valid UTF-8, so it cannot collide with any UTF-8 cwd or with a
/// different non-UTF-8 cwd.
fn cwd_identity_bytes(cwd: &Path) -> Result<Vec<u8>> {
    if let Some(text) = cwd.to_str() {
        return Ok(text.as_bytes().to_vec());
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let mut bytes = vec![0xff];
        bytes.extend_from_slice(cwd.as_os_str().as_bytes());
        Ok(bytes)
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let mut bytes = vec![0xff];
        for unit in cwd.as_os_str().encode_wide() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        Ok(bytes)
    }
    #[cfg(not(any(unix, windows)))]
    {
        bail!(
            "graph checkpoint identity cannot represent a non-UTF-8 working directory on this platform: {}",
            cwd.display()
        )
    }
}

fn checkpoint_identity_at(
    plan: &GraphPlan,
    cwd: &Path,
    key_path: &Path,
    resume: bool,
) -> Result<String> {
    let key = if resume {
        crate::agent::what_if_key::load_existing_at(key_path)
    } else {
        crate::agent::what_if_key::load_or_create_at(key_path)
    }
    .context("a graph checkpoint needs the per-user input key")?;
    let mut canonical = serde_json::to_value(plan)?;
    remove_source_lines(&mut canonical);
    let mut message = b"execute-graph-checkpoint-identity/v6\n".to_vec();
    message.extend_from_slice(&serde_json::to_vec(&canonical)?);
    message.push(b'\n');
    message.extend_from_slice(&cwd_identity_bytes(cwd)?);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resume_identity_never_creates_a_missing_key_but_a_fresh_run_does() {
        let directory = tempfile::tempdir().unwrap();
        let key_path = directory.path().join("execute").join("what-if.key");
        let plan = GraphPlan::new("resume-key");

        let error = checkpoint_identity_at(&plan, directory.path(), &key_path, true)
            .expect_err("a resume with a missing key must be refused");
        let message = format!("{error:#}");
        assert!(
            message.contains(&key_path.display().to_string()),
            "{message}"
        );
        assert!(
            message.contains("restore that key from backup"),
            "{message}"
        );
        assert!(!key_path.exists(), "a resume must not create a new key");

        // A fresh run creates the key, and a later resume with that restored
        // key reproduces the same identity.
        let fresh = checkpoint_identity_at(&plan, directory.path(), &key_path, false).unwrap();
        assert!(key_path.exists(), "a fresh run must create the key");
        let resumed = checkpoint_identity_at(&plan, directory.path(), &key_path, true).unwrap();
        assert_eq!(fresh, resumed);
    }

    #[test]
    fn utf8_cwd_identity_is_unchanged() {
        let directory = tempfile::tempdir().unwrap();
        let key_path = directory.path().join("execute").join("what-if.key");
        let plan = GraphPlan::new("utf8-cwd");

        let identity = checkpoint_identity_at(&plan, directory.path(), &key_path, false).unwrap();

        // The identity must still equal the pre-#795 computation, which mixed
        // in the cwd's lossy (here exact) UTF-8 bytes.
        let key = crate::agent::what_if_key::load_existing_at(&key_path).unwrap();
        let mut canonical = serde_json::to_value(&plan).unwrap();
        remove_source_lines(&mut canonical);
        let mut message = b"execute-graph-checkpoint-identity/v6\n".to_vec();
        message.extend_from_slice(&serde_json::to_vec(&canonical).unwrap());
        message.push(b'\n');
        message.extend_from_slice(directory.path().to_string_lossy().as_bytes());
        let expected: String = crate::agent::what_if_key::hmac_sha256(&key, &message)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        assert_eq!(identity, expected);
        assert_eq!(
            cwd_identity_bytes(directory.path()).unwrap(),
            directory.path().to_str().unwrap().as_bytes()
        );
    }

    // Windows paths are always UTF-16 and this host cannot create a
    // non-UTF-8 path, so the non-UTF-8 regression is Unix-only.
    #[cfg(unix)]
    #[test]
    fn non_utf8_cwds_with_same_lossy_rendering_have_different_identities() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;
        use std::path::PathBuf;

        let directory = tempfile::tempdir().unwrap();
        let key_path = directory.path().join("execute").join("what-if.key");
        let plan = GraphPlan::new("non-utf8-cwd");

        let first = PathBuf::from(OsString::from_vec(b"/tmp/graph-cwd-\xff".to_vec()));
        let second = PathBuf::from(OsString::from_vec(b"/tmp/graph-cwd-\xfe".to_vec()));
        assert_ne!(first, second);
        assert!(first.to_str().is_none() && second.to_str().is_none());
        assert_eq!(
            first.to_string_lossy(),
            second.to_string_lossy(),
            "precondition: both names render identically when lossy"
        );

        let first_identity = checkpoint_identity_at(&plan, &first, &key_path, false).unwrap();
        let second_identity = checkpoint_identity_at(&plan, &second, &key_path, true).unwrap();
        assert_ne!(first_identity, second_identity);

        // Neither may collide with the UTF-8 path that literally contains
        // the replacement character their lossy rendering produces.
        let lossy_utf8 = PathBuf::from(first.to_string_lossy().into_owned());
        let lossy_identity = checkpoint_identity_at(&plan, &lossy_utf8, &key_path, true).unwrap();
        assert_ne!(first_identity, lossy_identity);
        assert_ne!(second_identity, lossy_identity);

        // The same non-UTF-8 directory still resumes with the same identity.
        assert_eq!(
            first_identity,
            checkpoint_identity_at(&plan, &first, &key_path, true).unwrap()
        );
    }
}
