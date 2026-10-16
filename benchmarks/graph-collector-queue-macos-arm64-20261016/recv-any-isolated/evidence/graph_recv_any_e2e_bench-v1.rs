//! Small end-to-end single-queue recv_any comparison. Run only from a paired harness.
#![cfg(unix)]

use anyhow::{Context, Result, anyhow, bail, ensure};
use flow::ast::Value;
use flow::contracts::{OutputType, Sensitivity, ValueSchema};
use flow::graph::{
    self, BufferPolicy, EdgeSpec, EventEnvelope, GraphContext, GraphExecutor, GraphNode,
    GraphNodeKind, GraphPlan, GraphRunOptions, GraphSession, NodeIo, PortRef, PortSchema,
    PortValue, RemoteEventMetadata, RuntimeGraphExecutor,
};
use flow::manifest::Registry;
use flow::runtime::RunOptions;
use flow::security::SensitivitySet;
use serde_json::{Value as JsonValue, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const RECORD_COUNT: usize = 4096;
const TEXT_BYTES: usize = 256;
const EDGE_CAPACITY: usize = 8;
const TERMINAL_CAPACITY: usize = 16;
const GENERATION: u64 = 17;
const REVISION: u64 = 29;
const DEFAULT_WARMUPS: usize = 0;
const DEFAULT_RAW_SAMPLES: usize = 3;
const MAX_RUNS: usize = 20;

#[derive(Debug, serde::Serialize)]
struct ReceivedIdentity {
    sequence: u64,
    generation: u64,
    revision: u64,
    source_port_matches: bool,
    input_port_matches: bool,
    sensitivity_unknown_only: bool,
    remote_absent: bool,
}

#[derive(Debug, serde::Serialize)]
struct EventIdentity {
    port: PortRef,
    input: Option<PortRef>,
    generation: u64,
    revision: u64,
    sequence: u64,
    sensitivity: Vec<Sensitivity>,
    remote: Option<RemoteEventMetadata>,
}

impl From<&EventEnvelope> for EventIdentity {
    fn from(event: &EventEnvelope) -> Self {
        Self {
            port: event.port.clone(),
            input: event.input.clone(),
            generation: event.generation,
            revision: event.revision,
            sequence: event.sequence,
            sensitivity: event.sensitivity.iter().collect(),
            remote: event.remote.clone(),
        }
    }
}

fn only_unknown_sensitivity(labels: &[Sensitivity]) -> bool {
    labels.len() == 1 && labels[0] == Sensitivity::Unknown
}

struct PassThroughExecutor {
    runtime: RuntimeGraphExecutor,
    received: [Arc<Mutex<Vec<ReceivedIdentity>>>; 2],
}

impl GraphExecutor for PassThroughExecutor {
    fn execute(&self, node: &GraphNode, io: &mut NodeIo, context: &GraphContext) -> Result<()> {
        let consumer = match &node.kind {
            GraphNodeKind::Renderer { backend } if backend == "bench.pass.0" => 0,
            GraphNodeKind::Renderer { backend } if backend == "bench.pass.1" => 1,
            _ => return self.runtime.execute(node, io, context),
        };
        let mut received = self.received[consumer]
            .lock()
            .map_err(|_| anyhow!("benchmark receiver identity lock was poisoned"))?;
        while let Some(event) = io.recv_any()? {
            let mut labels = event.sensitivity.iter();
            let sensitivity_unknown_only =
                matches!(labels.next(), Some(Sensitivity::Unknown)) && labels.next().is_none();
            received.push(ReceivedIdentity {
                sequence: event.sequence,
                generation: event.generation,
                revision: event.revision,
                source_port_matches: event.port.node == "producer" && event.port.port == "stdout",
                input_port_matches: event
                    .input
                    .as_ref()
                    .is_some_and(|port| port.node == node.id && port.port == "events"),
                sensitivity_unknown_only,
                remote_absent: event.remote.is_none(),
            });
            io.emit("stdout", event.payload)?;
        }
        Ok(())
    }
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn leaf_schema(kind: OutputType) -> ValueSchema {
    ValueSchema {
        kind,
        nullable: false,
        items: None,
        fields: BTreeMap::new(),
        required: Vec::new(),
        enum_values: None,
    }
}

fn record_schema() -> ValueSchema {
    ValueSchema {
        kind: OutputType::Record,
        nullable: false,
        items: None,
        fields: BTreeMap::from([
            ("id".into(), leaf_schema(OutputType::Integer)),
            ("text".into(), leaf_schema(OutputType::Text)),
        ]),
        required: vec!["id".into(), "text".into()],
        enum_values: None,
    }
}

fn record_json_line(value: &Value) -> Result<Vec<u8>> {
    let Value::Record(fields) = value else {
        bail!("benchmark record is not a typed record")
    };
    let Some(Value::Integer(id)) = fields.get("id") else {
        bail!("benchmark record has no integer id")
    };
    let Some(Value::String(text)) = fields.get("text") else {
        bail!("benchmark record has no text field")
    };
    Ok(serde_json::to_vec(&json!({"id": id, "text": text}))?)
}

fn env_count(name: &str, default: usize, allow_zero: bool) -> Result<usize> {
    let Some(value) = env::var_os(name) else {
        return Ok(default);
    };
    let value = value
        .into_string()
        .map_err(|_| anyhow!("{name} must be valid UTF-8"))?;
    let count = value
        .parse::<usize>()
        .with_context(|| format!("{name} must be a nonnegative integer"))?;
    ensure!(
        (allow_zero || count > 0) && count <= MAX_RUNS,
        "{name} must be {} through {MAX_RUNS}",
        if allow_zero { "0" } else { "1" }
    );
    Ok(count)
}

fn report_path() -> Result<PathBuf> {
    let value = env::var_os("EXECUTE_GRAPH_RECV_ANY_E2E_REPORT").ok_or_else(|| {
        anyhow!("set EXECUTE_GRAPH_RECV_ANY_E2E_REPORT to a fresh absolute JSON path")
    })?;
    let path = PathBuf::from(value);
    ensure!(path.is_absolute(), "benchmark report path must be absolute");
    Ok(path)
}

fn fixture_path(report: &Path) -> Result<PathBuf> {
    let parent = report
        .parent()
        .ok_or_else(|| anyhow!("benchmark report path has no parent directory"))?;
    let stem = report
        .file_stem()
        .and_then(|stem| stem.to_str())
        .ok_or_else(|| anyhow!("benchmark report path has no UTF-8 file stem"))?;
    Ok(parent.join(format!("{stem}.input.jsonl")))
}

fn journal_path(report: &Path) -> Result<PathBuf> {
    let parent = report
        .parent()
        .ok_or_else(|| anyhow!("benchmark report path has no parent directory"))?;
    let stem = report
        .file_stem()
        .and_then(|stem| stem.to_str())
        .ok_or_else(|| anyhow!("benchmark report path has no UTF-8 file stem"))?;
    Ok(parent.join(format!("{stem}.journal.jsonl")))
}

fn create_fresh_file(path: &Path, label: &str) -> Result<File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("claiming fresh benchmark {label} {}", path.display()))
}

