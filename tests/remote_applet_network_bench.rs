//! Explicit remote-applet latency and client-resource workload.
//!
//! Build this ignored test target separately and run its prebuilt binary using
//! benchmarks/stress_evidence.py. The test performs one HTTP invocation per
//! sample and never retries an invocation.
#![cfg(target_os = "linux")]

use flow::{
    contracts::{RemoteAuthSource, RemoteEndpointConfig},
    remote::{RemoteClient, RemoteRequest, RemoteValue},
};
use serde_json::{json, Value as JsonValue};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[cfg(target_os = "linux")]
fn current_rss_bytes() -> usize {
    let status = fs::read_to_string("/proc/self/status").expect("read /proc/self/status");
    status
        .lines()
        .find_map(|line| line.strip_prefix("VmRSS:"))
        .and_then(|value| value.split_whitespace().next())
        .and_then(|value| value.parse::<usize>().ok())
        .expect("VmRSS")
        * 1024
}

#[cfg(not(target_os = "linux"))]
fn current_rss_bytes() -> usize {
    panic!("remote resource capture currently requires Linux /proc")
}

fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn env_usize(name: &str, default: usize, maximum: usize) -> usize {
    let value = std::env::var(name)
        .map(|raw| {
            raw.parse::<usize>()
                .unwrap_or_else(|_| panic!("{name} must be an integer"))
        })
        .unwrap_or(default);
    assert!(
        value > 0 && value <= maximum,
        "{name} must be in 1..={maximum}"
    );
    value
}

fn required(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} must be set"))
}

fn bool_env(name: &str) -> bool {
    matches!(std::env::var(name).as_deref(), Ok("1" | "true" | "yes"))
}

fn redacted_error(error: &flow::remote::RemoteError) -> JsonValue {
    json!({
        "kind": format!("{:?}", error.kind),
        "effect_certainty": format!("{:?}", error.effect_certainty)
    })
}

