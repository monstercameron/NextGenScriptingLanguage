//! Explicit release benchmark for repeated calls to the checked-in public
//! finite program-module example. Copy this file unchanged to both comparison
//! arms and run only after building the same release test target on each.

use flow::{
    ast::{LoopKind, StepKind, Value},
    manifest::Registry,
    parser, runtime,
};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, hint::black_box, path::Path, time::Instant};

const CALLS_PER_RUN: usize = 128;
const NAME_BYTES: usize = 4096;
const CALLER_SOURCE: &str = "accept report as json with default null\naccept payload as json\nrepeat 128 times\n  run examples.records.report with payload payload as report\nthen encode report as json\n";

fn run_count(variable: &str, default: usize) -> usize {
    std::env::var(variable)
        .map(|value| {
            value
                .parse::<usize>()
                .unwrap_or_else(|_| panic!("{variable} must be a nonnegative integer"))
        })
        .unwrap_or(default)
}

#[test]
#[ignore = "explicit release performance measurements"]
fn finite_module_repeated_calls_128_with_4kib_payload() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let modules_root = root.join("examples/modules");
    let registry = Registry::load(&modules_root).expect("load checked-in module example");
    let program = parser::parse(CALLER_SOURCE).expect("parse benchmark caller");
    assert_workload_shape(&program);
    let name = "x".repeat(NAME_BYTES);
    let payload = Value::Record(BTreeMap::from([
        ("count".into(), Value::Integer(3)),
        ("name".into(), Value::String(name.clone())),
    ]));
    let working_directory = tempfile::tempdir().expect("create runtime working directory");
    let options = runtime::RunOptions {
        inputs: BTreeMap::from([("payload".into(), payload)]),
        working_directory: working_directory.path().to_path_buf(),
        cache_directory: None,
        trace: false,
        max_output_bytes: 1024 * 1024,
        command_timeout: None,
        total_timeout: None,
    };
    let expected = serde_json::to_vec(&json!({"count": 3, "name": name}))
        .expect("serialize exact public workload output");
    let warmup_runs = run_count("PROGRAM_MODULE_CACHE_BENCH_WARMUPS", 10);
    let measured_runs = run_count("PROGRAM_MODULE_CACHE_BENCH_SAMPLES", 100);
    assert!(measured_runs > 0, "measured samples must be at least one");

    for _ in 0..warmup_runs {
        let result =
            runtime::run(&program, &registry, &options).expect("warmup repeated module workload");
        assert_eq!(
            result.launched_processes, 0,
            "module workload spawns no processes"
        );
        assert_eq!(result.output, expected, "warmup exact output");
    }

    let mut sample_ns = Vec::with_capacity(measured_runs);
    for _ in 0..measured_runs {
        let started = Instant::now();
        let result =
            runtime::run(&program, &registry, &options).expect("measured repeated module workload");
        let elapsed_ns = started.elapsed().as_secs_f64() * 1e9;
        assert_eq!(
            result.launched_processes, 0,
            "module workload spawns no processes"
        );
        assert_eq!(result.output, expected, "measured exact output");
        black_box(result.output);
        sample_ns.push(elapsed_ns);
    }

    let input_bytes = serde_json::to_vec(&json!({"count": 3, "name": "x".repeat(NAME_BYTES)}))
        .expect("serialize benchmark input identity");
    let report = json!({
        "version": env!("CARGO_PKG_VERSION"),
        "workload": "finite-program-module-128-calls-4kib-name",
        "measurement": "release runtime::run duration; includes top-level validation and one module preload per run, excludes compilation and CLI startup; each successful run completes one module call in each of 128 repeat iterations",
        "module": "examples.records.report",
        "calls_per_run": CALLS_PER_RUN,
        "successful_module_invocations_per_run": CALLS_PER_RUN,
        "per_call_output_retention": "RunResult retains only final caller output; every warmup and measured run output is checked against the exact module result after timing",
        "prepared_program_retention": "one validated finite child Program per ModuleKey in the execution session, shared across calls; retained only for that session",
        "name_utf8_bytes": NAME_BYTES,
        "warmup_runs": warmup_runs,
        "measured_runs": measured_runs,
        "input_sha256": format!("{:x}", Sha256::digest(&input_bytes)),
        "caller_source_sha256": format!("{:x}", Sha256::digest(CALLER_SOURCE.as_bytes())),
        "expected_output_bytes": expected.len(),
        "expected_output_sha256": format!("{:x}", Sha256::digest(&expected)),
        "sample_ns": sample_ns,
    });
    let report_path = std::env::var("PROGRAM_MODULE_CACHE_BENCH_REPORT")
        .expect("set PROGRAM_MODULE_CACHE_BENCH_REPORT to an output path");
    std::fs::write(report_path, serde_json::to_vec_pretty(&report).unwrap())
        .expect("write raw benchmark samples");
}

fn assert_workload_shape(program: &flow::ast::Program) {
    let repeat_counts = program
        .steps
        .iter()
        .enumerate()
        .filter_map(|(index, step)| match &step.kind {
            StepKind::Loop {
                kind:
                    LoopKind::Repeat {
                        count: Value::Integer(count),
                    },
                end,
            } => Some((index, *count, *end)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(repeat_counts.len(), 1);
    let (repeat_start, repeat_count, repeat_end) = repeat_counts[0];
    assert_eq!(repeat_count, CALLS_PER_RUN as i64);

    let applet_calls = program
        .steps
        .iter()
        .enumerate()
        .filter_map(|(index, step)| match &step.kind {
            StepKind::Invoke { call, .. } => Some((index, call.operation.as_str())),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(applet_calls.len(), 1);
    let (call_index, operation) = applet_calls[0];
    assert_eq!(operation, "examples.records.report");
    assert!(repeat_start < call_index && call_index < repeat_end);
}