fn write_journal(file: &mut File, entry: &JsonValue) -> Result<()> {
    serde_json::to_writer(&mut *file, entry)?;
    file.write_all(b"\n")?;
    file.flush()?;
    Ok(())
}

fn source_manifest() -> Result<String> {
    Ok(serde_json::to_string(&json!({
        "name": "bench.jsonl_source",
        "binary": "/bin/cat",
        "accepts_stdin": false,
        "parameters": {"path": {"type": "path", "required": true}},
        "arguments": [{"parameter": "path"}],
        "contract": {
            "version": 1,
            "output": {
                "type": "record",
                "encoding": "json",
                "fields": {
                    "id": {"type": "integer"},
                    "text": {"type": "text"}
                },
                "required": ["id", "text"]
            }
        }
    }))?)
}

fn add_receiver(plan: &mut GraphPlan, id: &str, backend: &str) -> Result<()> {
    plan.add_node(GraphNode {
        id: id.into(),
        kind: GraphNodeKind::Renderer {
            backend: backend.into(),
        },
        inputs: BTreeMap::from([("events".into(), PortSchema::records(record_schema()))]),
        outputs: BTreeMap::from([("stdout".into(), PortSchema::records(record_schema()))]),
        events: BTreeMap::new(),
        line: None,
    })?;
    plan.add_edge(EdgeSpec {
        from: PortRef::new("producer", "stdout"),
        to: PortRef::new(id, "events"),
        capacity: EDGE_CAPACITY,
        policy: BufferPolicy::Lossless,
        line: None,
    });
    Ok(())
}

