//! Explicit Linux graph lifecycle/resource stress workload.
//!
//! Build this ignored test target separately, then run its prebuilt test binary
//! through benchmarks/stress_evidence.py. It creates thousands of real graph
//! worker threads and queues; it is not part of ordinary cargo test runs.
#![cfg(feature = "graph")]
#![cfg(target_os = "linux")]

use flow::{
    ast::Value,
    contracts::{OutputType, PortDelivery, Sensitivity, ValueSchema},
    graph::{
        BufferPolicy, EdgeSpec, GraphContext, GraphExecutor, GraphNode, GraphNodeKind, GraphPlan,
        GraphRunOptions, GraphSession, NodeIo, PortRef, PortSchema, PortValue,
    },
};
use serde_json::{json, Value as JsonValue};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const POLL: Duration = Duration::from_millis(5);

#[derive(Debug, Clone)]
struct ProcSnapshot {
    rss_bytes: usize,
    threads: usize,
    context_switches: u64,
}

#[derive(Debug, Default)]
struct GenerationStats {
    source_emitted: AtomicU64,
    edge_dropped: AtomicU64,
    received: AtomicU64,
    forwarded: AtomicU64,
    terminal_dropped: AtomicU64,
    timer_sources: Mutex<BTreeSet<String>>,
}

#[derive(Debug, Default)]
struct StressStats {
    generations: Mutex<BTreeMap<u64, Arc<GenerationStats>>>,
}

impl StressStats {
    fn generation(&self, generation: u64) -> Arc<GenerationStats> {
        Arc::clone(
            self.generations
                .lock()
                .expect("stress stats lock")
                .entry(generation)
                .or_default(),
        )
    }

    fn snapshot(&self) -> BTreeMap<String, JsonValue> {
        self.generations
            .lock()
            .expect("stress stats lock")
            .iter()
            .map(|(generation, stats)| {
                let sources = stats.timer_sources.lock().expect("source set lock");
                (
                    generation.to_string(),
                    json!({
                        "source_emitted": stats.source_emitted.load(Ordering::Relaxed),
                        "edge_messages_dropped": stats.edge_dropped.load(Ordering::Relaxed),
                        "received": stats.received.load(Ordering::Relaxed),
                        "forwarded": stats.forwarded.load(Ordering::Relaxed),
                        "terminal_messages_dropped": stats.terminal_dropped.load(Ordering::Relaxed),
                        "unique_timer_sources": sources.len()
                    }),
                )
            })
            .collect()
    }
}

struct StressExecutor {
    stats: Arc<Mutex<StressStats>>,
}

impl GraphExecutor for StressExecutor {
    fn execute(
        &self,
        node: &GraphNode,
        io: &mut NodeIo,
        context: &GraphContext,
    ) -> anyhow::Result<()> {
        let generation_stats = self
            .stats
            .lock()
            .expect("stress stats lock")
            .generation(context.generation);
        match &node.kind {
            GraphNodeKind::Timer { interval_ms } => {
                let interval = Duration::from_millis((*interval_ms).max(1));
                let mut due = Instant::now() + interval;
                let mut tick = 0_i64;
                while !io.is_cancelled() {
                    while !io.is_cancelled() && Instant::now() < due {
                        thread::sleep(due.saturating_duration_since(Instant::now()).min(POLL));
                    }
                    if io.is_cancelled() {
                        break;
                    }
                    tick = tick.saturating_add(1);
                    let emitted = io.emit("tick", PortValue::Value(Value::Integer(tick)))?;
                    generation_stats
                        .source_emitted
                        .fetch_add(1, Ordering::Relaxed);
                    generation_stats
                        .edge_dropped
                        .fetch_add(emitted.dropped as u64, Ordering::Relaxed);
                    let now = Instant::now();
                    while due <= now {
                        due += interval;
                    }
                }
                Ok(())
            }
            GraphNodeKind::Renderer { backend } if backend == "bench.collect" => {
                while let Some(event) = io.recv("events")? {
                    let source = event
                        .input
                        .as_ref()
                        .map(|port| port.node.clone())
                        .ok_or_else(|| {
                            anyhow::anyhow!("merged event did not identify its source")
                        })?;
                    {
                        generation_stats.received.fetch_add(1, Ordering::Relaxed);
                        generation_stats
                            .timer_sources
                            .lock()
                            .map_err(|_| anyhow::anyhow!("source set lock was poisoned"))?
                            .insert(source);
                    }
                    let emitted = io.emit("accepted", event.payload)?;
                    generation_stats.forwarded.fetch_add(1, Ordering::Relaxed);
                    generation_stats
                        .terminal_dropped
                        .fetch_add(emitted.dropped as u64, Ordering::Relaxed);
                }
                Ok(())
            }
            GraphNodeKind::Renderer { backend } if backend == "bench.fail" => {
                anyhow::bail!("injected graph worker failure")
            }
            GraphNodeKind::Renderer { backend } => {
                anyhow::bail!("unsupported stress backend {backend:?}")
            }
            _ => anyhow::bail!("unexpected graph node kind for {}", node.id),
        }
    }
}

