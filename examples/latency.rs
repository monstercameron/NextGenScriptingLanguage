use anyhow::{Context, Result};
use flow::ast::Value;
use flow::manifest::Registry;
use flow::{parser, runtime};
use serde_json::json;
use std::collections::BTreeMap;
use std::hint::black_box;
use std::path::Path;
use std::time::Instant;

const BATCHES: usize = 100;
const ITERATIONS_PER_BATCH: usize = 1_000;

const SCRIPT: &str = "read file \"input.txt\" as source\n\
run printf with message source\n\
then tr with from \"[:lower:]\" and to \"[:upper:]\"\n\
then save to file \"output.txt\"\n";

#[derive(Debug, serde::Serialize)]
struct Percentiles {
    p50_ns: f64,
    p95_ns: f64,
    p99_ns: f64,
}

fn percentile(sorted: &[f64], quantile: f64) -> f64 {
    let index = ((sorted.len() as f64 * quantile).ceil() as usize)
        .saturating_sub(1)
        .min(sorted.len() - 1);
    sorted[index]
}

/// Percentiles are calculated from each batch's average ns/op, not individual requests.
fn benchmark(mut operation: impl FnMut()) -> Percentiles {
    // Warm caches, allocator paths, and instruction/data pages before timing.
    for _ in 0..ITERATIONS_PER_BATCH {
        operation();
    }

    let mut batch_averages_ns = Vec::with_capacity(BATCHES);
    for _ in 0..BATCHES {
        let started = Instant::now();
        for _ in 0..ITERATIONS_PER_BATCH {
            operation();
        }
        batch_averages_ns.push(started.elapsed().as_nanos() as f64 / ITERATIONS_PER_BATCH as f64);
    }
    batch_averages_ns.sort_by(f64::total_cmp);

    Percentiles {
        p50_ns: percentile(&batch_averages_ns, 0.50),
        p95_ns: percentile(&batch_averages_ns, 0.95),
        p99_ns: percentile(&batch_averages_ns, 0.99),
    }
}

fn main() -> Result<()> {
    let program = parser::parse(SCRIPT).context("parsing benchmark workflow")?;
    let manifests = Registry::load(&Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests"))
        .context("loading applet manifests")?;
    let printf = manifests
        .resolve("printf")
        .context("resolving printf manifest")?;
    let parameters =
        BTreeMap::from([("message".to_owned(), Value::String("benchmark text".into()))]);

    // Validate once before timing so setup errors are reported outside the samples.
    runtime::validate(&program, &manifests).context("validating benchmark workflow")?;
    black_box(parser::parse(black_box(SCRIPT)).context("warming parser")?);
    black_box(runtime::validate(
        black_box(&program),
        black_box(&manifests),
    )?);
    black_box(printf.build_arguments(black_box(&parameters))?);

    let parser_latency = benchmark(|| {
        black_box(parser::parse(black_box(SCRIPT)).expect("benchmark parse should succeed"));
    });
    let validation_latency = benchmark(|| {
        black_box(
            runtime::validate(black_box(&program), black_box(&manifests))
                .map(|()| true)
                .expect("workflow should validate"),
        );
    });
    let argument_latency = benchmark(|| {
        black_box(
            printf
                .build_arguments(black_box(&parameters))
                .expect("printf arguments should build"),
        );
    });

    let report = json!({
        "percentiles_are": "batch averages in nanoseconds per operation, not individual requests",
        "batch_count": BATCHES,
        "iterations_per_batch": ITERATIONS_PER_BATCH,
        "script_text": SCRIPT,
        "operations": {
            "parser_parse": parser_latency,
            "runtime_validate_loaded_registry": validation_latency,
            "printf_manifest_build_arguments": argument_latency,
        }
    });
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}
