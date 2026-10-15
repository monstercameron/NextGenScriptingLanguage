#!/usr/bin/env python3
"""Run prebuilt graph/remote stress probes and retain scoped raw evidence.

This wrapper never invokes Cargo or compiles. Use only after a separate final
release build and with no concurrent build/test load.
"""

from __future__ import annotations

import argparse
from datetime import datetime, timezone
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import shutil
import statistics
import subprocess
import sys
import tempfile


ROOT = Path(__file__).resolve().parents[1]
BENCHMARKS = ROOT / "benchmarks"
SAFE_ENV = (
    "LANG",
    "LC_ALL",
    "RUSTFLAGS",
    "RUSTUP_TOOLCHAIN",
    "CARGO_BUILD_TARGET",
    "CARGO_PROFILE_RELEASE_OPT_LEVEL",
    "CARGO_PROFILE_RELEASE_CODEGEN_UNITS",
    "CARGO_PROFILE_RELEASE_LTO",
    "TZ",
)


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for block in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def validate_report_path(path: Path, prefix: str, benchmark_directory: Path = BENCHMARKS) -> Path:
    directory = benchmark_directory.resolve()
    candidate = path if path.is_absolute() else Path.cwd() / path
    candidate = candidate.absolute()
    if candidate.parent.resolve() != directory:
        raise ValueError(f"report must be saved directly under {directory}")
    if not candidate.name.startswith(prefix) or not candidate.name.endswith(".json"):
        raise ValueError(f"report name must match {prefix}*.json")
    if candidate.exists() or candidate.is_symlink():
        raise FileExistsError(f"refusing to overwrite report: {candidate}")
    return candidate


def _verify_binary(path: Path) -> Path:
    resolved = path.expanduser().resolve(strict=True)
    if not resolved.is_file() or not os.access(resolved, os.X_OK):
        raise ValueError(f"prebuilt Rust test binary must be executable: {resolved}")
    return resolved


def _source_files(kind: str) -> list[Path]:
    files = [ROOT / "Cargo.toml", ROOT / "Cargo.lock"]
    files.extend((ROOT / "src").rglob("*.rs"))
    if (ROOT / "build.rs").is_file():
        files.append(ROOT / "build.rs")
    files.append(ROOT / "tests" / ("graph_lifecycle_bench.rs" if kind == "graph" else "remote_applet_network_bench.rs"))
    files.append(ROOT / "benchmarks" / "stress_evidence.py")
    return sorted(path for path in files if path.is_file())


def source_hashes(kind: str) -> dict[str, str]:
    return {path.relative_to(ROOT).as_posix(): sha256_file(path) for path in _source_files(kind)}


def _version(command: str) -> str | None:
    executable = shutil.which(command)
    if executable is None:
        return None
    try:
        completed = subprocess.run(
            [executable, "--version"],
            check=False,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            timeout=10,
        )
    except (OSError, subprocess.TimeoutExpired):
        return None
    return completed.stdout.strip() if completed.returncode == 0 else None


def environment_record() -> dict[str, object]:
    selected = {name: os.environ.get(name) for name in SAFE_ENV}
    canonical = json.dumps(selected, sort_keys=True, separators=(",", ":")).encode()
    return {
        "os": platform.system(),
        "os_release": platform.release(),
        "platform": platform.platform(),
        "architecture": platform.machine(),
        "cpu_model": platform.processor() or None,
        "logical_cpu_count": os.cpu_count(),
        "python": sys.version.splitlines()[0],
        "rustc": _version("rustc"),
        "cargo": _version("cargo"),
        "selected_build_environment": selected,
        "selected_environment_sha256": hashlib.sha256(canonical).hexdigest(),
        "path_sha256": hashlib.sha256(os.environ.get("PATH", "").encode()).hexdigest(),
    }


def _summary(values: list[int | float]) -> dict[str, int | float]:
    if not values:
        raise ValueError("cannot summarize an empty measurement")
    ordered = sorted(values)
    return {
        "sample_count": len(values),
        "median": statistics.median(values),
        "p95_nearest_rank": ordered[math.ceil(0.95 * len(ordered)) - 1],
        "minimum": min(values),
        "maximum": max(values),
    }