fn integer_schema() -> ValueSchema {
    ValueSchema {
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
    }
}

fn event_port() -> PortSchema {
    PortSchema {
        schema: integer_schema(),
        delivery: PortDelivery::Events,
        sensitivity: Sensitivity::Unknown,
        required: false,
        pointer: None,
        encoding: None,
    }
}

fn graph_plan(timer_count: usize, interval_ms: u64, generation: u64) -> GraphPlan {
    let mut plan = GraphPlan::new(format!("lifecycle-{generation}"));
    for index in 0..timer_count {
        let id = format!("timer-{index:05}");
        let mut events = BTreeMap::new();
        events.insert("tick".into(), event_port());
        plan.add_node(GraphNode {
            id: id.clone(),
            kind: GraphNodeKind::Timer { interval_ms },
            inputs: BTreeMap::new(),
            outputs: BTreeMap::new(),
            events,
            line: None,
        })
        .expect("unique timer ID");
        let mut edge = EdgeSpec::lossless(
            PortRef::new(id, "tick"),
            PortRef::new("collect", "events"),
            8,
        );
        edge.policy = BufferPolicy::DropOldest;
        plan.add_edge(edge);
    }
    let mut inputs = BTreeMap::new();
    inputs.insert("events".into(), event_port());
    let mut events = BTreeMap::new();
    events.insert("accepted".into(), event_port());
    plan.add_node(GraphNode {
        id: "collect".into(),
        kind: GraphNodeKind::Renderer {
            backend: "bench.collect".into(),
        },
        inputs,
        outputs: BTreeMap::new(),
        events,
        line: None,
    })
    .expect("unique collector ID");
    plan.validate().expect("generated stress graph validates");
    plan
}

fn fault_plan() -> GraphPlan {
    let mut plan = GraphPlan::new("injected-failure");
    plan.add_node(GraphNode {
        id: "fail".into(),
        kind: GraphNodeKind::Renderer {
            backend: "bench.fail".into(),
        },
        inputs: BTreeMap::new(),
        outputs: BTreeMap::new(),
        events: BTreeMap::new(),
        line: None,
    })
    .expect("unique fault node");
    plan.validate().expect("fault graph validates");
    plan
}

fn env_usize(name: &str, default: usize, minimum: usize, maximum: usize) -> usize {
    let value = std::env::var(name)
        .map(|raw| {
            raw.parse::<usize>()
                .unwrap_or_else(|_| panic!("{name} must be an integer"))
        })
        .unwrap_or(default);
    assert!(
        (minimum..=maximum).contains(&value),
        "{name} must be in {minimum}..={maximum}"
    );
    value
}

#[cfg(target_os = "linux")]
fn proc_snapshot() -> ProcSnapshot {
    let status = fs::read_to_string("/proc/self/status").expect("read /proc/self/status");
    let mut rss_kib = None;
    let mut threads = None;
    let mut context_switches = 0_u64;
    for line in status.lines() {
        if let Some(value) = line.strip_prefix("VmRSS:") {
            rss_kib = value
                .split_whitespace()
                .next()
                .and_then(|value| value.parse::<usize>().ok());
        } else if let Some(value) = line.strip_prefix("Threads:") {
            threads = value.trim().parse::<usize>().ok();
        } else if let Some(value) = line.strip_prefix("voluntary_ctxt_switches:") {
            context_switches += value.trim().parse::<u64>().expect("context switch count");
        } else if let Some(value) = line.strip_prefix("nonvoluntary_ctxt_switches:") {
            context_switches += value.trim().parse::<u64>().expect("context switch count");
        }
    }
    ProcSnapshot {
        rss_bytes: rss_kib.expect("VmRSS reported") * 1024,
        threads: threads.expect("Threads reported"),
        context_switches,
    }
}

