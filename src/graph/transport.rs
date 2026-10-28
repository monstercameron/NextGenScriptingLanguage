//! Bounded event transport and structured graph execution.

use super::checkpoint::{Checkpointing, GraphCheckpointStore};
use super::plan::{
    validate_payload, BufferPolicy, GraphNode, GraphNodeKind, GraphPlan, PortRef, PortSchema,
    PortValue,
};
use crate::ast::{Invocation, ListenSource, StepKind, Value};
use crate::contracts::{OutcomeKind, OutputEncoding, OutputType, PortDelivery, Sensitivity};
use crate::manifest::Registry;
use crate::runtime::{self, AppletInvocationResult, RunOptions};
use crate::security::SensitivitySet;
use crate::tasks::{CancellationToken, TaskHandle, TaskScope};
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const QUEUE_POLL: Duration = Duration::from_millis(10);
const INPUT_RECEIVE_POLL: Duration = Duration::from_millis(2);
/// Absolute ceiling for per-graph declared listener-port reader admission.
pub const MAX_GRAPH_LISTENER_READERS: usize = 128;
/// Maximum declared event ports on one applet listener. Remote listeners
/// create one pump worker for each port and share a 64-worker scope.
pub const MAX_LISTENER_PORTS_PER_SOURCE: usize = 64;

/// A payload crossing one graph edge. `sequence` is allocated once before
/// fanout so every subscriber observes the same event identity.
#[derive(Debug, Clone, PartialEq)]
pub struct EventEnvelope {
    pub port: PortRef,
    pub input: Option<PortRef>,
    pub generation: u64,
    pub revision: u64,
    pub sequence: u64,
    pub sensitivity: SensitivitySet,
    /// Remote event source metadata. Cursors are available to an embedding
    /// for deduplication/checkpointing but are redacted from Debug output.
    pub remote: Option<RemoteEventMetadata>,
    pub payload: PortValue,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteEventMetadata {
    pub event: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    #[serde(default)]
    pub duplicate_cursor: bool,
}

impl std::fmt::Debug for RemoteEventMetadata {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RemoteEventMetadata")
            .field("event", &self.event)
            .field("cursor", &self.cursor.as_ref().map(|_| "<redacted>"))
            .field("duplicate_cursor", &self.duplicate_cursor)
            .finish()
    }
}

#[derive(Debug, Clone)]
pub struct GraphContext {
    pub graph_name: String,
    pub generation: u64,
    pub revision: u64,
    pub deadline: Option<Instant>,
    pub metrics: Arc<GraphMetrics>,
}

#[derive(Debug, Default)]
pub struct GraphMetrics {
    launched_processes: AtomicUsize,
}

impl GraphMetrics {
    /// Successful child/helper process spawns observed in this graph session,
    /// including supervised resolver helpers. Reused workers and descendants
    /// started internally by an applet process are not counted.
    pub fn launched_processes(&self) -> usize {
        self.launched_processes.load(Ordering::Relaxed)
    }

    pub(crate) fn add_processes(&self, count: usize) {
        self.launched_processes.fetch_add(count, Ordering::Relaxed);
    }
}

/// Backend adapter for a graph node. Implementations receive only typed ports;
/// every emitted message is checked against the plan before it is queued.
pub trait GraphExecutor: Send + Sync + 'static {
    fn execute(&self, node: &GraphNode, io: &mut NodeIo, context: &GraphContext) -> Result<()>;
}

/// Injectable source for `listen to stdin`. Each call returns one typed graph
/// record (normally a line without its delimiter), EOF, or an error. Sources
/// must observe cancellation/deadlines while waiting so graph reconfiguration
/// can reap the listener promptly.
pub trait GraphStdinSource: Send + Sync + 'static {
    fn next_record(
        &self,
        cancelled: &AtomicBool,
        deadline: Option<Instant>,
    ) -> Result<Option<Vec<u8>>>;
}

/// A bounded stdin adapter for embedding applications and tests. The sender
/// side can push records into the graph without giving the executor ambient
/// access to process stdin.
pub struct ChannelStdinSource {
    receiver: Mutex<std::sync::mpsc::Receiver<Vec<u8>>>,
}

impl ChannelStdinSource {
    pub fn channel(capacity: usize) -> Result<(Arc<Self>, std::sync::mpsc::SyncSender<Vec<u8>>)> {
        if capacity == 0 {
            bail!("stdin channel capacity must be positive");
        }
        let (sender, receiver) = std::sync::mpsc::sync_channel(capacity);
        Ok((
            Arc::new(Self {
                receiver: Mutex::new(receiver),
            }),
            sender,
        ))
    }
}

impl GraphStdinSource for ChannelStdinSource {
    fn next_record(
        &self,
        cancelled: &AtomicBool,
        deadline: Option<Instant>,
    ) -> Result<Option<Vec<u8>>> {
        loop {
            if cancelled.load(Ordering::Acquire) {
                bail!("stdin source cancelled");
            }
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                bail!("stdin source deadline exceeded");
            }
            let timeout = deadline.map_or(QUEUE_POLL, |deadline| {
                deadline
                    .saturating_duration_since(Instant::now())
                    .min(QUEUE_POLL)
            });
            let receiver = self
                .receiver
                .lock()
                .map_err(|_| anyhow!("stdin source lock poisoned"))?;
            match receiver.recv_timeout(timeout) {
                Ok(record) => return Ok(Some(record)),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return Ok(None),
            }
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PortEmitReport {
    pub delivered: usize,
    pub dropped: usize,
}

struct QueueState {
    items: VecDeque<EventEnvelope>,
    closed: bool,
}

struct BoundedQueue {
    capacity: usize,
    policy: BufferPolicy,
    state: Mutex<QueueState>,
    changed: Condvar,
    dropped: AtomicUsize,
}

impl BoundedQueue {
    fn new(capacity: usize, policy: BufferPolicy) -> Arc<Self> {
        Arc::new(Self {
            capacity,
            policy,
            state: Mutex::new(QueueState {
                items: VecDeque::new(),
                closed: false,
            }),
            changed: Condvar::new(),
            dropped: AtomicUsize::new(0),
        })
    }

    fn send(
        &self,
        mut envelope: EventEnvelope,
        input: Option<PortRef>,
        token: &CancellationToken,
    ) -> Result<bool> {
        envelope.input = input;
        loop {
            token.check()?;
            let mut state = self
                .state
                .lock()
                .map_err(|_| anyhow!("graph queue lock poisoned"))?;
            if state.closed {
                bail!("graph edge was closed while its producer was active");
            }
            if state.items.len() < self.capacity {
                state.items.push_back(envelope);
                self.changed.notify_all();
                return Ok(true);
            }
            match self.policy {
                BufferPolicy::Lossless => {
                    let (next, _) = self
                        .changed
                        .wait_timeout(state, QUEUE_POLL)
                        .map_err(|_| anyhow!("graph queue lock poisoned"))?;
                    state = next;
                    drop(state);
                }
                BufferPolicy::DropNewest => {
                    self.dropped.fetch_add(1, Ordering::Relaxed);
                    return Ok(false);
                }
                BufferPolicy::DropOldest => {
                    state.items.pop_front();
                    self.dropped.fetch_add(1, Ordering::Relaxed);
                    state.items.push_back(envelope);
                    self.changed.notify_all();
                    return Ok(true);
                }
                BufferPolicy::Latest => {
                    let removed = state.items.len();
                    state.items.clear();
                    self.dropped.fetch_add(removed, Ordering::Relaxed);
                    state.items.push_back(envelope);
                    self.changed.notify_all();
                    return Ok(true);
                }
            }
        }
    }

    fn try_recv(&self) -> Result<Option<EventEnvelope>> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow!("graph queue lock poisoned"))?;
        let item = state.items.pop_front();
        if item.is_some() {
            self.changed.notify_all();
        }
        Ok(item)
    }

    fn wait_for_change(&self, timeout: Duration) {
        // Defer poison errors to the caller's next scan, after its cancellation check.
        let Ok(state) = self.state.lock() else {
            return;
        };
        if !state.items.is_empty() || state.closed {
            return;
        }
        let _ = self.changed.wait_timeout(state, timeout);
    }

    fn recv(&self, token: &CancellationToken) -> Result<Option<EventEnvelope>> {
        loop {
            token.check()?;
            let mut state = self
                .state
                .lock()
                .map_err(|_| anyhow!("graph queue lock poisoned"))?;
            if let Some(item) = state.items.pop_front() {
                self.changed.notify_all();
                return Ok(Some(item));
            }
            if state.closed {
                return Ok(None);
            }
            let (next, _) = self
                .changed
                .wait_timeout(state, QUEUE_POLL)
                .map_err(|_| anyhow!("graph queue lock poisoned"))?;
            drop(next);
        }
    }

    fn is_closed_and_empty(&self) -> Result<bool> {
        let state = self
            .state
            .lock()
            .map_err(|_| anyhow!("graph queue lock poisoned"))?;
        Ok(state.closed && state.items.is_empty())
    }

    fn close(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.closed = true;
            self.changed.notify_all();
        }
    }
}

#[derive(Clone)]
struct QueueSink {
    queue: Arc<BoundedQueue>,
    input: Option<PortRef>,
}

impl QueueSink {
    fn send_and_update_report(
        &self,
        envelope: EventEnvelope,
        token: &CancellationToken,
        report: &mut PortEmitReport,
    ) -> Result<()> {
        let before = self.queue.dropped.load(Ordering::Relaxed);
        if self.queue.send(envelope, self.input.clone(), token)? {
            report.delivered += 1;
        } else {
            report.dropped += 1;
        }
        report.dropped += self
            .queue
            .dropped
            .load(Ordering::Relaxed)
            .saturating_sub(before);
        Ok(())
    }
}

struct CloseQueues(Vec<Arc<BoundedQueue>>);

impl Drop for CloseQueues {
    fn drop(&mut self) {
        for queue in &self.0 {
            queue.close();
        }
    }
}

/// Per-node port access. Receives are cancellation-aware; emits validate types
/// and fan out to every bound edge and to a bounded terminal queue.
pub struct NodeIo {
    node: String,
    inputs: BTreeMap<String, Vec<Arc<BoundedQueue>>>,
    input_schemas: BTreeMap<String, PortSchema>,
    initial_inputs: BTreeMap<String, Value>,
    /// Sorted, stable name indices shared with the per-port queue cursors.
    input_names: Vec<String>,
    /// Compact active positions preserve the old rotation while seed-only
    /// names disappear after their initial value is consumed.
    active_input_names: Vec<usize>,
    input_name_cursor: usize,
    input_queue_cursors: Vec<usize>,
    output_sinks: BTreeMap<String, Vec<QueueSink>>,
    schemas: BTreeMap<String, PortSchema>,
    generation: u64,
    revision: u64,
    next_sequence: Arc<AtomicU64>,
    token: CancellationToken,
    external_cancelled: Arc<AtomicBool>,
    deadline: Option<Instant>,
    input_sensitivity: Option<SensitivitySet>,
    max_message_bytes: usize,
    /// Outputs held back while a checkpointed node runs; see `begin_recording`.
    recording: Option<Vec<RecordedEmit>>,
    /// Called once, just before the node first invokes an applet.
    effects_started: Option<EffectsHook>,
}

/// Runs just before a node's first applet invocation, so a checkpoint can mark the
/// node in flight only once its effects may have begun.
pub(crate) type EffectsHook = Arc<dyn Fn() -> Result<()> + Send + Sync>;

/// One output a node emitted while its outputs were held back for checkpointing.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RecordedEmit {
    pub port: String,
    pub payload: PortValue,
    pub sensitivity: SensitivitySet,
}

/// The executor for a plan with renderer nodes. Only the desktop feature has the
/// Quickshell renderer; a lean build refuses such a plan before any node runs.
#[cfg(feature = "desktop")]
fn renderer_executor(
    plan: &GraphPlan,
    registry: crate::manifest::Registry,
    options: crate::runtime::RunOptions,
    module_session: Arc<crate::modules::ProgramModuleSession>,
) -> anyhow::Result<Arc<dyn GraphExecutor>> {
    crate::desktop::graph_executor_with_module_session(plan, registry, options, module_session)
}

#[cfg(not(feature = "desktop"))]
fn renderer_executor(
    plan: &GraphPlan,
    registry: crate::manifest::Registry,
    options: crate::runtime::RunOptions,
    module_session: Arc<crate::modules::ProgramModuleSession>,
) -> anyhow::Result<Arc<dyn GraphExecutor>> {
    drop((plan, registry, options, module_session));
    anyhow::bail!("graph renderer nodes need the desktop feature; build with --features full")
}

