//! Opt-in mixed-backend graph lifecycle probe.
//!
//! This harness creates one graph containing timers, finite command applets,
//! registry-owned JSON-lines workers, in-process applets, scheduled program
//! services, and loopback remote listeners. It records bounded resource and
//! lifecycle receipts; it does not claim whole-machine or child-process RSS.
//! Run only from a prebuilt release test binary through
//! `benchmarks/stress_evidence.py mixed`.
#![cfg(feature = "graph")]
use anyhow::{anyhow, bail, Context, Result};
use flow::{
    ast::Value,
    graph::{
        self, EdgeSpec, GraphContext, GraphExecutor, GraphNode, GraphNodeKind, GraphRunOptions,
        GraphSession, NodeIo, PortRef, PortSchema, PortValue, RuntimeGraphExecutor,
    },
    manifest::Registry,
    modules::ProgramModuleSession,
    runtime::{worker_backend::WorkerPool, RunOptions},
};
use serde_json::{json, Value as JsonValue};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{BufRead, BufReader, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        Arc, Condvar, Mutex,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tempfile::TempDir;

const MAX_ACTORS: usize = 1_000;
const MAX_NATIVE_CHILDREN: usize = 64;
const WORKER_POOL_CAPACITY: usize = 16;
const MAX_PROGRAM_SERVICES: usize = 32;
const MAX_REMOTE_LISTENERS: usize = 8;
const MIN_ACTIVE_WINDOW_MS: u64 = 100;
const MAX_ACTIVE_WINDOW_MS: u64 = 300_000;
const PROBE_HELPER_POLL: Duration = Duration::from_millis(10);
const ACTOR_READINESS_TIMEOUT: Duration = Duration::from_secs(10);
const GRAPH_RUN_DEADLINE_GRACE: Duration = Duration::from_secs(20);
const ACTIVE_WINDOW_OBSERVATION_TOLERANCE: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy)]
struct ActorLayout {
    timers: usize,
    native: usize,
    workers: usize,
    in_process: usize,
    services: usize,
    remote: usize,
    collector: usize,
}

impl ActorLayout {
    fn for_total(total: usize) -> Result<Self> {
        if !(32..=MAX_ACTORS).contains(&total) {
            bail!("mixed graph actor count must be in 32..={MAX_ACTORS}");
        }
        let scale = |ceiling: usize| (total * ceiling).div_ceil(MAX_ACTORS).max(1).min(ceiling);
        let layout = Self {
            timers: 0,
            native: scale(MAX_NATIVE_CHILDREN),
            workers: scale(WORKER_POOL_CAPACITY),
            in_process: scale(32),
            services: scale(MAX_PROGRAM_SERVICES),
            remote: scale(MAX_REMOTE_LISTENERS),
            collector: 1,
        };
        let reserved = layout.native
            + layout.workers
            + layout.in_process
            + layout.services
            + layout.remote
            + layout.collector;
        if reserved >= total {
            bail!("mixed actor distribution leaves no timer slots");
        }
        Ok(Self {
            timers: total - reserved,
            ..layout
        })
    }

    fn total(self) -> usize {
        self.timers
            + self.native
            + self.workers
            + self.in_process
            + self.services
            + self.remote
            + self.collector
    }

    fn json(self) -> JsonValue {
        json!({
            "timers": self.timers,
            "native_finite_applets": self.native,
            "distinct_registry_workers": self.workers,
            "in_process_applets": self.in_process,
            "scheduled_program_services": self.services,
            "loopback_remote_listeners": self.remote,
            "timer_collector": self.collector,
            "total_graph_actors": self.total(),
            "ceilings": {
                "graph_actors": MAX_ACTORS,
                "native_finite_child_launches": MAX_NATIVE_CHILDREN,
                "registry_worker_pool": WORKER_POOL_CAPACITY,
                "program_service_actors": MAX_PROGRAM_SERVICES,
                "remote_listener_actors": MAX_REMOTE_LISTENERS,
                "remote_event_reader_slots": MAX_REMOTE_LISTENERS,
                "probe_helper_threads": 3
            }
        })
    }
}

#[derive(Debug, Clone, Copy)]
struct ProcSnapshot {
    rss_bytes: usize,
    threads: usize,
}

#[derive(Default)]
struct OutputReceipts {
    counts: BTreeMap<String, usize>,
    nodes: BTreeMap<String, BTreeSet<String>>,
}

struct ActiveTimerWindowState {
    active: bool,
    event_count: u64,
    source_ids: BTreeSet<String>,
    unexpected_source_id: Option<String>,
}

struct ActiveTimerProgress {
    expected_source_ids: BTreeSet<String>,
    state: Mutex<ActiveTimerWindowState>,
}

struct ActiveTimerWindowStart {
    started_at: Instant,
    event_count: u64,
}

struct ActiveTimerWindowSnapshot {
    ended_at: Instant,
    events_at_end: u64,
    event_count: u64,
    source_ids: BTreeSet<String>,
    unexpected_source_id: Option<String>,
}

impl ActiveTimerProgress {
    fn new(expected_source_ids: BTreeSet<String>) -> Self {
        Self {
            expected_source_ids,
            state: Mutex::new(ActiveTimerWindowState {
                active: false,
                event_count: 0,
                source_ids: BTreeSet::new(),
                unexpected_source_id: None,
            }),
        }
    }

