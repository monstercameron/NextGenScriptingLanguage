//! Explicit applet-boundary benchmark. Run the prebuilt release test binary
//! through benchmarks/applet_boundaries.py; this test never compiles itself.
#![cfg(unix)]

use flow::{
    ast::Value,
    manifest::Registry,
    parser,
    runtime::{self, RunOptions},
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value as JsonValue};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    hint::black_box,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

const WORKLOAD_TEXT: &str = "TODO: move TODO and TODO\n";
const NEEDLE: &str = "TODO";
const SUCCESS_PROGRAM: &str = "accept text as text\naccept needle as text\nrun bench.matches with text text and needle needle as result and keep status as code\nif code is 0\n  then encode result as json\notherwise\n  run \"false\"\n";
const ABSENCE_PROGRAM: &str = "accept path as text\nrun bench.snapshot with path path as result and keep status as code\ntake text at \"/outcome\" from result as outcome\nif code is 1\n  if outcome is \"absence\"\n    then encode result as json\n  otherwise\n    run \"false\"\notherwise\n  run \"false\"\n";

#[derive(Serialize)]
struct WorkerRequest {
    id: u64,
    arguments: Vec<String>,
    input: Vec<u8>,
}

#[derive(Deserialize)]
struct WorkerResponse {
    id: u64,
    status: i32,
    output: Vec<u8>,
    #[serde(default)]
    stderr: Option<String>,
}

#[derive(Serialize)]
struct WorkerResponseOut {
    id: u64,
    status: i32,
    output: Vec<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stderr: Option<String>,
}

fn output_schema() -> JsonValue {
    json!({
        "type":"record",
        "encoding":"json",
        "fields": {
            "matches":{"type":"boolean"},
            "count":{"type":"integer"},
            "byte_offsets":{"type":"json"}
        },
        "required":["matches","count","byte_offsets"]
    })
}

fn success_manifest(backend: &str, helper: &Path) -> JsonValue {
    let binary = if backend == "in_process" {
        "@execute-applet".to_owned()
    } else {
        helper.to_string_lossy().into_owned()
    };
    let mut manifest = json!({
        "name":"bench.matches",
        "binary":binary,
        "parameters":{
            "text":{"type":"string","required":true},
            "needle":{"type":"string","required":true}
        },
        "arguments":[
            {"literal":"text.matches"},
            {"literal":"--args"},
            {"json_arguments":true}
        ],
        "contract":{
            "version":1,
            "backend":backend,
            "output":output_schema()
        }
    });
    if backend == "json_lines_worker" {
        manifest["contract"]["lifecycle"] = json!({"kind":"json_lines"});
    }
    manifest
}

fn envelope_manifest(backend: &str, helper: &Path) -> JsonValue {
    let binary = if backend == "in_process" {
        "@execute-applet".to_owned()
    } else {
        helper.to_string_lossy().into_owned()
    };
    let mut manifest = json!({
        "name":"bench.snapshot",
        "binary":binary,
        "parameters":{"path":{"type":"path","required":true}},
        "arguments":[
            {"literal":"fs.read"},
            {"literal":"--args"},
            {"json_arguments":true}
        ],
        "contract":{
            "version":1,
            "backend":backend,
            "output":{"type":"json","encoding":"json"},
            "outcomes":[
                {"status":0,"name":"present","kind":"success"},
                {"status":1,"name":"absence","kind":"absence"},
                {"status":2,"name":"invalid","kind":"failure"},
                {"status":75,"name":"retryable","kind":"retryable"}
            ],
            "result_envelope":true
        }
    });
    if backend == "json_lines_worker" {
        manifest["contract"]["lifecycle"] = json!({"kind":"json_lines"});
    }
    manifest
}

fn registry(manifest: JsonValue) -> Registry {
    Registry::from_sources(&[manifest.to_string()]).expect("benchmark manifest validates")
}