fn graph_options(expected_payload_bytes: usize) -> Result<GraphRunOptions> {
    let collected_events = RECORD_COUNT
        .checked_mul(2)
        .and_then(|events| events.checked_add(1))
        .ok_or_else(|| anyhow!("benchmark output event limit overflowed"))?;
    let status_payload_bytes = serde_json::to_vec(&Value::Integer(0))?.len();
    let collected_bytes = expected_payload_bytes
        .checked_mul(2)
        .and_then(|bytes| bytes.checked_add(status_payload_bytes))
        .ok_or_else(|| anyhow!("benchmark output byte limit overflowed"))?;
    Ok(GraphRunOptions {
        max_parallel: 3,
        max_nodes: 3,
        max_edges: 2,
        max_queue_slots: 2 * EDGE_CAPACITY + 3 * TERMINAL_CAPACITY,
        max_message_bytes: 4096,
        terminal_capacity: TERMINAL_CAPACITY,
        max_collected_events: collected_events,
        max_collected_bytes: collected_bytes,
        generation: GENERATION,
        revision: REVISION,
        ..GraphRunOptions::default()
    })
}

fn runtime_options(cwd: &Path) -> RunOptions {
    RunOptions {
        inputs: BTreeMap::new(),
        working_directory: cwd.to_path_buf(),
        cache_directory: None,
        trace: false,
        max_output_bytes: 1024,
        command_timeout: Some(Duration::from_secs(30)),
        total_timeout: None,
    }
}