fn cached_input_names(
    inputs: &BTreeMap<String, Vec<Arc<BoundedQueue>>>,
    initial_inputs: &BTreeMap<String, Value>,
) -> Vec<String> {
    inputs
        .keys()
        .chain(initial_inputs.keys())
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn all_input_queues_closed(queues: &[Arc<BoundedQueue>]) -> Result<bool> {
    let mut all_closed = true;
    for queue in queues {
        // Evaluate every queue in order, even after finding an open queue, so
        // a later lock error is reported just as it was by the old collect.
        all_closed &= queue.is_closed_and_empty()?;
    }
    Ok(all_closed)
}

impl NodeIo {
    fn input_name_index(&self, port: &str) -> Option<usize> {
        self.input_names
            .binary_search_by(|candidate| candidate.as_str().cmp(port))
            .ok()
    }

    fn single_active_input_queue(&self) -> Option<&BoundedQueue> {
        if self.active_input_names.len() != 1 {
            return None;
        }
        let name = &self.input_names[self.active_input_names[0]];
        match self.inputs.get(name)?.as_slice() {
            [queue] => Some(queue),
            _ => None,
        }
    }

    fn discard_seed_only_name(&mut self, name_index: usize) {
        if self.inputs.contains_key(&self.input_names[name_index]) {
            return;
        }
        if let Some(active_index) = self
            .active_input_names
            .iter()
            .position(|index| *index == name_index)
        {
            self.active_input_names.remove(active_index);
        }
    }

    pub fn has_input(&self, port: &str) -> bool {
        self.initial_inputs.contains_key(port)
            || self
                .inputs
                .get(port)
                .is_some_and(|queues| !queues.is_empty())
    }

    pub fn input_names(&self) -> impl Iterator<Item = &str> {
        self.inputs.keys().map(String::as_str)
    }

    /// Effective sensitivity of values consumed by this node so far. The
    /// declared output label is joined with this provenance on every emit.
    pub fn input_sensitivity(&self) -> Option<&SensitivitySet> {
        self.input_sensitivity.as_ref()
    }

    /// Add trusted source or backend provenance to later events. This is a
    /// monotonic union: adapters can retain additional labels, never remove
    /// labels already declared by the graph or observed on inputs.
    pub fn include_sensitivity_provenance(&mut self, sensitivities: &SensitivitySet) {
        match &mut self.input_sensitivity {
            Some(labels) => labels.extend(sensitivities),
            None => self.input_sensitivity = Some(sensitivities.clone()),
        }
    }

    fn observe_input(&mut self, mut event: EventEnvelope) -> EventEnvelope {
        if let Some(input) = event.input.as_ref() {
            if input.node == self.node {
                if let Some(schema) = self.input_schemas.get(&input.port) {
                    event.sensitivity.insert(schema.sensitivity);
                }
            }
        }
        match &mut self.input_sensitivity {
            Some(labels) => labels.extend(&event.sensitivity),
            None => self.input_sensitivity = Some(event.sensitivity.clone()),
        }
        event
    }

    fn take_initial_input(&mut self, name_index: usize) -> Result<Option<EventEnvelope>> {
        let Some(value) = self.initial_inputs.remove(&self.input_names[name_index]) else {
            return Ok(None);
        };
        self.discard_seed_only_name(name_index);
        let port = self.input_names[name_index].as_str();
        let schema = self.input_schemas.get(port).ok_or_else(|| {
            anyhow!(
                "initial input {}.{} has no declared schema",
                self.node,
                port
            )
        })?;
        let payload = PortValue::Value(value);
        validate_payload(&schema.schema, &payload)
            .with_context(|| format!("initial input {}.{}", self.node, port))?;
        let sensitivity = SensitivitySet::from_label(schema.sensitivity);
        let sequence = next_sequence(&self.next_sequence)?;
        Ok(Some(self.observe_input(EventEnvelope {
            port: PortRef::new(self.node.clone(), port),
            input: Some(PortRef::new(self.node.clone(), port)),
            generation: self.generation,
            revision: self.revision,
            sequence,
            sensitivity,
            remote: None,
            payload,
        })))
    }

    fn try_recv_from_any_input(&self, name_index: usize) -> Result<Option<(EventEnvelope, usize)>> {
        let name = &self.input_names[name_index];
        let queues = self.inputs.get(name).map(Vec::as_slice).unwrap_or_default();
        let queue_start = self.input_queue_cursors[name_index] % queues.len().max(1);
        for queue_offset in 0..queues.len() {
            let queue_index = (queue_start + queue_offset) % queues.len();
            if let Some(event) = queues[queue_index].try_recv()? {
                return Ok(Some((event, queue_index)));
            }
        }
        Ok(None)
    }

    fn try_recv_from_named_input(
        &mut self,
        port: &str,
        cursor_index: usize,
        queue_count: usize,
    ) -> Result<Option<EventEnvelope>> {
        let Self {
            inputs,
            input_queue_cursors,
            ..
        } = self;
        let start = input_queue_cursors[cursor_index] % queue_count;
        let queues = inputs.get(port).expect("named receive has a bound input");
        for offset in 0..queue_count {
            let index = (start + offset) % queue_count;
            if let Some(event) = queues[index].try_recv()? {
                input_queue_cursors[cursor_index] = index + 1;
                return Ok(Some(event));
            }
        }
        Ok(None)
    }

    /// Receive the next message from one named input. Multi-edge inputs merge
    /// in the order messages become available, with a stable queue cursor.
    pub fn recv(&mut self, port: &str) -> Result<Option<EventEnvelope>> {
        self.check_cancelled()?;
        if let Some(name_index) = self.input_name_index(port) {
            if let Some(event) = self.take_initial_input(name_index)? {
                return Ok(Some(event));
            }
        }
        let queue_count = self.inputs.get(port).map_or(0, |queues| queues.len());
        if queue_count == 0 {
            return Ok(None);
        }
        let cursor_index = self
            .input_name_index(port)
            .expect("bound input has a cached queue cursor");
        if queue_count == 1 {
            let event = self
                .inputs
                .get(port)
                .expect("named receive has a bound input")[0]
                .recv(&self.token)?;
            return Ok(event.map(|event| self.observe_input(event)));
        }
        loop {
            self.check_cancelled()?;
            if let Some(event) = self.try_recv_from_named_input(port, cursor_index, queue_count)? {
                return Ok(Some(self.observe_input(event)));
            }
            let all_closed = {
                let queues = self
                    .inputs
                    .get(port)
                    .expect("named receive has a bound input");
                all_input_queues_closed(queues)?
            };
            if all_closed {
                return Ok(None);
            }
            thread::sleep(Duration::from_millis(2));
        }
    }

    /// Receive from one named input until a message arrives, every producer
    /// closes, or `timeout` elapses. Cancellation remains an error.
    pub fn recv_timeout(&mut self, port: &str, timeout: Duration) -> Result<Option<EventEnvelope>> {
        self.check_cancelled()?;
        let deadline = Instant::now().checked_add(timeout);
        if let Some(name_index) = self.input_name_index(port) {
            if let Some(event) = self.take_initial_input(name_index)? {
                return Ok(Some(event));
            }
        }
        let queue_count = self.inputs.get(port).map_or(0, |queues| queues.len());
        if queue_count == 0 {
            return Ok(None);
        }
        let cursor_index = self
            .input_name_index(port)
            .expect("bound input has a cached queue cursor");
        loop {
            self.check_cancelled()?;
            if let Some(event) = self.try_recv_from_named_input(port, cursor_index, queue_count)? {
                return Ok(Some(self.observe_input(event)));
            }
            let all_closed = {
                let queues = self
                    .inputs
                    .get(port)
                    .expect("named receive has a bound input");
                all_input_queues_closed(queues)?
            };
            if all_closed {
                return Ok(None);
            }
            let now = Instant::now();
            if deadline.is_some_and(|deadline| now >= deadline) {
                return Ok(None);
            }
            let wait = deadline.map_or(INPUT_RECEIVE_POLL, |deadline| {
                deadline
                    .saturating_duration_since(now)
                    .min(INPUT_RECEIVE_POLL)
            });
            if queue_count == 1 {
                self.inputs
                    .get(port)
                    .expect("named receive has a bound input")[0]
                    .wait_for_change(wait);
            } else {
                thread::sleep(wait);
            }
        }
    }

    /// Receive the next message from any bound input. `event.input` identifies
    /// the destination port so adapters can update the matching parameter.
    pub fn recv_any(&mut self) -> Result<Option<EventEnvelope>> {
        if self.active_input_names.is_empty() {
            return Ok(None);
        }
        loop {
            self.check_cancelled()?;
            let active_count = self.active_input_names.len();
            let start = self.input_name_cursor % active_count;
            for offset in 0..active_count {
                let active_index = (start + offset) % active_count;
                let name_index = self.active_input_names[active_index];
                if let Some(event) = self.take_initial_input(name_index)? {
                    self.input_name_cursor = active_index + 1;
                    return Ok(Some(event));
                }
                if let Some((event, queue_index)) = self.try_recv_from_any_input(name_index)? {
                    self.input_name_cursor = active_index + 1;
                    self.input_queue_cursors[name_index] = queue_index + 1;
                    return Ok(Some(self.observe_input(event)));
                }
            }
            let all_closed = self.active_input_names.iter().all(|name_index| {
                let name = &self.input_names[*name_index];
                !self.initial_inputs.contains_key(name)
                    && self
                        .inputs
                        .get(name)
                        .into_iter()
                        .flatten()
                        .map(|queue| queue.is_closed_and_empty())
                        .collect::<Result<Vec<_>>>()
                        .is_ok_and(|states| states.iter().all(|closed| *closed))
            });
            if all_closed {
                return Ok(None);
            }
            if let Some(queue) = self.single_active_input_queue() {
                queue.wait_for_change(INPUT_RECEIVE_POLL);
            } else {
                thread::sleep(INPUT_RECEIVE_POLL);
            }
        }
    }

    pub fn recv_any_timeout(&mut self, timeout: Duration) -> Result<Option<EventEnvelope>> {
        let deadline = Instant::now().checked_add(timeout);
        if self.active_input_names.is_empty() {
            return Ok(None);
        }
        loop {
            self.check_cancelled()?;
            let active_count = self.active_input_names.len();
            let start = self.input_name_cursor % active_count;
            for offset in 0..active_count {
                let active_index = (start + offset) % active_count;
                let name_index = self.active_input_names[active_index];
                if let Some(event) = self.take_initial_input(name_index)? {
                    self.input_name_cursor = active_index + 1;
                    return Ok(Some(event));
                }
                if let Some((event, queue_index)) = self.try_recv_from_any_input(name_index)? {
                    self.input_name_cursor = active_index + 1;
                    self.input_queue_cursors[name_index] = queue_index + 1;
                    return Ok(Some(self.observe_input(event)));
                }
            }
            let all_closed = self.active_input_names.iter().all(|name_index| {
                let name = &self.input_names[*name_index];
                !self.initial_inputs.contains_key(name)
                    && self
                        .inputs
                        .get(name)
                        .into_iter()
                        .flatten()
                        .map(|queue| queue.is_closed_and_empty())
                        .collect::<Result<Vec<_>>>()
                        .is_ok_and(|states| states.iter().all(|closed| *closed))
            });
            if all_closed || deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                return Ok(None);
            }
            let wait = deadline.map_or(INPUT_RECEIVE_POLL, |deadline| {
                deadline
                    .saturating_duration_since(Instant::now())
                    .min(INPUT_RECEIVE_POLL)
            });
            if let Some(queue) = self.single_active_input_queue() {
                queue.wait_for_change(wait);
            } else {
                thread::sleep(wait);
            }
        }
    }

    pub fn emit(&mut self, port: &str, payload: PortValue) -> Result<PortEmitReport> {
        self.emit_with_remote_metadata(port, payload, None, None)
    }

    /// Deliver an output exactly as a checkpointed node recorded it. Its sensitivity
    /// is restored from the record instead of recomputed from this replay's inputs.
    pub(crate) fn emit_recorded(
        &mut self,
        port: &str,
        payload: PortValue,
        sensitivity: SensitivitySet,
    ) -> Result<PortEmitReport> {
        self.emit_with_remote_metadata(port, payload, None, Some(sensitivity))
    }

    /// Hold back this node's outputs instead of delivering them, until the
    /// matching `finish_recording`. Checkpointed nodes do this so that a node's
    /// outputs reach downstream nodes only after its completion is recorded.
    pub(crate) fn begin_recording(&mut self, effects_started: EffectsHook) {
        self.recording = Some(Vec::new());
        self.effects_started = Some(effects_started);
    }

    /// Stop holding outputs and return them in emission order.
    pub(crate) fn finish_recording(&mut self) -> Vec<RecordedEmit> {
        self.effects_started = None;
        self.recording.take().unwrap_or_default()
    }

    /// Note that the node is about to invoke an applet. The first call runs the
    /// checkpoint hook, if one is installed.
    pub(crate) fn mark_effects_started(&self) -> Result<()> {
        match &self.effects_started {
            Some(hook) => hook(),
            None => Ok(()),
        }
    }

    /// Emit a validated remote event while preserving its source event name,
    /// cursor and duplicate-cursor signal on the graph envelope.
    pub fn emit_remote_event(
        &mut self,
        port: &str,
        payload: PortValue,
        metadata: RemoteEventMetadata,
    ) -> Result<PortEmitReport> {
        self.emit_with_remote_metadata(port, payload, Some(metadata), None)
    }

    fn emit_with_remote_metadata(
        &mut self,
        port: &str,
        payload: PortValue,
        remote: Option<RemoteEventMetadata>,
        sensitivity: Option<SensitivitySet>,
    ) -> Result<PortEmitReport> {
        self.check_cancelled()?;
        let schema = self.schemas.get(port).ok_or_else(|| {
            anyhow!(
                "node {:?} has no output or event port {:?}",
                self.node,
                port
            )
        })?;
        validate_payload(&schema.schema, &payload)
            .with_context(|| format!("payload for {}.{}", self.node, port))?;
        if payload_size(&payload)? > self.max_message_bytes {
            bail!(
                "payload for {}.{} exceeds graph message limit of {} bytes",
                self.node,
                port,
                self.max_message_bytes
            );
        }
        let sinks = self
            .output_sinks
            .get(port)
            .map(Vec::as_slice)
            .unwrap_or_default();
        if sinks.is_empty() {
            return Ok(PortEmitReport::default());
        }
        let sensitivity = sensitivity.unwrap_or_else(|| {
            let mut labels = SensitivitySet::from_label(schema.sensitivity);
            if let Some(input) = &self.input_sensitivity {
                labels.extend(input);
            }
            labels
        });
        // A held-back output takes no sequence number until it is delivered.
        if let Some(recording) = self.recording.as_mut() {
            if remote.is_some() {
                bail!(
                    "remote events from node {:?} cannot be checkpointed",
                    self.node
                );
            }
            recording.push(RecordedEmit {
                port: port.to_owned(),
                payload,
                sensitivity,
            });
            return Ok(PortEmitReport::default());
        }
        let sequence = next_sequence(&self.next_sequence)?;
        let envelope = EventEnvelope {
            port: PortRef::new(self.node.clone(), port),
            input: None,
            generation: self.generation,
            revision: self.revision,
            sequence,
            sensitivity,
            remote,
            payload,
        };
        let mut report = PortEmitReport::default();
        let (last_sink, earlier_sinks) = sinks
            .split_last()
            .expect("nonempty output sink list was checked above");
        for sink in earlier_sinks {
            sink.send_and_update_report(envelope.clone(), &self.token, &mut report)?;
        }
        last_sink.send_and_update_report(envelope, &self.token, &mut report)?;
        Ok(report)
    }

    pub fn is_cancelled(&self) -> bool {
        let deadline_expired = self
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline);
        if deadline_expired || self.external_cancelled.load(Ordering::Acquire) {
            self.token
                .cancellation_flag()
                .store(true, Ordering::Release);
        }
        self.token.is_cancelled()
    }

    pub fn cancellation_flag(&self) -> Arc<AtomicBool> {
        self.token.cancellation_flag()
    }

    fn check_cancelled(&self) -> Result<()> {
        if self
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            self.token
                .cancellation_flag()
                .store(true, Ordering::Release);
            bail!("graph execution deadline exceeded");
        }
        if self.external_cancelled.load(Ordering::Acquire) {
            self.token
                .cancellation_flag()
                .store(true, Ordering::Release);
        }
        self.token.check()
    }
}

#[derive(Debug, Clone)]
pub struct GraphRunOptions {
    pub max_parallel: usize,
    pub max_nodes: usize,
    /// Aggregate declared event-port slots for applet listener sources.
    /// Remote listeners use one additional pump worker per slot; local applet
    /// listeners are conservatively charged for their synthetic `line` port.
    pub max_listener_readers: usize,
    pub max_edges: usize,
    pub max_queue_slots: usize,
    pub max_message_bytes: usize,
    pub terminal_capacity: usize,
    pub max_collected_events: usize,
    pub max_collected_bytes: usize,
    pub generation: u64,
    pub revision: u64,
    pub deadline: Option<Instant>,
    pub cancelled: Arc<AtomicBool>,
}