    fn record_event(&self, source_id: &str, total_events: &AtomicU64) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.active {
            state.event_count = state.event_count.saturating_add(1);
            if self.expected_source_ids.contains(source_id) {
                state.source_ids.insert(source_id.to_owned());
            } else if state.unexpected_source_id.is_none() {
                state.unexpected_source_id = Some(source_id.to_owned());
            }
        }
        total_events.fetch_add(1, Ordering::AcqRel);
    }

    fn start_window(&self, total_events: &AtomicU64) -> ActiveTimerWindowStart {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.event_count = 0;
        state.source_ids.clear();
        state.unexpected_source_id = None;
        let started_at = Instant::now();
        state.active = true;
        let event_count = total_events.load(Ordering::Acquire);
        ActiveTimerWindowStart {
            started_at,
            event_count,
        }
    }

    fn stop_window(&self, total_events: &AtomicU64) -> ActiveTimerWindowSnapshot {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.active = false;
        let ended_at = Instant::now();
        let events_at_end = total_events.load(Ordering::Acquire);
        ActiveTimerWindowSnapshot {
            ended_at,
            events_at_end,
            event_count: state.event_count,
            source_ids: state.source_ids.clone(),
            unexpected_source_id: state.unexpected_source_id.clone(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct ActiveWindowChecks {
    duration_at_least_requested: bool,
    duration_within_deadline: bool,
    counters_monotonic: bool,
    counter_delta_matches_receipt: bool,
    positive_active_progress: bool,
    all_timer_sources_progressed: bool,
    watchdog_fired: bool,
    watchdog_joined: bool,
}

#[derive(Clone, Copy)]
struct ActiveWindowObservation<'a> {
    elapsed_us: u64,
    barrier_count: u64,
    start_count: u64,
    end_count: u64,
    total_count: u64,
    recorded_active_events: u64,
    active_source_ids: &'a BTreeSet<String>,
    unexpected_source_id: Option<&'a str>,
    watchdog_fired: bool,
    watchdog_joined: bool,
    window_observed: bool,
}

fn check_active_window(
    observation: ActiveWindowObservation<'_>,
    window_ms: u64,
    maximum_elapsed_us: u64,
    expected_source_ids: &BTreeSet<String>,
) -> ActiveWindowChecks {
    let ActiveWindowObservation {
        elapsed_us,
        barrier_count,
        start_count,
        end_count,
        total_count,
        recorded_active_events,
        active_source_ids,
        unexpected_source_id,
        watchdog_fired,
        watchdog_joined,
        window_observed,
    } = observation;
    let counter_delta = end_count.checked_sub(start_count);
    ActiveWindowChecks {
        duration_at_least_requested: window_observed
            && elapsed_us >= window_ms.saturating_mul(1_000),
        duration_within_deadline: window_observed && elapsed_us <= maximum_elapsed_us,
        counters_monotonic: window_observed
            && barrier_count <= start_count
            && start_count <= end_count
            && end_count <= total_count,
        counter_delta_matches_receipt: window_observed
            && counter_delta == Some(recorded_active_events),
        positive_active_progress: window_observed
            && recorded_active_events > 0
            && counter_delta.is_some_and(|delta| delta > 0),
        all_timer_sources_progressed: window_observed
            && active_source_ids == expected_source_ids
            && unexpected_source_id.is_none(),
        watchdog_fired: window_observed && watchdog_fired,
        watchdog_joined: window_observed && watchdog_joined,
    }
}

struct MixedExecutor {
    runtime: RuntimeGraphExecutor,
    timer_events: Arc<AtomicU64>,
    timer_sources: Arc<Mutex<BTreeSet<String>>>,
    active_timer_progress: Arc<ActiveTimerProgress>,
}

struct StalledTerminalExecutor;

impl GraphExecutor for StalledTerminalExecutor {
    fn execute(&self, _node: &GraphNode, io: &mut NodeIo, _context: &GraphContext) -> Result<()> {
        while !io.is_cancelled() {
            thread::sleep(Duration::from_millis(1));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReadinessWatchdogState {
    Waiting,
    Ready,
    Stopped,
    TimedOut,
}

impl ReadinessWatchdogState {
    fn label(self) -> &'static str {
        match self {
            Self::Waiting => "waiting",
            Self::Ready => "stopped_after_ready",
            Self::Stopped => "stopped_before_deadline",
            Self::TimedOut => "deadline_cancelled_graph",
        }
    }
}

struct ReadinessWatchdog {
    started: Instant,
    deadline: Instant,
    shared: Arc<(Mutex<ReadinessWatchdogState>, Condvar)>,
    cancelled: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl ReadinessWatchdog {
    fn start(timeout: Duration, cancelled: Arc<AtomicBool>) -> Self {
        let started = Instant::now();
        let deadline = started + timeout;
        let shared = Arc::new((Mutex::new(ReadinessWatchdogState::Waiting), Condvar::new()));
        let worker_shared = Arc::clone(&shared);
        let worker_cancelled = Arc::clone(&cancelled);
        let worker = thread::spawn(move || {
            let (state_lock, changed) = &*worker_shared;
            let mut state = state_lock
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            while *state == ReadinessWatchdogState::Waiting {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    *state = ReadinessWatchdogState::TimedOut;
                    worker_cancelled.store(true, Ordering::Release);
                    changed.notify_all();
                    break;
                }
                let waited = changed.wait_timeout(state, remaining);
                let (next_state, _) = waited.unwrap_or_else(std::sync::PoisonError::into_inner);
                state = next_state;
            }
        });
        Self {
            started,
            deadline,
            shared,
            cancelled,
            worker: Some(worker),
        }
    }

    fn mark_ready(&self) -> bool {
        let (state_lock, changed) = &*self.shared;
        let mut state = state_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if *state != ReadinessWatchdogState::Waiting {
            return *state == ReadinessWatchdogState::Ready;
        }
        if Instant::now() >= self.deadline {
            *state = ReadinessWatchdogState::TimedOut;
            self.cancelled.store(true, Ordering::Release);
            changed.notify_all();
            return false;
        }
        *state = ReadinessWatchdogState::Ready;
        changed.notify_all();
        true
    }

    fn state(&self) -> ReadinessWatchdogState {
        let (state_lock, changed) = &*self.shared;
        let mut state = state_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if *state == ReadinessWatchdogState::Waiting && Instant::now() >= self.deadline {
            *state = ReadinessWatchdogState::TimedOut;
            self.cancelled.store(true, Ordering::Release);
            changed.notify_all();
        }
        *state
    }

    fn finish(&mut self) -> (ReadinessWatchdogState, bool) {
        let (state_lock, changed) = &*self.shared;
        let mut state = state_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if *state == ReadinessWatchdogState::Waiting {
            if Instant::now() >= self.deadline {
                *state = ReadinessWatchdogState::TimedOut;
                self.cancelled.store(true, Ordering::Release);
            } else {
                *state = ReadinessWatchdogState::Stopped;
            }
        }
        let final_state = *state;
        changed.notify_all();
        drop(state);
        let joined = self
            .worker
            .take()
            .is_some_and(|worker| worker.join().is_ok());
        (final_state, joined)
    }
}

struct ReadinessWaitOutcome {
    ready: bool,
    timed_out: bool,
    elapsed: Duration,
    failure: Option<anyhow::Error>,
    watchdog_state: ReadinessWatchdogState,
    watchdog_joined: bool,
}

fn wait_for_readiness(
    session: &mut GraphSession,
    cancelled: Arc<AtomicBool>,
    timeout: Duration,
    state: &mut OutputReceipts,
    mut is_ready: impl FnMut(&OutputReceipts) -> bool,
    mut on_output: impl FnMut(&mut OutputReceipts, graph::EventEnvelope),
) -> ReadinessWaitOutcome {
    let mut watchdog = ReadinessWatchdog::start(timeout, cancelled);
    let mut ready = false;
    let mut failure = None;
    loop {
        if watchdog.state() == ReadinessWatchdogState::TimedOut {
            break;
        }
        if is_ready(state) {
            ready = watchdog.mark_ready();
            break;
        }
        if watchdog.state() == ReadinessWatchdogState::TimedOut {
            break;
        }
        match session.recv_output() {
            Ok(Some(event)) => on_output(state, event),
            Ok(None) => {
                failure = Some(anyhow!(
                    "graph closed terminal outputs before every actor source was observed"
                ));
                break;
            }
            Err(error) => {
                if watchdog.state() != ReadinessWatchdogState::TimedOut {
                    failure = Some(error);
                }
                break;
            }
        }
    }
    let elapsed = watchdog.started.elapsed();
    let (watchdog_state, watchdog_joined) = watchdog.finish();
    ReadinessWaitOutcome {
        ready: ready && watchdog_state == ReadinessWatchdogState::Ready,
        timed_out: watchdog_state == ReadinessWatchdogState::TimedOut,
        elapsed,
        failure,
        watchdog_state,
        watchdog_joined,
    }
}

impl GraphExecutor for MixedExecutor {
    fn execute(&self, node: &GraphNode, io: &mut NodeIo, context: &GraphContext) -> Result<()> {
        if matches!(
            &node.kind,
            GraphNodeKind::Renderer { backend } if backend == "stress.timer_collector"
        ) {
            loop {
                match io.recv_any() {
                    Ok(Some(event)) => {
                        if !matches!(event.payload, PortValue::Value(Value::Integer(_))) {
                            bail!("timer collector received a non-integer payload");
                        }
                        self.active_timer_progress
                            .record_event(&event.port.node, &self.timer_events);
                        self.timer_sources
                            .lock()
                            .map_err(|_| anyhow!("timer source receipt lock was poisoned"))?
                            .insert(event.port.node.clone());
                    }
                    Ok(None) => return Ok(()),
                    Err(_) if io.is_cancelled() => return Ok(()),
                    Err(error) => return Err(error),
                }
            }
        }
        self.runtime.execute(node, io, context)
    }
}

struct Evidence {
    path: PathBuf,
    raw: JsonValue,
}

impl Evidence {
    fn new(path: PathBuf, total: usize, window_ms: u64) -> Result<Self> {
        let layout = ActorLayout::for_total(total)?;
        let started_utc_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("system clock is before Unix epoch")?
            .as_millis();
        let raw = json!({
            "schema": "execute.graph-mixed-lifecycle.raw/1",
            "trial_type": "measurement",
            "status": "starting",
            "started_utc_ms": started_utc_ms,
            "parameters": {
                "total_graph_actors": total,
                "active_window_ms": window_ms
            },
            "actor_layout": layout.json(),
            "timings_us": {},
            "resources": {},
            "correctness": {},
            "scope_limits": {
                "rss_and_threads": "Rust probe process only; sampled RSS may miss short peaks",
                "native_processes": "finite command launch count is recorded; child RSS/CPU and peak concurrent process-tree size are not measured",
                "registry_workers": "ordinary applet workers are retained by the registry pool and are expected to reap when the final registry/executor reference is dropped",
                "program_services": "service fixture is pure program-module encoding; no child command or worker is launched by service handlers",
                "thread_admission": "graph actor tasks are capped at the requested actor count; remote event readers are separately capped at eight; the probe adds one resource monitor, one loopback server, and one watchdog, while runtime command I/O helpers have no separate probe-wide thread cap",
                "backpressure": "this probe uses capacity-one lossless timer fan-in, but does not assert that it saturated; graph_program_services has a separate observed full-queue cancellation regression",
                "active_window": "duration, exact event-counter deltas, and per-timer progress are measured with synchronized instrumentation; this is not a cross-revision throughput comparison",
                "remote": "loopback fixture only; remote server resources are not measured",
                "system": "does not certify machine-wide resource ceilings, disk, power, WAN, desktop, or platform fault behavior"
            }
        });
        let mut evidence = Self { path, raw };
        evidence.raw["harness_source_sha256"] = json!(format!(
            "{:x}",
            Sha256::digest(include_bytes!("graph_mixed_lifecycle_bench.rs"))
        ));
        evidence.checkpoint()?;
        Ok(evidence)
    }

    fn status(&mut self, status: &str) -> Result<()> {
        self.raw["status"] = json!(status);
        self.checkpoint()
    }

    fn checkpoint(&self) -> Result<()> {
        let parent = self
            .path
            .parent()
            .ok_or_else(|| anyhow!("raw report path has no parent directory"))?;
        fs::create_dir_all(parent)?;
        let file_name = self
            .path
            .file_name()
            .ok_or_else(|| anyhow!("raw report path has no file name"))?
            .to_string_lossy();
        let temporary = parent.join(format!(".{file_name}.partial"));
        fs::write(
            &temporary,
            serde_json::to_vec_pretty(&self.raw).context("encoding raw stress checkpoint")?,
        )?;
        fs::rename(temporary, &self.path).context("atomically replacing raw stress checkpoint")
    }

    fn fail(&mut self, error: &anyhow::Error) -> Result<()> {
        self.raw["status"] = json!("failed");
        self.raw["failure"] = json!(format!("{error:#}"));
        self.checkpoint()
    }
}

#[derive(Default)]
struct RemoteFixtureStats {
    requests: AtomicUsize,
    discoveries: AtomicUsize,
    event_streams: AtomicUsize,
}

#[test]
#[ignore = "explicit mixed RuntimeGraphExecutor stress measurement; run only through benchmarks/stress_evidence.py"]
fn mixed_graph_lifecycle_stress() {
    let total = env_usize("GRAPH_MIXED_STRESS_NODES", 32, 32, MAX_ACTORS);
    let window_ms = env_u64(
        "GRAPH_MIXED_STRESS_WINDOW_MS",
        1_000,
        MIN_ACTIVE_WINDOW_MS,
        MAX_ACTIVE_WINDOW_MS,
    );
    let report_path = PathBuf::from(
        std::env::var_os("GRAPH_MIXED_STRESS_RAW_REPORT")
            .expect("GRAPH_MIXED_STRESS_RAW_REPORT must be set by stress_evidence.py"),
    );
    let mut evidence = Evidence::new(report_path, total, window_ms).expect("create raw report");
    if let Err(error) = run_mixed_stress(total, window_ms, &mut evidence) {
        evidence
            .fail(&error)
            .expect("persist failed raw report checkpoint");
        panic!("mixed graph stress failed: {error:#}");
    }
}

fn run_mixed_stress(total: usize, window_ms: u64, evidence: &mut Evidence) -> Result<()> {
    let layout = ActorLayout::for_total(total)?;
    let expected_timer_sources = (0..layout.timers)
        .map(|index| format!("timer_{index:04}"))
        .collect::<BTreeSet<_>>();
    let fixture_started = Instant::now();
    let fixture = mixed_fixture(layout)?;
    let fixture_ready_us = fixture_started.elapsed().as_micros() as u64;
    evidence.raw["timings_us"]["fixture_setup"] = json!(fixture_ready_us);
    evidence.raw["fixture"] = json!({
        "worker_identities": layout.workers,
        "worker_pool_capacity": WORKER_POOL_CAPACITY,
        "native_binary": fixture.native_binary,
        "worker_interpreter": fixture.worker_interpreter,
        "remote_endpoint": "127.0.0.1:<ephemeral-port>",
        "service_handler": "accept one record; scheduled pure encode to declared typed event"
    });
    evidence.raw["workers"] = json!({
        "expected_distinct_identities": layout.workers,
        "worker_pids": [],
        "marker_paths": fixture
            .worker_markers
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>(),
        "ownership": "registry-shared ordinary applet pool; worker PIDs are isolated process-group leaders"
    });
    evidence.status("fixtures_ready")?;

    let plan_started = Instant::now();
    let mut plan = graph::parse(&fixture.graph_source, &fixture.registry)
        .context("parsing mixed runtime graph")?;
    let timer_schema = flow::contracts::ValueSchema {
        scalar_only: false,
        additional_properties: None,
        min_properties: None,
        max_properties: None,
        nonempty: false,
        kind: flow::contracts::OutputType::Integer,
        nullable: false,
        items: None,
        fields: BTreeMap::new(),
        required: Vec::new(),
        enum_values: None,
        single_line: false,
    };
    plan.add_node(GraphNode {
        id: "timer_collector".into(),
        kind: GraphNodeKind::Renderer {
            backend: "stress.timer_collector".into(),
        },
        inputs: BTreeMap::from([("ticks".into(), PortSchema::events(timer_schema))]),
        outputs: BTreeMap::new(),
        events: BTreeMap::new(),
        line: None,
    })?;
    for index in 0..layout.timers {
        plan.add_edge(EdgeSpec::lossless(
            PortRef::new(format!("timer_{index:04}"), "tick"),
            PortRef::new("timer_collector", "ticks"),
            1,
        ));
    }
    if plan.nodes().len() != layout.total() {
        bail!(
            "mixed graph has {} nodes; expected {}",
            plan.nodes().len(),
            layout.total()
        );
    }
    plan.validate_with_registry(&fixture.registry)
        .context("validating mixed runtime graph")?;
    let plan_build_us = plan_started.elapsed().as_micros() as u64;
    evidence.raw["timings_us"]["plan_parse_validate"] = json!(plan_build_us);
    evidence.raw["graph"] = json!({
        "node_count": plan.nodes().len(),
        "edge_count": plan.edges().len(),
        "lossless_timer_edge_capacity": 1,
        "max_parallel_admission": total,
        "max_node_admission": total,
        "listener_reader_admission": layout.remote,
        "graph_deadline_budget_ms": window_ms + GRAPH_RUN_DEADLINE_GRACE.as_millis() as u64,
        "active_window_observation_tolerance_ms": ACTIVE_WINDOW_OBSERVATION_TOLERANCE.as_millis()
    });
    evidence.status("plan_validated")?;

    let module_session = Arc::new(
        ProgramModuleSession::preload_invocations(&fixture.registry, plan.invocations())
            .context("preloading scheduled program module")?,
    );
    let run_options = RunOptions {
        max_output_bytes: 1024 * 1024,
        command_timeout: Some(Duration::from_secs(5)),
        total_timeout: Some(Duration::from_secs(15)),
        ..RunOptions::new(fixture.root.path().to_path_buf())
    };
    let runtime = RuntimeGraphExecutor::with_module_session(
        fixture.registry.clone(),
        run_options,
        Arc::clone(&module_session),
    );
    let timer_events = Arc::new(AtomicU64::new(0));
    let timer_sources = Arc::new(Mutex::new(BTreeSet::new()));
    let active_timer_progress = Arc::new(ActiveTimerProgress::new(expected_timer_sources.clone()));
    let executor: Arc<dyn GraphExecutor> = Arc::new(MixedExecutor {
        runtime,
        timer_events: Arc::clone(&timer_events),
        timer_sources: Arc::clone(&timer_sources),
        active_timer_progress: Arc::clone(&active_timer_progress),
    });
    let cancelled = Arc::new(AtomicBool::new(false));
    let mut graph_options = GraphRunOptions {
        max_parallel: total,
        max_nodes: total,
        cancelled: Arc::clone(&cancelled),
        deadline: Some(
            Instant::now() + Duration::from_millis(window_ms) + GRAPH_RUN_DEADLINE_GRACE,
        ),
        ..GraphRunOptions::default()
    };
    graph_options.max_listener_readers = layout.remote;

    let baseline = proc_snapshot()?;
    let peak_rss = Arc::new(AtomicUsize::new(baseline.rss_bytes));
    let peak_threads = Arc::new(AtomicUsize::new(baseline.threads));
    let (remote_server, remote_stop) = fixture.spawn_remote_server()?;
    let monitor_stop = Arc::new(AtomicBool::new(false));
    let monitor = spawn_resource_monitor(
        Arc::clone(&peak_rss),
        Arc::clone(&peak_threads),
        Arc::clone(&monitor_stop),
    );
    let start = Instant::now();
    let mut session = match GraphSession::start(plan, executor.clone(), graph_options) {
        Ok(session) => session,
        Err(error) => {
            remote_stop.store(true, Ordering::Release);
            let _ = remote_server.join();
            monitor_stop.store(true, Ordering::Release);
            let _ = monitor.join();
            return Err(error).context("starting mixed graph session");
        }
    };
    let startup_us = start.elapsed().as_micros() as u64;
    evidence.raw["timings_us"]["graph_start"] = json!(startup_us);
    if let Err(error) = evidence.status("running") {
        cancelled.store(true, Ordering::Release);
        let _ = session.cancel_and_join();
        remote_stop.store(true, Ordering::Release);
        let _ = remote_server.join();
        monitor_stop.store(true, Ordering::Release);
        let _ = monitor.join();
        return Err(error).context("writing running-state stress checkpoint");
    }

    let metrics = Arc::clone(&session.context().metrics);
    let mut output_receipts = OutputReceipts::default();
    let mut readiness = wait_for_readiness(
        &mut session,
        Arc::clone(&cancelled),
        ACTOR_READINESS_TIMEOUT,
        &mut output_receipts,
        |receipts| {
            let observed_timers = timer_sources
                .lock()
                .map(|sources| sources.clone())
                .unwrap_or_default();
            missing_actor_sources(layout, &receipts.nodes, &observed_timers).is_empty()
        },
        |receipts, event| record_terminal_output(event, &mut receipts.counts, &mut receipts.nodes),
    );
    let actors_ready = readiness.ready;
    let readiness_timed_out = readiness.timed_out;
    let readiness_error = readiness.failure.as_ref().map(|error| format!("{error:#}"));
    let mut reader_error = readiness.failure.take();
    let mut output_counts = output_receipts.counts;
    let mut output_nodes = output_receipts.nodes;
    let readiness_elapsed_us = readiness.elapsed.as_micros() as u64;
    let timer_sources_at_readiness = timer_sources
        .lock()
        .map(|sources| sources.clone())
        .unwrap_or_default();
    let timer_events_at_readiness_barrier = timer_events.load(Ordering::Acquire);
    let missing_sources_at_readiness =
        missing_actor_sources(layout, &output_nodes, &timer_sources_at_readiness);
    let readiness_status = if actors_ready {
        "ready"
    } else if readiness_timed_out {
        "timed_out"
    } else {
        "incomplete"
    };
    evidence.raw["timings_us"]["actor_readiness_warmup"] = json!(readiness_elapsed_us);
    evidence.raw["readiness"] = json!({
        "status": readiness_status,
        "timeout_ms": ACTOR_READINESS_TIMEOUT.as_millis(),
        "warmup_elapsed_us": readiness_elapsed_us,
        "watchdog_status": readiness.watchdog_state.label(),
        "watchdog_joined": readiness.watchdog_joined,
        "active_window_started_after_readiness": actors_ready,
        "expected_graph_actor_count": layout.total(),
        "observed_terminal_node_count": output_nodes.values().map(BTreeSet::len).sum::<usize>(),
        "observed_timer_source_ids": timer_sources_at_readiness,
        "timer_events_consumed_at_barrier": timer_events_at_readiness_barrier,
        "missing_actor_sources": missing_sources_at_readiness,
        "failure": readiness_error
    });
    let readiness_checkpoint = if actors_ready {
        evidence.status("actors_ready")
    } else {
        evidence.status("readiness_failed")
    };
    if let Err(error) = readiness_checkpoint {
        cancelled.store(true, Ordering::Release);
        let _ = session.cancel_and_join();
        remote_stop.store(true, Ordering::Release);
        let _ = remote_server.join();
        monitor_stop.store(true, Ordering::Release);
        let _ = monitor.join();
        return Err(error).context("writing actor-readiness stress checkpoint");
    }

    let mut active_timer_window = None;
    let (
        active_elapsed_us,
        watchdog_join,
        active_watchdog_fired,
        timer_events_at_active_start,
        timer_events_at_active_end,
    ) = if actors_ready {
        if let Err(error) = evidence.status("active_window") {
            cancelled.store(true, Ordering::Release);
            let _ = session.cancel_and_join();
            remote_stop.store(true, Ordering::Release);
            let _ = remote_server.join();
            monitor_stop.store(true, Ordering::Release);
            let _ = monitor.join();
            return Err(error).context("writing active-window stress checkpoint");
        }
        let watchdog_stop = Arc::new(AtomicBool::new(false));
        let watchdog_cancel = Arc::clone(&cancelled);
        let watchdog_done = Arc::clone(&watchdog_stop);
        let watchdog_fired = Arc::new(AtomicBool::new(false));
        let watchdog_did_fire = Arc::clone(&watchdog_fired);
        let active_start = active_timer_progress.start_window(&timer_events);
        let active_deadline = active_start.started_at + Duration::from_millis(window_ms);
        let watchdog = thread::spawn(move || {
            while !watchdog_done.load(Ordering::Acquire) && Instant::now() < active_deadline {
                thread::sleep(PROBE_HELPER_POLL);
            }
            if !watchdog_done.load(Ordering::Acquire) {
                watchdog_did_fire.store(true, Ordering::Release);
                watchdog_cancel.store(true, Ordering::Release);
            }
        });

        while !cancelled.load(Ordering::Acquire) {
            match session.recv_output() {
                Ok(Some(event)) => {
                    record_terminal_output(event, &mut output_counts, &mut output_nodes);
                }
                Ok(None) => break,
                Err(_) if cancelled.load(Ordering::Acquire) => break,
                Err(error) => {
                    reader_error = Some(error);
                    break;
                }
            }
        }
        let active_window = active_timer_progress.stop_window(&timer_events);
        watchdog_stop.store(true, Ordering::Release);
        let watchdog_join = watchdog.join();
        let active_elapsed_us = active_window
            .ended_at
            .duration_since(active_start.started_at)
            .as_micros() as u64;
        let active_watchdog_fired = watchdog_fired.load(Ordering::Acquire);
        let timer_events_at_active_start = active_start.event_count;
        let timer_events_at_active_end = active_window.events_at_end;
        active_timer_window = Some(active_window);
        (
            active_elapsed_us,
            Some(watchdog_join),
            active_watchdog_fired,
            Some(timer_events_at_active_start),
            Some(timer_events_at_active_end),
        )
    } else {
        cancelled.store(true, Ordering::Release);
        (0, None, false, None, None)
    };
    let active_watchdog_joined = watchdog_join.as_ref().is_some_and(Result::is_ok);
    cancelled.store(true, Ordering::Release);
    let cancellation_started = Instant::now();
    let cancel_result = session.cancel_and_join();
    let cancel_join_us = cancellation_started.elapsed().as_micros() as u64;
    remote_stop.store(true, Ordering::Release);
    let remote_join = remote_server.join();
    let metrics_processes = metrics.launched_processes();
    let service_nodes = output_nodes.get("service").cloned().unwrap_or_default();
    let remote_nodes = output_nodes.get("remote").cloned().unwrap_or_default();
    let native_nodes = output_nodes.get("native").cloned().unwrap_or_default();
    let worker_nodes = output_nodes.get("worker").cloned().unwrap_or_default();
    let in_process_nodes = output_nodes.get("inprocess").cloned().unwrap_or_default();
    let (worker_pids, worker_marker_error) = match read_worker_pids(&fixture.worker_markers) {
        Ok(pids) => (pids, None),
        Err(error) => (Vec::new(), Some(format!("{error:#}"))),
    };
    let workers_alive_before_registry_drop =
        !worker_pids.is_empty() && worker_pids.iter().all(|pid| process_exists(*pid));
    drop(session);
    drop(executor);
    drop(module_session);
    drop(fixture.registry);
    let worker_reap_started = Instant::now();
    let workers_reaped =
        wait_for_workers_reaped(&worker_pids, Instant::now() + Duration::from_secs(5));
    let worker_reap_us = worker_reap_started.elapsed().as_micros() as u64;
    drop(fixture.root);

    monitor_stop.store(true, Ordering::Release);
    let monitor_join = monitor.join();
    let after = proc_snapshot()?;
    let peak = ProcSnapshot {
        rss_bytes: peak_rss.load(Ordering::Relaxed),
        threads: peak_threads.load(Ordering::Relaxed),
    };
    let requests = fixture.remote_stats.requests.load(Ordering::Relaxed);
    let discoveries = fixture.remote_stats.discoveries.load(Ordering::Relaxed);
    let event_streams = fixture.remote_stats.event_streams.load(Ordering::Relaxed);
    let timer_sources_with_output = timer_sources
        .lock()
        .map(|sources| sources.clone())
        .unwrap_or_default();
    let missing_timer_source_ids = expected_timer_sources
        .difference(&timer_sources_with_output)
        .cloned()
        .collect::<Vec<_>>();
    let unexpected_timer_source_ids = timer_sources_with_output
        .difference(&expected_timer_sources)
        .cloned()
        .collect::<Vec<_>>();
    let all_expected_timer_sources_observed = timer_sources_with_output == expected_timer_sources;
    let timer_source_ids_with_output = timer_sources_with_output
        .iter()
        .cloned()
        .collect::<Vec<_>>();
    let distinct_timer_sources_with_output = timer_source_ids_with_output.len();
    let timer_events_total = timer_events.load(Ordering::Acquire);
    let active_timer_source_ids = active_timer_window
        .as_ref()
        .map(|window| window.source_ids.iter().cloned().collect::<Vec<_>>())
        .unwrap_or_default();
    let active_timer_source_set = active_timer_source_ids
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    let missing_active_timer_source_ids = expected_timer_sources
        .difference(&active_timer_source_set)
        .cloned()
        .collect::<Vec<_>>();
    let active_timer_unexpected_source_id = active_timer_window
        .as_ref()
        .and_then(|window| window.unexpected_source_id.clone());
    let active_timer_events_recorded = active_timer_window
        .as_ref()
        .map(|window| window.event_count);
    let timer_events_during_active_window = timer_events_at_active_start
        .zip(timer_events_at_active_end)
        .and_then(|(start, end)| end.checked_sub(start));
    let timer_events_after_active_window_end =
        timer_events_at_active_end.and_then(|end| timer_events_total.checked_sub(end));
    let active_window_deadline_max_us = (window_ms
        + GRAPH_RUN_DEADLINE_GRACE.as_millis() as u64
        + ACTIVE_WINDOW_OBSERVATION_TOLERANCE.as_millis() as u64)
        .saturating_mul(1_000);
    let active_window_checks = check_active_window(
        ActiveWindowObservation {
            elapsed_us: active_elapsed_us,
            barrier_count: timer_events_at_readiness_barrier,
            start_count: timer_events_at_active_start.unwrap_or_default(),
            end_count: timer_events_at_active_end.unwrap_or_default(),
            total_count: timer_events_total,
            recorded_active_events: active_timer_events_recorded.unwrap_or_default(),
            active_source_ids: &active_timer_source_set,
            unexpected_source_id: active_timer_unexpected_source_id.as_deref(),
            watchdog_fired: active_watchdog_fired,
            watchdog_joined: active_watchdog_joined,
            window_observed: active_timer_window.is_some(),
        },
        window_ms,
        active_window_deadline_max_us,
        &expected_timer_sources,
    );

    evidence.raw["timings_us"]["active_window_observed"] = json!(active_elapsed_us);
    evidence.raw["timings_us"]["cancel_and_join"] = json!(cancel_join_us);
    evidence.raw["timings_us"]["registry_worker_reap"] = json!(worker_reap_us);
    evidence.raw["outputs"] = json!({
        "terminal_event_count_by_category": output_counts,
        "distinct_nodes_with_terminal_output": {
            "native": native_nodes.len(),
            "worker": worker_nodes.len(),
            "in_process": in_process_nodes.len(),
            "service_event": service_nodes.len(),
            "remote_event": remote_nodes.len()
        },
        "distinct_timer_sources_with_output": distinct_timer_sources_with_output,
        "timer_source_ids_with_output": timer_source_ids_with_output,
        "missing_timer_source_ids": missing_timer_source_ids,
        "unexpected_timer_source_ids": unexpected_timer_source_ids,
        "timer_events_consumed_by_capacity_one_lossless_collector": timer_events_total,
        "timer_events_consumed_total_including_warmup": timer_events_total,
        "timer_events_consumed_at_readiness_barrier": timer_events_at_readiness_barrier,
        "timer_events_consumed_before_active_window": timer_events_at_active_start,
        "timer_events_consumed_at_active_window_end": timer_events_at_active_end,
        "timer_events_consumed_during_active_window": timer_events_during_active_window,
        "timer_events_recorded_during_active_window": active_timer_events_recorded,
        "timer_events_consumed_after_active_window_end": timer_events_after_active_window_end,
        "active_timer_source_ids": active_timer_source_ids,
        "missing_active_timer_source_ids": missing_active_timer_source_ids,
        "unexpected_active_timer_source_id": active_timer_unexpected_source_id,
        "active_window_watchdog_fired": active_watchdog_fired,
        "active_window_watchdog_joined": active_watchdog_joined,
    });
    evidence.raw["remote_fixture"] = json!({
        "requests": requests,
        "discoveries": discoveries,
        "event_streams": event_streams,
        "expected_discoveries": layout.remote,
        "expected_event_streams": layout.remote
    });
    evidence.raw["workers"] = json!({
        "expected_distinct_identities": layout.workers,
        "marker_pid_count": worker_pids.len(),
        "worker_pids": worker_pids.clone(),
        "marker_paths": fixture
            .worker_markers
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>(),
        "marker_error": worker_marker_error.clone(),
        "processes_alive_before_registry_drop": workers_alive_before_registry_drop,
        "all_reaped_after_registry_drop": workers_reaped,
        "ownership": "registry-shared ordinary applet pool; graph cancellation joins actor tasks, final registry/executor drop reaps retained workers"
    });
    evidence.raw["resources"] = json!({
        "baseline": {"rss_bytes": baseline.rss_bytes, "threads": baseline.threads},
        "peak_sampled": {"rss_bytes": peak.rss_bytes, "threads": peak.threads},
        "after_join_and_reap": {"rss_bytes": after.rss_bytes, "threads": after.threads},
        "launched_processes_reported_by_graph_metrics": metrics_processes,
        "expected_finite_native_launches": layout.native,
        "expected_registry_worker_launches": layout.workers,
        "worker_pids": worker_pids.clone()
    });
    evidence.raw["correctness"] = json!({
        "expected_graph_actor_count": layout.total(),
        "all_actor_sources_ready_before_active_window": actors_ready,
        "all_native_finite_nodes_emitted": native_nodes.len() == layout.native,
        "all_worker_nodes_emitted": worker_nodes.len() == layout.workers,
        "all_in_process_nodes_emitted": in_process_nodes.len() == layout.in_process,
        "all_program_services_emitted": service_nodes.len() == layout.services,
        "all_remote_listeners_emitted": remote_nodes.len() == layout.remote,
        "all_timer_nodes_emitted": all_expected_timer_sources_observed,
        "readiness_watchdog_joined": readiness.watchdog_joined,
        "active_window_duration_at_least_requested": active_window_checks.duration_at_least_requested,
        "active_window_duration_within_graph_deadline": active_window_checks.duration_within_deadline,
        "timer_event_counter_monotonic": active_window_checks.counters_monotonic,
        "active_timer_counter_matches_window_delta": active_window_checks.counter_delta_matches_receipt,
        "positive_active_timer_progress": active_window_checks.positive_active_progress,
        "all_expected_timer_sources_progressed_during_active_window": active_window_checks.all_timer_sources_progressed,
        "active_window_watchdog_fired": active_window_checks.watchdog_fired,
        "active_window_watchdog_joined": active_window_checks.watchdog_joined,
        "timer_events_consumed": timer_events_total > 0,
        "remote_requests_complete": discoveries == layout.remote && event_streams == layout.remote && requests == 2 * layout.remote,
        "worker_pool_capacity_respected": layout.workers <= WORKER_POOL_CAPACITY && worker_pids.len() == layout.workers,
        "workers_alive_before_registry_drop": workers_alive_before_registry_drop,
        "workers_reaped_after_registry_drop": workers_reaped,
        "graph_metrics_match_native_plus_worker_starts": metrics_processes == layout.native + layout.workers,
        "threads_returned_to_baseline_plus_two": after.threads <= baseline.threads + 2,
        "cancel_join_and_helper_threads_succeeded": cancel_result.is_ok() && readiness.watchdog_joined && active_watchdog_joined && watchdog_join.as_ref().is_some_and(Result::is_ok) && monitor_join.is_ok() && remote_join.is_ok(),
        "unexpected_output_reader_error": reader_error.as_ref().map(|error| format!("{error:#}"))
    });
    evidence.status("completed")?;

    if let Some(error) = reader_error {
        return Err(error).context("reading mixed graph terminal outputs");
    }
    if !readiness.watchdog_joined {
        bail!("actor-readiness watchdog thread did not join cleanly");
    }
    if !actors_ready {
        let missing = missing_actor_sources(layout, &output_nodes, &timer_sources_with_output);
        let readiness_failure = if readiness_timed_out {
            format!("timed out after {} ms", ACTOR_READINESS_TIMEOUT.as_millis())
        } else if let Some(error) = readiness_error.as_deref() {
            format!("ended before readiness: {error}")
        } else {
            "ended before readiness".to_owned()
        };
        bail!(
            "actor readiness {readiness_status} ({readiness_failure}); missing sources: {}",
            if missing.is_empty() {
                "none".to_owned()
            } else {
                missing.join(", ")
            }
        );
    }
    if let Some(error) = worker_marker_error {
        bail!("reading worker process receipts: {error}");
    }
    cancel_result.context("cancelling and joining mixed graph")?;
    if watchdog_join.as_ref().is_some_and(Result::is_err)
        || monitor_join.is_err()
        || remote_join.is_err()
    {
        bail!("one of the mixed stress helper threads panicked");
    }
    if native_nodes.len() != layout.native
        || worker_nodes.len() != layout.workers
        || in_process_nodes.len() != layout.in_process
        || service_nodes.len() != layout.services
        || remote_nodes.len() != layout.remote
    {
        bail!("one or more mixed runtime actor classes did not emit an output");
    }
    if !all_expected_timer_sources_observed {
        bail!(
            "timer fan-in observed {distinct_timer_sources_with_output} distinct timer nodes; expected {}; missing: {}; unexpected: {}",
            layout.timers,
            if missing_timer_source_ids.is_empty() { "none".to_owned() } else { missing_timer_source_ids.join(", ") },
            if unexpected_timer_source_ids.is_empty() { "none".to_owned() } else { unexpected_timer_source_ids.join(", ") }
        );
    }
    if !active_window_checks.duration_at_least_requested {
        bail!(
            "observed active window lasted {active_elapsed_us} us; requested at least {} us",
            window_ms.saturating_mul(1_000)
        );
    }
    if !active_window_checks.duration_within_deadline {
        bail!(
            "observed active window lasted {active_elapsed_us} us; graph-deadline bound is {active_window_deadline_max_us} us"
        );
    }
    if !active_window_checks.counters_monotonic {
        bail!("timer event counters were missing, negative, or non-monotonic across the active window");
    }
    if !active_window_checks.counter_delta_matches_receipt {
        bail!("active timer event count did not match the start/end counter delta");
    }
    if !active_window_checks.positive_active_progress {
        bail!("timer collector observed no events during the active window");
    }
    if !active_window_checks.all_timer_sources_progressed {
        bail!(
            "active window did not observe every timer source; missing: {}; unexpected: {}",
            if missing_active_timer_source_ids.is_empty() {
                "none".to_owned()
            } else {
                missing_active_timer_source_ids.join(", ")
            },
            active_timer_unexpected_source_id
                .as_deref()
                .unwrap_or("none")
        );
    }
    if !active_window_checks.watchdog_fired || !active_window_checks.watchdog_joined {
        bail!("active-window watchdog did not fire and join cleanly");
    }
    if requests != 2 * layout.remote
        || discoveries != layout.remote
        || event_streams != layout.remote
    {
        bail!("loopback remote fixture did not serve every discovery and event stream");
    }
    if !workers_alive_before_registry_drop || worker_pids.len() != layout.workers || !workers_reaped
    {
        bail!("registry-owned worker process launch/reap receipts were incomplete");
    }
    if metrics_processes != layout.native + layout.workers {
        bail!("graph launch count did not equal finite native plus worker process starts");
    }
    if after.threads > baseline.threads + 2 {
        bail!("Rust process thread count did not return to within two of baseline");
    }
    evidence.status("passed")?;
    Ok(())
}

struct MixedFixture {
    root: TempDir,
    registry: Registry,
    graph_source: String,
    native_binary: String,
    worker_interpreter: String,
    worker_markers: Vec<PathBuf>,
    remote_stats: Arc<RemoteFixtureStats>,
    remote_listener: TcpListener,
    remote_descriptor: Vec<u8>,
    remote_expected_requests: usize,
}

impl MixedFixture {
    fn spawn_remote_server(&self) -> Result<(thread::JoinHandle<()>, Arc<AtomicBool>)> {
        let listener = self
            .remote_listener
            .try_clone()
            .context("cloning loopback listener for fixture")?;
        let descriptor = self.remote_descriptor.clone();
        let expected_requests = self.remote_expected_requests;
        let stats = Arc::clone(&self.remote_stats);
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let server = thread::spawn(move || {
            serve_loopback(
                listener,
                &descriptor,
                expected_requests,
                &stats,
                &thread_stop,
            );
        });
        Ok((server, stop))
    }
}

fn mixed_fixture(layout: ActorLayout) -> Result<MixedFixture> {
    let root = tempfile::tempdir().context("creating mixed fixture directory")?;
    let applets = root.path().join("applets");
    fs::create_dir_all(&applets)?;
    let native_binary = executable_in_path("printf")
        .context("locating printf for finite native applet fixture")?
        .to_string_lossy()
        .into_owned();
    let worker_interpreter = executable_in_path("python3")
        .context("locating Python 3 for JSON-lines worker fixture")?
        .to_string_lossy()
        .into_owned();
    let builtin_in_process =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests/core.number_parse_integer.json");
    fs::copy(
        builtin_in_process,
        applets.join("core.number_parse_integer.json"),
    )
    .context("copying declared in-process applet manifest")?;

    let listener =
        TcpListener::bind(("127.0.0.1", 0)).context("binding loopback remote fixture")?;
    listener.set_nonblocking(true)?;
    let endpoint = format!("http://127.0.0.1:{}", listener.local_addr()?.port());
    let remote_contract = json!({
        "version": 1,
        "backend": "remote",
        "remote": {
            "endpoint": endpoint,
            "allow_loopback_http_fixture": true,
            "allow_discovery": true
        },
        "ports": {
            "events": {
                "updates": {
                    "schema": {"type": "text"},
                    "delivery": "events"
                }
            }
        }
    });
    let service_source =
        "accept snapshot as json\nevery 20 milliseconds\n  then encode snapshot as json\n";
    fs::write(applets.join("service.program"), service_source)?;
    let service_manifest = json!({
        "name": "stress.snapshot_service",
        "binary": "@program-module",
        "accepts_stdin": false,
        "parameters": {"snapshot": {"type": "json", "required": true}},
        "arguments": [],
        "contract": {
            "version": 1,
            "backend": "program_module",
            "program": {"source": "service.program", "entry": "main", "interface_version": 2},
            "lifecycle": {"kind": "program_service"},
            "output": {
                "type": "record",
                "encoding": "json",
                "fields": {"count": {"type": "integer"}},
                "required": ["count"]
            },
            "ports": {
                "inputs": {
                    "snapshot": {"schema": {"type": "json"}, "delivery": "single"}
                },
                "events": {
                    "updated": {
                        "schema": {
                            "type": "record",
                            "fields": {"count": {"type": "integer"}},
                            "required": ["count"]
                        },
                        "delivery": "events"
                    }
                }
            }
        }
    });
    write_manifest(&applets, "service.json", &service_manifest)?;

    let native_manifest = json!({
        "name": "stress.native_finite",
        "binary": native_binary.clone(),
        "accepts_stdin": false,
        "parameters": {},
        "arguments": [{"literal": "7"}],
        "contract": {
            "version": 1,
            "backend": "command",
            "lifecycle": {"kind": "oneshot"},
            "output": {"type": "integer", "encoding": "json"},
            "capabilities": [],
            "effects": []
        }
    });
    write_manifest(&applets, "native.json", &native_manifest)?;

    let remote_manifest = json!({
        "name": "stress.remote_feed",
        "binary": "@remote-applet",
        "parameters": {},
        "arguments": [],
        "contract": remote_contract.clone()
    });
    write_manifest(&applets, "remote.json", &remote_manifest)?;

    let worker_fixture =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/json_lines_worker.py");
    let mut worker_markers = Vec::with_capacity(layout.workers);
    for index in 0..layout.workers {
        let marker = root.path().join(format!("worker-{index:04}.pids"));
        let worker_manifest = json!({
            "name": format!("stress.worker_{index:04}"),
            "binary": worker_interpreter.clone(),
            "accepts_stdin": false,
            "parameters": {},
            "arguments": [],
            "contract": {
                "version": 1,
                "backend": "json_lines_worker",
                "lifecycle": {
                    "kind": "json_lines",
                    "startup_arguments": [
                        worker_fixture.clone(),
                        "--worker",
                        "sequence",
                        marker.clone(),
                        marker.with_extension("effects")
                    ],
                    "timeout_ms": 5000,
                    "max_frame_bytes": 1048576,
                    "max_stderr_bytes": 8192
                },
                "output": {"type": "integer", "encoding": "json"},
                "capabilities": [],
                "effects": []
            }
        });
        write_manifest(
            &applets,
            &format!("worker-{index:04}.json"),
            &worker_manifest,
        )?;
        worker_markers.push(marker);
    }
    let registry = Registry::load(&applets).context("loading mixed fixture catalog")?;
    registry.set_host_state(
        WorkerPool::with_capacity(WORKER_POOL_CAPACITY)
            .context("creating explicit registry worker cap")?,
    );

    let descriptor = json!({
        "protocol_version": 1,
        "identity": {"name": "stress.remote_feed", "version": "1"},
        "contract": remote_contract,
        "events": [{
            "port": "updates",
            "format": "sse",
            "max_event_bytes": 4096,
            "resumable": true,
            "backpressure": "pull"
        }]
    });
    let remote_descriptor = serde_json::to_vec(&descriptor)?;
    let mut graph_source = String::from("graph mixed_runtime_stress\n");
    for index in 0..layout.timers {
        graph_source.push_str(&format!("every 10 milliseconds as timer_{index:04}\n"));
    }
    for index in 0..layout.native {
        graph_source.push_str(&format!("run stress.native_finite as native_{index:04}\n"));
    }
    for index in 0..layout.workers {
        graph_source.push_str(&format!(
            "run stress.worker_{index:04} as worker_{index:04}\n"
        ));
    }
    for index in 0..layout.in_process {
        graph_source.push_str(&format!(
            "run number.parse_integer with text \"123\" as inprocess_{index:04}\n"
        ));
    }
    for index in 0..layout.services {
        graph_source.push_str(&format!(
            "run stress.snapshot_service with snapshot {{\"count\":1}} as service_{index:04}\n"
        ));
    }
    for index in 0..layout.remote {
        graph_source.push_str(&format!(
            "listen to stress.remote_feed as remote_{index:04}\n"
        ));
    }
    graph_source.push_str("end graph\n");
    Ok(MixedFixture {
        root,
        registry,
        graph_source,
        native_binary,
        worker_interpreter,
        worker_markers,
        remote_stats: Arc::new(RemoteFixtureStats::default()),
        remote_listener: listener,
        remote_descriptor,
        remote_expected_requests: 2 * layout.remote,
    })
}

fn write_manifest(directory: &Path, filename: &str, manifest: &JsonValue) -> Result<()> {
    fs::write(
        directory.join(filename),
        serde_json::to_vec_pretty(manifest).context("encoding fixture manifest")?,
    )?;
    Ok(())
}

fn serve_loopback(
    listener: TcpListener,
    descriptor: &[u8],
    expected_requests: usize,
    stats: &RemoteFixtureStats,
    stop: &AtomicBool,
) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while stats.requests.load(Ordering::Acquire) < expected_requests
        && !stop.load(Ordering::Acquire)
        && Instant::now() < deadline
    {
        match listener.accept() {
            Ok((stream, _)) => {
                if let Err(error) = serve_request(stream, descriptor, stats) {
                    eprintln!("loopback fixture request failed: {error:#}");
                    return;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(PROBE_HELPER_POLL);
            }
            Err(error) => {
                eprintln!("loopback fixture accept failed: {error}");
                return;
            }
        }
    }
}

fn serve_request(
    mut stream: TcpStream,
    descriptor: &[u8],
    stats: &RemoteFixtureStats,
) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut first_line = String::new();
    reader.read_line(&mut first_line)?;
    if first_line.is_empty() {
        bail!("loopback client closed before sending an HTTP request");
    }
    loop {
        let mut header = String::new();
        let bytes = reader.read_line(&mut header)?;
        if bytes == 0 || header == "\r\n" {
            break;
        }
    }
    let discovery = first_line.contains("/.well-known/execute-applet/v1");
    let (content_type, body) = if discovery {
        stats.discoveries.fetch_add(1, Ordering::Relaxed);
        (
            "application/vnd.execute.applet+json;version=1",
            descriptor.to_vec(),
        )
    } else if first_line.contains("/events/updates") {
        stats.event_streams.fetch_add(1, Ordering::Relaxed);
        (
            "text/event-stream",
            b"id: stress-1\nevent: updated\ndata: \"remote\"\n\n".to_vec(),
        )
    } else {
        bail!("unexpected loopback fixture request {first_line:?}");
    };
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(response.as_bytes())?;
    stream.write_all(&body)?;
    stream.flush()?;
    stats.requests.fetch_add(1, Ordering::Release);
    Ok(())
}

fn spawn_resource_monitor(
    peak_rss: Arc<AtomicUsize>,
    peak_threads: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        while !stop.load(Ordering::Acquire) {
            if let Ok(snapshot) = proc_snapshot() {
                peak_rss.fetch_max(snapshot.rss_bytes, Ordering::Relaxed);
                peak_threads.fetch_max(snapshot.threads, Ordering::Relaxed);
            }
            thread::sleep(Duration::from_millis(5));
        }
    })
}

fn proc_snapshot() -> Result<ProcSnapshot> {
    let status = fs::read_to_string("/proc/self/status").context("reading /proc/self/status")?;
    let mut rss_kib: Option<usize> = None;
    let mut threads: Option<usize> = None;
    for line in status.lines() {
        if let Some(value) = line.strip_prefix("VmRSS:") {
            rss_kib = value
                .split_whitespace()
                .next()
                .and_then(|value| value.parse::<usize>().ok());
        } else if let Some(value) = line.strip_prefix("Threads:") {
            threads = value.trim().parse::<usize>().ok();
        }
    }
    Ok(ProcSnapshot {
        rss_bytes: rss_kib
            .ok_or_else(|| anyhow!("/proc/self/status omitted VmRSS"))?
            .saturating_mul(1024),
        threads: threads.ok_or_else(|| anyhow!("/proc/self/status omitted Threads"))?,
    })
}

fn executable_in_path(name: &str) -> Result<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|directory| directory.join(name))
        .find(|candidate| candidate.is_file())
        .ok_or_else(|| anyhow!("could not find {name:?} on PATH"))
}

