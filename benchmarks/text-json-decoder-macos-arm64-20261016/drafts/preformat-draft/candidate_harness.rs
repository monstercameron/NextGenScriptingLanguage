//! Public API microbenchmark for JSON text output decoding.
//!
//! Each release invocation decodes the same fixed 4 KiB ASCII JSON string
//! through `OutputSchema::decode` 10,000 times in each of three samples. Every
//! value is checked byte-for-byte and dropped before the next iteration. Schema
//! and fixture setup, hashing, report writing, and JSON serialization for the
//! payload happen before the timer; there are no filesystem or child-process
//! operations in the measured region.

use flow::{
    ast::Value,
    contracts::{AppletOutput, OutputEncoding, OutputSchema, OutputType},
};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::OpenOptions,
    hint::black_box,
    io::Write,
    path::PathBuf,
    time::Instant,
};

const ITERATIONS: usize = 10_000;
const STRING_BYTES: usize = 4_096;
const WARMUPS: usize = 0;
const SAMPLES: usize = 3;

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn verify_value(output: AppletOutput, expected: &str) {
    match output {
        AppletOutput::Value(Value::String(actual)) => {
            assert_eq!(actual.len(), expected.len(), "decoded string length changed");
            assert_eq!(actual.as_bytes(), expected.as_bytes(), "decoded string bytes changed");
        }
        other => panic!("expected decoded text value, got {other:?}"),
    }
}

#[test]
#[ignore = "explicit paired release public-decoder performance measurement"]
fn text_json_decode_10000() {
    assert_eq!(WARMUPS, 0);
    assert_eq!(SAMPLES, 3);
    let report_path = PathBuf::from(
        std::env::var("OPT_BENCH_REPORT")
            .expect("set OPT_BENCH_REPORT to a new absolute JSON report path"),
    );
    assert!(report_path.is_absolute(), "OPT_BENCH_REPORT must be absolute");
    assert!(
        !report_path.exists(),
        "refusing to overwrite benchmark report {}",
        report_path.display()
    );

    let expected = "x".repeat(STRING_BYTES);
    let input = serde_json::to_vec(&expected).expect("encode fixed JSON string fixture");
    assert_eq!(input.len(), STRING_BYTES + 2, "ASCII JSON fixture size changed");
    let schema = OutputSchema {
        kind: OutputType::Text,
        encoding: OutputEncoding::Json,
        items: None,
        fields: BTreeMap::new(),
        required: Vec::new(),
        enum_values: None,
        trim_final_lf: false,
    };
    let input_sha256 = digest(&input);
    let expected_sha256 = digest(expected.as_bytes());
    let mut raw_sample_ns = Vec::with_capacity(SAMPLES);
    let mut semantic_oracles = Vec::with_capacity(SAMPLES);

    for _ in 0..SAMPLES {
        let started = Instant::now();
        for _ in 0..ITERATIONS {
            let output = schema.decode(black_box(input.as_slice())).expect("decode fixed JSON text");
            verify_value(output, black_box(expected.as_str()));
        }
        let elapsed_ns = started.elapsed().as_nanos() as u64;
        raw_sample_ns.push(elapsed_ns);
        semantic_oracles.push(json!({
            "decoded_values": ITERATIONS,
            "exact_value_matches": ITERATIONS,
            "errors": 0,
            "decoded_string_bytes": STRING_BYTES,
            "decoded_string_sha256": expected_sha256,
        }));
    }

    let mut sorted = raw_sample_ns.clone();
    sorted.sort_unstable();
    let report = json!({
        "schema": "output_schema_text_json_decode_bench/v1",
        "method": "Public flow::contracts::OutputSchema::decode API; 10,000 decodes per sample of one fixed 4,096-byte ASCII JSON string. Each returned Value::String is checked byte-for-byte and dropped before the next decode. Includes JSON parsing, decoded string materialization/ownership, and per-iteration equality validation. Schema/fixture construction, payload serialization, hashes, report writing, and decoded outputs are outside the measured region. No filesystem reads or child processes.",
        "warmup_runs": WARMUPS,
        "measured_samples": SAMPLES,
        "workload": {
            "case": "text_json_decode_10000",
            "api": "flow::contracts::OutputSchema::decode",
            "output_type": "text",
            "encoding": "json",
            "iterations_per_sample": ITERATIONS,
            "fixture": "ASCII byte x repeated 4096 times, JSON encoded once before timing",
            "decoded_string_bytes": STRING_BYTES,
            "json_payload_bytes": input.len(),
            "json_payload_sha256": input_sha256,
            "decoded_string_sha256": expected_sha256,
        },
        "results": {
            "text_json_decode_10000": {
                "p50_ns": sorted[1],
                "p95_ns": sorted[2],
                "raw_sample_ns": raw_sample_ns,
                "semantic_oracles": semantic_oracles,
            }
        }
    });
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&report_path)
        .expect("create fresh benchmark report");
    serde_json::to_writer_pretty(&mut file, &report).expect("write benchmark report JSON");
    file.write_all(b"\n").expect("terminate benchmark report");
    file.sync_all().expect("sync benchmark report");
}