impl Default for GraphRunOptions {
    fn default() -> Self {
        Self {
            max_parallel: 64,
            max_nodes: 64,
            max_listener_readers: 64,
            max_edges: 4096,
            max_queue_slots: 65_536,
            max_message_bytes: 8 * 1024 * 1024,
            terminal_capacity: 64,
            max_collected_events: 100_000,
            max_collected_bytes: 64 * 1024 * 1024,
            generation: 1,
            revision: 1,
            deadline: None,
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }
}

#[derive(Debug, Default)]
pub struct GraphRunResult {
    pub generation: u64,
    pub revision: u64,
    pub outputs: Vec<EventEnvelope>,
    pub dropped_messages: usize,
    pub launched_processes: usize,
}

pub struct GraphSession {
    context: GraphContext,
    scope: TaskScope,
    tasks: Vec<Option<TaskHandle<()>>>,
    terminals: Vec<(PortRef, Arc<BoundedQueue>)>,
    all_queues: Vec<Arc<BoundedQueue>>,
    cancelled: Arc<AtomicBool>,
    external_cancelled: Arc<AtomicBool>,
    worker_error: Arc<Mutex<Option<String>>>,
    options: GraphRunOptions,
}

fn preflight_graph_admission(plan: &GraphPlan, options: &GraphRunOptions) -> Result<()> {
    plan.validate()?;
    if plan.nodes().is_empty() {
        bail!("graph {:?} has no nodes", plan.name);
    }
    if options.max_nodes == 0 || plan.nodes().len() > options.max_nodes {
        bail!(
            "graph node count {} exceeds max_nodes {}",
            plan.nodes().len(),
            options.max_nodes
        );
    }
    if options.max_edges == 0 || plan.edges().len() > options.max_edges {
        bail!(
            "graph edge count {} exceeds max_edges {}",
            plan.edges().len(),
            options.max_edges
        );
    }
    if options.max_parallel == 0 || plan.nodes().len() > options.max_parallel {
        bail!(
            "graph node count {} exceeds max_parallel {}",
            plan.nodes().len(),
            options.max_parallel
        );
    }
    if options.max_listener_readers == 0 {
        bail!("graph max_listener_readers must be positive");
    }
    if options.max_listener_readers > MAX_GRAPH_LISTENER_READERS {
        bail!(
            "graph max_listener_readers {} exceeds absolute maximum {}",
            options.max_listener_readers,
            MAX_GRAPH_LISTENER_READERS
        );
    }
    let listener_readers = plan.nodes().iter().try_fold(0usize, |total, node| {
        let declared = match &node.kind {
            GraphNodeKind::Listener {
                source: ListenSource::Applet(_),
                ..
            } => {
                if node.events.len() > MAX_LISTENER_PORTS_PER_SOURCE {
                    bail!(
                        "graph listener {:?} declares {} event ports; per-source limit is {}",
                        node.id,
                        node.events.len(),
                        MAX_LISTENER_PORTS_PER_SOURCE
                    );
                }
                node.events.len()
            }
            _ => 0,
        };
        total
            .checked_add(declared)
            .ok_or_else(|| anyhow!("graph listener reader count overflowed"))
    })?;
    if listener_readers > options.max_listener_readers {
        bail!(
            "graph listener reader slots {listener_readers} exceed max_listener_readers {}",
            options.max_listener_readers
        );
    }
    if options.terminal_capacity == 0 {
        bail!("graph terminal capacity must be positive");
    }
    if options.max_message_bytes == 0 {
        bail!("graph message byte limit must be positive");
    }
    if options.max_collected_events == 0 || options.max_collected_bytes == 0 {
        bail!("graph collected-output limits must be positive");
    }

    let terminal_ports = plan.output_terminals();
    let queue_slots = plan
        .edges()
        .iter()
        .try_fold(0usize, |total, edge| total.checked_add(edge.capacity))
        .and_then(|total| {
            terminal_ports
                .len()
                .checked_mul(options.terminal_capacity)
                .and_then(|terminal_slots| total.checked_add(terminal_slots))
        })
        .ok_or_else(|| anyhow!("graph queue capacity sum overflowed"))?;
    if queue_slots > options.max_queue_slots {
        bail!(
            "graph queue slots {queue_slots} exceed max_queue_slots {}",
            options.max_queue_slots
        );
    }
    Ok(())
}

impl GraphSession {
    pub fn start(
        plan: GraphPlan,
        executor: Arc<dyn GraphExecutor>,
        options: GraphRunOptions,
    ) -> Result<Self> {
        preflight_graph_admission(&plan, &options)?;

        let terminal_ports = plan.output_terminals();

        let mut incoming: BTreeMap<(String, String), Vec<Arc<BoundedQueue>>> = BTreeMap::new();
        let mut outgoing: BTreeMap<(String, String), Vec<QueueSink>> = BTreeMap::new();
        let mut all_queues = Vec::new();
        for edge in plan.edges() {
            let queue = BoundedQueue::new(edge.capacity, edge.policy);
            incoming
                .entry((edge.to.node.clone(), edge.to.port.clone()))
                .or_default()
                .push(Arc::clone(&queue));
            outgoing
                .entry((edge.from.node.clone(), edge.from.port.clone()))
                .or_default()
                .push(QueueSink {
                    queue: Arc::clone(&queue),
                    input: Some(PortRef::new(edge.to.node.clone(), edge.to.port.clone())),
                });
            all_queues.push(queue);
        }

        let mut terminals = Vec::new();
        for port in terminal_ports {
            let queue = BoundedQueue::new(options.terminal_capacity, BufferPolicy::Lossless);
            outgoing
                .entry((port.node.clone(), port.port.clone()))
                .or_default()
                .push(QueueSink {
                    queue: Arc::clone(&queue),
                    input: None,
                });
            terminals.push((port, Arc::clone(&queue)));
            all_queues.push(queue);
        }

        let external_cancelled = Arc::clone(&options.cancelled);
        let cancelled = Arc::new(AtomicBool::new(false));
        let mut scope =
            TaskScope::new_with_cancellation(options.max_parallel, None, Arc::clone(&cancelled))?;
        let worker_error = Arc::new(Mutex::new(None));
        let context = GraphContext {
            graph_name: plan.name.clone(),
            generation: options.generation,
            revision: options.revision,
            deadline: options.deadline,
            metrics: Arc::new(GraphMetrics::default()),
        };
        let sequence = Arc::new(AtomicU64::new(1));
        let mut tasks = Vec::with_capacity(plan.nodes().len());
        for node in plan.nodes() {
            let mut node_inputs = BTreeMap::new();
            for name in node.inputs.keys() {
                if let Some(queues) = incoming.get(&(node.id.clone(), name.clone())) {
                    node_inputs.insert(name.clone(), queues.clone());
                }
            }
            let initial_inputs = plan
                .initial_inputs()
                .get(&node.id)
                .cloned()
                .unwrap_or_default();
            let input_names = cached_input_names(&node_inputs, &initial_inputs);
            let active_input_names = (0..input_names.len()).collect();
            let input_queue_cursors = vec![0; input_names.len()];
            let mut node_outputs = BTreeMap::new();
            let mut schemas = BTreeMap::new();
            for (name, schema) in node.outputs.iter().chain(node.events.iter()) {
                node_outputs.insert(
                    name.clone(),
                    outgoing
                        .get(&(node.id.clone(), name.clone()))
                        .cloned()
                        .unwrap_or_default(),
                );
                schemas.insert(name.clone(), schema.clone());
            }
            let mut io = NodeIo {
                node: node.id.clone(),
                inputs: node_inputs,
                input_schemas: node.inputs.clone(),
                initial_inputs,
                input_names,
                active_input_names,
                input_name_cursor: 0,
                input_queue_cursors,
                output_sinks: node_outputs,
                schemas,
                generation: context.generation,
                revision: context.revision,
                next_sequence: Arc::clone(&sequence),
                token: scope.cancellation_token(),
                external_cancelled: Arc::clone(&external_cancelled),
                deadline: context.deadline,
                input_sensitivity: initial_input_sensitivity(node),
                max_message_bytes: options.max_message_bytes,
                recording: None,
                effects_started: None,
            };
            let node_owned = node.clone();
            let executor = Arc::clone(&executor);
            let context = context.clone();
            let queues = outgoing
                .iter()
                .filter(|((node_id, _), _)| node_id == &node.id)
                .flat_map(|(_, sinks)| sinks.iter().map(|sink| Arc::clone(&sink.queue)))
                .collect::<Vec<_>>();
            let cancel_on_error = Arc::clone(&cancelled);
            let worker_error_on_error = Arc::clone(&worker_error);
            let handle = scope.spawn(move |token| {
                #[cfg(target_os = "linux")]
                let _sandbox_probe_run = crate::platform::sandbox::begin_probe_run();
                io.token = token;
                let _close = CloseQueues(queues);
                let result = executor.execute(&node_owned, &mut io, &context);
                if result.is_err() {
                    if let Err(error) = &result {
                        if let Ok(mut first) = worker_error_on_error.lock() {
                            first.get_or_insert_with(|| format!("{error:#}"));
                        }
                    }
                    cancel_on_error.store(true, Ordering::Release);
                }
                result
            })?;
            tasks.push(Some(handle));
        }

        Ok(Self {
            context,
            scope,
            tasks,
            terminals,
            all_queues,
            cancelled,
            external_cancelled,
            worker_error,
            options,
        })
    }

    pub fn context(&self) -> &GraphContext {
        &self.context
    }

    /// Read one terminal output without allowing stale revisions into the
    /// current session. Returns `None` once every terminal has closed.
    pub fn recv_output(&mut self) -> Result<Option<EventEnvelope>> {
        loop {
            self.poll_finished_workers()?;
            self.cancellation_check()?;
            for (_, queue) in &self.terminals {
                if let Some(event) = queue.try_recv()? {
                    if event.generation == self.context.generation
                        && event.revision == self.context.revision
                    {
                        return Ok(Some(event));
                    }
                }
            }
            if self
                .terminals
                .iter()
                .map(|(_, queue)| queue.is_closed_and_empty())
                .collect::<Result<Vec<_>>>()?
                .iter()
                .all(|closed| *closed)
            {
                return Ok(None);
            }
            if self.terminals.is_empty() {
                return Ok(None);
            }
            thread::sleep(Duration::from_millis(2));
        }
    }

    pub fn cancel(&mut self) {
        self.cancelled.store(true, Ordering::Release);
        for queue in &self.all_queues {
            queue.close();
        }
    }

    pub fn cancel_and_join(&mut self) -> Result<()> {
        self.cancel();
        self.scope.cancel_and_join()
    }

    /// Consume, stop, and reap this session before starting a new generation.
    /// Static admission errors are checked first; because this method consumes
    /// the session, an error drops the old generation. Use `try_reconfigure`
    /// when the caller needs it to remain live after a preflight rejection.
    pub fn reconfigure(
        mut self,
        plan: GraphPlan,
        executor: Arc<dyn GraphExecutor>,
        mut options: GraphRunOptions,
    ) -> Result<Self> {
        let generation = self
            .context
            .generation
            .checked_add(1)
            .ok_or_else(|| anyhow!("graph generation exhausted"))?;
        let revision = self
            .context
            .revision
            .checked_add(1)
            .ok_or_else(|| anyhow!("graph revision exhausted"))?;
        options.generation = generation;
        options.revision = revision;
        preflight_graph_admission(&plan, &options)?;
        self.cancel_and_join()?;
        Self::start(plan, executor, options)
    }

    /// Reconfigure this session in place. A static preflight error leaves the
    /// current generation live. After admission succeeds, the current
    /// generation is cancelled and reaped before replacement activation; an
    /// activation failure after that point is not transactional.
    pub fn try_reconfigure(
        &mut self,
        plan: GraphPlan,
        executor: Arc<dyn GraphExecutor>,
        mut options: GraphRunOptions,
    ) -> Result<()> {
        options.generation = self
            .context
            .generation
            .checked_add(1)
            .ok_or_else(|| anyhow!("graph generation exhausted"))?;
        options.revision = self
            .context
            .revision
            .checked_add(1)
            .ok_or_else(|| anyhow!("graph revision exhausted"))?;
        preflight_graph_admission(&plan, &options)?;
        self.cancel_and_join()?;
        *self = Self::start(plan, executor, options)?;
        Ok(())
    }

    /// Drain terminal outputs while polling all workers, then join every task.
    /// Use this for finite graphs; persistent producers should be kept as a
    /// `GraphSession` and consumed through `recv_output` until cancelled.
    pub fn run_to_completion(mut self) -> Result<GraphRunResult> {
        let mut outputs = Vec::new();
        let mut collected_bytes = 0usize;
        let mut first_error = None;
        let mut remaining = self.tasks.iter().filter(|task| task.is_some()).count();
        while remaining > 0 {
            let mut made_progress = false;
            if self.external_cancelled.load(Ordering::Acquire) {
                self.cancelled.store(true, Ordering::Release);
                for queue in &self.all_queues {
                    queue.close();
                }
            }
            if self
                .context
                .deadline
                .is_some_and(|deadline| Instant::now() >= deadline)
            {
                self.cancelled.store(true, Ordering::Release);
                for queue in &self.all_queues {
                    queue.close();
                }
                first_error.get_or_insert_with(|| anyhow!("graph execution deadline exceeded"));
            }
            for (_, queue) in &self.terminals {
                if let Some(event) = queue.try_recv()? {
                    // Count queue progress before rejecting stale revisions so
                    // one event per terminal drains on each pass without idle sleeps.
                    made_progress = true;
                    if event.generation == self.context.generation
                        && event.revision == self.context.revision
                    {
                        let event_bytes = event_size(&event)?;
                        if outputs.len() >= self.options.max_collected_events
                            || collected_bytes.saturating_add(event_bytes)
                                > self.options.max_collected_bytes
                        {
                            self.cancelled.store(true, Ordering::Release);
                            for queue in &self.all_queues {
                                queue.close();
                            }
                            let _ = self.scope.cancel_and_join();
                            bail!("graph collected output exceeded configured event or byte limit");
                        }
                        collected_bytes = collected_bytes.saturating_add(event_bytes);
                        outputs.push(event);
                    }
                }
            }
            for task in &mut self.tasks {
                let Some(handle) = task.as_mut() else {
                    continue;
                };
                if !handle.is_finished() {
                    continue;
                }
                match self.scope.try_join(handle) {
                    Ok(Some(())) => {
                        made_progress = true;
                        *task = None;
                        remaining -= 1;
                    }
                    Ok(None) => {}
                    Err(error) => {
                        made_progress = true;
                        first_error.get_or_insert(error);
                        self.cancelled.store(true, Ordering::Release);
                        *task = None;
                        remaining -= 1;
                    }
                }
            }
            if remaining > 0 && !made_progress {
                thread::sleep(Duration::from_millis(2));
            }
        }
        for (_, queue) in &self.terminals {
            while let Some(event) = queue.try_recv()? {
                if event.generation == self.context.generation
                    && event.revision == self.context.revision
                {
                    let event_bytes = event_size(&event)?;
                    if outputs.len() >= self.options.max_collected_events
                        || collected_bytes.saturating_add(event_bytes)
                            > self.options.max_collected_bytes
                    {
                        self.cancelled.store(true, Ordering::Release);
                        for queue in &self.all_queues {
                            queue.close();
                        }
                        let _ = self.scope.cancel_and_join();
                        bail!("graph collected output exceeded configured event or byte limit");
                    }
                    collected_bytes = collected_bytes.saturating_add(event_bytes);
                    outputs.push(event);
                }
            }
        }
        let dropped_messages = self
            .all_queues
            .iter()
            .map(|queue| queue.dropped.load(Ordering::Relaxed))
            .sum();
        if let Some(error) = first_error {
            let _ = self.scope.cancel_and_join();
            return Err(error);
        }
        if self.external_cancelled.load(Ordering::Acquire) {
            let _ = self.scope.cancel_and_join();
            bail!("graph session cancelled");
        }
        if self
            .context
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            let _ = self.scope.cancel_and_join();
            bail!("graph execution deadline exceeded");
        }
        self.scope.join_all()?;
        outputs.sort_by_key(|event| event.sequence);
        Ok(GraphRunResult {
            generation: self.context.generation,
            revision: self.context.revision,
            outputs,
            dropped_messages,
            launched_processes: self.context.metrics.launched_processes(),
        })
    }

    fn cancellation_check(&self) -> Result<()> {
        if let Ok(error) = self.worker_error.lock() {
            if let Some(error) = error.as_ref() {
                bail!("graph worker failed: {error}");
            }
        }
        if self.external_cancelled.load(Ordering::Acquire) {
            self.cancelled.store(true, Ordering::Release);
            for queue in &self.all_queues {
                queue.close();
            }
        }
        if self
            .context
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            self.cancelled.store(true, Ordering::Release);
            for queue in &self.all_queues {
                queue.close();
            }
            bail!("graph execution deadline exceeded");
        }
        if self.cancelled.load(Ordering::Acquire) {
            bail!("graph session cancelled")
        }
        Ok(())
    }

    fn poll_finished_workers(&mut self) -> Result<()> {
        let mut failed = None;
        for index in 0..self.tasks.len() {
            let Some(handle) = self.tasks[index].as_mut() else {
                continue;
            };
            if !handle.is_finished() {
                continue;
            }
            match self.scope.try_join(handle) {
                Ok(Some(())) => self.tasks[index] = None,
                Ok(None) => {}
                Err(error) => {
                    failed.get_or_insert_with(|| format!("{error:#}"));
                    self.tasks[index] = None;
                }
            }
        }
        if let Some(error) = failed {
            if let Ok(mut first) = self.worker_error.lock() {
                first.get_or_insert(error);
            }
            self.cancelled.store(true, Ordering::Release);
            for queue in &self.all_queues {
                queue.close();
            }
        }
        Ok(())
    }
}

impl Drop for GraphSession {
    fn drop(&mut self) {
        self.cancel();
    }
}

fn event_size(event: &EventEnvelope) -> Result<usize> {
    payload_size(&event.payload)
}

fn initial_input_sensitivity(node: &GraphNode) -> Option<SensitivitySet> {
    let call = match &node.kind {
        GraphNodeKind::Applet { call }
        | GraphNodeKind::AppletResultEvent { call, .. }
        | GraphNodeKind::ProgramService { call } => call,
        _ => return None,
    };
    let labels = call
        .parameters
        .keys()
        .filter_map(|name| node.inputs.get(name).map(|port| port.sensitivity))
        .collect::<Vec<_>>();
    (!labels.is_empty()).then(|| SensitivitySet::from_labels(labels))
}

fn service_cancellation_failure(message: &str) -> anyhow::Error {
    crate::runtime::execution_result::ExecutionFailure::new(
        crate::runtime::execution_result::ExecutionResult::runtime_failure(
            crate::runtime::execution_result::ExecutionKind::Cancelled,
            message,
            crate::contracts::EffectCertainty::Uncertain,
        ),
    )
    .into()
}

struct JsonSizeCounter {
    bytes: usize,
}