fn read_worker_pids(markers: &[PathBuf]) -> Result<Vec<u32>> {
    let mut pids = Vec::with_capacity(markers.len());
    for marker in markers {
        let contents = fs::read_to_string(marker)
            .with_context(|| format!("reading worker marker {}", marker.display()))?;
        let values = contents
            .lines()
            .map(|line| {
                line.parse::<u32>()
                    .with_context(|| format!("invalid worker pid in {}", marker.display()))
            })
            .collect::<Result<Vec<_>>>()?;
        if values.len() != 1 {
            bail!(
                "worker marker {} has {} process ids; expected one retained worker",
                marker.display(),
                values.len()
            );
        }
        pids.push(values[0]);
    }
    Ok(pids)
}

fn wait_for_workers_reaped(pids: &[u32], deadline: Instant) -> bool {
    while Instant::now() < deadline {
        if pids.iter().all(|pid| !process_exists(*pid)) {
            return true;
        }
        thread::sleep(PROBE_HELPER_POLL);
    }
    pids.iter().all(|pid| !process_exists(*pid))
}

fn process_exists(pid: u32) -> bool {
    Path::new("/proc").join(pid.to_string()).exists()
}

fn actor_category(node_id: &str) -> &str {
    if node_id.starts_with("native_") {
        "native"
    } else if node_id.starts_with("worker_") {
        "worker"
    } else if node_id.starts_with("inprocess_") {
        "inprocess"
    } else if node_id.starts_with("service_") {
        "service"
    } else if node_id.starts_with("remote_") {
        "remote"
    } else {
        "other"
    }
}

