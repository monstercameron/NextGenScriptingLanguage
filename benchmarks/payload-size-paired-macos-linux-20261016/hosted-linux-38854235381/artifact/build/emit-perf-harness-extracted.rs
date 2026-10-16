#[cfg(test)]
mod emit_perf_harness {
    use super::*;
    use crate::contracts::ValueSchema;
    use serde::Serialize;
    use serde_json::json;
    use sha2::{Digest, Sha256};
    use std::collections::BTreeMap;
    use std::hint::black_box;
    use std::io::Write;

    const WARMUP_REPETITIONS: usize = 2;
    const MEASURED_REPETITIONS: usize = 7;
    const TEXT_BYTES: usize = 4096;
    const RECORD_BODY_BYTES: usize = 2048;

    #[derive(Clone, Copy)]
    enum PayloadKind {
        Integer,
        Text,
        TypedRecord,
    }

    impl PayloadKind {
        fn name(self) -> &'static str {
            match self {
                Self::Integer => "integer",
                Self::Text => "text",
                Self::TypedRecord => "typed_record",
            }
        }
    }

    struct Case {
        name: &'static str,
        sinks: usize,
        events: usize,
        payload: PayloadKind,
        remote_metadata: bool,
    }

    const CASES: &[Case] = &[
        Case {
            name: "single_sink_integer",
            sinks: 1,
            events: 256,
            payload: PayloadKind::Integer,
            remote_metadata: false,
        },
        Case {
            name: "fanout_8_integer",
            sinks: 8,
            events: 128,
            payload: PayloadKind::Integer,
            remote_metadata: false,
        },
        Case {
            name: "fanout_4_text",
            sinks: 4,
            events: 16,
            payload: PayloadKind::Text,
            remote_metadata: true,
        },
        Case {
            name: "fanout_4_typed_record",
            sinks: 4,
            events: 16,
            payload: PayloadKind::TypedRecord,
            remote_metadata: true,
        },
    ];

    #[derive(Serialize)]
    struct OracleEvent<'a> {
        port: &'a PortRef,
        input: &'a Option<PortRef>,
        generation: u64,
        revision: u64,
        sequence: u64,
        sensitivity: &'a SensitivitySet,
        remote: &'a Option<RemoteEventMetadata>,
        payload: &'a PortValue,
    }

    fn scalar_schema(kind: OutputType) -> ValueSchema {
        ValueSchema {
            kind,
            nullable: false,
            items: None,
            fields: BTreeMap::new(),
            required: Vec::new(),
            enum_values: None,
        }
    }

    fn schema_for(kind: PayloadKind) -> ValueSchema {
        match kind {
            PayloadKind::Integer => scalar_schema(OutputType::Integer),
            PayloadKind::Text => scalar_schema(OutputType::Text),
            PayloadKind::TypedRecord => {
                let integer = scalar_schema(OutputType::Integer);
                let text = scalar_schema(OutputType::Text);
                ValueSchema {
                    kind: OutputType::Record,
                    nullable: false,
                    items: None,
                    fields: BTreeMap::from([
                        ("body".into(), text.clone()),
                        ("index".into(), integer.clone()),
                        ("label".into(), text),
                        (
                            "values".into(),
                            ValueSchema {
                                kind: OutputType::List,
                                nullable: false,
                                items: Some(Box::new(integer)),
                                fields: BTreeMap::new(),
                                required: Vec::new(),
                                enum_values: None,
                            },
                        ),
                    ]),
                    required: vec![
                        "body".into(),
                        "index".into(),
                        "label".into(),
                        "values".into(),
                    ],
                    enum_values: None,
                }
            }
        }
    }

    fn payload_for(kind: PayloadKind, index: usize) -> PortValue {
        match kind {
            PayloadKind::Integer => PortValue::Value(Value::Integer(index as i64)),
            PayloadKind::Text => PortValue::Value(Value::String(format!(
                "event-{index:04}:{}",
                "x".repeat(TEXT_BYTES)
            ))),
            PayloadKind::TypedRecord => {
                let mut fields = BTreeMap::new();
                fields.insert("body".into(), Value::String("r".repeat(RECORD_BODY_BYTES)));
                fields.insert("index".into(), Value::Integer(index as i64));
                fields.insert("label".into(), Value::String(format!("record-{index:04}")));
                fields.insert(
                    "values".into(),
                    Value::List(
                        (0..8)
                            .map(|offset| Value::Integer(index as i64 + offset))
                            .collect(),
                    ),
                );
                PortValue::Value(Value::Record(fields))
            }
        }
    }

    fn remote_metadata(index: usize) -> RemoteEventMetadata {
        RemoteEventMetadata {
            event: format!("bench-event-{index:04}"),
            cursor: Some(format!("cursor-{index:04}")),
            duplicate_cursor: index.is_multiple_of(5),
        }
    }

    fn preallocated_queue(capacity: usize) -> Arc<BoundedQueue> {
        Arc::new(BoundedQueue {
            capacity,
            policy: BufferPolicy::Lossless,
            state: Mutex::new(QueueState {
                items: VecDeque::with_capacity(capacity),
                closed: false,
            }),
            changed: Condvar::new(),
            dropped: AtomicUsize::new(0),
        })
    }

    fn node_io(case: &Case) -> (TaskScope, NodeIo, Vec<Arc<BoundedQueue>>) {
        let task_scope = TaskScope::new(1, None).expect("benchmark task scope");
        let token = task_scope.cancellation_token();
        let queues = (0..case.sinks)
            .map(|_| preallocated_queue(case.events))
            .collect::<Vec<_>>();
        let sinks = queues
            .iter()
            .enumerate()
            .map(|(index, queue)| QueueSink {
                queue: Arc::clone(queue),
                input: Some(PortRef::new(format!("sink_{index:02}"), "in")),
            })
            .collect::<Vec<_>>();
        let schema = PortSchema {
            schema: schema_for(case.payload),
            delivery: PortDelivery::Events,
            sensitivity: Sensitivity::Confidential,
            required: false,
            pointer: None,
            encoding: None,
        };
        let cancelled = token.cancellation_flag();
        let io = NodeIo {
            node: "emitter".into(),
            inputs: BTreeMap::new(),
            input_schemas: BTreeMap::new(),
            initial_inputs: BTreeMap::new(),
            input_name_cursor: 0,
            input_queue_cursors: BTreeMap::new(),
            output_sinks: BTreeMap::from([("out".into(), sinks)]),
            schemas: BTreeMap::from([("out".into(), schema)]),
            generation: 17,
            revision: 23,
            next_sequence: Arc::new(AtomicU64::new(1)),
            token,
            external_cancelled: cancelled,
            deadline: None,
            input_sensitivity: Some(SensitivitySet::from_label(Sensitivity::Internal)),
            max_message_bytes: 8 * 1024,
        };
        (task_scope, io, queues)
    }

    fn envelope_json(event: &EventEnvelope) -> serde_json::Value {
        serde_json::from_slice(&envelope_bytes(event))
            .expect("deserialize exact emitted event oracle")
    }

    fn envelope_bytes(event: &EventEnvelope) -> Vec<u8> {
        serde_json::to_vec(&OracleEvent {
            port: &event.port,
            input: &event.input,
            generation: event.generation,
            revision: event.revision,
            sequence: event.sequence,
            sensitivity: &event.sensitivity,
            remote: &event.remote,
            payload: &event.payload,
        })
        .expect("serialize exact emitted event oracle")
    }

    fn expected_envelope(case: &Case, sink_index: usize, event_index: usize) -> EventEnvelope {
        EventEnvelope {
            port: PortRef::new("emitter", "out"),
            input: Some(PortRef::new(format!("sink_{sink_index:02}"), "in")),
            generation: 17,
            revision: 23,
            sequence: (event_index + 1) as u64,
            sensitivity: SensitivitySet::from_labels([
                Sensitivity::Internal,
                Sensitivity::Confidential,
            ]),
            remote: case.remote_metadata.then(|| remote_metadata(event_index)),
            payload: payload_for(case.payload, event_index),
        }
    }

    fn env_count(name: &str, default: usize) -> usize {
        std::env::var(name)
            .map(|value| value.parse().expect("benchmark count must be an integer"))
            .unwrap_or(default)
    }

    #[test]
    #[ignore = "manual release-only graph emit benchmark; writes raw samples and exact event oracle JSON"]
    #[allow(clippy::assertions_on_constants)]
    fn emit_raw_samples_with_full_oracle() {
        assert!(
            !cfg!(debug_assertions),
            "run this ignored harness with --release for representative timings"
        );
        let report_path = std::env::var_os("EXECUTE_EMIT_PERF_REPORT")
            .expect("set EXECUTE_EMIT_PERF_REPORT to a fresh JSON output path");
        let warmups = env_count("EXECUTE_EMIT_PERF_WARMUPS", WARMUP_REPETITIONS);
        let measured = env_count("EXECUTE_EMIT_PERF_SAMPLES", MEASURED_REPETITIONS);
        assert!(measured > 0, "at least one measured repetition is required");

        let mut cases = Vec::with_capacity(CASES.len());
        for case in CASES {
            let mut samples = Vec::with_capacity(warmups + measured);
            for repetition in 0..warmups + measured {
                // Build every owned input and queue before starting the clock.
                let payloads = (0..case.events)
                    .map(|index| payload_for(case.payload, index))
                    .collect::<Vec<_>>();
                let input_sha256 = format!(
                    "{:x}",
                    Sha256::digest(
                        serde_json::to_vec(&payloads).expect("serialize benchmark inputs")
                    )
                );
                let remote_metadata = (0..case.events)
                    .map(|index| case.remote_metadata.then(|| remote_metadata(index)))
                    .collect::<Vec<_>>();
                let (_task_scope, mut io, queues) = node_io(case);
                let mut delivered = 0usize;
                let mut dropped = 0usize;
                let started = Instant::now();
                for (payload, metadata) in payloads.into_iter().zip(remote_metadata) {
                    let result = match metadata {
                        Some(metadata) => io.emit_remote_event("out", black_box(payload), metadata),
                        None => io.emit("out", black_box(payload)),
                    }
                    .expect("bounded emit should succeed");
                    delivered += result.delivered;
                    dropped += result.dropped;
                }
                let duration_ns = started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64;
                assert_eq!(delivered, case.events * case.sinks);
                assert_eq!(dropped, 0);

                // Drain and compare every complete envelope after timing.
                let mut streams = Vec::with_capacity(case.sinks);
                for (sink_index, queue) in queues.iter().enumerate() {
                    let mut canonical_stream = Sha256::new();
                    let mut event_count = 0usize;
                    let mut representative_event = None;
                    for event_index in 0..case.events {
                        let actual = queue
                            .try_recv()
                            .expect("queue lock should remain healthy")
                            .expect("each sink should receive every event");
                        let expected = expected_envelope(case, sink_index, event_index);
                        assert_eq!(
                            actual, expected,
                            "{} sink {sink_index} event {event_index}",
                            case.name
                        );
                        let actual_bytes = envelope_bytes(&actual);
                        let expected_bytes = envelope_bytes(&expected);
                        assert_eq!(
                            actual_bytes, expected_bytes,
                            "{} serialized sink {sink_index} event {event_index}",
                            case.name
                        );
                        canonical_stream.update((actual_bytes.len() as u64).to_be_bytes());
                        canonical_stream.update(&actual_bytes);
                        if representative_event.is_none() {
                            representative_event = Some(envelope_json(&actual));
                        }
                        event_count += 1;
                    }
                    assert!(queue
                        .try_recv()
                        .expect("queue lock should remain healthy")
                        .is_none());
                    streams.push(json!({
                        "sink": format!("sink_{sink_index:02}.in"),
                        "event_count": event_count,
                        "canonical_stream_sha256": format!("{:x}", canonical_stream.finalize()),
                        "representative_event": representative_event,
                    }));
                }

                samples.push(json!({
                    "repetition": repetition,
                    "warmup": repetition < warmups,
                    "duration_ns": duration_ns,
                    "input_payloads_sha256": input_sha256,
                    "delivered": delivered,
                    "dropped": dropped,
                    "exact_correctness_oracle": streams,
                }));
            }
            cases.push(json!({
                "name": case.name,
                "payload_kind": case.payload.name(),
                "sink_count": case.sinks,
                "events": case.events,
                "remote_metadata": case.remote_metadata,
                "queue_policy": "lossless",
                "queue_capacity_per_sink": case.events,
                "queue_storage_preallocated": true,
                "input_payloads_sha256": samples[0]["input_payloads_sha256"].clone(),
                "samples_in_execution_order": samples,
            }));
        }
        let report = json!({
            "format": "execute.graph.emit_perf.v2",
            "method": concat!(
                "In-process NodeIo emit calls only; inputs, schemas, metadata, and bounded ",
                "preallocated lossless queues are prepared before timing. Each raw duration ",
                "is retained in execution order. After timing, every full envelope is checked ",
                "for payload, ports, generation, revision, sequence, sensitivity, and remote ",
                "metadata by value and exact serialized bytes. Per-sink stream hashes and one ",
                "representative envelope retain the correctness oracle compactly. No sorting ",
                "or percentile aggregation is performed."
            ),
            "warmup_repetitions": warmups,
            "measured_repetitions": measured,
            "cases": cases,
        });
        let path = std::path::PathBuf::from(report_path);
        let mut output = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .expect("benchmark report path must be fresh");
        output
            .write_all(&serde_json::to_vec_pretty(&report).expect("serialize benchmark report"))
            .expect("write benchmark report");
    }
}