def _validate_graph(raw: dict[str, object], args: argparse.Namespace) -> dict[str, object]:
    if raw.get("schema") != "execute.graph-lifecycle.raw/1" or raw.get("trial_type") != "measurement":
        raise ValueError("graph probe returned an unknown raw schema")
    parameters = raw.get("parameters")
    if not isinstance(parameters, dict):
        raise ValueError("graph probe omitted parameters")
    expected = {
        "timers_per_generation": args.nodes,
        "reconfigurations": args.reconfigurations,
        "outputs_consumed_per_generation": args.outputs,
        "idle_window_ms": args.idle_ms,
        "timer_interval_ms": args.interval_ms,
        "idle_timer_interval_ms": args.idle_interval_ms or max(args.idle_ms * 4, 10_000),
    }
    for key, value in expected.items():
        if parameters.get(key) != value:
            raise ValueError(f"graph probe returned unexpected {key}: {parameters.get(key)!r}")
    correctness = raw.get("correctness")
    if not isinstance(correctness, dict):
        raise ValueError("graph probe omitted correctness checks")
    for key in ("all_expected_generations_observed", "threads_returned_to_baseline"):
        if correctness.get(key) is not True:
            raise ValueError(f"graph probe did not pass {key}")
    if correctness.get("stale_outputs_observed") is not False:
        raise ValueError("graph probe observed a stale generation/revision output")
    if correctness.get("idle_source_events") != 0:
        raise ValueError("graph probe observed an event during the idle interval")
    if correctness.get("worker_failure_injection") != "surfaced_and_joined":
        raise ValueError("graph probe did not surface and join its injected worker failure")
    if correctness.get("total_terminal_outputs_consumed") != (args.reconfigurations + 1) * args.outputs:
        raise ValueError("graph probe did not consume the expected output count")
    events_by_generation = correctness.get("events_by_generation")
    if not isinstance(events_by_generation, dict):
        raise ValueError("graph probe omitted per-generation event counts")
    for generation in range(1, args.reconfigurations + 2):
        counts = events_by_generation.get(str(generation))
        if not isinstance(counts, dict) or counts.get("received", 0) < args.outputs or counts.get("unique_timer_sources", 0) < 1:
            raise ValueError(f"graph generation {generation} did not receive enough timer events")
    timings = raw.get("timings_us")
    if not isinstance(timings, dict):
        raise ValueError("graph probe omitted raw timings")
    generation_windows = timings.get("generation_output_windows")
    if not isinstance(generation_windows, list) or len(generation_windows) != args.reconfigurations + 1:
        raise ValueError("graph probe did not return output windows for every generation")
    if any(type(value) is not int or value < 0 for value in generation_windows):
        raise ValueError("graph probe returned invalid generation timing")
    return {
        "generations": _summary(generation_windows),
        "reconfiguration_inclusive_us": timings,
        "resources": raw.get("resources"),
        "correctness": correctness,
    }


def _validate_remote(raw: dict[str, object], args: argparse.Namespace) -> dict[str, object]:
    if raw.get("schema") != "execute.remote-applet-network.raw/1" or raw.get("trial_type") != "measurement":
        raise ValueError("remote probe returned an unknown raw schema")
    iterations = raw.get("iterations")
    if not isinstance(iterations, dict) or iterations.get("warmups") != args.warmups or iterations.get("samples") != args.samples:
        raise ValueError("remote probe returned unexpected sample counts")
    requests = raw.get("requests")
    if not isinstance(requests, list) or len(requests) != args.warmups + args.samples:
        raise ValueError("remote probe did not preserve every invocation attempt")
    if iterations.get("per_request_timeout_ms") != args.timeout_ms or iterations.get("automatic_retries") != 0:
        raise ValueError("remote probe changed the per-request timeout or retry policy")
    measured = [item for item in requests if isinstance(item, dict) and item.get("phase") == "sample"]
    if len(measured) != args.samples:
        raise ValueError("remote probe returned the wrong measured sample count")
    latency = raw.get("raw_sample_latencies_us")
    if not isinstance(latency, list) or any(type(value) is not int or value < 0 for value in latency):
        raise ValueError("remote probe returned invalid latencies")
    correctness = raw.get("correctness")
    if not isinstance(correctness, dict):
        raise ValueError("remote probe omitted correctness summary")
    all_latencies = [
        item["elapsed_us"]
        for item in measured
        if isinstance(item.get("elapsed_us"), int) and not isinstance(item.get("elapsed_us"), bool)
    ]
    successful_latencies = [
        item["elapsed_us"]
        for item in measured
        if isinstance(item.get("elapsed_us"), int)
        and not isinstance(item.get("elapsed_us"), bool)
        and "transport_error" not in item
    ]
    if len(all_latencies) != args.samples or latency != all_latencies:
        raise ValueError("remote probe did not report one raw latency per measured attempt")
    return {
        "network_scope_operator_declared": raw.get("network_scope_operator_declared"),
        "samples_recorded": args.samples,
        "all_attempt_latency_us": _summary(all_latencies) if all_latencies else None,
        "protocol_response_latency_us": _summary(successful_latencies) if successful_latencies else None,
        "correctness": correctness,
        "client_resources": raw.get("resources"),
        "remote_service_resources": "not_measured",
    }