fn missing_actor_sources(
    layout: ActorLayout,
    output_nodes: &BTreeMap<String, BTreeSet<String>>,
    timer_sources: &BTreeSet<String>,
) -> Vec<String> {
    let mut missing = Vec::new();
    for (category, prefix, count) in [
        ("native", "native", layout.native),
        ("worker", "worker", layout.workers),
        ("inprocess", "inprocess", layout.in_process),
        ("service", "service", layout.services),
        ("remote", "remote", layout.remote),
    ] {
        for index in 0..count {
            let node = format!("{prefix}_{index:04}");
            if !output_nodes
                .get(category)
                .is_some_and(|observed| observed.contains(&node))
            {
                missing.push(format!("{category}:{node}"));
            }
        }
    }
    let expected_timers = (0..layout.timers)
        .map(|index| format!("timer_{index:04}"))
        .collect::<BTreeSet<_>>();
    for node in expected_timers.difference(timer_sources) {
        missing.push(format!("timer:{node}"));
    }
    for node in timer_sources.difference(&expected_timers) {
        missing.push(format!("timer:unexpected:{node}"));
    }
    if timer_sources.is_empty() {
        missing.push("renderer:timer_collector".into());
    }
    missing
}

fn record_terminal_output(
    event: graph::EventEnvelope,
    output_counts: &mut BTreeMap<String, usize>,
    output_nodes: &mut BTreeMap<String, BTreeSet<String>>,
) {
    let category = actor_category(&event.port.node);
    if category != "other" {
        *output_counts.entry(category.to_owned()).or_default() += 1;
        output_nodes
            .entry(category.to_owned())
            .or_default()
            .insert(event.port.node);
    }
}

