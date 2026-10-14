//! Explicit native Rust API timings: pure selection/encoding, no child processes.
use flow::{ast::Value, manifest::Registry, parser, runtime};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    hint::black_box,
    time::{Duration, Instant},
};

#[test]
#[ignore = "explicit paired release performance measurements"]
fn supplied_json_input_setup() {
    let directory = tempfile::tempdir().unwrap();
    let registry = Registry::default();
    let mut results = BTreeMap::new();
    for (name, bytes) in [
        ("no_input_control", 0),
        ("json_64", 64),
        ("json_64k", 64 * 1024),
        ("json_1m", 1024 * 1024),
        ("json_4m", 4 * 1024 * 1024),
    ] {
        let source = if bytes == 0 {
            "then encode 42 as json\n"
        } else {
            "accept payload as json\ntake integer at \"/n\" from payload as number\nthen encode number as json\n"
        };
        let program = parser::parse(source).unwrap();
        let inputs = if bytes == 0 {
            BTreeMap::new()
        } else {
            BTreeMap::from([(
                "payload".into(),
                Value::Record(BTreeMap::from([
                    ("n".into(), Value::Integer(42)),
                    ("padding".into(), Value::String("x".repeat(bytes))),
                ])),
            )])
        };
        let fingerprint = format!("{:x}", Sha256::digest(serde_json::to_vec(&inputs).unwrap()));
        let options = runtime::RunOptions {
            inputs,
            working_directory: directory.path().into(),
            cache_directory: None,
            trace: false,
            max_output_bytes: 32,
            command_timeout: Some(Duration::from_secs(5)),
        };
        let check = runtime::run(&program, &registry, &options).unwrap();
        assert_eq!(check.output, b"42");
        assert_eq!(check.launched_processes, 0);
        assert_eq!(check.cache_hits, 0);
        let mut raw = Vec::new();
        for batch in 0..60 {
            let start = Instant::now();
            for _ in 0..10 {
                black_box(
                    runtime::run(
                        black_box(&program),
                        black_box(&registry),
                        black_box(&options),
                    )
                    .unwrap(),
                );
            }
            if batch >= 10 {
                raw.push(start.elapsed().as_secs_f64() * 1e9 / 10.0);
            }
        }
        let mut sorted = raw.clone();
        sorted.sort_by(f64::total_cmp);
        results.insert(
            name,
            json!({
                "source": source,
                "supplied_padding_bytes": bytes,
                "inputs_sha256": fingerprint,
                "p50_ns": (sorted[24] + sorted[25]) / 2.0,
                "p95_ns": sorted[47],
                "raw_batch_average_ns": raw,
            }),
        );
    }
    let report = json!({
        "method": "Native Rust runtime API, no child processes. Fixture construction, parsing and input hashing excluded; validation, supplied input resolution, preflight, signal registration, cwd canonicalization, pure typed JSON selection/encoding and result destruction included. Every workload verified as exact bytes 42, zero processes, zero cache hits before timing. 10 warmup batches and 50 measured batches, 10 operations per batch. Warm filesystem caches, uncontrolled background activity; not isolated input resolution or CLI latency.",
        "version": env!("CARGO_PKG_VERSION"),
        "results": results,
    });
    std::fs::write(
        std::env::var("OPT_BENCH_REPORT").expect("set OPT_BENCH_REPORT"),
        serde_json::to_vec_pretty(&report).unwrap(),
    )
    .unwrap();
}