impl std::io::Write for JsonSizeCounter {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.bytes = self
            .bytes
            .checked_add(buffer.len())
            .ok_or_else(|| std::io::Error::other("serialized JSON size overflow"))?;
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn serialized_json_size(value: &impl Serialize) -> Result<usize> {
    let mut counter = JsonSizeCounter { bytes: 0 };
    serde_json::to_writer(&mut counter, value)?;
    Ok(counter.bytes)
}

fn payload_size(payload: &PortValue) -> Result<usize> {
    match payload {
        PortValue::Bytes(bytes) => Ok(bytes.len()),
        PortValue::Value(value) => serialized_json_size(value),
    }
}

#[cfg(test)]
mod payload_size_tests {
    use super::*;
    use crate::contracts::ValueSchema;

    fn scalar_schema(kind: OutputType) -> ValueSchema {
        ValueSchema {
            min_items: None,
            scalar_only: false,
            additional_properties: None,
            min_properties: None,
            max_properties: None,
            nonempty: false,
            kind,
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

    fn value_cases() -> Vec<Value> {
        vec![
            Value::String("é🦀\n\t\0\"\\".into()),
            Value::Integer(i64::MIN),
            Value::Bool(true),
            Value::Null,
            Value::Number(serde_json::Number::from_f64(1.25).unwrap()),
            Value::Reference("node.port".into()),
            Value::List(vec![Value::Integer(1), Value::String("line\nfeed".into())]),
            Value::Record(BTreeMap::from([
                ("escaped\"key".into(), Value::String("\\\t".into())),
                (
                    "nested".into(),
                    Value::List(vec![Value::Bool(false), Value::Null]),
                ),
            ])),
        ]
    }

    fn node_io(max_message_bytes: usize) -> (TaskScope, NodeIo, Arc<BoundedQueue>) {
        let scope = TaskScope::new(1, None).unwrap();
        let token = scope.cancellation_token();
        let queue = BoundedQueue::new(1, BufferPolicy::Lossless);
        let sink = QueueSink {
            queue: Arc::clone(&queue),
            input: Some(PortRef::new("sink", "in")),
        };
        let cancelled = token.cancellation_flag();
        let io = NodeIo {
            node: "emitter".into(),
            inputs: BTreeMap::new(),
            input_schemas: BTreeMap::new(),
            initial_inputs: BTreeMap::new(),
            input_names: Vec::new(),
            active_input_names: Vec::new(),
            input_name_cursor: 0,
            input_queue_cursors: Vec::new(),
            output_sinks: BTreeMap::from([("out".into(), vec![sink])]),
            schemas: BTreeMap::from([(
                "out".into(),
                PortSchema::events(scalar_schema(OutputType::Text)),
            )]),
            generation: 1,
            revision: 1,
            next_sequence: Arc::new(AtomicU64::new(1)),
            token,
            external_cancelled: cancelled,
            deadline: None,
            input_sensitivity: None,
            max_message_bytes,
            recording: None,
            effects_started: None,
        };
        (scope, io, queue)
    }

    #[test]
    fn counted_json_sizes_match_full_serde_json_bytes() {
        for value in value_cases() {
            let expected_bytes = serde_json::to_vec(&value).unwrap();
            assert_eq!(serialized_json_size(&value).unwrap(), expected_bytes.len());
            assert_eq!(
                payload_size(&PortValue::Value(value)).unwrap(),
                expected_bytes.len()
            );
        }
    }

    #[test]
    fn byte_payload_size_remains_raw_byte_length() {
        let bytes = vec![0, 255, 10, b'"', b'\\'];
        assert_eq!(
            payload_size(&PortValue::Bytes(bytes.clone())).unwrap(),
            bytes.len()
        );
    }

    #[test]
    fn emit_preserves_exact_size_boundaries_and_validation_order() {
        let value = Value::String("é🦀\n\t\0\"\\".into());
        let exact_size = serde_json::to_vec(&value).unwrap().len();

        let (_scope, mut at_limit, queue) = node_io(exact_size);
        let sequence = Arc::clone(&at_limit.next_sequence);
        let report = at_limit
            .emit("out", PortValue::Value(value.clone()))
            .unwrap();
        assert_eq!(report.delivered, 1);
        assert_eq!(sequence.load(Ordering::Relaxed), 2);
        assert_eq!(
            queue.try_recv().unwrap().unwrap().payload,
            PortValue::Value(value.clone())
        );

        let (_scope, mut below_limit, queue) = node_io(exact_size - 1);
        let sequence = Arc::clone(&below_limit.next_sequence);
        let error = below_limit
            .emit("out", PortValue::Value(value))
            .unwrap_err()
            .to_string();
        assert!(error.contains(&format!("graph message limit of {} bytes", exact_size - 1)));
        assert_eq!(sequence.load(Ordering::Relaxed), 1);
        assert!(queue.try_recv().unwrap().is_none());

        let (_scope, mut invalid_type, queue) = node_io(0);
        let sequence = Arc::clone(&invalid_type.next_sequence);
        let error = invalid_type
            .emit("out", PortValue::Value(Value::Integer(1)))
            .unwrap_err()
            .to_string();
        assert!(error.contains("payload for emitter.out"));
        assert_eq!(sequence.load(Ordering::Relaxed), 1);
        assert!(queue.try_recv().unwrap().is_none());
    }

    #[test]
    fn json_size_counter_rejects_usize_overflow() {
        let mut counter = JsonSizeCounter { bytes: usize::MAX };
        assert!(std::io::Write::write(&mut counter, &[0]).is_err());
        assert_eq!(counter.bytes, usize::MAX);
    }
}

fn next_sequence(sequence: &AtomicU64) -> Result<u64> {
    loop {
        let current = sequence.load(Ordering::Relaxed);
        let next = current
            .checked_add(1)
            .ok_or_else(|| anyhow!("graph event sequence exhausted"))?;
        if sequence
            .compare_exchange_weak(current, next, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            return Ok(current);
        }
    }
}

/// Default backend-neutral executor for applet, timer, listener, and native
/// stream nodes. Renderer nodes are supplied by desktop/remote adapters.
pub struct RuntimeGraphExecutor {
    registry: Registry,
    options: RunOptions,
    module_session: Option<Arc<crate::modules::ProgramModuleSession>>,
    stdin_source: Option<Arc<dyn GraphStdinSource>>,
    resolver_helper: Option<PathBuf>,
}

impl RuntimeGraphExecutor {
    pub fn new(registry: Registry, options: RunOptions) -> Self {
        Self {
            registry,
            options,
            module_session: None,
            stdin_source: None,
            resolver_helper: None,
        }
    }

    pub fn with_module_session(
        registry: Registry,
        options: RunOptions,
        module_session: Arc<crate::modules::ProgramModuleSession>,
    ) -> Self {
        Self {
            registry,
            options,
            module_session: Some(module_session),
            stdin_source: None,
            resolver_helper: None,
        }
    }

    pub fn with_stdin_source(mut self, source: Arc<dyn GraphStdinSource>) -> Self {
        self.stdin_source = Some(source);
        self
    }

    /// Select the executable used for cancellation-supervised DNS helper
    /// processes. Embedders normally rely on the current executable; this is
    /// useful when a graph executor is hosted by a test runner or wrapper.
    pub fn with_resolver_helper(mut self, executable: impl Into<PathBuf>) -> Self {
        self.resolver_helper = Some(executable.into());
        self
    }

    fn run_applet(
        &self,
        node: &GraphNode,
        call: &Invocation,
        io: &mut NodeIo,
        context: &GraphContext,
    ) -> Result<()> {
        let mut parameters = call.parameters.clone();
        let mut stdin = Vec::new();
        let mut streams = Vec::new();
        for (name, schema) in &node.inputs {
            if !io.has_input(name) || parameters.contains_key(name) {
                continue;
            }
            match schema.delivery {
                PortDelivery::Single => {
                    if let Some(event) = io.recv(name)? {
                        apply_input(&mut parameters, &mut stdin, name, schema, event.payload)?;
                    } else if schema.required {
                        bail!(
                            "required graph input {}.{} closed without a value",
                            node.id,
                            name
                        );
                    }
                }
                PortDelivery::Records | PortDelivery::Events => streams.push(name.clone()),
            }
        }
        if streams.is_empty() {
            let (input_sensitivities, invocation_sensitivities) =
                self.invocation_sensitivity_sets(node, call, io)?;
            self.authorize_remote_input(call, &input_sensitivities)?;
            io.mark_effects_started()?;
            let options = self.invocation_options(context)?;
            let result = account_invocation_processes(
                invoke_for_graph(
                    &self.registry,
                    &call.operation,
                    parameters,
                    stdin,
                    GraphInvocationContext {
                        options: &options,
                        cancelled: io.cancellation_flag(),
                        module_session: self.module_session.as_ref(),
                        sensitivities: invocation_sensitivities,
                    },
                ),
                &context.metrics,
            )?;
            emit_applet_result(node, &result, io, &self.registry)?;
            return Ok(());
        }
        let mut emitted = BTreeMap::<String, usize>::new();
        let mut received = 0usize;
        loop {
            let Some(event) = io.recv_any()? else {
                break;
            };
            let Some(input) = event.input.as_ref().map(|reference| reference.port.clone()) else {
                bail!("graph input message has no destination port");
            };
            if !streams.contains(&input) {
                continue;
            }
            let schema = node.inputs.get(&input).expect("stream input declared");
            apply_input(&mut parameters, &mut stdin, &input, schema, event.payload)?;
            received = received.saturating_add(1);
            let (input_sensitivities, invocation_sensitivities) =
                self.invocation_sensitivity_sets(node, call, io)?;
            self.authorize_remote_input(call, &input_sensitivities)?;
            io.mark_effects_started()?;
            let options = self.invocation_options(context)?;
            let result = account_invocation_processes(
                invoke_for_graph(
                    &self.registry,
                    &call.operation,
                    parameters.clone(),
                    std::mem::take(&mut stdin),
                    GraphInvocationContext {
                        options: &options,
                        cancelled: io.cancellation_flag(),
                        module_session: self.module_session.as_ref(),
                        sensitivities: invocation_sensitivities,
                    },
                ),
                &context.metrics,
            )?;
            emit_applet_result_counted(node, &result, io, &mut emitted, &self.registry)?;
        }
        if received == 0 && streams.iter().any(|name| node.inputs[name].required) {
            bail!(
                "required graph stream input on node {:?} closed without a value",
                node.id
            );
        }
        Ok(())
    }

    fn run_program_service(
        &self,
        node: &GraphNode,
        call: &Invocation,
        io: &mut NodeIo,
        context: &GraphContext,
    ) -> Result<()> {
        let manifest = self.registry.resolve_call(call)?;
        let session = self.module_session.as_ref().ok_or_else(|| {
            anyhow!(
                "graph program service node {:?} requires a preloaded program module session",
                node.id
            )
        })?;
        let loaded = session
            .get_for_manifest(&self.registry, manifest)
            .cloned()
            .ok_or_else(|| {
                anyhow!(
                    "program module source for graph service {:?} was not preloaded before graph actors started",
                    manifest.name
                )
            })?;
        if !loaded.is_program_service() {
            bail!(
                "graph node {:?} resolves to a finite program module, not a persistent service",
                node.id
            );
        }
        let service_registry = session
            .registry_for(&self.registry, manifest)
            .cloned()
            .ok_or_else(|| {
                anyhow!(
                    "program module registry scope for graph service {:?} was not preloaded",
                    manifest.name
                )
            })?;
        if !node.outputs.is_empty() || node.events.len() != 1 {
            bail!(
                "graph program service node {:?} must expose exactly one event and no finite outputs",
                node.id
            );
        }
        let event_port = node
            .events
            .keys()
            .next()
            .expect("the service event count was checked above")
            .clone();
        let mut parameters = call.parameters.clone();
        for (name, schema) in &node.inputs {
            if !io.has_input(name) {
                if schema.required && !parameters.contains_key(name) {
                    bail!("required graph input {}.{} was not supplied", node.id, name);
                }
                continue;
            }
            match io.recv(name)? {
                Some(event) => match event.payload {
                    PortValue::Value(value) => {
                        parameters.insert(name.clone(), value);
                    }
                    PortValue::Bytes(_) => {
                        bail!(
                            "program service startup input {}.{} must be a typed value",
                            node.id,
                            name
                        )
                    }
                },
                None if schema.required => {
                    bail!(
                        "required graph input {}.{} closed without a value",
                        node.id,
                        name
                    )
                }
                None => {}
            }
        }

        let (input_sensitivities, invocation_sensitivities) =
            self.invocation_sensitivity_sets(node, call, io)?;
        self.authorize_remote_input(call, &input_sensitivities)?;
        let mut options = self.invocation_options(context)?;
        options.inputs = parameters;
        let output_status = manifest.success_status().unwrap_or(0);
        let outcome = manifest.outcome_for(output_status);
        let metrics = Arc::clone(&context.metrics);
        let process_observer: runtime::ProcessObserver =
            Arc::new(move |count| metrics.add_processes(count));
        let cancelled = io.cancellation_flag();
        let deadline = context.deadline;
        crate::runtime::run_program_module_service(
            &loaded,
            &service_registry,
            &options,
            crate::runtime::ProgramServiceContext {
                cancelled: Arc::clone(&cancelled),
                deadline,
                module_session: Arc::clone(session),
                additional_sensitivity: invocation_sensitivities,
                process_observer,
            },
            |bytes, run_cancelled, run_deadline| {
                if run_cancelled.load(Ordering::Acquire) {
                    return Err(service_cancellation_failure(
                        "graph-owned program service cancelled before event emission",
                    ));
                }
                if run_deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                    bail!("graph execution deadline exceeded before service event emission");
                }
                let decoded = manifest
                    .decode_output(bytes, output_status)
                    .map_err(|error| {
                        crate::runtime::execution_result::contract_decode_failure(
                            &error,
                            outcome.clone(),
                            Some(bytes.to_vec()),
                            None,
                            Vec::new(),
                        )
                    })?;
                let payload = match decoded {
                    crate::contracts::AppletOutput::Value(value) => PortValue::Value(value),
                    crate::contracts::AppletOutput::Bytes(_) => {
                        let error = anyhow!(
                            "program service {:?} decoded output as raw bytes; its event port requires a typed value",
                            manifest.name
                        );
                        return Err(crate::runtime::execution_result::contract_decode_failure(
                            &error,
                            outcome.clone(),
                            Some(bytes.to_vec()),
                            None,
                            Vec::new(),
                        )
                        .into());
                    }
                };
                if let Err(error) = io.emit(&event_port, payload) {
                    if run_cancelled.load(Ordering::Acquire)
                        && run_deadline.is_none_or(|deadline| Instant::now() < deadline)
                    {
                        return Err(service_cancellation_failure(
                            "graph-owned program service cancelled while emitting an event",
                        ));
                    }
                    return Err(error).context(format!(
                        "emitting program service event {}.{}",
                        node.id, event_port
                    ));
                }
                Ok(())
            },
        )?;
        Ok(())
    }

    fn invocation_sensitivity_sets(
        &self,
        node: &GraphNode,
        call: &Invocation,
        io: &mut NodeIo,
    ) -> Result<(SensitivitySet, SensitivitySet)> {
        let manifest = self.registry.resolve_call(call)?;
        let ports = manifest
            .execution_contract()
            .and_then(|contract| contract.ports.as_ref());
        let mut sensitivities = io
            .input_sensitivity()
            .cloned()
            .unwrap_or_else(SensitivitySet::empty);

        // A pooled JSON-lines worker can return process-local state from an
        // earlier runtime or graph session. Include matching launch
        // metadata's retained labels in both the invocation floor and the
        // NodeIo provenance before dispatch, so downstream graph edges and
        // remote authorization see the same history as the worker request.
        let cwd = std::fs::canonicalize(&self.options.working_directory)?;
        let worker_history = crate::runtime::worker_sensitivity_history_for_call(
            call,
            manifest,
            &self.registry,
            &cwd,
        )?;
        sensitivities.extend(&worker_history);

        // Include literal parameters even for older applet contracts that do
        // not declare graph ports. Such values remain Unknown until labeled.
        for name in call.parameters.keys() {
            let sensitivity = node
                .inputs
                .get(name)
                .map(|port| port.sensitivity)
                .or_else(|| {
                    ports
                        .and_then(|ports| ports.inputs.get(name))
                        .map(|port| port.sensitivity)
                })
                .unwrap_or(Sensitivity::Unknown);
            sensitivities.insert(sensitivity);
        }
        if crate::security::applet_has_declared_secret_environment(&self.registry, &manifest.name) {
            sensitivities.insert(Sensitivity::Secret);
        }
        if sensitivities.is_empty() {
            sensitivities.insert(Sensitivity::Unknown);
        }
        let input_sensitivities = sensitivities;
        let mut invocation_sensitivities = input_sensitivities.clone();
        for port in node.outputs.values().chain(node.events.values()) {
            invocation_sensitivities.insert(port.sensitivity);
        }
        if let Some(outputs) = ports.map(|ports| &ports.outputs) {
            for port in outputs.values() {
                invocation_sensitivities.insert(port.sensitivity);
            }
        }
        io.include_sensitivity_provenance(&input_sensitivities);
        Ok((input_sensitivities, invocation_sensitivities))
    }

    fn authorize_remote_input(
        &self,
        call: &Invocation,
        sensitivities: &SensitivitySet,
    ) -> Result<()> {
        let manifest = self.registry.resolve_call(call)?;
        let Some(remote) = manifest
            .execution_contract()
            .and_then(|contract| contract.remote.as_ref())
        else {
            return Ok(());
        };
        crate::security::authorize_data_flow_set(
            &self.registry,
            &manifest.name,
            &remote.endpoint,
            sensitivities,
        )
        .map_err(|error| {
            anyhow!(
                "runtime data flow into remote applet {:?} was denied: {error}",
                manifest.name
            )
        })?;
        Ok(())
    }

    fn invocation_options(&self, context: &GraphContext) -> Result<RunOptions> {
        let mut options = self.options.clone();
        if let Some(deadline) = context.deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                bail!("graph execution deadline exceeded");
            }
            options.total_timeout = Some(remaining);
        }
        Ok(options)
    }

