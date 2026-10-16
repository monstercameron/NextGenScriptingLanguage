#![cfg(unix)]

use flow::{manifest::Registry, runtime, stream};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{BufWriter, Write},
    path::Path,
    time::{Duration, Instant},
};

const RECORDS: usize = 131_072;
const DEFAULT_WARMUPS: usize = 3;
const DEFAULT_SAMPLES: usize = 24;

fn sample_count(name: &str, default: usize) -> usize {
    std::env::var(name).map_or(default, |value| {
        value
            .parse()
            .unwrap_or_else(|_| panic!("{name} must be a non-negative integer"))
    })
}

fn fixed_jsonl() -> Vec<u8> {
    let mut bytes = Vec::with_capacity(RECORDS * 32);
    for index in 0..RECORDS {
        writeln!(bytes, r#"{{"n":{index},"value":"record-{index:08}"}}"#).unwrap();
    }
    bytes
}

fn stream_program(stages: usize, input: &Path) -> flow::stream::StreamProgram {
    let input_argument = input.to_string_lossy().into_owned();
    let mut source = format!(
        "stream from \"/bin/cat\" with arguments [{}]\n",
        serde_json::to_string(&input_argument).unwrap()
    );
    for _ in 1..stages {
        source.push_str("pipe to \"/bin/cat\"\n");
    }
    source.push_str("show each line\n");
    stream::parse(&source).unwrap()
}

#[test]
#[ignore = "paired release benchmark for public native stream status polling"]
fn public_native_stream_status_polling() {
    let report_path = std::env::var("OPT_BENCH_REPORT").expect("set OPT_BENCH_REPORT");
    let warmups = sample_count("OPT_BENCH_WARMUPS", DEFAULT_WARMUPS);
    let samples = sample_count("OPT_BENCH_SAMPLES", DEFAULT_SAMPLES);
    assert!(samples > 0, "OPT_BENCH_SAMPLES must be positive");
    let mut report_file = BufWriter::new(
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&report_path)
            .expect("claim a unique benchmark report path"),
    );
    let source_revision = std::env::var("OPT_BENCH_SOURCE_REV").expect("set OPT_BENCH_SOURCE_REV");
    let test_binary = std::env::current_exe().unwrap();
    let test_binary_sha256 = format!("{:x}", Sha256::digest(fs::read(&test_binary).unwrap()));
    let cat_path = fs::canonicalize("/bin/cat").unwrap();
    let cat_sha256 = format!("{:x}", Sha256::digest(fs::read(&cat_path).unwrap()));
    let mut all_samples_valid = true;

    let directory = tempfile::tempdir().unwrap();
    let input = fixed_jsonl();
    let expected_bytes = input.len();
    let expected_sha256 = format!("{:x}", Sha256::digest(&input));
    let input_path = directory.path().join("fixed.jsonl");
    fs::write(&input_path, &input).unwrap();
    let options = runtime::RunOptions {
        inputs: Default::default(),
        working_directory: directory.path().to_path_buf(),
        cache_directory: None,
        trace: false,
        max_output_bytes: expected_bytes + 1,
        command_timeout: Some(Duration::from_secs(30)),
        total_timeout: None,
    };
    let registry = Registry::default();
    let cases = [1usize, 4, 8]
        .into_iter()
        .map(|stages| (stages, stream_program(stages, &input_path)))
        .collect::<Vec<_>>();
    let mut warmup_samples = Vec::new();
    let mut raw_samples = Vec::with_capacity(samples * cases.len());
    let mut interleaving = Vec::with_capacity(warmups + samples);

    for round in 0..(warmups + samples) {
        let rotation = round % cases.len();
        let order = (0..cases.len())
            .map(|offset| (rotation + offset) % cases.len())
            .collect::<Vec<_>>();
        interleaving.push(
            order
                .iter()
                .map(|index| cases[*index].0)
                .collect::<Vec<_>>(),
        );
        for index in order {
            let (stages, program) = &cases[index];
            let mut actual_hash = Sha256::new();
            let mut actual_records = 0usize;
            let mut actual_bytes = 0usize;
            let started = Instant::now();
            let result = runtime::run_stream(program, &registry, &options, 256, |record, _, _| {
                actual_hash.update(record);
                actual_records += 1;
                actual_bytes += record.len();
                Ok(())
            });
            let elapsed_ns = started.elapsed().as_nanos();
            let actual_sha256 = format!("{:x}", actual_hash.finalize());
            let (result_records, launched_processes, successful_statuses, error) = match result {
                Ok(result) => (
                    Some(result.records),
                    Some(result.launched_processes),
                    true,
                    None,
                ),
                Err(error) => (None, None, false, Some(format!("{error:#}"))),
            };
            let result_counts_match = result_records == Some(RECORDS);
            let launch_count_matches = launched_processes == Some(*stages);
            let callback_counts_match = actual_records == RECORDS && actual_bytes == expected_bytes;
            let output_matches = actual_sha256 == expected_sha256;
            let sample_valid = successful_statuses
                && result_counts_match
                && launch_count_matches
                && callback_counts_match
                && output_matches;
            all_samples_valid &= sample_valid;
            let sample = json!({
                "round": round,
                "stages": stages,
                "elapsed_ns": elapsed_ns,
                "result_records": result_records,
                "callback_records": actual_records,
                "launched_processes": launched_processes,
                "bytes": actual_bytes,
                "output_sha256": actual_sha256,
                "successful_statuses": successful_statuses,
                "result_counts_match": result_counts_match,
                "launch_count_matches": launch_count_matches,
                "callback_counts_match": callback_counts_match,
                "output_matches": output_matches,
                "sample_valid": sample_valid,
                "error": error
            });
            if round < warmups {
                warmup_samples.push(sample);
            } else {
                raw_samples.push(sample);
            }
        }
    }

    let report = json!({
        "schema": "execute.native-stream-status-polling.raw/1",
        "source_revision": source_revision,
        "test_binary": test_binary.display().to_string(),
        "test_binary_sha256": test_binary_sha256,
        "stage_binary": cat_path.display().to_string(),
        "stage_binary_sha256": cat_sha256,
        "workload": {
            "api": "runtime::run_stream",
            "stages": [1, 4, 8],
            "records": RECORDS,
            "input_bytes": expected_bytes,
            "input_sha256": expected_sha256,
            "expected_status": "all native stages exit successfully",
            "setup_outside_timing": "temporary JSONL fixture, program parsing, registry/options construction, and input/output hash setup",
            "timed_scope": "public run_stream call, native launches/pipeline/readiness/status supervision, and synchronous record callback hashing/counting"
        },
        "warmups_per_case": warmups,
        "samples_per_case": samples,
        "interleaving_stage_order_by_round": interleaving,
        "warmup_samples": warmup_samples,
        "raw_samples": raw_samples,
        "paired_plan": format!("Build baseline and candidate from the same harness bytes into fresh immutable test binaries; run {warmups} warmups and {samples} measured rounds per binary, with stage-count order rotated as recorded. Pair baseline/candidate in preregistered AB/BA order per case on the same host; retain every sample, failure, timeout, and report without retries."),
    });
    let report_bytes = serde_json::to_vec_pretty(&report).unwrap();
    report_file.write_all(&report_bytes).unwrap();
    report_file.flush().unwrap();
    report_file.get_ref().sync_all().unwrap();
    assert!(
        all_samples_valid,
        "benchmark sample oracle failed; see report"
    );
}
