//! Bounded event transport and structured graph execution.

use super::plan::{
    validate_payload, BufferPolicy, GraphNode, GraphNodeKind, GraphPlan, PortRef, PortSchema,
    PortValue,
};
use crate::ast::{Invocation, ListenSource, StepKind, Value};
use crate::contracts::{OutputEncoding, OutputType, PortDelivery, Sensitivity};
use crate::manifest::Registry;
use crate::runtime::{self, AppletInvocationResult, RunOptions};
use crate::security::SensitivitySet;
use crate::tasks::{CancellationToken, TaskHandle, TaskScope};
use anyhow::{anyhow, bail, Context, Result};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const QUEUE_POLL: Duration = Duration::from_millis(10);

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
    pub payload: PortValue,
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
    pub fn launched_processes(&self) -> usize {
        self.launched_processes.load(Ordering::Relaxed)
    }

    fn add_processes(&self, count: usize) {
        self.launched_processes.fetch_add(count, Ordering::Relaxed);
    }
}

/// Backend adapter for a graph node. Implementations receive only typed ports;
/// every emitted message is checked against the plan before it is queued.
pub trait GraphExecutor: Send + Sync + 'static {
    fn execute(&self, node: &GraphNode, io: &mut NodeIo, context: &GraphContext) -> Result<()>;
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
    input_cursor: usize,
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
}

impl NodeIo {
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