#[test]
fn actor_readiness_reports_exact_missing_and_unexpected_timer_sources() {
    let layout = ActorLayout::for_total(32).unwrap();
    let mut output_nodes = BTreeMap::<String, BTreeSet<String>>::new();
    for (category, prefix, count) in [
        ("native", "native", layout.native),
        ("worker", "worker", layout.workers),
        ("inprocess", "inprocess", layout.in_process),
        ("service", "service", layout.services),
        ("remote", "remote", layout.remote),
    ] {
        output_nodes.insert(
            category.to_owned(),
            (0..count)
                .map(|index| format!("{prefix}_{index:04}"))
                .collect(),
        );
    }
    let timer_sources = (1..layout.timers)
        .map(|index| format!("timer_{index:04}"))
        .collect::<BTreeSet<_>>();

    assert_eq!(
        missing_actor_sources(layout, &output_nodes, &timer_sources),
        ["timer:timer_0000"]
    );

    let mut output_nodes_missing_native = output_nodes.clone();
    output_nodes_missing_native
        .get_mut("native")
        .unwrap()
        .remove("native_0000");
    assert_eq!(
        missing_actor_sources(layout, &output_nodes_missing_native, &timer_sources),
        ["native:native_0000", "timer:timer_0000"]
    );

    let mut timer_sources = timer_sources;
    timer_sources.insert("timer_unexpected".into());
    assert_eq!(
        missing_actor_sources(layout, &output_nodes, &timer_sources),
        ["timer:timer_0000", "timer:unexpected:timer_unexpected"]
    );
}