fn verify_outputs(
    result: &graph::GraphRunResult,
    received: &[Arc<Mutex<Vec<ReceivedIdentity>>>; 2],
    expected: &[Value],
    input_sha256: &str,
) -> Result<JsonValue> {
    ensure!(
        result.dropped_messages == 0,
        "graph dropped benchmark events"
    );
    ensure!(
        result.launched_processes == 1,
        "producer did not launch exactly once"
    );
    ensure!(result.generation == GENERATION, "graph generation changed");
    ensure!(result.revision == REVISION, "graph revision changed");
    ensure!(
        result.outputs.len() == RECORD_COUNT * 2 + 1,
        "graph returned a partial or extra terminal output set"
    );

    let consumer_ids = ["consumer-0", "consumer-1"];
    let mut outputs: [Vec<_>; 2] = [
        Vec::with_capacity(RECORD_COUNT),
        Vec::with_capacity(RECORD_COUNT),
    ];
    let mut source_status = Vec::with_capacity(1);
    for event in &result.outputs {
        match (event.port.node.as_str(), event.port.port.as_str()) {
            ("consumer-0", "stdout") => outputs[0].push(event),
            ("consumer-1", "stdout") => outputs[1].push(event),
            ("producer", "status") => source_status.push(event),
            other => bail!("unexpected terminal event port {other:?}"),
        }
    }
    ensure!(
        source_status.len() == 1,
        "producer status output is absent or duplicated"
    );
    ensure!(
        source_status[0].payload == PortValue::Value(Value::Integer(0)),
        "producer did not report successful completion"
    );
    let source_status_identity = EventIdentity::from(source_status[0]);
    ensure!(
        source_status_identity.port == PortRef::new("producer", "status"),
        "producer status output identity changed"
    );
    ensure!(
        source_status_identity.input.is_none(),
        "producer status names an input port"
    );
    ensure!(
        source_status_identity.generation == result.generation,
        "producer status generation changed"
    );
    ensure!(
        source_status_identity.revision == result.revision,
        "producer status revision changed"
    );
    ensure!(
        only_unknown_sensitivity(&source_status_identity.sensitivity),
        "producer status sensitivity changed"
    );
    ensure!(
        source_status_identity.remote.is_none(),
        "producer status has remote metadata"
    );

    let mut checksums = Vec::with_capacity(2);
    let mut input_sequences_by_consumer =
        std::array::from_fn::<_, 2, _>(|_| Vec::with_capacity(RECORD_COUNT));
    let mut output_sequences = BTreeSet::new();
    for consumer in 0..2 {
        let received = received[consumer]
            .lock()
            .map_err(|_| anyhow!("benchmark receiver identity lock was poisoned"))?;
        ensure!(
            received.len() == RECORD_COUNT,
            "{} consumed a partial event stream",
            consumer_ids[consumer]
        );
        let mut input_sequence = Sha256::new();
        let mut previous_input_sequence = 0;
        for identity in received.iter() {
            ensure!(
                identity.sequence > previous_input_sequence,
                "consumer input order changed"
            );
            previous_input_sequence = identity.sequence;
            ensure!(
                identity.generation == result.generation,
                "input generation changed"
            );
            ensure!(
                identity.revision == result.revision,
                "input revision changed"
            );
            ensure!(
                identity.source_port_matches,
                "consumer received an unexpected source port"
            );
            ensure!(
                identity.input_port_matches,
                "consumer received an unexpected input port"
            );
            ensure!(
                identity.sensitivity_unknown_only,
                "input sensitivity changed"
            );
            ensure!(
                identity.remote_absent,
                "input unexpectedly has remote metadata"
            );
            input_sequence.update(identity.sequence.to_le_bytes());
            input_sequences_by_consumer[consumer].push(identity.sequence);
        }

        ensure!(
            outputs[consumer].len() == RECORD_COUNT,
            "{} returned a partial event stream",
            consumer_ids[consumer]
        );
        let mut output_bytes = Vec::with_capacity(RECORD_COUNT * (TEXT_BYTES + 32));
        let mut output_identities = Vec::with_capacity(RECORD_COUNT);
        let mut previous_output_sequence = 0;
        for (index, event) in outputs[consumer].iter().enumerate() {
            ensure!(
                event.sequence > previous_output_sequence,
                "consumer output order changed"
            );
            previous_output_sequence = event.sequence;
            ensure!(
                event.sequence > input_sequences_by_consumer[consumer][index],
                "consumer output sequence precedes its input event"
            );
            ensure!(
                output_sequences.insert(event.sequence),
                "consumer output sequence was duplicated"
            );
            ensure!(
                event.port.node == consumer_ids[consumer] && event.port.port == "stdout",
                "consumer output identity changed"
            );
            ensure!(
                event.input.is_none(),
                "terminal output unexpectedly names an input port"
            );
            ensure!(
                event.generation == result.generation,
                "output generation changed"
            );
            ensure!(event.revision == result.revision, "output revision changed");
            ensure!(
                event.sensitivity == SensitivitySet::from_label(Sensitivity::Unknown),
                "output sensitivity changed"
            );
            ensure!(
                event.remote.is_none(),
                "output unexpectedly has remote metadata"
            );
            ensure!(
                event.payload == PortValue::Value(expected[index].clone()),
                "consumer changed or reordered full event payload at index {index}"
            );
            output_identities.push(EventIdentity::from(*event));
            output_bytes.extend_from_slice(&record_json_line(match &event.payload {
                PortValue::Value(value) => value,
                PortValue::Bytes(_) => bail!("consumer emitted bytes instead of a typed record"),
            })?);
            output_bytes.push(b'\n');
        }
        let output_sha256 = sha256(&output_bytes);
        ensure!(
            output_sha256 == input_sha256,
            "consumer output checksum differs from fixture"
        );
        checksums.push(json!({
            "consumer": consumer_ids[consumer],
            "input_event_count": received.len(),
            "input_sequence_sha256": format!("{:x}", input_sequence.finalize()),
            "input_source_port": {"node": "producer", "port": "stdout"},
            "input_target_port": {"node": consumer_ids[consumer], "port": "events"},
            "input_sensitivity": ["unknown"],
            "input_remote_metadata": null,
            "input_event_identities": &*received,
            "output_event_count": outputs[consumer].len(),
            "output_payload_sha256": output_sha256,
            "output_event_identities": output_identities,
        }));
    }
    ensure!(
        input_sequences_by_consumer[0] == input_sequences_by_consumer[1],
        "fanout consumers received different source event identities"
    );
    let source_sequences = &input_sequences_by_consumer[0];
    ensure!(
        source_status_identity.sequence > *source_sequences.last().expect("nonempty source stream"),
        "producer status sequence does not follow the source records"
    );
    ensure!(
        output_sequences.insert(source_status_identity.sequence),
        "producer status sequence duplicates a consumer output"
    );
    let mut all_emission_sequences = BTreeSet::new();
    for sequence in source_sequences
        .iter()
        .copied()
        .chain(output_sequences.iter().copied())
    {
        ensure!(
            all_emission_sequences.insert(sequence),
            "graph emission sequence was reused across distinct events"
        );
    }
    Ok(json!({
        "terminal_event_count": result.outputs.len(),
        "source_status_event_count": source_status.len(),
        "source_status_identity": source_status_identity,
        "consumers": checksums,
    }))
}