#[cfg(not(target_os = "linux"))]
fn proc_snapshot() -> ProcSnapshot {
    panic!(
        "this workload measures Linux /proc RSS, threads, and context switches; use a Linux runner"
    )
}

#[cfg(target_os = "linux")]
fn cpu_time_us() -> u64 {
    // SAFETY: `libc::rusage` is a plain C struct of integers and `timeval`s, for which the
    // all-zero bit pattern is valid. It is overwritten by `getrusage` on the next line.
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: `RUSAGE_SELF` is a valid `who` value and `&mut usage` points to a live,
    // initialised `rusage` that the kernel fills in. The pointer is not retained.
    let result = unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) };
    assert_eq!(result, 0, "getrusage");
    let time = |value: libc::timeval| value.tv_sec as u64 * 1_000_000 + value.tv_usec as u64;
    time(usage.ru_utime) + time(usage.ru_stime)
}

#[cfg(not(target_os = "linux"))]
fn cpu_time_us() -> u64 {
    0
}

fn options(
    node_count: usize,
    generation: u64,
    revision: u64,
    terminal_capacity: usize,
) -> GraphRunOptions {
    GraphRunOptions {
        max_parallel: node_count + 1,
        max_nodes: node_count + 1,
        max_edges: node_count,
        max_queue_slots: node_count
            .saturating_mul(8)
            .saturating_add(terminal_capacity),
        max_message_bytes: 8 * 1024 * 1024,
        terminal_capacity,
        generation,
        revision,
        cancelled: Arc::new(AtomicBool::new(false)),
        ..GraphRunOptions::default()
    }
}

