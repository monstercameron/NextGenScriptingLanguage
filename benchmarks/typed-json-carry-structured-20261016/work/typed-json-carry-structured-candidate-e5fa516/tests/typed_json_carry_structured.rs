//! Mechanistic public-runtime benchmark for typed structured JSON outputs.
//!
//! The ignored release test runs 1,000 uncached in-process `csv.parse` calls
//! over the same frozen CSV input, storing each result in its own binding,
//! then encodes the final table. The language requires unique result names,
//! so all 1,000 parsed tables remain bound during execution. Catalog loading,
//! fixture/program construction, hashing, and report writing stay outside the
//! timed region.

use flow::{
    ast::Value,
    manifest::Registry,
    parser,
    runtime::{self, execution_result::ExecutionKind},
};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fmt::Write as _,
    fs::OpenOptions,
    hint::black_box,
    io::Write as _,
    path::{Path, PathBuf},
    time::Instant,
};

const CALLS: usize = 1_000;
const ROWS: usize = 64;
const WARMUPS: usize = 0;
const SAMPLES: usize = 3;
const EXPECTED_RETAINED_EXECUTION_RECORDS: usize = 256;
const EXPECTED_DROPPED_EXECUTION_RECORDS: usize = CALLS - EXPECTED_RETAINED_EXECUTION_RECORDS;

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(encoded, "{byte:02x}").unwrap();
    }
    encoded
}

fn frozen_csv() -> (String, Vec<String>, Vec<Vec<String>>) {
    let headers = vec![
        "identifier".to_owned(),
        "label".to_owned(),
        "category".to_owned(),
        "note".to_owned(),
    ];
    let rows = (0..ROWS)
        .map(|index| {
            vec![
                format!("record-{index:03}"),
                format!("label-{index:03}"),
                format!("group-{:02}", index % 8),
                format!("note-{index:03}-fixed-structured-output"),
            ]
        })
        .collect::<Vec<_>>();
    let mut text = String::with_capacity(4_096);
    writeln!(text, "{}", headers.join(",")).unwrap();
    for row in &rows {
        writeln!(text, "{}", row.join(",")).unwrap();
    }
    (text, headers, rows)
}

fn verify(result: &runtime::RunResult, expected_output: &[u8], expected_call_stdout_bytes: usize) {
    assert_eq!(
        result.output, expected_output,
        "final encoded CSV table changed"
    );
    assert_eq!(
        result.launched_processes, 0,
        "csv.parse launched a child process"
    );
    assert_eq!(
        result.cache_hits, 0,
        "unexpected cache hit in uncached workload"
    );
    assert_eq!(
        result.executions.len(),
        EXPECTED_RETAINED_EXECUTION_RECORDS,
        "retained execution history count changed"
    );
    assert_eq!(
        result.dropped_executions, EXPECTED_DROPPED_EXECUTION_RECORDS,
        "runtime did not drop exactly 744 overflow calls"
    );
    assert_eq!(
        result.dropped_stages, EXPECTED_DROPPED_EXECUTION_RECORDS,
        "runtime did not drop exactly 744 overflow stages"
    );
    assert!(
        result.executions.iter().all(|record| {
            record.applet == "csv.parse"
                && record.kind == ExecutionKind::Success
                && record.outcome.as_deref() == Some("success")
                && record.status == Some(0)
                && record.stdout_captured
                && record.stdout_bytes == expected_call_stdout_bytes
                && !record.stderr_captured
                && record.stderr_bytes == 0
                && record.stages.len() == 1
                && record.stages[0].applet == "csv.parse"
                && record.stages[0].status == Some(0)
                && record.stages[0].kind == Some(ExecutionKind::Success)
                && record.stages[0].outcome.as_deref() == Some("success")
        }),
        "a retained csv.parse record did not match the success/byte oracle"
    );
}