#[test]
#[ignore = "explicit network measurement; run only through benchmarks/stress_evidence.py"]
fn remote_applet_network_stress() {
    let endpoint = required("REMOTE_STRESS_ENDPOINT");
    let operation = required("REMOTE_STRESS_OPERATION");
    let effect_ack = required("REMOTE_STRESS_EFFECT_ACK");
    assert_eq!(
        effect_ack, "read_only_or_idempotent",
        "repeated remote calls require a read-only or independently idempotent operation"
    );
    let network_scope =
        std::env::var("REMOTE_STRESS_NETWORK_SCOPE").unwrap_or_else(|_| "unknown".into());
    assert!(
        matches!(
            network_scope.as_str(),
            "loopback" | "lan" | "wan" | "unknown"
        ),
        "REMOTE_STRESS_NETWORK_SCOPE must be loopback, lan, wan, or unknown"
    );
    let warmups = env_usize("REMOTE_STRESS_WARMUPS", 5, 10_000);
    let samples = env_usize("REMOTE_STRESS_SAMPLES", 40, 100_000);
    let timeout_ms = env_usize("REMOTE_STRESS_TIMEOUT_MS", 10_000, 300_000);
    let report_path = PathBuf::from(
        std::env::var_os("REMOTE_STRESS_RAW_REPORT").expect("REMOTE_STRESS_RAW_REPORT"),
    );

    let arguments: BTreeMap<String, JsonValue> = std::env::var("REMOTE_STRESS_ARGUMENTS_JSON")
        .map(|value| serde_json::from_str(&value).expect("arguments JSON is an object"))
        .unwrap_or_default();
    let expected_outputs: Option<BTreeMap<String, RemoteValue>> =
        std::env::var("REMOTE_STRESS_EXPECTED_OUTPUTS_JSON")
            .ok()
            .map(|value| {
                serde_json::from_str(&value).expect("expected outputs are a RemoteValue map")
            });
    let input: Option<RemoteValue> = std::env::var("REMOTE_STRESS_INPUT_JSON")
        .ok()
        .map(|value| serde_json::from_str(&value).expect("input is a RemoteValue"));

    let mut config = RemoteEndpointConfig::new(endpoint.clone());
    config.allow_discovery = true;
    config.allow_loopback_http_fixture = bool_env("REMOTE_STRESS_ALLOW_LOOPBACK_HTTP");
    config.timeout_ms = timeout_ms as u64;
    if let Ok(name) = std::env::var("REMOTE_STRESS_AUTH_ENV") {
        config.auth = Some(RemoteAuthSource::BearerEnv { name });
    }
    if let Ok(path) = std::env::var("REMOTE_STRESS_AUTH_FILE") {
        config.auth = Some(RemoteAuthSource::BearerFile {
            path: PathBuf::from(path),
        });
    }
    let client = RemoteClient::new(config).expect("validate endpoint configuration");

    let source = include_bytes!("remote_applet_network_bench.rs");
    let source_sha256 = hash(source);
    let endpoint_sha256 = hash(endpoint.as_bytes());
    let arguments_sha256 = hash(&serde_json::to_vec(&arguments).expect("serialize arguments"));
    let expected_outputs_sha256 = expected_outputs
        .as_ref()
        .map(|outputs| hash(&serde_json::to_vec(outputs).expect("serialize expected outputs")));
    let test_started_utc_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("wall clock")
        .as_millis();
    let before_rss = current_rss_bytes();
    let peak_rss = Arc::new(AtomicUsize::new(before_rss));
    let stop_monitor = Arc::new(AtomicBool::new(false));
    let monitor_peak = Arc::clone(&peak_rss);
    let monitor_stop = Arc::clone(&stop_monitor);
    let monitor = thread::spawn(move || {
        while !monitor_stop.load(Ordering::Acquire) {
            let rss = current_rss_bytes();
            monitor_peak.fetch_max(rss, Ordering::Relaxed);
            thread::sleep(Duration::from_millis(10));
        }
    });

    let discovery_started = Instant::now();
    let descriptor = client
        .discover()
        .expect("explicit remote descriptor discovery");
    let discovery_us = discovery_started.elapsed().as_micros() as u64;

    let mut records = Vec::with_capacity(warmups + samples);
    let mut output_mismatch_count = 0_usize;
    let mut successful_responses = 0_usize;
    let mut application_successes = 0_usize;
    for index in 0..warmups + samples {
        let mut request = RemoteRequest::new(operation.clone());
        request.arguments = arguments.clone();
        request.input = input.clone();
        let request_bytes = request
            .validate(16 * 1_048_576)
            .expect("request remains protocol-valid");
        let started = Instant::now();
        match client.invoke(&descriptor, &request) {
            Ok(response) => {
                let elapsed_us = started.elapsed().as_micros() as u64;
                let encoded = serde_json::to_vec(&response).expect("encode validated response");
                successful_responses += 1;
                let application_success = response.status == 0 && response.error.is_none();
                application_successes += usize::from(application_success);
                let outputs_match = expected_outputs
                    .as_ref()
                    .is_none_or(|expected| expected == &response.outputs);
                output_mismatch_count += usize::from(!outputs_match);
                let output_digest =
                    hash(&serde_json::to_vec(&response.outputs).expect("serialize outputs"));
                records.push(json!({
                    "phase": if index < warmups { "warmup" } else { "sample" },
                    "sample_index": index.saturating_sub(warmups),
                    "elapsed_us": elapsed_us,
                    "request_bytes": request_bytes.len(),
                    "response_bytes": encoded.len(),
                    "response_status": response.status,
                    "application_success": application_success,
                    "outputs_sha256": output_digest,
                    "expected_outputs_match": expected_outputs.as_ref().map(|_| outputs_match),
                    "remote_error_code": response.error.as_ref().map(|error| error.code.clone()),
                    "remote_error_retryable": response.error.as_ref().map(|error| error.retryable)
                }));
            }
            Err(error) => {
                let elapsed_us = started.elapsed().as_micros() as u64;
                records.push(json!({
                    "phase": if index < warmups { "warmup" } else { "sample" },
                    "sample_index": index.saturating_sub(warmups),
                    "elapsed_us": elapsed_us,
                    "request_bytes": request_bytes.len(),
                    "response_bytes": null,
                    "response_status": null,
                    "application_success": false,
                    "outputs_sha256": null,
                    "expected_outputs_match": null,
                    "transport_error": redacted_error(&error)
                }));
            }
        }
    }
    stop_monitor.store(true, Ordering::Release);
    monitor.join().expect("RSS monitor joins");
    let after_rss = current_rss_bytes();
    let measured = records
        .iter()
        .filter(|record| record["phase"] == "sample")
        .cloned()
        .collect::<Vec<_>>();
    let sample_latencies = measured
        .iter()
        .filter_map(|record| record["elapsed_us"].as_u64())
        .collect::<Vec<_>>();
    let errors = measured
        .iter()
        .filter(|record| record["transport_error"].is_object())
        .count();
    let report = json!({
        "schema": "execute.remote-applet-network.raw/1",
        "trial_type": "measurement",
        "claim": "none",
        "platform_gate": "linux_only",
        "test_started_utc_ms": test_started_utc_ms,
        "source_sha256": source_sha256,
        "endpoint_sha256": endpoint_sha256,
        "network_scope_operator_declared": network_scope,
        "operation": operation,
        "arguments_sha256": arguments_sha256,
        "expected_outputs_sha256": expected_outputs_sha256,
        "auth_configured": std::env::var_os("REMOTE_STRESS_AUTH_ENV").is_some() || std::env::var_os("REMOTE_STRESS_AUTH_FILE").is_some(),
        "loopback_http_fixture_allowed": bool_env("REMOTE_STRESS_ALLOW_LOOPBACK_HTTP"),
        "descriptor": {
            "protocol_version": descriptor.protocol_version,
            "identity": descriptor.identity,
            "contract_sha256": hash(&serde_json::to_vec(&descriptor.contract).expect("serialize descriptor contract"))
        },
        "iterations": {
            "warmups": warmups,
            "samples": samples,
            "total_invocation_attempts": warmups + samples,
            "automatic_retries": 0,
            "per_request_timeout_ms": timeout_ms
        },
        "discovery_us": discovery_us,
        "resources": {
            "client_rss_before_bytes": before_rss,
            "client_rss_peak_sampled_bytes": peak_rss.load(Ordering::Relaxed),
            "client_rss_end_bytes": after_rss,
            "client_rss_peak_sampled_delta_bytes": peak_rss.load(Ordering::Relaxed).saturating_sub(before_rss),
            "remote_server_resource_metrics": "not_available_from_client"
        },
        "correctness": {
            "valid_protocol_responses": successful_responses,
            "application_success_responses": application_successes,
            "sample_transport_errors": errors,
            "expected_output_oracle_configured": expected_outputs.is_some(),
            "expected_output_mismatches": output_mismatch_count,
            "status": if expected_outputs.is_some() && output_mismatch_count == 0 && errors == 0 { "exact_outputs_match" } else if expected_outputs.is_some() { "oracle_failure_or_transport_error" } else { "protocol_only_no_output_oracle" }
        },
        "raw_sample_latencies_us": sample_latencies,
        "sample_response_bytes": measured.iter().map(|record| record["response_bytes"].clone()).collect::<Vec<_>>(),
        "requests": records,
        "limitations": [
            "One request is sent per measured invocation, with no automatic retry; transport failures may follow an uncertain remote effect.",
            "The endpoint/network scope is operator-declared and is not independently verified as a different machine.",
            "Client RSS includes this test process and the remote client; remote server RSS, power, and process-tree resources are not measured.",
            "An output oracle is optional; without it this measures a valid protocol exchange, not functional correctness.",
            "The harness performs explicit descriptor discovery before warmups and does not measure graph integration or streaming/event latency."
        ]
    });
    fs::write(
        &report_path,
        serde_json::to_vec_pretty(&report).expect("encode report"),
    )
    .expect("write raw report");
}