#[test]
fn active_timer_window_tracks_only_events_and_sources_inside_its_boundaries() {
    let expected = BTreeSet::from(["timer_0000".to_owned(), "timer_0001".to_owned()]);
    let progress = ActiveTimerProgress::new(expected.clone());
    let total = AtomicU64::new(0);

    progress.record_event("timer_0000", &total);
    let start = progress.start_window(&total);
    progress.record_event("timer_0000", &total);
    progress.record_event("timer_0001", &total);
    let window = progress.stop_window(&total);
    progress.record_event("timer_0000", &total);

    assert_eq!(start.event_count, 1);
    assert_eq!(window.events_at_end, 3);
    assert_eq!(window.event_count, 2);
    assert_eq!(window.source_ids, expected);
    assert_eq!(window.unexpected_source_id, None);
    assert_eq!(total.load(Ordering::Acquire), 4);
}

#[test]
fn active_window_success_requires_duration_timer_progress_and_valid_counters() {
    let expected = BTreeSet::from(["timer_0000".to_owned(), "timer_0001".to_owned()]);
    let observation = ActiveWindowObservation {
        elapsed_us: 1_000,
        barrier_count: 4,
        start_count: 6,
        end_count: 10,
        total_count: 13,
        recorded_active_events: 4,
        active_source_ids: &expected,
        unexpected_source_id: None,
        watchdog_fired: true,
        watchdog_joined: true,
        window_observed: true,
    };
    let checks = check_active_window(observation, 1, 22_000_000, &expected);
    assert!(checks.duration_at_least_requested);
    assert!(checks.duration_within_deadline);
    assert!(checks.counters_monotonic);
    assert!(checks.counter_delta_matches_receipt);
    assert!(checks.positive_active_progress);
    assert!(checks.all_timer_sources_progressed);
    assert!(checks.watchdog_fired);
    assert!(checks.watchdog_joined);

    let short = check_active_window(
        ActiveWindowObservation {
            elapsed_us: 999,
            ..observation
        },
        1,
        22_000_000,
        &expected,
    );
    assert!(!short.duration_at_least_requested);

    let overlong = check_active_window(
        ActiveWindowObservation {
            elapsed_us: 22_000_001,
            watchdog_fired: false,
            watchdog_joined: false,
            ..observation
        },
        1,
        22_000_000,
        &expected,
    );
    assert!(!overlong.duration_within_deadline);
    assert!(!overlong.watchdog_fired);
    assert!(!overlong.watchdog_joined);

    let ready_only = check_active_window(
        ActiveWindowObservation {
            end_count: 6,
            total_count: 8,
            recorded_active_events: 0,
            active_source_ids: &BTreeSet::new(),
            ..observation
        },
        1,
        22_000_000,
        &expected,
    );
    assert!(ready_only.counters_monotonic);
    assert!(ready_only.counter_delta_matches_receipt);
    assert!(!ready_only.positive_active_progress);
    assert!(!ready_only.all_timer_sources_progressed);

    let non_monotonic = check_active_window(
        ActiveWindowObservation {
            barrier_count: 7,
            end_count: 5,
            recorded_active_events: 0,
            active_source_ids: &BTreeSet::new(),
            ..observation
        },
        1,
        22_000_000,
        &expected,
    );
    assert!(!non_monotonic.counters_monotonic);
    assert!(!non_monotonic.counter_delta_matches_receipt);
}