#[test]
#[ignore = "explicit Linux stress measurement; run only through benchmarks/stress_evidence.py"]
fn graph_lifecycle_stress() {
    let timer_count = env_usize("GRAPH_STRESS_NODES", 2048, 1000, 4096);
    let reconfigurations = env_usize("GRAPH_STRESS_RECONFIGURATIONS", 4, 1, 100);
    let outputs_per_generation = env_usize("GRAPH_STRESS_OUTPUTS", 64, 1, 4096);
    let idle_window_ms = env_usize("GRAPH_STRESS_IDLE_MS", 250, 10, 10_000);
    let active_interval_ms = env_usize("GRAPH_STRESS_INTERVAL_MS", 5, 1, 60_000) as u64;
    let idle_interval_ms = env_usize(
        "GRAPH_STRESS_IDLE_INTERVAL_MS",
        (idle_window_ms as u64).saturating_mul(4).max(10_000) as usize,
        idle_window_ms + 1,
        300_000,
    ) as u64;
    let report_path = PathBuf::from(
        std::env::var_os("GRAPH_STRESS_RAW_REPORT").expect("GRAPH_STRESS_RAW_REPORT"),
    );
    let source = include_bytes!("graph_lifecycle_bench.rs");
    use sha2::{Digest, Sha256};
    let source_sha256 = format!("{:x}", Sha256::digest(source));
    let test_started_utc_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("wall clock")
        .as_millis();

    let stats = Arc::new(Mutex::new(StressStats::default()));
    let executor: Arc<dyn GraphExecutor> = Arc::new(StressExecutor {
        stats: Arc::clone(&stats),
    });
    let baseline = proc_snapshot();
    let peak_rss = Arc::new(AtomicUsize::new(baseline.rss_bytes));
    let stop_monitor = Arc::new(AtomicBool::new(false));
    let monitor_peak = Arc::clone(&peak_rss);
    let monitor_stop = Arc::clone(&stop_monitor);
    let monitor = thread::spawn(move || {
        while !monitor_stop.load(Ordering::Acquire) {
            if let Ok(sample) = std::panic::catch_unwind(proc_snapshot) {
                monitor_peak.fetch_max(sample.rss_bytes, Ordering::Relaxed);
            }
            thread::sleep(Duration::from_millis(5));
        }
    });

    let idle_plan_started = Instant::now();
    let idle_plan = graph_plan(timer_count, idle_interval_ms, 0);
    let idle_plan_build_us = idle_plan_started.elapsed().as_micros() as u64;
    let idle_start = Instant::now();
    let mut idle_session = GraphSession::start(
        idle_plan,
        Arc::clone(&executor),
        options(timer_count, 1, 1, outputs_per_generation),
    )
    .expect("start idle graph");
    let idle_started_us = idle_start.elapsed().as_micros() as u64;
    let idle_cpu_before = cpu_time_us();
    let idle_proc_before = proc_snapshot();
    thread::sleep(Duration::from_millis(idle_window_ms as u64));
    let idle_proc_after = proc_snapshot();
    let idle_cpu_after = cpu_time_us();
    let idle_stats = stats.lock().expect("stress stats lock").generation(1);
    assert_eq!(
        idle_stats.received.load(Ordering::Relaxed),
        0,
        "long-interval idle timers must not emit"
    );
    let idle_source_events = idle_stats.received.load(Ordering::Relaxed);
    let idle_cancel_started = Instant::now();
    idle_session
        .cancel_and_join()
        .expect("cancel and join idle graph");
    let idle_cancelled_us = idle_cancel_started.elapsed().as_micros() as u64;

    let active_first_plan_started = Instant::now();
    let active_plan = graph_plan(timer_count, active_interval_ms, 1);
    let active_first_plan_build_us = active_first_plan_started.elapsed().as_micros() as u64;
    let active_start = Instant::now();
    let mut session = GraphSession::start(
        active_plan,
        Arc::clone(&executor),
        options(timer_count, 1, 1, outputs_per_generation),
    )
    .expect("start active graph");
    let active_first_started_us = active_start.elapsed().as_micros() as u64;

    let mut generation_output_windows_us = Vec::new();
    let mut reconfiguration_plan_build_us = Vec::new();
    let mut reconfiguration_us = Vec::new();
    let mut current_generation = 1_u64;
    let mut total_outputs = 0_usize;
    let mut stale_outputs_observed = false;
    for generation_index in 0..=reconfigurations {
        let expected_generation = session.context().generation;
        let expected_revision = session.context().revision;
        assert_eq!(expected_generation, current_generation);
        let sample_start = Instant::now();
        for _ in 0..outputs_per_generation {
            let event = session
                .recv_output()
                .expect("read graph output")
                .expect("active timers should keep graph output available");
            if event.generation != expected_generation || event.revision != expected_revision {
                stale_outputs_observed = true;
            }
            total_outputs += 1;
        }
        generation_output_windows_us.push(sample_start.elapsed().as_micros() as u64);
        if generation_index == reconfigurations {
            break;
        }
        current_generation += 1;
        let plan_started = Instant::now();
        let next_plan = graph_plan(timer_count, active_interval_ms, current_generation);
        let plan_build_us = plan_started.elapsed().as_micros() as u64;
        let reconfigure_started = Instant::now();
        session = session
            .reconfigure(
                next_plan,
                Arc::clone(&executor),
                options(timer_count, 1, 1, outputs_per_generation),
            )
            .expect("cancel old graph and start the next generation");
        reconfiguration_plan_build_us.push(plan_build_us);
        reconfiguration_us.push(reconfigure_started.elapsed().as_micros() as u64);
    }
    let active_cancel_started = Instant::now();
    session
        .cancel_and_join()
        .expect("cancel and join active graph");
    let active_cancel_us = active_cancel_started.elapsed().as_micros() as u64;
    assert!(
        !stale_outputs_observed,
        "stale event crossed graph generation/revision boundary"
    );

    let failure_started = Instant::now();
    let mut failure = GraphSession::start(
        fault_plan(),
        Arc::clone(&executor),
        GraphRunOptions::default(),
    )
    .expect("start fault-injection graph")
    .run_to_completion();
    let failure_duration_us = failure_started.elapsed().as_micros() as u64;
    assert!(
        failure
            .as_mut()
            .err()
            .is_some_and(|error| format!("{error:#}").contains("injected graph worker failure")),
        "the injected worker fault must be surfaced"
    );
    drop(failure);

    stop_monitor.store(true, Ordering::Release);
    monitor.join().expect("RSS monitor joins");
    let after = proc_snapshot();
    let final_stats = stats.lock().expect("stress stats lock").snapshot();
    let generations = (1..=reconfigurations as u64 + 1).collect::<Vec<_>>();
    for generation in &generations {
        let observed = final_stats
            .get(&generation.to_string())
            .cloned()
            .unwrap_or(JsonValue::Null);
        let received = observed
            .get("received")
            .and_then(JsonValue::as_u64)
            .unwrap_or(0);
        let unique_sources = observed
            .get("unique_timer_sources")
            .and_then(JsonValue::as_u64)
            .unwrap_or(0);
        assert!(
            received >= outputs_per_generation as u64,
            "generation {generation} did not consume enough source events"
        );
        assert!(
            unique_sources > 0,
            "generation {generation} did not receive timer events"
        );
    }
    assert!(
        after.threads <= baseline.threads + 1,
        "graph worker threads leaked: baseline {}, after {}",
        baseline.threads,
        after.threads
    );
    let report = json!({
        "schema": "execute.graph-lifecycle.raw/1",
        "trial_type": "measurement",
        "claim": "none",
        "platform_gate": "linux_only",
        "test_started_utc_ms": test_started_utc_ms,
        "source_sha256": source_sha256,
        "parameters": {
            "timers_per_generation": timer_count,
            "graph_edges_per_generation": timer_count,
            "terminal_queues_per_generation": 1,
            "timer_interval_ms": active_interval_ms,
            "idle_timer_interval_ms": idle_interval_ms,
            "idle_window_ms": idle_window_ms,
            "reconfigurations": reconfigurations,
            "generations": generations,
            "terminal_capacity": outputs_per_generation,
            "edge_capacity": 8,
            "edge_policy": "drop_oldest",
            "outputs_consumed_per_generation": outputs_per_generation
        },
        "resources": {
            "baseline_rss_bytes": baseline.rss_bytes,
            "sampled_peak_rss_bytes": peak_rss.load(Ordering::Relaxed),
            "sampled_peak_rss_delta_bytes": peak_rss.load(Ordering::Relaxed).saturating_sub(baseline.rss_bytes),
            "baseline_threads": baseline.threads,
            "threads_after_all_joins": after.threads,
            "idle_context_switches": idle_proc_after.context_switches.saturating_sub(idle_proc_before.context_switches),
            "idle_context_switches_per_second": (idle_proc_after.context_switches.saturating_sub(idle_proc_before.context_switches) as f64 * 1000.0) / idle_window_ms as f64,
            "idle_cpu_us": idle_cpu_after.saturating_sub(idle_cpu_before),
            "idle_sample_rss_start_bytes": idle_proc_before.rss_bytes,
            "idle_sample_rss_end_bytes": idle_proc_after.rss_bytes,
            "active_context_switches_total": after.context_switches.saturating_sub(idle_proc_after.context_switches),
            "active_cpu_us_total": cpu_time_us().saturating_sub(idle_cpu_after)
        },
        "timings_us": {
            "idle_plan_build": idle_plan_build_us,
            "idle_graph_start": idle_started_us,
            "idle_graph_cancel_and_join": idle_cancelled_us,
            "active_initial_plan_build": active_first_plan_build_us,
            "active_initial_graph_start": active_first_started_us,
            "generation_output_windows": generation_output_windows_us,
            "reconfiguration_plan_build": reconfiguration_plan_build_us,
            "reconfiguration_inclusive": reconfiguration_us,
            "active_final_cancel_and_join": active_cancel_us,
            "injected_failure_cancel_and_join": failure_duration_us
        },
        "correctness": {
            "idle_source_events": idle_source_events,
            "total_terminal_outputs_consumed": total_outputs,
            "events_by_generation": final_stats,
            "stale_outputs_observed": stale_outputs_observed,
            "all_expected_generations_observed": true,
            "worker_failure_injection": "surfaced_and_joined",
            "threads_returned_to_baseline": after.threads <= baseline.threads + 1
        },
        "limitations": [
            "RSS is the local Rust process only; there are no remote service child processes in this graph workload.",
            "Sampled RSS may miss short peaks and is not a process-tree measurement.",
            "Timer tests exercise graph-owned threads and local queues, not thousands of real IPC services or systemd timers.",
            "The idle sample includes the graph implementation's polling/wakeup behavior; it does not measure power draw.",
            "Network loss, OS process crashes, disk exhaustion, unusual permission failures, and Arch/Omarchy session faults are not injected by this test."
        ]
    });
    fs::write(
        &report_path,
        serde_json::to_vec_pretty(&report).expect("encode report"),
    )
    .expect("write raw report");
}