def run_probe(args: argparse.Namespace) -> dict[str, object]:
    if not args.final_release_build_confirmed:
        raise ValueError("pass --final-release-build-confirmed only after the final release build is ready")
    if platform.system() != "Linux":
        raise ValueError("the graph/remote resource workloads require a Linux runner; macOS evidence does not certify Arch/Omarchy")
    if not 1 <= args.overall_timeout_s <= 86_400:
        raise ValueError("--overall-timeout-s must be in 1..86400")
    if args.kind == "graph":
        if not 1000 <= args.nodes <= 4096:
            raise ValueError("--nodes must be in 1000..4096")
        if not 1 <= args.reconfigurations <= 100 or not 1 <= args.outputs <= 4096:
            raise ValueError("reconfigurations and outputs exceed the supported stress bounds")
        if not 10 <= args.idle_ms <= 10_000 or not 1 <= args.interval_ms <= 60_000:
            raise ValueError("idle and active intervals exceed the supported stress bounds")
        if args.idle_interval_ms is not None and not args.idle_ms < args.idle_interval_ms <= 300_000:
            raise ValueError("--idle-interval-ms must be greater than idle-ms and at most 300000")
    elif not 1 <= args.warmups <= 10_000 or not 1 <= args.samples <= 1_000:
        raise ValueError("remote warmups/samples exceed supported bounds (1..10000 / 1..1000)")
    elif not 1 <= args.timeout_ms <= 300_000:
        raise ValueError("remote timeouts exceed supported bounds")
    binary = _verify_binary(args.test_binary)
    kind = args.kind
    prefix = "graph-lifecycle-" if kind == "graph" else "remote-applet-network-"
    output_path = validate_report_path(args.report, prefix)
    if kind == "remote" and not args.read_only_or_idempotent_confirmed:
        raise ValueError("remote samples repeat the operation; confirm it is read-only or independently idempotent")
    env = os.environ.copy()
    with tempfile.TemporaryDirectory(prefix=f"execute-{kind}-stress-") as directory:
        raw_path = Path(directory) / "raw.json"
        if kind == "graph":
            env.update({
                "GRAPH_STRESS_RAW_REPORT": str(raw_path),
                "GRAPH_STRESS_NODES": str(args.nodes),
                "GRAPH_STRESS_RECONFIGURATIONS": str(args.reconfigurations),
                "GRAPH_STRESS_OUTPUTS": str(args.outputs),
                "GRAPH_STRESS_IDLE_MS": str(args.idle_ms),
                "GRAPH_STRESS_INTERVAL_MS": str(args.interval_ms),
            })
            if args.idle_interval_ms is not None:
                env["GRAPH_STRESS_IDLE_INTERVAL_MS"] = str(args.idle_interval_ms)
            test_name = "graph_lifecycle_stress"
        else:
            if not env.get("REMOTE_STRESS_ENDPOINT") or not env.get("REMOTE_STRESS_OPERATION"):
                raise ValueError("set REMOTE_STRESS_ENDPOINT and REMOTE_STRESS_OPERATION in the environment")
            env.update({
                "REMOTE_STRESS_RAW_REPORT": str(raw_path),
                "REMOTE_STRESS_WARMUPS": str(args.warmups),
                "REMOTE_STRESS_SAMPLES": str(args.samples),
                "REMOTE_STRESS_TIMEOUT_MS": str(args.timeout_ms),
                "REMOTE_STRESS_NETWORK_SCOPE": args.network_scope,
                "REMOTE_STRESS_EFFECT_ACK": "read_only_or_idempotent",
            })
            if args.allow_loopback_http_fixture:
                env["REMOTE_STRESS_ALLOW_LOOPBACK_HTTP"] = "1"
            if args.arguments_json:
                env["REMOTE_STRESS_ARGUMENTS_JSON"] = args.arguments_json
            if args.input_json:
                env["REMOTE_STRESS_INPUT_JSON"] = args.input_json
            if args.expected_outputs_json:
                env["REMOTE_STRESS_EXPECTED_OUTPUTS_JSON"] = args.expected_outputs_json
            test_name = "remote_applet_network_stress"
        command = [
            str(binary),
            test_name,
            "--ignored",
            "--exact",
            "--nocapture",
        ]
        try:
            completed = subprocess.run(
                command,
                check=False,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                text=True,
                env=env,
                timeout=args.overall_timeout_s,
            )
        except subprocess.TimeoutExpired as error:
            raise RuntimeError(f"{kind} stress test exceeded {args.overall_timeout_s}s; no report imported") from error
        if completed.returncode != 0:
            raise RuntimeError(
                f"{kind} stress test exited {completed.returncode}; no report imported\n{completed.stdout[-8000:]}"
            )
        if not raw_path.is_file():
            raise RuntimeError(f"{kind} stress test did not create its raw report")
        raw = json.loads(raw_path.read_text())
        report_summary = _validate_graph(raw, args) if kind == "graph" else _validate_remote(raw, args)
        sources = source_hashes(kind)
        report = {
            "schema": f"execute.{kind.replace('_', '-')}-stress/1",
            "status": "measurement_recorded_no_claim",
            "claim": "none",
            "created_utc": datetime.now(timezone.utc).isoformat(),
            "kind": kind,
            "test_binary_sha256": sha256_file(binary),
            "source_files_sha256": sources,
            "environment": environment_record(),
            "summary": report_summary,
            "raw": raw,
            "measurement_scope": (
                "Linux process-local graph timers, typed queues, reconfiguration and worker-failure join behavior."
                if kind == "graph"
                else "One explicitly configured remote applet endpoint, measured from this Linux client; server resources are not measured."
            ),
        }
        output_path.write_text(json.dumps(report, sort_keys=True, indent=2) + "\n")
        return report


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="kind", required=True)
    graph = subparsers.add_parser("graph", help="thousands of local timer nodes, queues, idle period, reconfiguration and injected worker failure")
    graph.add_argument("--test-binary", required=True, type=Path)
    graph.add_argument("--report", required=True, type=Path)
    graph.add_argument("--final-release-build-confirmed", action="store_true")
    graph.add_argument("--nodes", type=int, default=2048)
    graph.add_argument("--reconfigurations", type=int, default=4)
    graph.add_argument("--outputs", type=int, default=64)
    graph.add_argument("--idle-ms", type=int, default=250)
    graph.add_argument("--idle-interval-ms", type=int)
    graph.add_argument("--interval-ms", type=int, default=5)
    graph.add_argument("--overall-timeout-s", type=int, default=3600)
    remote = subparsers.add_parser("remote", help="remote protocol latency and local client resource workload")
    remote.add_argument("--test-binary", required=True, type=Path)
    remote.add_argument("--report", required=True, type=Path)
    remote.add_argument("--final-release-build-confirmed", action="store_true")
    remote.add_argument("--read-only-or-idempotent-confirmed", action="store_true")
    remote.add_argument("--allow-loopback-http-fixture", action="store_true")
    remote.add_argument("--network-scope", choices=("loopback", "lan", "wan", "unknown"), required=True)
    remote.add_argument("--warmups", type=int, default=5)
    remote.add_argument("--samples", type=int, default=40)
    remote.add_argument("--timeout-ms", type=int, default=10_000, help="maximum time for each discovery/request, 1..300000")
    remote.add_argument("--overall-timeout-s", type=int, default=3600, help="stop the whole probe after this many seconds")
    remote.add_argument("--arguments-json", help="JSON object passed to the remote operation")
    remote.add_argument("--input-json", help="RemoteValue JSON input, if the contract requires it")
    remote.add_argument("--expected-outputs-json", help="Optional exact JSON map of output RemoteValues")
    args = parser.parse_args()
    try:
        run_probe(args)
    except (ValueError, OSError, RuntimeError, json.JSONDecodeError) as error:
        parser.error(str(error))
    print(f"saved {args.kind} stress report: {args.report}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