    fn run_stream(
        &self,
        node: &GraphNode,
        source: &str,
        io: &mut NodeIo,
        context: &GraphContext,
        emit_status: bool,
    ) -> Result<()> {
        let stream = crate::stream::parse(source).context("parsing graph native stream")?;
        crate::stream::validate(&stream, &self.registry)?;
        // A native stream is one graph node but may invoke several applets.
        // Preserve every declared stage label in graph events emitted from
        // the stream, including applet-scoped secret environment inputs.
        let mut sensitivity_floor = SensitivitySet::empty();
        for call in node.invocations() {
            let (_, sensitivities) = self.invocation_sensitivity_sets(node, call, io)?;
            sensitivity_floor.extend(&sensitivities);
            io.include_sensitivity_provenance(&sensitivities);
        }
        let final_call = stream
            .pipeline
            .steps
            .last()
            .and_then(|step| match &step.kind {
                StepKind::Invoke { call, .. } => Some(call),
                _ => None,
            })
            .ok_or_else(|| anyhow!("validated graph stream has no final applet"))?;
        let output_schema = self
            .registry
            .resolve_call(final_call)?
            .execution_contract()
            .and_then(|contract| contract.output.clone());
        let (sink_port, sink_schema, expected_delivery, raw_listener_records) = match &node.kind {
            GraphNodeKind::Listener {
                source: ListenSource::Applet(_),
                binding,
            } => (
                binding.as_str(),
                node.events.get(binding).ok_or_else(|| {
                    anyhow!("graph listener {:?} has no line event port", node.id)
                })?,
                PortDelivery::Events,
                true,
            ),
            _ => (
                "stdout",
                node.outputs
                    .get("stdout")
                    .ok_or_else(|| anyhow!("graph stream node {:?} has no stdout port", node.id))?,
                PortDelivery::Records,
                false,
            ),
        };
        let expected_schema = output_schema
            .as_ref()
            .map(super::plan::value_schema_from_output);
        if sink_schema.delivery != expected_delivery
            || (raw_listener_records && sink_schema.schema.kind != OutputType::Bytes)
            || (!raw_listener_records
                && (expected_schema
                    .as_ref()
                    .is_some_and(|schema| &sink_schema.schema != schema)
                    || (expected_schema.is_none() && sink_schema.schema.kind != OutputType::Bytes)))
        {
            bail!(
                "graph stream node {:?} stdout port does not match its final applet output contract",
                node.id
            );
        }
        let cancelled = io.cancellation_flag();
        let mut stream_context = runtime::StreamExecutionContext::new(Arc::clone(&cancelled))
            .with_deadline(context.deadline)
            .with_sensitivity_floor(sensitivity_floor);
        if let Some(module_session) = &self.module_session {
            stream_context = stream_context.with_module_session(Arc::clone(module_session));
        }
        let options = self.invocation_options(context)?;
        let metrics = Arc::clone(&context.metrics);
        let process_observer: runtime::ProcessObserver =
            Arc::new(move |count| metrics.add_processes(count));
        runtime::run_stream_with_context_and_process_observer(
            &stream,
            &self.registry,
            &options,
            self.options.max_output_bytes,
            stream_context,
            Some(process_observer),
            |record, run_cancelled, deadline| {
                if run_cancelled.load(Ordering::Acquire) {
                    bail!("graph stream cancelled");
                }
                if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                    bail!("graph execution deadline exceeded");
                }
                let payload = if raw_listener_records {
                    PortValue::Bytes(record.to_vec())
                } else {
                    match output_schema.as_ref() {
                        Some(schema) => match schema.decode(record)? {
                            crate::contracts::AppletOutput::Bytes(bytes) => PortValue::Bytes(bytes),
                            crate::contracts::AppletOutput::Value(value) => PortValue::Value(value),
                        },
                        None => PortValue::Bytes(record.to_vec()),
                    }
                };
                io.emit(sink_port, payload)?;
                Ok(())
            },
        )?;
        if emit_status && node.outputs.contains_key("status") {
            io.emit("status", PortValue::Value(Value::Integer(0)))?;
        }
        Ok(())
    }

    fn run_stdin_listener(
        &self,
        node: &GraphNode,
        binding: &str,
        io: &mut NodeIo,
        context: &GraphContext,
    ) -> Result<()> {
        let source = self
            .stdin_source
            .as_ref()
            .ok_or_else(|| anyhow!("graph stdin listener has no injected GraphStdinSource"))?;
        if !node.events.contains_key(binding) {
            bail!("stdin listener {:?} has no event port {binding:?}", node.id);
        }
        let cancelled = io.cancellation_flag();
        loop {
            if context
                .deadline
                .is_some_and(|deadline| Instant::now() >= deadline)
            {
                bail!("graph execution deadline exceeded");
            }
            if io.is_cancelled() {
                return Ok(());
            }
            let record = match source.next_record(&cancelled, context.deadline) {
                Ok(record) => record,
                Err(_) if io.is_cancelled() => return Ok(()),
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("reading stdin source for graph node {:?}", node.id)
                    });
                }
            };
            let Some(record) = record else { return Ok(()) };
            io.emit(binding, PortValue::Bytes(record))?;
        }
    }

    #[cfg(feature = "remote")]
    fn run_remote_listener(
        &self,
        node: &GraphNode,
        call: &Invocation,
        io: &mut NodeIo,
        context: &GraphContext,
    ) -> Result<()> {
        use crate::contracts::{AppletBackend, RemoteAuthSource};
        use crate::remote::RemoteClient;

        if context
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            bail!("graph execution deadline exceeded");
        }
        if io.is_cancelled() {
            return Ok(());
        }
        if !call.parameters.is_empty() {
            bail!(
                "remote event listener {:?} cannot pass applet parameters through the GET event protocol",
                node.id
            );
        }
        let manifest = self.registry.resolve_call(call)?;
        let contract = manifest
            .execution_contract()
            .context("remote event listener requires an execution contract")?;
        if contract.backend != AppletBackend::Remote {
            bail!("listener source {:?} is not remote", manifest.name);
        }
        let endpoint = contract
            .remote
            .as_ref()
            .context("remote event listener requires endpoint configuration")?;
        crate::capabilities::authorize_network_endpoint(
            &self.registry,
            &manifest.name,
            &endpoint.endpoint,
        )?;
        if let Some(RemoteAuthSource::BearerEnv { name }) = endpoint.auth.as_ref() {
            crate::capabilities::authorize_environment_variable(
                &self.registry,
                &manifest.name,
                name,
            )?;
        }
        if endpoint.auth.is_some() {
            let credential_sensitivity = SensitivitySet::from_label(Sensitivity::Secret);
            crate::security::authorize_data_flow_set(
                &self.registry,
                &manifest.name,
                &endpoint.endpoint,
                &credential_sensitivity,
            )
            .map_err(|error| {
                anyhow!(
                    "runtime authentication data flow into remote listener {:?} was denied: {error}",
                    node.id
                )
            })?;
        }

        let options = self.invocation_options(context)?;
        let cwd = std::fs::canonicalize(&options.working_directory)?;
        let credential_base = manifest
            .origin()
            .and_then(std::path::Path::parent)
            .unwrap_or(&cwd)
            .to_path_buf();
        let mut config = endpoint.clone();
        if let Some(timeout) = options.total_timeout {
            let remaining_ms = timeout.as_millis().clamp(1, u128::from(u64::MAX)) as u64;
            config.timeout_ms = config.timeout_ms.min(remaining_ms);
        }
        let cancelled = io.cancellation_flag();
        let metrics = Arc::clone(&context.metrics);
        let process_observer: runtime::ProcessObserver =
            Arc::new(move |count| metrics.add_processes(count));
        let resolver_helper = match &self.resolver_helper {
            Some(path) => path.clone(),
            None => std::env::current_exe()?,
        };
        let client = RemoteClient::new_with_credential_base(config, credential_base)?
            .with_secret_redactor(crate::security::secret_redactor(&self.registry))
            .with_resolver_helper(resolver_helper)
            .with_process_observer(process_observer)
            .with_ssrf_grant(crate::capabilities::remote_origin_is_explicitly_granted(
                &self.registry,
                &manifest.name,
                &endpoint.endpoint,
            ));
        if let Some(path) = client.descriptor_cache_path() {
            crate::capabilities::authorize_applet_host_path(
                &self.registry,
                &manifest.name,
                &path,
                crate::capabilities::HostAccess::Read,
                &cwd,
            )?;
        }
        if let Some(path) = client.bearer_file_path() {
            crate::capabilities::authorize_applet_host_path(
                &self.registry,
                &manifest.name,
                &path,
                crate::capabilities::HostAccess::Read,
                &cwd,
            )?;
        }
        let descriptor = match client.load_configured_descriptor_cache()? {
            Some(descriptor) => {
                client.use_cached_descriptor(descriptor.clone())?;
                descriptor
            }
            None if endpoint.allow_discovery => {
                if context
                    .deadline
                    .is_some_and(|deadline| Instant::now() >= deadline)
                {
                    bail!("graph execution deadline exceeded");
                }
                if io.is_cancelled() {
                    return Ok(());
                }
                client.discover_cancellable(&cancelled)?
            }
            None => bail!(
                "remote event listener {:?} needs a pinned descriptor cache or allow_discovery:true",
                manifest.name
            ),
        };
        client.check_compatible(&manifest.name, contract)?;
        for name in node.events.keys() {
            if !descriptor.events.iter().any(|event| &event.port == name) {
                bail!(
                    "remote descriptor for {:?} does not expose declared event port {name:?}",
                    manifest.name
                );
            }
        }

        let ports = node.events.keys().cloned().collect::<Vec<_>>();
        if ports.is_empty() {
            bail!("remote listener {:?} has no event ports", node.id);
        }
        if context
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            bail!("graph execution deadline exceeded");
        }
        if io.is_cancelled() {
            return Ok(());
        }
        let port_count = ports.len();
        let (sender, receiver) = std::sync::mpsc::sync_channel(port_count);
        let reader_cancelled = Arc::new(AtomicBool::new(false));
        let mut scope = TaskScope::new_with_cancellation(
            port_count,
            context.deadline,
            Arc::clone(&reader_cancelled),
        )?;
        for port in ports {
            let client = client.clone();
            let descriptor = descriptor.clone();
            let sender = sender.clone();
            let cancelled = Arc::clone(&reader_cancelled);
            let deadline = context.deadline;
            scope.spawn(move |_| {
                let result = pump_remote_event_port(
                    &client,
                    &descriptor,
                    &port,
                    &sender,
                    &cancelled,
                    deadline,
                );
                let error = result.as_ref().err().map(|error| format!("{error:#}"));
                let _ = send_remote_listener_message(
                    &sender,
                    RemoteListenerMessage::Finished {
                        port: port.clone(),
                        error,
                    },
                    &cancelled,
                    deadline,
                );
                result
            })?;
        }
        drop(sender);

        let mut finished = 0usize;
        while finished < port_count {
            if context
                .deadline
                .is_some_and(|deadline| Instant::now() >= deadline)
            {
                bail!("graph execution deadline exceeded");
            }
            if io.is_cancelled() {
                scope.cancel_and_join()?;
                return Ok(());
            }
            match receiver.recv_timeout(QUEUE_POLL) {
                Ok(RemoteListenerMessage::Event { port, event }) => {
                    let payload = PortValue::Value(crate::structured::from_json(event.data));
                    io.emit_remote_event(
                        &port,
                        payload,
                        RemoteEventMetadata {
                            event: event.event,
                            cursor: event.cursor,
                            duplicate_cursor: event.duplicate_cursor,
                        },
                    )?;
                }
                Ok(RemoteListenerMessage::Finished { port, error }) => {
                    finished += 1;
                    if let Some(error) = error {
                        if !io.is_cancelled() {
                            bail!("remote event port {port:?} failed: {error}");
                        }
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    bail!("remote event readers stopped before reporting completion");
                }
            }
        }
        scope.join_all()?;
        Ok(())
    }

    #[cfg(not(feature = "remote"))]
    fn run_remote_listener(
        &self,
        node: &GraphNode,
        call: &Invocation,
        _io: &mut NodeIo,
        _context: &GraphContext,
    ) -> Result<()> {
        let _ = call;
        bail!(
            "remote event listener {:?} needs a remote backend, and this build has none; use the full build",
            node.id
        )
    }
}

#[cfg(feature = "remote")]
enum RemoteListenerMessage {
    Event {
        port: String,
        event: crate::remote::RemoteStreamEvent,
    },
    Finished {
        port: String,
        error: Option<String>,
    },
}

#[cfg(feature = "remote")]
fn pump_remote_event_port(
    client: &crate::remote::RemoteClient,
    descriptor: &crate::remote::RemoteAppletDescriptor,
    port: &str,
    sender: &std::sync::mpsc::SyncSender<RemoteListenerMessage>,
    cancelled: &Arc<AtomicBool>,
    deadline: Option<Instant>,
) -> Result<()> {
    let mut stream = client.open_events(descriptor, port, None, Arc::clone(cancelled))?;
    loop {
        if check_remote_listener_budget(cancelled, deadline).is_err() {
            if cancelled.load(Ordering::Acquire) {
                return Ok(());
            }
            bail!("remote event listener deadline exceeded");
        }
        let next = match stream.next_event() {
            Ok(next) => next,
            Err(_) if cancelled.load(Ordering::Acquire) => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        match next {
            Some(event) => {
                if let Err(error) = send_remote_listener_message(
                    sender,
                    RemoteListenerMessage::Event {
                        port: port.to_owned(),
                        event,
                    },
                    cancelled,
                    deadline,
                ) {
                    if cancelled.load(Ordering::Acquire) {
                        return Ok(());
                    }
                    return Err(error);
                }
            }
            None => return Ok(()),
        }
    }
}

#[cfg(feature = "remote")]
fn send_remote_listener_message(
    sender: &std::sync::mpsc::SyncSender<RemoteListenerMessage>,
    mut message: RemoteListenerMessage,
    cancelled: &AtomicBool,
    deadline: Option<Instant>,
) -> Result<()> {
    loop {
        check_remote_listener_budget(cancelled, deadline)?;
        match sender.try_send(message) {
            Ok(()) => return Ok(()),
            Err(std::sync::mpsc::TrySendError::Full(returned)) => {
                message = returned;
                thread::sleep(QUEUE_POLL.min(Duration::from_millis(2)));
            }
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                bail!("remote listener event receiver closed");
            }
        }
    }
}