// Keep this ignored benchmark helper's per-sample inputs explicit.
#[allow(clippy::too_many_arguments)]
fn run_one(
    plan: &GraphPlan,
    registry: &Registry,
    cwd: &Path,
    expected: &[Value],
    expected_payload_bytes: usize,
    input_sha256: &str,
    received: [Arc<Mutex<Vec<ReceivedIdentity>>>; 2],
    journal: &mut File,
    phase: &str,
    index: usize,
) -> Result<(u64, JsonValue)> {
    let runtime = RuntimeGraphExecutor::new(registry.clone(), runtime_options(cwd));
    let executor = Arc::new(PassThroughExecutor {
        runtime,
        received: received.clone(),
    });
    let options = graph_options(expected_payload_bytes)?;
    let plan = plan.clone();
    let started = Instant::now();
    let run_result = match GraphSession::start(plan, executor, options) {
        Ok(session) => session.run_to_completion(),
        Err(error) => Err(error),
    };
    let duration_ns = u64::try_from(started.elapsed().as_nanos())
        .context("graph execution duration exceeds u64 nanoseconds")?;
    let result = match run_result {
        Ok(result) => result,
        Err(error) => {
            write_journal(
                journal,
                &json!({
                    "phase": phase,
                    "index": index,
                    "status": "runtime_failed",
                    "duration_ns": duration_ns,
                    "error": format!("{error:#}"),
                }),
            )?;
            return Err(error).context("timed graph execution failed");
        }
    };
    let verification = match verify_outputs(&result, &received, expected, input_sha256) {
        Ok(verification) => verification,
        Err(error) => {
            write_journal(
                journal,
                &json!({
                    "phase": phase,
                    "index": index,
                    "status": "validation_failed",
                    "duration_ns": duration_ns,
                    "error": format!("{error:#}"),
                }),
            )?;
            return Err(error).context("post-timer graph validation failed");
        }
    };
    let verification_sha256 = sha256(&serde_json::to_vec(&verification)?);
    write_journal(
        journal,
        &json!({
            "phase": phase,
            "index": index,
            "status": "ok",
            "duration_ns": duration_ns,
            "dropped_messages": result.dropped_messages,
            "launched_processes": result.launched_processes,
            "input_fixture_sha256": input_sha256,
            "verification_sha256": verification_sha256,
        }),
    )?;
    Ok((
        duration_ns,
        json!({
            "duration_ns": duration_ns,
            "dropped_messages": result.dropped_messages,
            "launched_processes": result.launched_processes,
            "verification": verification,
        }),
    ))
}