#[test]
#[ignore = "explicit paired release public-runtime performance measurement"]
fn structured_csv_parse_1000() {
    assert_eq!(WARMUPS, 0);
    assert_eq!(SAMPLES, 3);

    let report_path = PathBuf::from(
        std::env::var("OPT_BENCH_REPORT")
            .expect("set OPT_BENCH_REPORT to a new absolute JSON report path"),
    );
    assert!(
        report_path.is_absolute(),
        "OPT_BENCH_REPORT must be absolute"
    );
    assert!(
        !report_path.exists(),
        "refusing to overwrite benchmark report {}",
        report_path.display()
    );

    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let registry = Registry::load(&root.join("manifests")).unwrap();
    let (csv_text, headers, rows) = frozen_csv();
    assert!(
        (3_000..=5_000).contains(&csv_text.len()),
        "fixed structured fixture should stay near 4 KiB"
    );
    let mut source = String::with_capacity(CALLS * 72 + 128);
    source.push_str("accept csv as text\n");
    for index in 0..CALLS {
        writeln!(
            source,
            "run csv.parse with text csv and has_headers true as table{index:04}"
        )
        .unwrap();
    }
    writeln!(source, "then encode table{:04} as json", CALLS - 1).unwrap();
    let program = parser::parse(&source).unwrap();
    let options = runtime::RunOptions {
        inputs: BTreeMap::from([("csv".to_owned(), Value::String(csv_text.clone()))]),
        working_directory: root.to_path_buf(),
        cache_directory: None,
        trace: false,
        max_output_bytes: 1024 * 1024,
        command_timeout: None,
        total_timeout: None,
    };

    let expected_value = json!({"headers": headers.clone(), "rows": rows.clone()});
    let expected_output = serde_json::to_vec(&expected_value).unwrap();
    let expected_call_stdout_bytes = expected_output.len() + 1;
    let source_sha256 = digest(source.as_bytes());
    let csv_sha256 = digest(csv_text.as_bytes());
    let catalog_sha256 = digest(&serde_json::to_vec(registry.manifests()).unwrap());
    let expected_output_sha256 = digest(&expected_output);
    let mut raw_sample_ns = Vec::with_capacity(SAMPLES);
    let mut semantic_oracles = Vec::with_capacity(SAMPLES);

    for _ in 0..SAMPLES {
        let started = Instant::now();
        let result = runtime::run(
            black_box(&program),
            black_box(&registry),
            black_box(&options),
        )
        .unwrap();
        let elapsed_ns = started.elapsed().as_nanos() as u64;

        // Check each timed invocation after stopping its timer, so an error or
        // shorter execution can never appear as a successful fast sample.
        verify(&result, &expected_output, expected_call_stdout_bytes);
        black_box(&result);
        raw_sample_ns.push(elapsed_ns);
        semantic_oracles.push(json!({
            "output_sha256": expected_output_sha256,
            "output_bytes": expected_output.len(),
            "launched_processes": result.launched_processes,
            "cache_hits": result.cache_hits,
            "retained_execution_records": result.executions.len(),
            "retained_all_success_status_0": true,
            "retained_stdout_bytes_per_call": expected_call_stdout_bytes,
            "dropped_execution_records": result.dropped_executions,
            "dropped_stages": result.dropped_stages,
        }));
    }

    let mut sorted = raw_sample_ns.clone();
    sorted.sort_unstable();
    let report = json!({
        "schema": "typed_json_carry_public_runtime_structured_bench/v1",
        "method": "Public runtime::run API; 1,000 uncached in-process csv.parse calls over one fixed 64-row, four-column CSV input. The language requires distinct result names, so calls bind table0000 through table0999 and all 1,000 tables remain live; the final table is encoded as compact JSON. Includes repeated in-process parsing, typed output validation/carry, binding retention, and runtime result construction; excludes registry/program/fixture setup, hashing, report writing, and RunResult destruction. No child processes.",
        "warmup_runs": WARMUPS,
        "measured_samples": SAMPLES,
        "workload": {
            "calls": CALLS,
            "applet": "csv.parse",
            "binding_pattern": "table0000 through table0999 (distinct bindings required by the DSL)",
            "retained_parsed_tables": CALLS,
            "rows": ROWS,
            "columns": 4,
            "input_bytes": csv_text.len(),
            "input_sha256": csv_sha256,
            "source_bytes": source.len(),
            "source_sha256": source_sha256,
            "catalog_sha256": catalog_sha256,
            "expected_headers": headers,
            "expected_rows": rows,
            "expected_output_hex": hex_encode(&expected_output),
            "expected_output_bytes": expected_output.len(),
            "expected_output_sha256": expected_output_sha256,
            "expected_per_call_stdout_bytes_including_lf": expected_call_stdout_bytes,
        },
        "results": {
            "structured_csv_parse_1000": {
                "p50_ns": sorted[1],
                "p95_ns": sorted[2],
                "raw_sample_ns": raw_sample_ns,
                "semantic_oracles": semantic_oracles,
            }
        },
    });
    let mut report_file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&report_path)
        .unwrap_or_else(|error| {
            panic!(
                "creating fresh benchmark report {}: {error}",
                report_path.display()
            )
        });
    report_file
        .write_all(&serde_json::to_vec_pretty(&report).unwrap())
        .unwrap();
    report_file.sync_all().unwrap();
}