    fn take_initial_input(&mut self, port: &str) -> Result<Option<EventEnvelope>> {
        let Some(value) = self.initial_inputs.remove(port) else {
            return Ok(None);
        };
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
            payload,
        })))
    }

    /// Receive the next message from one named input. Multi-edge inputs merge
    /// in the order messages become available, with a stable queue cursor.
    pub fn recv(&mut self, port: &str) -> Result<Option<EventEnvelope>> {
        self.check_cancelled()?;
        if let Some(event) = self.take_initial_input(port)? {
            return Ok(Some(event));
        }
        let queues = self.inputs.get(port).cloned().unwrap_or_default();
        if queues.is_empty() {
            return Ok(None);
        }
        if queues.len() == 1 {
            let event = queues[0].recv(&self.token)?;
            return Ok(event.map(|event| self.observe_input(event)));
        }
        loop {
            self.check_cancelled()?;
            let start = self.input_cursor % queues.len();
            for offset in 0..queues.len() {
                let index = (start + offset) % queues.len();
                if let Some(event) = queues[index].try_recv()? {
                    self.input_cursor = index + 1;
                    return Ok(Some(self.observe_input(event)));
                }
            }
            if queues
                .iter()
                .map(|q| q.is_closed_and_empty())
                .collect::<Result<Vec<_>>>()?
                .iter()
                .all(|closed| *closed)
            {
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
        if let Some(event) = self.take_initial_input(port)? {
            return Ok(Some(event));
        }
        let queues = self.inputs.get(port).cloned().unwrap_or_default();
        if queues.is_empty() {
            return Ok(None);
        }
        loop {
            self.check_cancelled()?;
            let start = self.input_cursor % queues.len();
            for offset in 0..queues.len() {
                let index = (start + offset) % queues.len();
                if let Some(event) = queues[index].try_recv()? {
                    self.input_cursor = index + 1;
                    return Ok(Some(self.observe_input(event)));
                }
            }
            if queues
                .iter()
                .map(|queue| queue.is_closed_and_empty())
                .collect::<Result<Vec<_>>>()?
                .iter()
                .all(|closed| *closed)
            {
                return Ok(None);
            }
            let now = Instant::now();
            if deadline.is_some_and(|deadline| now >= deadline) {
                return Ok(None);
            }
            thread::sleep(deadline.map_or(Duration::from_millis(2), |deadline| {
                deadline
                    .saturating_duration_since(now)
                    .min(Duration::from_millis(2))
            }));
        }
    }

    /// Receive the next message from any bound input. `event.input` identifies
    /// the destination port so adapters can update the matching parameter.
    pub fn recv_any(&mut self) -> Result<Option<EventEnvelope>> {
        let names = self
            .inputs
            .keys()
            .chain(self.initial_inputs.keys())
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        if names.is_empty() {
            return Ok(None);
        }
        loop {
            self.check_cancelled()?;
            let start = self.input_cursor % names.len();
            for offset in 0..names.len() {
                let index = (start + offset) % names.len();
                if let Some(event) = self.take_initial_input(&names[index])? {
                    self.input_cursor = index + 1;
                    return Ok(Some(event));
                }
                let queues = self
                    .inputs
                    .get(&names[index])
                    .map(Vec::as_slice)
                    .unwrap_or_default();
                let queue_start = self.input_cursor % queues.len().max(1);
                for queue_offset in 0..queues.len() {
                    let queue_index = (queue_start + queue_offset) % queues.len();
                    if let Some(event) = queues[queue_index].try_recv()? {
                        self.input_cursor = index + 1;
                        return Ok(Some(self.observe_input(event)));
                    }
                }
            }
            let all_closed = names.iter().all(|name| {
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
            thread::sleep(Duration::from_millis(2));
        }
    }

    pub fn recv_any_timeout(&mut self, timeout: Duration) -> Result<Option<EventEnvelope>> {
        let deadline = Instant::now().checked_add(timeout);
        let names = self
            .inputs
            .keys()
            .chain(self.initial_inputs.keys())
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        if names.is_empty() {
            return Ok(None);
        }
        loop {
            self.check_cancelled()?;
            let start = self.input_cursor % names.len();
            for offset in 0..names.len() {
                let index = (start + offset) % names.len();
                if let Some(event) = self.take_initial_input(&names[index])? {
                    self.input_cursor = index + 1;
                    return Ok(Some(event));
                }
                let queues = self
                    .inputs
                    .get(&names[index])
                    .map(Vec::as_slice)
                    .unwrap_or_default();
                for queue in queues {
                    if let Some(event) = queue.try_recv()? {
                        self.input_cursor = index + 1;
                        return Ok(Some(self.observe_input(event)));
                    }
                }
            }
            let all_closed = names.iter().all(|name| {
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
            thread::sleep(Duration::from_millis(2));
        }
    }

    pub fn emit(&mut self, port: &str, payload: PortValue) -> Result<PortEmitReport> {
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
        let sinks = self.output_sinks.get(port).cloned().unwrap_or_default();
        if sinks.is_empty() {
            return Ok(PortEmitReport::default());
        }
        let sequence = next_sequence(&self.next_sequence)?;
        let envelope = EventEnvelope {
            port: PortRef::new(self.node.clone(), port),
            input: None,
            generation: self.generation,
            revision: self.revision,
            sequence,
            sensitivity: {
                let mut labels = SensitivitySet::from_label(schema.sensitivity);
                if let Some(input) = &self.input_sensitivity {
                    labels.extend(input);
                }
                labels
            },
            payload,
        };
        let mut report = PortEmitReport::default();
        for sink in sinks {
            let before = sink.queue.dropped.load(Ordering::Relaxed);
            if sink.queue.send(envelope.clone(), sink.input, &self.token)? {
                report.delivered += 1;
            } else {
                report.dropped += 1;
            }
            report.dropped += sink
                .queue
                .dropped
                .load(Ordering::Relaxed)
                .saturating_sub(before);
        }
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

impl GraphSession {
    pub fn start(
        plan: GraphPlan,
        executor: Arc<dyn GraphExecutor>,
        options: GraphRunOptions,
    ) -> Result<Self> {
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
                input_cursor: 0,
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

    /// Stop and reap this session before starting a new graph generation.
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
        self.cancel_and_join()?;
        options.generation = generation;
        options.revision = revision;
        Self::start(plan, executor, options)
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
                        *task = None;
                        remaining -= 1;
                    }
                    Ok(None) => {}
                    Err(error) => {
                        first_error.get_or_insert(error);
                        self.cancelled.store(true, Ordering::Release);
                        *task = None;
                        remaining -= 1;
                    }
                }
            }
            if remaining > 0 {
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
    let GraphNodeKind::Applet { call } = &node.kind else {
        return None;
    };
    let labels = call
        .parameters
        .keys()
        .filter_map(|name| node.inputs.get(name).map(|port| port.sensitivity))
        .collect::<Vec<_>>();
    (!labels.is_empty()).then(|| SensitivitySet::from_labels(labels))
}

fn payload_size(payload: &PortValue) -> Result<usize> {
    Ok(match payload {
        PortValue::Bytes(bytes) => bytes.len(),
        PortValue::Value(value) => serde_json::to_vec(value)?.len(),
    })
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
}

impl RuntimeGraphExecutor {
    pub fn new(registry: Registry, options: RunOptions) -> Self {
        Self {
            registry,
            options,
            module_session: None,
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
        }
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
            let options = self.invocation_options(context)?;
            let result = invoke_for_graph(
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
            )?;
            context.metrics.add_processes(result.launched_processes);
            emit_applet_result(node, &result, io)?;
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
            let options = self.invocation_options(context)?;
            let result = invoke_for_graph(
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
            )?;
            context.metrics.add_processes(result.launched_processes);
            emit_applet_result_counted(node, &result, io, &mut emitted)?;
        }
        if received == 0 && streams.iter().any(|name| node.inputs[name].required) {
            bail!(
                "required graph stream input on node {:?} closed without a value",
                node.id
            );
        }
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
        for call in node.invocations() {
            let (_, sensitivities) = self.invocation_sensitivity_sets(node, call, io)?;
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
        let stdout = node
            .outputs
            .get("stdout")
            .ok_or_else(|| anyhow!("graph stream node {:?} has no stdout port", node.id))?;
        let expected_schema = output_schema
            .as_ref()
            .map(super::plan::value_schema_from_output);
        if stdout.delivery != PortDelivery::Records
            || expected_schema
                .as_ref()
                .is_some_and(|schema| &stdout.schema != schema)
            || (expected_schema.is_none() && stdout.schema.kind != OutputType::Bytes)
        {
            bail!(
                "graph stream node {:?} stdout port does not match its final applet output contract",
                node.id
            );
        }
        let cancelled = io.cancellation_flag();
        let result = runtime::run_stream(
            &stream,
            &self.registry,
            &self.invocation_options(context)?,
            self.options.max_output_bytes,
            |record, _, _| {
                if cancelled.load(Ordering::Acquire) {
                    bail!("graph stream cancelled");
                }
                let port = if node.outputs.contains_key("stdout") {
                    "stdout"
                } else {
                    node.events
                        .keys()
                        .next()
                        .ok_or_else(|| anyhow!("stream node has no output port"))?
                };
                let payload = match output_schema.as_ref() {
                    Some(schema) => match schema.decode(record)? {
                        crate::contracts::AppletOutput::Bytes(bytes) => PortValue::Bytes(bytes),
                        crate::contracts::AppletOutput::Value(value) => PortValue::Value(value),
                    },
                    None => PortValue::Bytes(record.to_vec()),
                };
                io.emit(port, payload)?;
                Ok(())
            },
        )?;
        context.metrics.add_processes(result.launched_processes);
        if emit_status && node.outputs.contains_key("status") {
            io.emit("status", PortValue::Value(Value::Integer(0)))?;
        }
        Ok(())
    }
}

struct GraphInvocationContext<'a> {
    options: &'a RunOptions,
    cancelled: Arc<AtomicBool>,
    module_session: Option<&'a Arc<crate::modules::ProgramModuleSession>>,
    sensitivities: SensitivitySet,
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
        parameters: parameters.clone(),
        binding: None,
        cache: false,
        status_binding: Some("__flow_internal_status".to_owned()),
        result_binding: None,
    };
    let session = match context.module_session {
        Some(session) => Arc::clone(session),
        None => Arc::new(crate::modules::ProgramModuleSession::preload_invocations(
            registry,
            std::iter::once(&call),
        )?),
    };
    runtime::invoke_applet_with_module_session_and_sensitivity(
        registry,
        name,
        parameters,
        input,
        context.options,
        context.cancelled,
        session,
        context.sensitivities,
    )
}

impl GraphExecutor for RuntimeGraphExecutor {
    fn execute(&self, node: &GraphNode, io: &mut NodeIo, context: &GraphContext) -> Result<()> {
        match &node.kind {
            GraphNodeKind::Applet { call } => self.run_applet(node, call, io, context),
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
                ..
            } => {
                let source = format!("stream from {}\nshow each line\n", format_invocation(call)?);
                self.run_stream(node, &source, io, context, false)
            }
            GraphNodeKind::Listener {
                source: ListenSource::Stdin,
                ..
            } => {
                bail!("graph stdin listener requires an injected cancellable source adapter")
            }
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
) -> Result<()> {
    emit_applet_result_counted(node, result, io, &mut BTreeMap::new())
}

fn emit_applet_result_counted(
    node: &GraphNode,
    result: &AppletInvocationResult,
    io: &mut NodeIo,
    emitted: &mut BTreeMap<String, usize>,
) -> Result<()> {
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
pub fn run(
    plan: GraphPlan,
    registry: &Registry,
    options: &RunOptions,
    cancelled: Arc<AtomicBool>,
) -> Result<GraphRunResult> {
    let deadline = options
        .total_timeout
        .map(|timeout| {
            Instant::now()
                .checked_add(timeout)
                .ok_or_else(|| anyhow!("graph total timeout is too large"))
        })
        .transpose()?;
    check_preflight_budget(&cancelled, deadline)?;
    plan.validate_with_registry(registry)?;
    let cwd = std::fs::canonicalize(&options.working_directory)?;
    crate::capabilities::preflight_invocations(plan.invocations(), registry, &cwd)?;
    check_preflight_budget(&cancelled, deadline)?;
    let module_session = Arc::new(crate::modules::ProgramModuleSession::preload_invocations(
        registry,
        plan.invocations(),
    )?);
    let graph_sensitivities = graph_module_sensitivity_floor(&plan, registry)?;
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
    let executor: Arc<dyn GraphExecutor> = if plan
        .nodes()
        .iter()
        .any(|node| matches!(node.kind, GraphNodeKind::Renderer { .. }))
    {
        crate::desktop::graph_executor_with_module_session(
            &plan,
            registry.clone(),
            options.clone(),
            module_session,
        )?
    } else {
        Arc::new(RuntimeGraphExecutor::with_module_session(
            registry.clone(),
            options.clone(),
            module_session,
        ))
    };
    let graph_options = GraphRunOptions {
        cancelled,
        deadline,
        ..GraphRunOptions::default()
    };
    GraphSession::start(plan, executor, graph_options)?.run_to_completion()
}

fn graph_module_sensitivity_floor(plan: &GraphPlan, registry: &Registry) -> Result<SensitivitySet> {
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