fn fresh_capture() -> [Arc<Mutex<Vec<ReceivedIdentity>>>; 2] {
    std::array::from_fn(|_| Arc::new(Mutex::new(Vec::with_capacity(RECORD_COUNT))))
}

fn write_report(file: &mut BufWriter<File>, report: &JsonValue) -> Result<()> {
    serde_json::to_writer_pretty(&mut *file, report)?;
    file.write_all(b"\n")?;
    file.flush()?;
    Ok(())
}

#[test]
#[ignore = "manual finite RuntimeGraphExecutor transport comparison; run only from a paired harness"]
fn bounded_native_stream_recv_any_has_exact_end_to_end_samples() -> Result<()> {
    let report_output = report_path()?;
    let fixture = fixture_path(&report_output)?;
    let journal_output = journal_path(&report_output)?;
    let mut report_file = BufWriter::new(create_fresh_file(&report_output, "report")?);
    let mut fixture_file = create_fresh_file(&fixture, "input fixture")?;
    let mut journal_file = create_fresh_file(&journal_output, "sample journal")?;
    let warmups = env_count("EXECUTE_GRAPH_RECV_ANY_E2E_WARMUPS", DEFAULT_WARMUPS, true)?;
    let raw_sample_count = env_count(
        "EXECUTE_GRAPH_RECV_ANY_E2E_SAMPLES",
        DEFAULT_RAW_SAMPLES,
        false,
    )?;
    ensure!(
        warmups
            .checked_add(raw_sample_count)
            .is_some_and(|runs| runs <= MAX_RUNS),
        "combined warmup and raw sample count must not exceed {MAX_RUNS}"
    );

    let mut fixture_bytes = Vec::with_capacity(RECORD_COUNT * (TEXT_BYTES + 48));
    let mut expected = Vec::with_capacity(RECORD_COUNT);
    for id in 1..=RECORD_COUNT {
        expected.push(Value::Record(BTreeMap::from([
            ("id".into(), Value::Integer(i64::try_from(id)?)),
            ("text".into(), Value::String("x".repeat(TEXT_BYTES))),
        ])));
        fixture_bytes.extend_from_slice(&record_json_line(expected.last().expect("just pushed"))?);
        fixture_bytes.push(b'\n');
    }
    let input_sha256 = sha256(&fixture_bytes);
    let expected_payload_bytes = expected.iter().try_fold(0usize, |total, value| {
        total
            .checked_add(serde_json::to_vec(value)?.len())
            .ok_or_else(|| anyhow!("benchmark expected output byte count overflowed"))
    })?;
    let max_payload_bytes = expected.iter().try_fold(0usize, |maximum, value| {
        Ok::<_, anyhow::Error>(maximum.max(serde_json::to_vec(value)?.len()))
    })?;
    ensure!(
        max_payload_bytes < 4096,
        "fixture record exceeds the explicit message limit"
    );
    fixture_file.write_all(&fixture_bytes)?;
    fixture_file.flush()?;
    drop(fixture_file);

    let manifest = source_manifest()?;
    let registry = Registry::from_sources(&[manifest.as_str()])?;
    let graph_template = String::from(
        "graph graph_recv_any_e2e\nstream from bench.jsonl_source with path __FIXTURE__ as producer\nshow each line\nend graph\n",
    );
    let fixture_utf8 = fixture
        .to_str()
        .ok_or_else(|| anyhow!("benchmark fixture path is not UTF-8"))?;
    let graph_source = graph_template.replace("__FIXTURE__", &serde_json::to_string(fixture_utf8)?);
    let mut plan = graph::parse(&graph_source, &registry)?;
    add_receiver(&mut plan, "consumer-0", "bench.pass.0")?;
    add_receiver(&mut plan, "consumer-1", "bench.pass.1")?;
    plan.validate_with_registry(&registry)?;
    let terminals = plan.output_terminals();
    ensure!(
        terminals.len() == 3,
        "expected two consumer terminals and one producer status"
    );

    write_journal(
        &mut journal_file,
        &json!({
            "status": "prepared",
            "planned_warmups": warmups,
            "planned_samples": raw_sample_count,
            "record_count": RECORD_COUNT,
            "input_fixture_sha256": input_sha256,
        }),
    )?;

    let mut warmup_runs = Vec::with_capacity(warmups);
    for index in 0..warmups {
        let (duration_ns, evidence) = run_one(
            &plan,
            &registry,
            fixture.parent().expect("fixture has a parent"),
            &expected,
            expected_payload_bytes,
            &input_sha256,
            fresh_capture(),
            &mut journal_file,
            "warmup",
            index,
        )?;
        warmup_runs.push(json!({"index": index, "duration_ns": duration_ns, "evidence": evidence}));
    }

    let mut raw_samples = Vec::with_capacity(raw_sample_count);
    for index in 0..raw_sample_count {
        let (duration_ns, evidence) = run_one(
            &plan,
            &registry,
            fixture.parent().expect("fixture has a parent"),
            &expected,
            expected_payload_bytes,
            &input_sha256,
            fresh_capture(),
            &mut journal_file,
            "sample",
            index,
        )?;
        raw_samples
            .push(json!({"sample_index": index, "duration_ns": duration_ns, "evidence": evidence}));
    }

    let manifest_sha256 = sha256(manifest.as_bytes());
    let harness_sha256 = sha256(include_bytes!("graph_recv_any_e2e_bench.rs"));
    let report_value = json!({
        "format": "execute.graph.recv_any_e2e.v1",
        "method": "wall-clock GraphSession start through run_to_completion; real finite cat NativeStream, typed JSON decoding, two bounded lossless fanout edges, two custom pass-through receivers using recv_any on one queue each, and terminal collection; all exact payload/order/hash validation follows the timed region",
        "configuration": {
            "inner_warmups": warmups,
            "raw_samples": raw_sample_count,
            "record_count": RECORD_COUNT,
            "text_bytes_per_record": TEXT_BYTES,
            "edge_capacity": EDGE_CAPACITY,
            "edge_policy": "lossless",
            "terminal_capacity": TERMINAL_CAPACITY,
            "generation": GENERATION,
            "revision": REVISION,
            "max_collected_events": RECORD_COUNT * 2 + 1,
            "max_collected_bytes": expected_payload_bytes * 2 + serde_json::to_vec(&Value::Integer(0))?.len(),
            "max_message_bytes": 4096,
            "expected_launched_processes": 1,
        },
        "identity": {
            "harness_sha256": harness_sha256,
            "source_manifest_sha256": manifest_sha256,
            "graph_template_sha256": sha256(graph_template.as_bytes()),
            "input_fixture_path": fixture.display().to_string(),
            "input_fixture_bytes": fixture_bytes.len(),
            "sample_journal_path": journal_output.display().to_string(),
            "input_fixture_sha256": input_sha256,
            "expected_payload_sha256_per_consumer": input_sha256,
        },
        "warmups": warmup_runs,
        "raw_samples_in_execution_order": raw_samples,
    });
    write_report(&mut report_file, &report_value)?;
    Ok(())
}
