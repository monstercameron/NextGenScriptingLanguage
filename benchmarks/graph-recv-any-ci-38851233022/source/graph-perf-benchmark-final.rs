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