#[cfg(feature = "remote")]
fn check_remote_listener_budget(cancelled: &AtomicBool, deadline: Option<Instant>) -> Result<()> {
    if cancelled.load(Ordering::Acquire) {
        bail!("remote event listener cancelled");
    }
    if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
        bail!("remote event listener deadline exceeded");
    }
    Ok(())
}

struct GraphInvocationContext<'a> {
    options: &'a RunOptions,
    cancelled: Arc<AtomicBool>,
    module_session: Option<&'a Arc<crate::modules::ProgramModuleSession>>,
    sensitivities: SensitivitySet,
}

fn account_invocation_processes(
    result: Result<AppletInvocationResult>,
    metrics: &GraphMetrics,
) -> Result<AppletInvocationResult> {
    match result {
        Ok(result) => {
            metrics.add_processes(result.launched_processes);
            Ok(result)
        }
        Err(error) => {
            if let Some(failure) = error.downcast_ref::<runtime::AppletInvocationError>() {
                metrics.add_processes(failure.launched_processes());
            }
            Err(error)
        }
    }
}

fn invoke_for_graph(
    registry: &Registry,
    name: &str,
    parameters: BTreeMap<String, Value>,
    input: Vec<u8>,
    context: GraphInvocationContext<'_>,
) -> Result<AppletInvocationResult> {
    let call = Invocation {
        operation: name.to_owned(),
        direct: false,
        discard_stderr: false,
        parameters: parameters.clone(),
        binding: None,
        cache: false,
        sensitive_binding: false,
        status_binding: Some("__flow_internal_status".to_owned()),
        result_binding: None,
        retry: None,
    };
    let session = match context.module_session {
        Some(session) => Arc::clone(session),
        None => Arc::new(crate::modules::ProgramModuleSession::preload_invocations(
            registry,
            std::iter::once(&call),
        )?),
    };
    runtime::invoke_applet_with_context(
        registry,
        name,
        parameters,
        input,
        context.options,
        context.cancelled,
        runtime::AppletInvocationContext::new(session, context.sensitivities),
    )
}

impl GraphExecutor for RuntimeGraphExecutor {
    fn execute(&self, node: &GraphNode, io: &mut NodeIo, context: &GraphContext) -> Result<()> {
        match &node.kind {
            GraphNodeKind::Applet { call } | GraphNodeKind::AppletResultEvent { call, .. } => {
                self.run_applet(node, call, io, context)
            }
            GraphNodeKind::ProgramService { call } => {
                self.run_program_service(node, call, io, context)
            }
            GraphNodeKind::Timer { interval_ms } => {
                let delay = Duration::from_millis(*interval_ms);
                let mut due = Instant::now()
                    .checked_add(delay)
                    .ok_or_else(|| anyhow!("timer interval exceeds monotonic clock range"))?;
                let mut tick: i64 = 0;
                while !io.is_cancelled() {
                    let remaining = due.saturating_duration_since(Instant::now());
                    if !remaining.is_zero() {
                        thread::sleep(remaining.min(QUEUE_POLL));
                        continue;
                    }
                    tick = tick
                        .checked_add(1)
                        .ok_or_else(|| anyhow!("timer tick counter exhausted"))?;
                    let port = node
                        .events
                        .keys()
                        .next()
                        .ok_or_else(|| anyhow!("timer node has no event port"))?;
                    io.emit(port, PortValue::Value(Value::Integer(tick)))?;
                    let now = Instant::now();
                    while due <= now {
                        due = due.checked_add(delay).ok_or_else(|| {
                            anyhow!("timer deadline exceeds monotonic clock range")
                        })?;
                    }
                }
                Ok(())
            }
            GraphNodeKind::NativeStream { source, .. } => {
                self.run_stream(node, source, io, context, true)
            }
            GraphNodeKind::Listener {
                source: ListenSource::Applet(call),
                binding,
            } => {
                let manifest = self.registry.resolve_call(call)?;
                if manifest.execution_backend() == crate::contracts::AppletBackend::Remote {
                    self.run_remote_listener(node, call, io, context)
                } else {
                    if !node.events.contains_key(binding) {
                        bail!("listener node {:?} has no event port {binding:?}", node.id);
                    }
                    let source =
                        format!("stream from {}\nshow each line\n", format_invocation(call)?);
                    self.run_stream(node, &source, io, context, false)
                }
            }
            GraphNodeKind::Listener {
                source: ListenSource::Stdin,
                binding,
            } => self.run_stdin_listener(node, binding, io, context),
            GraphNodeKind::Renderer { backend } => {
                bail!("renderer backend {backend:?} is not installed in this executor")
            }
        }
    }
}

fn apply_input(
    parameters: &mut BTreeMap<String, Value>,
    stdin: &mut Vec<u8>,
    name: &str,
    schema: &PortSchema,
    payload: PortValue,
) -> Result<()> {
    if name == "stdin" {
        let bytes = match payload {
            PortValue::Bytes(bytes) => bytes,
            PortValue::Value(value) => encode_stdin_value(
                value,
                schema
                    .encoding
                    .ok_or_else(|| anyhow!("typed stdin requires an explicit encoding"))?,
            )?,
        };
        stdin.extend_from_slice(&bytes);
    } else {
        match payload {
            PortValue::Bytes(_) => {
                bail!("bytes cannot be passed through named applet parameter {name:?}")
            }
            PortValue::Value(value) => {
                parameters.insert(name.to_owned(), value);
            }
        }
    }
    Ok(())
}

fn encode_stdin_value(value: Value, encoding: OutputEncoding) -> Result<Vec<u8>> {
    match encoding {
        OutputEncoding::Raw => match value {
            Value::String(text) => Ok(text.into_bytes()),
            _ => bail!("raw stdin encoding accepts only text or bytes"),
        },
        OutputEncoding::Json => Ok(serde_json::to_vec(&value_to_json(value)?)?),
    }
}

fn value_to_json(value: Value) -> Result<serde_json::Value> {
    Ok(match value {
        Value::String(value) => value.into(),
        Value::Integer(value) => value.into(),
        Value::Bool(value) => value.into(),
        Value::Number(value) => value.into(),
        Value::List(values) => serde_json::Value::Array(
            values
                .into_iter()
                .map(value_to_json)
                .collect::<Result<_>>()?,
        ),
        Value::Record(fields) => serde_json::Value::Object(
            fields
                .into_iter()
                .map(|(name, value)| Ok((name, value_to_json(value)?)))
                .collect::<Result<_>>()?,
        ),
        Value::Null => serde_json::Value::Null,
        Value::Reference(_) => bail!("unresolved graph references cannot be encoded"),
    })
}

fn emit_applet_result(
    node: &GraphNode,
    result: &AppletInvocationResult,
    io: &mut NodeIo,
    registry: &Registry,
) -> Result<()> {
    emit_applet_result_counted(node, result, io, &mut BTreeMap::new(), registry)
}

fn emit_applet_result_counted(
    node: &GraphNode,
    result: &AppletInvocationResult,
    io: &mut NodeIo,
    emitted: &mut BTreeMap<String, usize>,
    registry: &Registry,
) -> Result<()> {
    if let GraphNodeKind::AppletResultEvent { call, event_port } = &node.kind {
        let manifest = registry.resolve_call(call)?;
        let contract = manifest.execution_contract().ok_or_else(|| {
            anyhow!(
                "result projection for node {:?} lost its applet contract",
                node.id
            )
        })?;
        if result.outcome.kind != OutcomeKind::Success && !contract.result_envelope {
            runtime::check_retained_status(
                runtime::CallSite {
                    call,
                    manifest,
                    line: node.line.unwrap_or(1),
                },
                runtime::RetainedStatus {
                    status: result.status,
                    diagnostic: &String::from_utf8_lossy(&result.stderr),
                    stdout: Some(result.output.clone()),
                    stderr: Some(result.stderr.clone()),
                },
                registry,
            )?;
            bail!(
                "result projection for node {:?} received a non-success outcome without a result envelope",
                node.id
            );
        }
        let schema = node.events.get(event_port).ok_or_else(|| {
            anyhow!(
                "result projection for node {:?} has no selected event port {:?}",
                node.id,
                event_port
            )
        })?;
        let payload = if schema.schema.kind == OutputType::Bytes {
            PortValue::Bytes(result.output.clone())
        } else {
            PortValue::Value(result.value.clone().ok_or_else(|| {
                anyhow!(
                    "applet result for node {:?} has no decoded value for event port {:?}",
                    node.id,
                    event_port
                )
            })?)
        };
        io.emit(event_port, payload)?;
        return Ok(());
    }
    for (name, schema) in &node.outputs {
        let payload = if name == "status" {
            Some(PortValue::Value(Value::Integer(i64::from(result.status))))
        } else if name == "stdout" {
            Some(if schema.schema.kind == OutputType::Bytes {
                PortValue::Bytes(result.output.clone())
            } else {
                PortValue::Value(result.value.clone().ok_or_else(|| {
                    anyhow!(
                        "applet returned bytes but graph output {name:?} requires {:?}",
                        schema.schema.kind
                    )
                })?)
            })
        } else {
            let Some(value) = result.value.as_ref() else {
                continue;
            };
            let selected = schema
                .pointer
                .as_deref()
                .map(|pointer| json_pointer(value, pointer))
                .transpose()?
                .unwrap_or(value)
                .clone();
            Some(PortValue::Value(selected))
        };
        if let Some(payload) = payload {
            let count = emitted.entry(name.clone()).or_default();
            if schema.delivery == PortDelivery::Single && *count > 0 {
                bail!("single output {}.{} emitted more than once", node.id, name);
            }
            io.emit(name, payload)?;
            *count += 1;
        }
    }
    if !node.events.is_empty() {
        bail!(
            "generic one-shot invocation does not expose backend event payloads; events require a streaming or renderer adapter"
        );
    }
    Ok(())
}

fn json_pointer<'a>(value: &'a Value, pointer: &str) -> Result<&'a Value> {
    if pointer.is_empty() {
        return Ok(value);
    }
    if !pointer.starts_with('/') {
        bail!("invalid JSON Pointer {pointer:?}");
    }
    let mut current = value;
    for token in pointer[1..].split('/') {
        let token = token.replace("~1", "/").replace("~0", "~");
        current = match current {
            Value::Record(fields) => fields
                .get(&token)
                .ok_or_else(|| anyhow!("output has no JSON Pointer field {token:?}"))?,
            Value::List(items) => items
                .get(
                    token
                        .parse::<usize>()
                        .context("invalid list index in JSON Pointer")?,
                )
                .ok_or_else(|| anyhow!("output has no JSON Pointer index {token:?}"))?,
            _ => bail!("JSON Pointer traverses a scalar at {token:?}"),
        };
    }
    Ok(current)
}

fn format_invocation(call: &Invocation) -> Result<String> {
    let mut phrase = call.operation.clone();
    if !call.parameters.is_empty() {
        phrase.push_str(" with ");
        for (index, (name, value)) in call.parameters.iter().enumerate() {
            if index > 0 {
                phrase.push_str(" and ");
            }
            phrase.push_str(name);
            phrase.push(' ');
            phrase.push_str(&value_to_source(value)?);
        }
    }
    Ok(phrase)
}

fn value_to_source(value: &Value) -> Result<String> {
    Ok(match value {
        Value::String(value) => serde_json::to_string(value)?,
        Value::Integer(value) => value.to_string(),
        Value::Bool(value) => value.to_string(),
        Value::List(values) => format!(
            "[{}]",
            values
                .iter()
                .map(value_to_source)
                .collect::<Result<Vec<_>>>()?
                .join(", ")
        ),
        Value::Record(_) | Value::Null | Value::Number(_) | Value::Reference(_) => {
            bail!(
                "listener source parameter cannot be represented by the current program invocation grammar"
            )
        }
    })
}

/// Execute a finite graph with the shared applet backend adapter.
/// Run a finite graph without checkpoints. Terminal outputs keep arrival order.
pub fn run(
    plan: GraphPlan,
    registry: &Registry,
    options: &RunOptions,
    cancelled: Arc<AtomicBool>,
) -> Result<GraphRunResult> {
    run_plan(plan, registry, options, cancelled, None)
}

/// Run a finite applet graph with its node completions recorded in `store` (#71
/// phase 1). Terminal outputs come in node completion order. A node recorded by an
/// earlier run replays its outputs instead of running. `retry_uncertain` lets a node
/// that was interrupted with side effects run again.
pub fn run_checkpointed(
    plan: GraphPlan,
    registry: &Registry,
    options: &RunOptions,
    cancelled: Arc<AtomicBool>,
    store: Arc<dyn GraphCheckpointStore>,
    retry_uncertain: bool,
) -> Result<GraphRunResult> {
    run_plan(
        plan,
        registry,
        options,
        cancelled,
        Some((store, retry_uncertain)),
    )
}

fn run_plan(
    plan: GraphPlan,
    registry: &Registry,
    options: &RunOptions,
    cancelled: Arc<AtomicBool>,
    checkpoint: Option<(Arc<dyn GraphCheckpointStore>, bool)>,
) -> Result<GraphRunResult> {
    #[cfg(target_os = "linux")]
    let _sandbox_probe_run = crate::platform::sandbox::begin_probe_run();
    let deadline = options
        .total_timeout
        .map(|timeout| {
            Instant::now()
                .checked_add(timeout)
                .ok_or_else(|| anyhow!("graph total timeout is too large"))
        })
        .transpose()?;
    check_preflight_budget(&cancelled, deadline)?;
    let plan = plan.with_input_values(&options.inputs, true)?;
    plan.validate_with_registry(registry)?;
    let cwd = std::fs::canonicalize(&options.working_directory)?;
    crate::capabilities::preflight_invocations(plan.invocations(), registry, &cwd)?;
    check_preflight_budget(&cancelled, deadline)?;
    let mut module_session_value =
        crate::modules::ProgramModuleSession::preload_invocations(registry, plan.invocations())?;
    if checkpoint.is_some() {
        // Identify ordinary applets that module bodies call, as a checkpointed program
        // does. Any the checkpoint cannot identify are refused in `checkpoint_refusal`.
        module_session_value
            .fingerprint_body_calls(&cwd, crate::runtime::program_module_body_call_fingerprint)?;
    }
    let module_session = Arc::new(module_session_value);
    let graph_sensitivities = graph_module_sensitivity_floor(&plan, registry, &cwd)?;
    for (scope_registry, invocation) in module_session.scoped_body_invocations() {
        crate::capabilities::preflight_invocations(
            std::iter::once(invocation),
            scope_registry,
            &cwd,
        )
        .context("preflighting program module body capabilities")?;
        let manifest = scope_registry.resolve_call(invocation)?;
        if let Some(remote) = manifest
            .execution_contract()
            .and_then(|contract| contract.remote.as_ref())
        {
            let mut sensitivities = graph_sensitivities.clone();
            let ports = manifest
                .execution_contract()
                .and_then(|contract| contract.ports.as_ref());
            for name in invocation.parameters.keys() {
                if let Some(port) = ports.and_then(|ports| ports.inputs.get(name)) {
                    sensitivities.insert(port.sensitivity);
                } else {
                    sensitivities.insert(Sensitivity::Unknown);
                }
            }
            if crate::security::applet_has_declared_secret_environment(
                scope_registry,
                &manifest.name,
            ) {
                sensitivities.insert(Sensitivity::Secret);
            }
            crate::security::authorize_data_flow_set(
                scope_registry,
                &manifest.name,
                &remote.endpoint,
                &sensitivities,
            )
            .map_err(|error| {
                anyhow!(
                    "unclassified module data flow into remote applet {:?} was denied: {error}",
                    manifest.name
                )
            })?;
        }
    }
    check_preflight_budget(&cancelled, deadline)?;
    let checkpointing = match checkpoint {
        Some((store, retry_uncertain)) => Some(super::checkpoint::prepare(
            &plan,
            registry,
            &cwd,
            &module_session,
            store,
            retry_uncertain,
        )?),
        None => None,
    };
    let ordering_store = checkpointing.as_ref().map(Checkpointing::store);
    let executor: Arc<dyn GraphExecutor> = if plan
        .nodes()
        .iter()
        .any(|node| matches!(node.kind, GraphNodeKind::Renderer { .. }))
    {
        renderer_executor(&plan, registry.clone(), options.clone(), module_session)?
    } else {
        Arc::new(RuntimeGraphExecutor::with_module_session(
            registry.clone(),
            options.clone(),
            module_session,
        ))
    };
    let executor: Arc<dyn GraphExecutor> = match checkpointing {
        Some(checkpointing) => Arc::new(super::checkpoint::CheckpointedExecutor::new(
            executor,
            checkpointing,
        )),
        None => executor,
    };
    let graph_options = GraphRunOptions {
        cancelled,
        deadline,
        ..GraphRunOptions::default()
    };
    let mut result = GraphSession::start(plan, executor, graph_options)?.run_to_completion()?;
    if let Some(store) = ordering_store {
        super::checkpoint::order_by_completion(&mut result.outputs, store.as_ref())?;
    }
    Ok(result)
}