#[test]
fn readiness_watchdog_cancels_a_stalled_terminal_graph_at_its_deadline() {
    let mut plan = graph::GraphPlan::new("stalled_readiness");
    plan.add_node(GraphNode {
        id: "stalled".into(),
        kind: GraphNodeKind::Renderer {
            backend: "test.stalled_terminal".into(),
        },
        inputs: BTreeMap::new(),
        outputs: BTreeMap::from([(
            "result".into(),
            PortSchema::single(flow::contracts::ValueSchema {
                scalar_only: false,
                additional_properties: None,
                min_properties: None,
                max_properties: None,
                nonempty: false,
                kind: flow::contracts::OutputType::Integer,
                nullable: false,
                items: None,
                fields: BTreeMap::new(),
                required: Vec::new(),
                enum_values: None,
                single_line: false,
            }),
        )]),
        events: BTreeMap::new(),
        line: None,
    })
    .unwrap();
    let cancelled = Arc::new(AtomicBool::new(false));
    let mut session = GraphSession::start(
        plan,
        Arc::new(StalledTerminalExecutor),
        GraphRunOptions {
            cancelled: Arc::clone(&cancelled),
            deadline: Some(Instant::now() + Duration::from_secs(2)),
            ..GraphRunOptions::default()
        },
    )
    .unwrap();

    let timeout = Duration::from_millis(50);
    let outcome = wait_for_readiness(
        &mut session,
        Arc::clone(&cancelled),
        timeout,
        &mut OutputReceipts::default(),
        |_| false,
        |_, _| panic!("stalled graph must not emit a terminal output"),
    );
    session.cancel_and_join().unwrap();

    assert!(!outcome.ready);
    assert!(outcome.timed_out);
    assert!(outcome.failure.is_none());
    assert_eq!(outcome.watchdog_state, ReadinessWatchdogState::TimedOut);
    assert!(outcome.watchdog_joined);
    assert!(cancelled.load(Ordering::Acquire));
    assert!(outcome.elapsed >= timeout);
    assert!(outcome.elapsed < Duration::from_secs(1));
}

fn env_usize(name: &str, default: usize, minimum: usize, maximum: usize) -> usize {
    match std::env::var(name) {
        Ok(value) => value
            .parse::<usize>()
            .ok()
            .filter(|value| (*value >= minimum) && (*value <= maximum))
            .unwrap_or_else(|| panic!("{name} must be in {minimum}..={maximum}")),
        Err(_) => default,
    }
}

fn env_u64(name: &str, default: u64, minimum: u64, maximum: u64) -> u64 {
    match std::env::var(name) {
        Ok(value) => value
            .parse::<u64>()
            .ok()
            .filter(|value| (*value >= minimum) && (*value <= maximum))
            .unwrap_or_else(|| panic!("{name} must be in {minimum}..={maximum}")),
        Err(_) => default,
    }
}
