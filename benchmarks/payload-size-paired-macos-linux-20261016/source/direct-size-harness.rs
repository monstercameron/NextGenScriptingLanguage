#[cfg(test)]
mod payload_size_perf_harness {
    use super::*;
    use serde_json::json;
    use sha2::{Digest, Sha256};
    use std::hint::black_box;
    use std::io::Write;

    const CALLS_PER_SAMPLE: usize = 4096;
    const RAW_SAMPLES: usize = 3;

    struct SizeCase {
        name: &'static str,
        payload: PortValue,
    }

    struct PreparedCase<'a> {
        case: &'a SizeCase,
        payload_refs: Vec<&'a PortValue>,
        expected_size: usize,
        input_sha256: String,
    }

    fn cases() -> Vec<SizeCase> {
        let escaped = "é🦀\n\t\0\"\\\r\x01".repeat(32);
        let mut nested = BTreeMap::new();
        nested.insert("count".into(), Value::Integer(17));
        nested.insert(
            "items".into(),
            Value::List(vec![
                Value::Bool(true),
                Value::String("nested\ntext".into()),
                Value::Record(BTreeMap::from([(
                    "label".into(),
                    Value::String("typed".into()),
                )])),
            ]),
        );
        vec![
            SizeCase {
                name: "scalar_integer",
                payload: PortValue::Value(Value::Integer(17)),
            },
            SizeCase {
                name: "escape_heavy_unicode_control_text",
                payload: PortValue::Value(Value::String(escaped)),
            },
            SizeCase {
                name: "nested_record",
                payload: PortValue::Value(Value::Record(nested)),
            },
            SizeCase {
                name: "text_4k",
                payload: PortValue::Value(Value::String("x".repeat(4096))),
            },
        ]
    }

    fn serialized_value_bytes(payload: &PortValue) -> Vec<u8> {
        match payload {
            PortValue::Value(value) => serde_json::to_vec(value),
            PortValue::Bytes(_) => unreachable!("size harness cases use Value payloads"),
        }
        .expect("serialize benchmark input")
    }

    fn assert_full_oracle(prepared: &PreparedCase<'_>) {
        let bytes = serialized_value_bytes(&prepared.case.payload);
        assert_eq!(
            bytes.len(),
            prepared.expected_size,
            "{} oracle",
            prepared.case.name
        );
        assert_eq!(
            payload_size(&prepared.case.payload).expect("count serialized payload bytes"),
            prepared.expected_size,
            "{} counted size",
            prepared.case.name
        );
    }

    #[test]
    #[ignore = "manual release-only exact payload-size benchmark; writes raw samples to JSON"]
    #[allow(clippy::assertions_on_constants)]
    fn payload_size_raw_samples_with_exact_oracle() {
        assert!(
            !cfg!(debug_assertions),
            "run this ignored harness with --release for representative timings"
        );
        let report_path = std::env::var_os("EXECUTE_PAYLOAD_SIZE_PERF_REPORT")
            .expect("set EXECUTE_PAYLOAD_SIZE_PERF_REPORT to a fresh absolute JSON path");
        let path = std::path::PathBuf::from(report_path);
        assert!(path.is_absolute(), "benchmark report path must be absolute");

        let cases = cases();
        let prepared = cases
            .iter()
            .map(|case| {
                let oracle = serialized_value_bytes(&case.payload);
                let expected_size = oracle.len();
                let input_sha256 = format!("{:x}", Sha256::digest(&oracle));
                let payload_refs = vec![&case.payload; CALLS_PER_SAMPLE];
                PreparedCase {
                    case,
                    payload_refs,
                    expected_size,
                    input_sha256,
                }
            })
            .collect::<Vec<_>>();

        let mut report_cases = Vec::with_capacity(prepared.len());
        for prepared_case in &prepared {
            assert_full_oracle(prepared_case);

            // Warm one complete batch without retaining a timing sample.
            let mut warmup_checksum = 0_u64;
            for payload in &prepared_case.payload_refs {
                let size = payload_size(black_box(*payload)).expect("count warmup payload bytes");
                warmup_checksum = warmup_checksum.wrapping_add(size as u64);
            }
            assert_eq!(
                warmup_checksum,
                prepared_case.expected_size as u64 * CALLS_PER_SAMPLE as u64
            );
            assert_full_oracle(prepared_case);

            let mut samples = Vec::with_capacity(RAW_SAMPLES);
            for sample_index in 0..RAW_SAMPLES {
                let mut checksum = 0_u64;
                let started = Instant::now();
                for payload in &prepared_case.payload_refs {
                    let size =
                        payload_size(black_box(*payload)).expect("count benchmark payload bytes");
                    checksum = checksum.wrapping_add(size as u64);
                    black_box(checksum);
                }
                let duration_ns = started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64;
                black_box(checksum);
                assert_eq!(
                    checksum,
                    prepared_case.expected_size as u64 * CALLS_PER_SAMPLE as u64,
                    "{} sample {sample_index} checksum",
                    prepared_case.case.name
                );
                assert_full_oracle(prepared_case);
                samples.push(json!({
                    "sample_index": sample_index,
                    "duration_ns": duration_ns,
                    "calls": CALLS_PER_SAMPLE,
                    "checksum": checksum,
                }));
            }
            report_cases.push(json!({
                "name": prepared_case.case.name,
                "payload_kind": "typed_value",
                "input_payload_sha256": prepared_case.input_sha256,
                "serialized_bytes": prepared_case.expected_size,
                "calls_per_sample": CALLS_PER_SAMPLE,
                "warmup_calls": CALLS_PER_SAMPLE,
                "samples_in_execution_order": samples,
            }));
        }

        let report = json!({
            "format": "execute.graph.payload_size_perf.v1",
            "method": concat!(
                "In-process payload_size calls only; immutable payload references and all ",
                "case data are prepared before timing. Each sample has exactly 4096 calls ",
                "and retains its raw duration and checksum. Exact serde_json::to_vec byte ",
                "lengths are checked before and after each timed sample. Three unsorted ",
                "samples are retained in execution order; no aggregation is performed."
            ),
            "calls_per_sample": CALLS_PER_SAMPLE,
            "raw_samples": RAW_SAMPLES,
            "warmup_calls_per_case": CALLS_PER_SAMPLE,
            "cases": report_cases,
        });
        let mut output = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .expect("benchmark report path must be fresh");
        output
            .write_all(&serde_json::to_vec_pretty(&report).expect("serialize benchmark report"))
            .expect("write benchmark report");
    }
}