fn graph_module_sensitivity_floor(
    plan: &GraphPlan,
    registry: &Registry,
    cwd: &Path,
) -> Result<SensitivitySet> {
    let mut sensitivities = SensitivitySet::empty();
    for labels in plan.effective_sensitivity_labels(registry)?.into_values() {
        sensitivities.extend(&labels);
    }
    for node in plan.nodes() {
        for port in node
            .inputs
            .values()
            .chain(node.outputs.values())
            .chain(node.events.values())
        {
            sensitivities.insert(port.sensitivity);
        }
        for call in node.invocations() {
            let manifest = registry.resolve_call(call)?;
            sensitivities.extend(&crate::runtime::worker_sensitivity_history_for_call(
                call, manifest, registry, cwd,
            )?);
            let ports = manifest
                .execution_contract()
                .and_then(|contract| contract.ports.as_ref());
            for name in call.parameters.keys() {
                sensitivities.insert(
                    node.inputs
                        .get(name)
                        .map(|port| port.sensitivity)
                        .or_else(|| {
                            ports
                                .and_then(|ports| ports.inputs.get(name))
                                .map(|port| port.sensitivity)
                        })
                        .unwrap_or(Sensitivity::Unknown),
                );
            }
            if crate::security::applet_has_declared_secret_environment(registry, &manifest.name) {
                sensitivities.insert(Sensitivity::Secret);
            }
        }
        for name in plan
            .initial_inputs()
            .get(&node.id)
            .into_iter()
            .flat_map(|values| values.keys())
        {
            if let Some(port) = node.inputs.get(name) {
                sensitivities.insert(port.sensitivity);
            }
        }
    }
    if sensitivities.is_empty() {
        sensitivities.insert(Sensitivity::Unknown);
    }
    Ok(sensitivities)
}

fn check_preflight_budget(cancelled: &AtomicBool, deadline: Option<Instant>) -> Result<()> {
    if cancelled.load(Ordering::Acquire) {
        bail!("graph execution cancelled during preflight");
    }
    if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
        bail!("graph execution deadline exceeded during preflight");
    }
    Ok(())
}

#[cfg(test)]
mod input_cursor_tests {
    use super::*;

    fn queued_event(source: String, sequence: u64) -> EventEnvelope {
        EventEnvelope {
            port: PortRef::new(source, "tick"),
            input: None,
            generation: 1,
            revision: 1,
            sequence,
            sensitivity: SensitivitySet::empty(),
            remote: None,
            payload: PortValue::Value(Value::Integer(sequence as i64)),
        }
    }

    fn enqueue(
        queue: &BoundedQueue,
        source: String,
        input: &str,
        sequence: u64,
        token: &CancellationToken,
    ) {
        queue
            .send(
                queued_event(source, sequence),
                Some(PortRef::new("collector", input)),
                token,
            )
            .unwrap();
    }

    fn node_io(
        inputs: BTreeMap<String, Vec<Arc<BoundedQueue>>>,
        token: CancellationToken,
    ) -> NodeIo {
        node_io_with_initial(inputs, BTreeMap::new(), BTreeMap::new(), token)
    }

    fn node_io_with_initial(
        inputs: BTreeMap<String, Vec<Arc<BoundedQueue>>>,
        initial_inputs: BTreeMap<String, Value>,
        input_schemas: BTreeMap<String, PortSchema>,
        token: CancellationToken,
    ) -> NodeIo {
        let cancelled = token.cancellation_flag();
        let input_names = cached_input_names(&inputs, &initial_inputs);
        let active_input_names = (0..input_names.len()).collect();
        let input_queue_cursors = vec![0; input_names.len()];
        NodeIo {
            node: "collector".into(),
            inputs,
            input_schemas,
            initial_inputs,
            input_names,
            active_input_names,
            input_name_cursor: 0,
            input_queue_cursors,
            output_sinks: BTreeMap::new(),
            schemas: BTreeMap::new(),
            generation: 1,
            revision: 1,
            next_sequence: Arc::new(AtomicU64::new(1)),
            token,
            external_cancelled: cancelled,
            deadline: None,
            input_sensitivity: None,
            max_message_bytes: 1024,
            recording: None,
            effects_started: None,
        }
    }

    fn lossless_queue() -> Arc<BoundedQueue> {
        BoundedQueue::new(1, BufferPolicy::Lossless)
    }

    fn seeded_node_io(token: CancellationToken) -> NodeIo {
        let beta = lossless_queue();
        enqueue(&beta, "beta_source".into(), "beta", 20, &token);
        let gamma = lossless_queue();
        enqueue(&gamma, "gamma_source".into(), "gamma", 30, &token);
        let inputs = BTreeMap::from([("beta".into(), vec![beta]), ("gamma".into(), vec![gamma])]);
        let input_schemas = BTreeMap::from([(
            "alpha".into(),
            PortSchema::single(crate::contracts::ValueSchema {
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
            }),
        )]);
        node_io_with_initial(
            inputs,
            BTreeMap::from([("alpha".into(), Value::Integer(10))]),
            input_schemas,
            token,
        )
    }

    fn assert_input_order(io: &mut NodeIo, expected_ports: &[&str]) {
        for expected_port in expected_ports {
            let event = io.recv_any().unwrap().unwrap();
            assert_eq!(event.input.as_ref().unwrap().port.as_str(), *expected_port);
        }
    }

    #[test]
    fn seed_only_ports_leave_cached_rotation_at_the_compacted_position() {
        let task_scope = TaskScope::new(1, None).unwrap();
        let token = task_scope.cancellation_token();

        let mut named = seeded_node_io(token.clone());
        let seed = named.recv("alpha").unwrap().unwrap();
        assert_eq!(seed.payload, PortValue::Value(Value::Integer(10)));
        assert_input_order(&mut named, &["beta", "gamma"]);

        let mut any = seeded_node_io(token.clone());
        let seed = any.recv_any().unwrap().unwrap();
        assert_eq!(seed.input.as_ref().unwrap().port, "alpha");
        assert_input_order(&mut any, &["gamma", "beta"]);

        let mut timed = seeded_node_io(token);
        let seed = timed
            .recv_any_timeout(Duration::from_millis(1))
            .unwrap()
            .unwrap();
        assert_eq!(seed.input.as_ref().unwrap().port, "alpha");
        assert_input_order(&mut timed, &["gamma", "beta"]);
    }

    #[test]
    fn cached_seed_names_and_edge_cursors_survive_mixed_receive_methods() {
        let task_scope = TaskScope::new(1, None).unwrap();
        let token = task_scope.cancellation_token();
        let beta_queues = (0..2)
            .map(|index| {
                let queue = BoundedQueue::new(2, BufferPolicy::Lossless);
                for tick in 0..2 {
                    enqueue(
                        &queue,
                        format!("beta_source_{index}"),
                        "beta",
                        100 + (index * 10 + tick) as u64,
                        &token,
                    );
                }
                queue
            })
            .collect::<Vec<_>>();
        let gamma_queues = (0..2)
            .map(|index| {
                let queue = BoundedQueue::new(2, BufferPolicy::Lossless);
                for tick in 0..2 {
                    enqueue(
                        &queue,
                        format!("gamma_source_{index}"),
                        "gamma",
                        200 + (index * 10 + tick) as u64,
                        &token,
                    );
                }
                queue
            })
            .collect::<Vec<_>>();
        let inputs = BTreeMap::from([("beta".into(), beta_queues), ("gamma".into(), gamma_queues)]);
        let input_schemas = BTreeMap::from([(
            "alpha".into(),
            PortSchema::single(crate::contracts::ValueSchema {
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
            }),
        )]);
        let mut io = node_io_with_initial(
            inputs,
            BTreeMap::from([("alpha".into(), Value::Integer(10))]),
            input_schemas,
            token,
        );

        let seed = io.recv_any().unwrap().unwrap();
        assert_eq!(seed.input.as_ref().unwrap().port, "alpha");
        let beta_from_timeout = io
            .recv_timeout("beta", Duration::from_millis(1))
            .unwrap()
            .unwrap();
        assert_eq!(beta_from_timeout.sequence, 100);
        assert_eq!(beta_from_timeout.port.node, "beta_source_0");
        let gamma_from_any = io.recv_any().unwrap().unwrap();
        assert_eq!(gamma_from_any.sequence, 200);
        assert_eq!(gamma_from_any.port.node, "gamma_source_0");
        let beta_from_recv = io.recv("beta").unwrap().unwrap();
        assert_eq!(beta_from_recv.sequence, 110);
        assert_eq!(beta_from_recv.port.node, "beta_source_1");
        let beta_from_any_timeout = io
            .recv_any_timeout(Duration::from_millis(1))
            .unwrap()
            .unwrap();
        assert_eq!(beta_from_any_timeout.sequence, 101);
        assert_eq!(beta_from_any_timeout.port.node, "beta_source_0");
        let gamma_from_timeout = io
            .recv_timeout("gamma", Duration::from_millis(1))
            .unwrap()
            .unwrap();
        assert_eq!(gamma_from_timeout.sequence, 210);
        assert_eq!(gamma_from_timeout.port.node, "gamma_source_1");
        let gamma_from_any_again = io.recv_any().unwrap().unwrap();
        assert_eq!(gamma_from_any_again.sequence, 201);
        assert_eq!(gamma_from_any_again.port.node, "gamma_source_0");
    }

    #[test]
    fn single_queue_receive_paths_preserve_timeout_eof_and_cancellation() {
        let task_scope = TaskScope::new(1, None).unwrap();
        let token = task_scope.cancellation_token();
        let queue = BoundedQueue::new(4, BufferPolicy::Lossless);
        let mut io = node_io(
            BTreeMap::from([("events".into(), vec![Arc::clone(&queue)])]),
            token.clone(),
        );

        assert!(io.recv_timeout("events", Duration::ZERO).unwrap().is_none());
        assert!(io.recv_any_timeout(Duration::ZERO).unwrap().is_none());
        assert!(io
            .recv_timeout("events", Duration::from_millis(1))
            .unwrap()
            .is_none());
        assert!(io
            .recv_any_timeout(Duration::from_millis(1))
            .unwrap()
            .is_none());

        enqueue(&queue, "source".into(), "events", 41, &token);
        enqueue(&queue, "source".into(), "events", 42, &token);
        let first = io.recv_any_timeout(Duration::MAX).unwrap().unwrap();
        assert_eq!(first.sequence, 41);
        assert_eq!(first.input.as_ref().unwrap().port.as_str(), "events");
        let second = io
            .recv_timeout("events", Duration::from_millis(1))
            .unwrap()
            .unwrap();
        assert_eq!(second.sequence, 42);
        queue.close();
        assert!(io.recv_any().unwrap().is_none());

        let cancelled_scope = TaskScope::new(1, None).unwrap();
        let cancelled_token = cancelled_scope.cancellation_token();
        let cancelled_queue = lossless_queue();
        let mut cancelled_io = node_io(
            BTreeMap::from([("events".into(), vec![cancelled_queue])]),
            cancelled_token.clone(),
        );
        cancelled_token
            .cancellation_flag()
            .store(true, Ordering::Release);
        assert!(cancelled_io
            .recv_any()
            .unwrap_err()
            .to_string()
            .contains("cancelled"));

        let expired_scope = TaskScope::new(1, None).unwrap();
        let expired_token = expired_scope.cancellation_token();
        let expired_queue = lossless_queue();
        let mut expired_io = node_io(
            BTreeMap::from([("events".into(), vec![expired_queue])]),
            expired_token,
        );
        expired_io.deadline = Some(Instant::now() - Duration::from_millis(1));
        assert!(expired_io
            .recv_any_timeout(Duration::from_secs(1))
            .unwrap_err()
            .to_string()
            .contains("graph execution deadline exceeded"));
    }

    #[test]
    fn recv_any_rotates_across_many_edges_on_one_port() {
        const EDGE_COUNT: usize = 64;
        let task_scope = TaskScope::new(1, None).unwrap();
        let token = task_scope.cancellation_token();
        let queues = (0..EDGE_COUNT)
            .map(|_| lossless_queue())
            .collect::<Vec<_>>();
        for (index, queue) in queues.iter().enumerate() {
            enqueue(
                queue,
                format!("source_{index:04}"),
                "ticks",
                index as u64,
                &token,
            );
        }
        let mut io = node_io(
            BTreeMap::from([("ticks".into(), queues.clone())]),
            token.clone(),
        );

        for turn in 0..EDGE_COUNT * 2 {
            let event = io.recv_any().unwrap().unwrap();
            let index = event
                .port
                .node
                .strip_prefix("source_")
                .unwrap()
                .parse::<usize>()
                .unwrap();
            assert_eq!(
                index,
                turn % EDGE_COUNT,
                "edge receive order at turn {turn}"
            );
            enqueue(
                &queues[index],
                format!("source_{index:04}"),
                "ticks",
                (EDGE_COUNT + turn) as u64,
                &token,
            );
        }
    }

    #[test]
    fn recv_any_timeout_rotates_each_port_and_its_edges_independently() {
        const EDGES_PER_PORT: usize = 4;
        let task_scope = TaskScope::new(1, None).unwrap();
        let token = task_scope.cancellation_token();
        let mut all_queues = BTreeMap::<String, Vec<Arc<BoundedQueue>>>::new();
        for port in ["alpha", "beta"] {
            let queues = (0..EDGES_PER_PORT)
                .map(|index| {
                    let queue = lossless_queue();
                    enqueue(
                        &queue,
                        format!("source_{port}_{index}"),
                        port,
                        index as u64,
                        &token,
                    );
                    queue
                })
                .collect();
            all_queues.insert(port.into(), queues);
        }
        let mut io = node_io(all_queues.clone(), token.clone());

        for turn in 0..EDGES_PER_PORT * 4 {
            let event = io
                .recv_any_timeout(Duration::from_millis(1))
                .unwrap()
                .unwrap();
            let port = event.input.as_ref().unwrap().port.as_str();
            let expected_port = if turn % 2 == 0 { "alpha" } else { "beta" };
            let expected_edge = (turn / 2) % EDGES_PER_PORT;
            assert_eq!(port, expected_port, "input port at turn {turn}");
            assert_eq!(
                event.port.node,
                format!("source_{expected_port}_{expected_edge}"),
                "input edge at turn {turn}"
            );
            enqueue(
                &all_queues[expected_port][expected_edge],
                format!("source_{expected_port}_{expected_edge}"),
                expected_port,
                (turn + EDGES_PER_PORT * 2) as u64,
                &token,
            );
        }
    }

    mod recv_any_preloaded_bench {
        use super::*;

        const EVENTS_PER_CASE: usize = 54_208;
        const SAMPLES_PER_CASE: usize = 3;
        const ORDER_ORACLE: &str =
            "sorted input-name round robin; per-input edge round robin; FIFO within each edge";

        #[derive(serde::Serialize)]
        struct ThroughputSample {
            case: String,
            source_count: usize,
            input_count: usize,
            messages_per_source: usize,
            sample_index: usize,
            events: usize,
            duration_ns: u64,
            ns_per_event: f64,
            checksum: u128,
            order_oracle: &'static str,
        }

        #[derive(serde::Serialize)]
        struct ThroughputCase {
            case: String,
            p50_ns_per_event: f64,
            samples: Vec<ThroughputSample>,
        }

        #[derive(serde::Serialize)]
        struct ThroughputReport {
            format_version: u32,
            harness: &'static str,
            cases: Vec<ThroughputCase>,
        }

        #[derive(Debug, PartialEq, Eq)]
        struct ExpectedReceive {
            source: String,
            input: String,
            sequence: u64,
        }