fn options(cwd: &Path, inputs: BTreeMap<String, Value>) -> RunOptions {
    RunOptions {
        inputs,
        max_output_bytes: 1024 * 1024,
        command_timeout: Some(Duration::from_secs(30)),
        ..RunOptions::new(cwd.to_path_buf())
    }
}

fn expected_success_output() -> Vec<u8> {
    serde_json::to_vec(&json!({
        "byte_offsets":[0,11,20],
        "count":3,
        "matches":true
    }))
    .unwrap()
}

fn run_checked(
    program: &flow::ast::Program,
    registry: &Registry,
    options: &RunOptions,
    expected: &[u8],
    backend: &str,
    expected_launches: Option<usize>,
) -> runtime::RunResult {
    let result = runtime::run(program, registry, options)
        .unwrap_or_else(|error| panic!("{backend} benchmark invocation failed: {error:#}"));
    assert_eq!(
        result.output, expected,
        "{backend} exact typed output bytes"
    );
    if let Some(expected) = expected_launches {
        assert_eq!(
            result.launched_processes, expected,
            "{backend} process launches"
        );
    }
    result
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[derive(Serialize, Deserialize)]
struct WorkerResponseFrame {
    id: u64,
    status: i32,
    output: Vec<u8>,
    #[serde(default)]
    stderr: Option<String>,
}

fn frame_samples(expected_stdout: &[u8], samples: usize) -> JsonValue {
    const BATCH: usize = 100;
    let request = WorkerRequest {
        id: 7,
        arguments: vec![
            "text.matches".into(),
            "--args".into(),
            json!({"needle":NEEDLE,"text":WORKLOAD_TEXT}).to_string(),
        ],
        input: Vec::new(),
    };
    let mut request_bytes = serde_json::to_vec(&request).unwrap();
    request_bytes.push(b'\n');
    let response = WorkerResponseOut {
        id: 7,
        status: 0,
        output: [expected_stdout, b"\n"].concat(),
        stderr: None,
    };
    let mut response_bytes = serde_json::to_vec(&response).unwrap();
    response_bytes.push(b'\n');
    let response_line = response_bytes.strip_suffix(b"\n").unwrap();
    let decoded: WorkerResponse = serde_json::from_slice(response_line).unwrap();
    assert_eq!(decoded.id, 7);
    assert_eq!(decoded.status, 0);
    assert_eq!(decoded.output, [expected_stdout, b"\n"].concat());
    assert!(decoded.stderr.is_none());

    let mut encode_ns = Vec::with_capacity(samples);
    let mut decode_ns = Vec::with_capacity(samples);
    for batch in 0..samples {
        let started = Instant::now();
        for id in 0..BATCH {
            let mut frame = serde_json::to_vec(black_box(&request)).unwrap();
            frame.push(b'\n');
            black_box(frame);
            black_box(id);
        }
        encode_ns.push(started.elapsed().as_nanos() as f64 / BATCH as f64);

        let started = Instant::now();
        for _ in 0..BATCH {
            let parsed: WorkerResponseFrame =
                serde_json::from_slice(black_box(response_line)).unwrap();
            black_box((parsed.id, parsed.status, parsed.output.len(), parsed.stderr));
        }
        decode_ns.push(started.elapsed().as_nanos() as f64 / BATCH as f64);
        black_box(batch);
    }
    json!({
        "request_frame_bytes_including_lf":request_bytes.len(),
        "response_frame_bytes_including_lf":response_bytes.len(),
        "operations_per_sample":BATCH,
        "request_encode_samples_ns":encode_ns,
        "response_decode_samples_ns":decode_ns,
        "scope":"serde_json frame encode/decode only; excludes channel I/O, applet execution and runtime validation"
    })
}

#[cfg(unix)]
#[test]
#[ignore = "explicit release applet-boundary measurements; run through the Python harness"]
fn applet_boundaries() {
    let helper = PathBuf::from(std::env::var_os("APPLET_BOUNDARY_HELPER").expect("helper path"));
    let raw_report =
        PathBuf::from(std::env::var_os("APPLET_BOUNDARY_RAW_REPORT").expect("raw report path"));
    let samples: usize = std::env::var("APPLET_BOUNDARY_SAMPLES")
        .unwrap()
        .parse()
        .unwrap();
    let warmups: usize = std::env::var("APPLET_BOUNDARY_WARMUPS")
        .unwrap()
        .parse()
        .unwrap();
    assert!(samples > 0 && warmups > 0);

    let cwd = tempfile::tempdir().expect("benchmark cwd");
    let success_inputs = BTreeMap::from([
        ("text".into(), Value::String(WORKLOAD_TEXT.into())),
        ("needle".into(), Value::String(NEEDLE.into())),
    ]);
    let success_options = options(cwd.path(), success_inputs);
    let success_program = parser::parse(SUCCESS_PROGRAM).expect("success benchmark program parses");
    let expected_stdout = expected_success_output();
    let expected_value: JsonValue = serde_json::from_slice(&expected_stdout).unwrap();
    let modes = ["command", "in_process", "json_lines_worker"];
    let mut registries = BTreeMap::new();
    let mut manifest_hashes = BTreeMap::new();
    for mode in modes {
        let manifest = success_manifest(mode, &helper);
        manifest_hashes.insert(mode, sha256(manifest.to_string().as_bytes()));
        let registry = registry(manifest);
        runtime::validate(&success_program, &registry)
            .unwrap_or_else(|error| panic!("{mode} success program fails preflight: {error:#}"));
        registries.insert(mode, registry);
    }

    // Correctness comes first. This also establishes each backend's expected
    // first-call launch count before any timings are collected.
    let mut launches_total = BTreeMap::new();
    for mode in modes {
        let expected_first_launches = usize::from(mode != "in_process");
        let result = run_checked(
            &success_program,
            &registries[mode],
            &success_options,
            &expected_stdout,
            mode,
            Some(expected_first_launches),
        );
        launches_total.insert(mode, result.launched_processes);
    }

    // Explicitly exercise non-success routing with a typed result envelope on
    // every backend. The status check and outcome branch must both succeed.
    let missing = cwd.path().join("intentionally-absent.json");
    assert!(!missing.exists());
    let absence_program = parser::parse(ABSENCE_PROGRAM).expect("absence program parses");
    let absence_inputs = BTreeMap::from([(
        "path".into(),
        Value::String(missing.to_string_lossy().into_owned()),
    )]);
    let absence_options = options(cwd.path(), absence_inputs);
    let expected_absence = serde_json::to_vec(&json!({
        "effect_certainty":"known_response",
        "kind":"absence",
        "outcome":"absence",
        "status":1,
        "value":null
    }))
    .unwrap();
    let mut envelope_hashes = BTreeMap::new();
    let mut envelope_launches = BTreeMap::new();
    for mode in modes {
        let manifest = envelope_manifest(mode, &helper);
        envelope_hashes.insert(mode, sha256(manifest.to_string().as_bytes()));
        let registry = registry(manifest);
        runtime::validate(&absence_program, &registry)
            .unwrap_or_else(|error| panic!("{mode} absence program fails preflight: {error:#}"));
        let expected_launches = usize::from(mode != "in_process");
        let result = run_checked(
            &absence_program,
            &registry,
            &absence_options,
            &expected_absence,
            mode,
            Some(expected_launches),
        );
        envelope_launches.insert(mode, result.launched_processes);
        // Drop the correctness worker before creating the timed worker registry;
        // correctness/setup work is separate from the timing pool.
    }

    let mut timed_registries = BTreeMap::new();
    for mode in modes {
        timed_registries.insert(mode, registry(success_manifest(mode, &helper)));
    }
    let source_hash = sha256(SUCCESS_PROGRAM.as_bytes());
    let mut raw_samples: BTreeMap<&str, Vec<u128>> = modes
        .into_iter()
        .map(|mode| (mode, Vec::with_capacity(samples)))
        .collect();
    let mut timed_launches: BTreeMap<&str, usize> =
        modes.into_iter().map(|mode| (mode, 0)).collect();
    let mut interleaving = Vec::with_capacity(warmups + samples);

    for round in 0..(warmups + samples) {
        let rotation = round % modes.len();
        let order = (0..modes.len())
            .map(|offset| modes[(rotation + offset) % modes.len()])
            .collect::<Vec<_>>();
        interleaving.push(order.clone());
        for mode in order {
            let started = Instant::now();
            let result = runtime::run(&success_program, &timed_registries[mode], &success_options)
                .unwrap_or_else(|error| panic!("{mode} timed invocation failed: {error:#}"));
            let elapsed = started.elapsed().as_nanos();
            assert_eq!(
                result.output, expected_stdout,
                "{mode} output changed during timing"
            );
            let timed_mode_launches = timed_launches.get_mut(mode).unwrap();
            *timed_mode_launches += result.launched_processes;
            if round >= warmups {
                raw_samples.get_mut(mode).unwrap().push(elapsed);
            }
        }
    }
    for mode in modes {
        let timed_calls = warmups + samples;
        let measured_runs = raw_samples[mode].len();
        assert_eq!(measured_runs, samples, "{mode} sample count");
        if mode == "command" {
            assert_eq!(timed_launches[mode], timed_calls);
        } else if mode == "in_process" {
            assert_eq!(timed_launches[mode], 0);
        } else {
            assert_eq!(
                timed_launches[mode], 1,
                "one helper launch per persistent registry"
            );
        }
    }

    let backend_samples = modes
        .into_iter()
        .map(|mode| {
            (
                mode.to_owned(),
                json!({
                    "samples_ns":raw_samples[mode],
                    "launches_timed_calls_total":timed_launches[mode],
                    "timed_calls_including_warmups":warmups + samples,
                    "launches_in_timed_sample_window":if mode == "command" { samples } else { 0 },
                    "successful_output_sha256":sha256(&expected_stdout)
                }),
            )
        })
        .collect::<serde_json::Map<_, _>>();

    let success_oracle_bytes = expected_stdout.clone();
    let report = json!({
        "schema":"execute.applet-boundaries.raw/1",
        "workload":{
            "operation":"text.matches",
            "text":WORKLOAD_TEXT,
            "needle":NEEDLE,
            "program_source_sha256":source_hash,
            "expected_typed_value":expected_value,
            "expected_final_output_hex":success_oracle_bytes.iter().map(|byte| format!("{byte:02x}")).collect::<String>(),
            "expected_status":0
        },
        "correctness":{
            "success_backend_checks":modes,
            "absence_envelope_backends":modes,
            "absence_envelope_expected_status":1,
            "absence_envelope_expected_value":{"value":null,"outcome":"absence","kind":"absence","effect_certainty":"known_response","status":1},
            "absence_envelope_output_sha256":sha256(&expected_absence)
        },
        "iterations":{"warmups":warmups,"samples":samples,"interleaved_order":interleaving},
        "manifest_sha256":manifest_hashes,
        "envelope_manifest_sha256":envelope_hashes,
        "launches":{
            "success_correctness_call_by_backend":launches_total,
            "absence_envelope_correctness_call_by_backend":envelope_launches,
            "timed_calls_and_worker_launches_by_backend":timed_launches
        },
        "backends":backend_samples,
        "worker_serialization":frame_samples(&expected_stdout, samples),
        "timing_scope":"release runtime::run API, one parsed program and one Registry per backend; includes runtime validation, argument serialization, backend boundary, typed output decoding and result encoding; excludes CLI process startup",
        "runtime_test_binary":std::env::current_exe().unwrap().display().to_string(),
        "helper_binary":helper.display().to_string()
    });
    fs::write(&raw_report, serde_json::to_vec_pretty(&report).unwrap())
        .unwrap_or_else(|error| panic!("writing {}: {error}", raw_report.display()));
}
