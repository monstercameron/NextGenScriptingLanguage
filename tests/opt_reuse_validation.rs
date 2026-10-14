//! Explicit in-process reusable-definition validation measurements; no commands run.
use flow::{manifest::Registry, parser, runtime};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, hint::black_box, time::Instant};

#[test]
#[ignore = "explicit paired release performance measurements"]
fn reusable_definition_validation() {
    let registry = Registry::default();
    let mut results = BTreeMap::new();
    for (name, steps, payload_bytes) in [
        ("small", 8, 32),
        ("many_commands", 1000, 32),
        ("large_literals", 256, 4096),
    ] {
        let payload = "x".repeat(payload_bytes);
        for called in [false, true] {
            let source = format!(
                "define a program named helper\n{}{}\n",
                format!("  run \"printf\" with arguments [\"%s\", \"{payload}\"]\n").repeat(steps),
                if called {
                    "run program helper"
                } else {
                    "run \"true\""
                }
            );
            let program = parser::parse(&source).unwrap();
            runtime::validate(&program, &registry).unwrap();
            let mut raw = Vec::new();
            for batch in 0..60 {
                let start = Instant::now();
                for _ in 0..20 {
                    runtime::validate(black_box(&program), black_box(&registry)).unwrap();
                }
                if batch >= 10 {
                    raw.push(start.elapsed().as_secs_f64() * 1e9 / 20.0);
                }
            }
            let mut sorted = raw.clone();
            sorted.sort_by(f64::total_cmp);
            results.insert(
                format!("{name}_{}", if called { "called" } else { "unused" }),
                json!({
                    "steps_in_definition": steps,
                    "literal_bytes_per_step": payload_bytes,
                    "source_bytes": source.len(),
                    "source_sha256": format!("{:x}", Sha256::digest(source.as_bytes())),
                    "p50_ns": (sorted[24] + sorted[25]) / 2.0,
                    "p95_ns": sorted[47],
                    "raw_batch_average_ns": raw,
                }),
            );
        }
    }
    let report = json!({
        "method": "In-process release validation only; parsing/registry setup excluded; validation allocation/drop included. 10 warmup batches, 50 measured batches, 20 validations per batch. No native commands execute. Warm caches and uncontrolled host background activity.",
        "version": env!("CARGO_PKG_VERSION"),
        "results": results,
    });
    std::fs::write(
        std::env::var("OPT_BENCH_REPORT").expect("set OPT_BENCH_REPORT"),
        serde_json::to_vec_pretty(&report).unwrap(),
    )
    .unwrap();
}