        fn reference_receive_order(
            mut queues: Vec<Vec<VecDeque<ExpectedReceive>>>,
        ) -> Vec<ExpectedReceive> {
            let total = queues
                .iter()
                .flat_map(|port_queues| port_queues.iter())
                .map(VecDeque::len)
                .sum();
            let mut expected = Vec::with_capacity(total);
            let mut input_cursor = 0usize;
            let mut queue_cursors = vec![0usize; queues.len()];

            while expected.len() < total {
                let mut selected = None;
                for input_offset in 0..queues.len() {
                    let input_index = (input_cursor + input_offset) % queues.len();
                    let port_queues = &mut queues[input_index];
                    if port_queues.is_empty() {
                        continue;
                    }
                    let queue_start = queue_cursors[input_index] % port_queues.len();
                    for queue_offset in 0..port_queues.len() {
                        let queue_index = (queue_start + queue_offset) % port_queues.len();
                        if let Some(message) = port_queues[queue_index].pop_front() {
                            selected = Some((input_index, queue_index, message));
                            break;
                        }
                    }
                    if selected.is_some() {
                        break;
                    }
                }

                let (input_index, queue_index, message) =
                    selected.expect("reference scheduler must find a preloaded event");
                input_cursor = input_index + 1;
                queue_cursors[input_index] = queue_index + 1;
                expected.push(message);
            }
            expected
        }

        fn run_preloaded_case(source_count: usize, input_count: usize) -> (Duration, u128) {
            assert!(source_count > 0);
            assert!(input_count > 0);
            assert_eq!(
                EVENTS_PER_CASE % source_count,
                0,
                "benchmark event count must divide evenly across sources"
            );
            let messages_per_source = EVENTS_PER_CASE / source_count;

            let task_scope = TaskScope::new(1, None).unwrap();
            let token = task_scope.cancellation_token();
            let input_names = (0..input_count)
                .map(|index| format!("port_{index:02}"))
                .collect::<Vec<_>>();
            let mut inputs = input_names
                .iter()
                .cloned()
                .map(|name| (name, Vec::new()))
                .collect::<BTreeMap<_, Vec<Arc<BoundedQueue>>>>();
            let mut expected_queues = (0..input_count)
                .map(|_| Vec::<VecDeque<ExpectedReceive>>::new())
                .collect::<Vec<_>>();
            let mut sequence = 1u64;

            for source_index in 0..source_count {
                let input_index = source_index % input_count;
                let input = &input_names[input_index];
                let source = format!("source_{source_index:04}");
                let queue = BoundedQueue::new(messages_per_source, BufferPolicy::Lossless);
                let mut queue_expected = VecDeque::with_capacity(messages_per_source);
                for _ in 0..messages_per_source {
                    enqueue(&queue, source.clone(), input, sequence, &token);
                    queue_expected.push_back(ExpectedReceive {
                        source: source.clone(),
                        input: input.clone(),
                        sequence,
                    });
                    sequence += 1;
                }
                inputs.get_mut(input).unwrap().push(Arc::clone(&queue));
                expected_queues[input_index].push(queue_expected);
            }

            let expected = reference_receive_order(expected_queues);
            let total = expected.len();
            let expected_checksum = expected
                .iter()
                .map(|message| u128::from(message.sequence))
                .sum::<u128>();
            let queues = inputs
                .values()
                .flat_map(|port_queues| port_queues.iter().cloned())
                .collect::<Vec<_>>();
            let mut io = node_io(inputs, token);
            let mut received = Vec::with_capacity(total);

            // All producers, queue setup, expected-order generation, and output
            // allocation happen before timing. The known event count avoids the
            // 2 ms idle receive wait; EOF is checked after the timer stops.
            let started = Instant::now();
            for _ in 0..total {
                received.push(io.recv_any().unwrap().expect("preloaded event"));
            }
            let elapsed = started.elapsed();

            let mut checksum = 0u128;
            for (turn, (event, expected)) in received.iter().zip(&expected).enumerate() {
                assert_eq!(event.port.node, expected.source, "source at turn {turn}");
                assert_eq!(event.port.port, "tick", "source port at turn {turn}");
                assert_eq!(
                    event.input.as_ref().map(|input| input.port.as_str()),
                    Some(expected.input.as_str()),
                    "destination input at turn {turn}"
                );
                assert_eq!(event.input.as_ref().unwrap().node, "collector");
                assert_eq!(event.generation, 1, "generation at turn {turn}");
                assert_eq!(event.revision, 1, "revision at turn {turn}");
                assert_eq!(event.sequence, expected.sequence, "sequence at turn {turn}");
                assert_eq!(
                    event.payload,
                    PortValue::Value(Value::Integer(expected.sequence as i64)),
                    "typed payload at turn {turn}"
                );
                let observed_value = match &event.payload {
                    PortValue::Value(Value::Integer(value)) => *value,
                    payload => panic!("unexpected payload at turn {turn}: {payload:?}"),
                };
                checksum += u128::from(observed_value as u64);
            }
            assert_eq!(received.len(), total, "all preloaded events were delivered");
            assert_eq!(checksum, expected_checksum, "exact event checksum");

            for queue in queues {
                queue.close();
            }
            assert!(io.recv_any().unwrap().is_none(), "closed queues reach EOF");
            (elapsed, checksum)
        }

        #[test]
        #[ignore = "diagnostic throughput measurement; run explicitly after a paired-baseline gate"]
        fn recv_any_preloaded_bounded_queue_throughput() {
            let mut report = ThroughputReport {
                format_version: 1,
                harness: "recv_any_preloaded_bounded_queue_throughput",
                cases: Vec::with_capacity(6),
            };
            for source_count in [1, 16, 847] {
                for input_count in [1, 8] {
                    let case = format!("sources={source_count},inputs={input_count}");
                    let events = EVENTS_PER_CASE;
                    let mut raw_samples = Vec::with_capacity(SAMPLES_PER_CASE);
                    let mut checksums = Vec::with_capacity(SAMPLES_PER_CASE);
                    for sample_index in 0..SAMPLES_PER_CASE {
                        let (elapsed, sample_checksum) =
                            run_preloaded_case(source_count, input_count);
                        checksums.push(sample_checksum);
                        raw_samples.push(ThroughputSample {
                            case: case.clone(),
                            source_count,
                            input_count,
                            messages_per_source: events / source_count,
                            sample_index,
                            events,
                            duration_ns: elapsed.as_nanos() as u64,
                            ns_per_event: elapsed.as_nanos() as f64 / events as f64,
                            checksum: sample_checksum,
                            order_oracle: ORDER_ORACLE,
                        });
                    }
                    assert!(
                        checksums.iter().all(|checksum| *checksum == checksums[0]),
                        "checksums must agree across samples for {case}"
                    );
                    let mut sorted_durations = raw_samples
                        .iter()
                        .map(|sample| sample.duration_ns)
                        .collect::<Vec<_>>();
                    sorted_durations.sort_unstable();
                    let p50_ns_per_event =
                        sorted_durations[SAMPLES_PER_CASE / 2] as f64 / events as f64;
                    for sample in &raw_samples {
                        eprintln!(
                            "recv_any_preloaded sample case={} index={} events={} duration_ns={} ns_per_event={:.1} checksum={} order={}",
                            sample.case,
                            sample.sample_index,
                            sample.events,
                            sample.duration_ns,
                            sample.ns_per_event,
                            sample.checksum,
                            sample.order_oracle
                        );
                    }
                    eprintln!(
                        "recv_any_preloaded case={} p50_ns_per_event={:.1} samples={}",
                        case, p50_ns_per_event, SAMPLES_PER_CASE
                    );
                    report.cases.push(ThroughputCase {
                        case,
                        p50_ns_per_event,
                        samples: raw_samples,
                    });
                }
            }
            if let Some(path) = std::env::var_os("FLOW_RECV_ANY_BENCH_JSON") {
                let path = PathBuf::from(path);
                assert!(
                    path.is_absolute(),
                    "FLOW_RECV_ANY_BENCH_JSON must be an absolute output path"
                );
                std::fs::write(path, serde_json::to_vec_pretty(&report).unwrap()).unwrap();
            }
        }
    }
}

#[cfg(test)]
mod emit_fanout_tests {
    use super::*;
    use crate::contracts::ValueSchema;

    fn value_schema(kind: OutputType) -> ValueSchema {
        ValueSchema {
            min_items: None,
            scalar_only: false,
            additional_properties: None,
            min_properties: None,
            max_properties: None,
            nonempty: false,
            kind,
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

    fn test_io(
        sink_policies: &[(BufferPolicy, usize)],
        schema: ValueSchema,
        max_message_bytes: usize,
    ) -> (TaskScope, NodeIo, Vec<Arc<BoundedQueue>>) {
        let scope = TaskScope::new(1, None).expect("test task scope");
        let token = scope.cancellation_token();
        let queues = sink_policies
            .iter()
            .map(|(policy, capacity)| BoundedQueue::new(*capacity, *policy))
            .collect::<Vec<_>>();
        let sinks = queues
            .iter()
            .enumerate()
            .map(|(index, queue)| QueueSink {
                queue: Arc::clone(queue),
                input: Some(PortRef::new(format!("sink_{index:02}"), "in")),
            })
            .collect::<Vec<_>>();
        let mut output_schema = PortSchema::events(schema);
        output_schema.sensitivity = Sensitivity::Confidential;
        let cancelled = token.cancellation_flag();
        let io = NodeIo {
            node: "emitter".into(),
            inputs: BTreeMap::new(),
            input_schemas: BTreeMap::new(),
            initial_inputs: BTreeMap::new(),
            input_names: Vec::new(),
            active_input_names: Vec::new(),
            input_name_cursor: 0,
            input_queue_cursors: Vec::new(),
            output_sinks: BTreeMap::from([("out".into(), sinks)]),
            schemas: BTreeMap::from([("out".into(), output_schema)]),
            generation: 7,
            revision: 13,
            next_sequence: Arc::new(AtomicU64::new(41)),
            token,
            external_cancelled: cancelled,
            deadline: None,
            input_sensitivity: Some(SensitivitySet::from_label(Sensitivity::Internal)),
            max_message_bytes,
            recording: None,
            effects_started: None,
        };
        (scope, io, queues)
    }

    fn remote_metadata(index: usize) -> RemoteEventMetadata {
        RemoteEventMetadata {
            event: format!("event-{index}"),
            cursor: Some(format!("cursor-{index}")),
            duplicate_cursor: index == 2,
        }
    }

    fn expected_event(
        sink: usize,
        event_index: usize,
        payload: PortValue,
        remote: Option<RemoteEventMetadata>,
    ) -> EventEnvelope {
        EventEnvelope {
            port: PortRef::new("emitter", "out"),
            input: Some(PortRef::new(format!("sink_{sink:02}"), "in")),
            generation: 7,
            revision: 13,
            sequence: 41 + event_index as u64,
            sensitivity: SensitivitySet::from_labels([
                Sensitivity::Internal,
                Sensitivity::Confidential,
            ]),
            remote,
            payload,
        }
    }

    #[test]
    fn single_sink_and_fanout_preserve_identity_order_sensitivity_and_metadata() {
        for sink_count in [1, 3] {
            let policies = vec![(BufferPolicy::Lossless, 3); sink_count];
            let (_scope, mut io, queues) =
                test_io(&policies, value_schema(OutputType::Integer), 1024);

            for event_index in 0..3 {
                let payload = PortValue::Value(Value::Integer(100 + event_index as i64));
                let report = if event_index % 2 == 0 {
                    io.emit_remote_event("out", payload, remote_metadata(event_index))
                } else {
                    io.emit("out", payload)
                }
                .expect("valid event should be emitted");
                assert_eq!(
                    report,
                    PortEmitReport {
                        delivered: sink_count,
                        dropped: 0,
                    }
                );
            }

            for (sink_index, queue) in queues.iter().enumerate() {
                for event_index in 0..3 {
                    assert_eq!(
                        queue.try_recv().unwrap().unwrap(),
                        expected_event(
                            sink_index,
                            event_index,
                            PortValue::Value(Value::Integer(100 + event_index as i64)),
                            (event_index % 2 == 0).then(|| remote_metadata(event_index)),
                        ),
                        "sink {sink_index} event {event_index}"
                    );
                }
                assert!(queue.try_recv().unwrap().is_none());
            }
        }
    }

    #[test]
    fn drop_policies_keep_their_existing_queue_contents_and_counts() {
        for (policy, expected_sequence, expected_deliveries) in [
            (BufferPolicy::DropNewest, 41_u64, [1_usize, 0, 0]),
            (BufferPolicy::DropOldest, 43_u64, [1_usize, 1, 1]),
            (BufferPolicy::Latest, 43_u64, [1_usize, 1, 1]),
        ] {
            let (_scope, mut io, queues) =
                test_io(&[(policy, 1)], value_schema(OutputType::Integer), 1024);
            for (index, delivered) in expected_deliveries.into_iter().enumerate() {
                let report = io
                    .emit("out", PortValue::Value(Value::Integer(100 + index as i64)))
                    .unwrap();
                assert_eq!(report.delivered, delivered, "{policy:?} event {index}");
                if policy == BufferPolicy::DropNewest && delivered == 0 {
                    assert_eq!(report.dropped, 2, "DropNewest rejected-event report");
                }
            }
            assert_eq!(queues[0].dropped.load(Ordering::Relaxed), 2, "{policy:?}");
            let event = queues[0].try_recv().unwrap().unwrap();
            assert_eq!(event.sequence, expected_sequence, "{policy:?}");
            assert_eq!(
                event.payload,
                PortValue::Value(Value::Integer(if expected_sequence == 41 {
                    100
                } else {
                    102
                }))
            );
            assert!(queues[0].try_recv().unwrap().is_none());
        }
    }

    #[test]
    fn validation_size_and_cancellation_failures_do_not_deliver_or_consume_sequence() {
        let (_scope, mut io, queues) = test_io(
            &[(BufferPolicy::Lossless, 1)],
            value_schema(OutputType::Integer),
            1024,
        );
        let next_sequence = Arc::clone(&io.next_sequence);
        let error = io
            .emit("out", PortValue::Value(Value::String("wrong type".into())))
            .unwrap_err()
            .to_string();
        assert!(error.contains("payload for emitter.out"), "{error}");
        assert_eq!(next_sequence.load(Ordering::Relaxed), 41);
        assert!(queues[0].try_recv().unwrap().is_none());

        let (_scope, mut io, queues) = test_io(
            &[(BufferPolicy::Lossless, 1)],
            value_schema(OutputType::Bytes),
            4,
        );
        let next_sequence = Arc::clone(&io.next_sequence);
        let error = io
            .emit("out", PortValue::Bytes(vec![0; 8]))
            .unwrap_err()
            .to_string();
        assert!(error.contains("exceeds graph message limit"), "{error}");
        assert_eq!(next_sequence.load(Ordering::Relaxed), 41);
        assert!(queues[0].try_recv().unwrap().is_none());

        let (_scope, mut io, queues) = test_io(
            &[(BufferPolicy::Lossless, 1)],
            value_schema(OutputType::Integer),
            1024,
        );
        let next_sequence = Arc::clone(&io.next_sequence);
        io.external_cancelled.store(true, Ordering::Release);
        let error = io
            .emit("out", PortValue::Value(Value::Integer(1)))
            .unwrap_err()
            .to_string();
        assert!(error.contains("task group cancelled"), "{error}");
        assert_eq!(next_sequence.load(Ordering::Relaxed), 41);
        assert!(queues[0].try_recv().unwrap().is_none());
    }

    #[test]
    fn fanout_error_keeps_prior_delivery_and_stops_before_later_sinks() {
        let policies = vec![(BufferPolicy::Lossless, 1); 3];
        let (_scope, mut io, queues) = test_io(&policies, value_schema(OutputType::Integer), 1024);
        queues[1].close();

        let error = io
            .emit("out", PortValue::Value(Value::Integer(9)))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("graph edge was closed while its producer was active"),
            "{error}"
        );
        assert_eq!(
            queues[0].try_recv().unwrap().unwrap(),
            expected_event(0, 0, PortValue::Value(Value::Integer(9)), None,)
        );
        assert!(queues[1].try_recv().unwrap().is_none());
        assert!(queues[2].try_recv().unwrap().is_none());
        assert_eq!(io.next_sequence.load(Ordering::Relaxed), 42);
    }
}
