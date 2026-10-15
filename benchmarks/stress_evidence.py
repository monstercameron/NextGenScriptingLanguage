#!/usr/bin/env python3
"""Run prebuilt graph, mixed-runtime and remote stress probes with scoped evidence.

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
import signal
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
    test_name = {
        "graph": "graph_lifecycle_bench.rs",
        "mixed": "graph_mixed_lifecycle_bench.rs",
        "remote": "remote_applet_network_bench.rs",
    }[kind]
    files.append(ROOT / "tests" / test_name)
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


def _mixed_actor_layout(total: int) -> dict[str, int]:
    def scaled(ceiling: int) -> int:
        return min(ceiling, max(1, (total * ceiling + 999) // 1000))

    native = scaled(64)
    workers = scaled(16)
    in_process = scaled(32)
    services = scaled(32)
    remote = scaled(8)
    collector = 1
    timers = total - native - workers - in_process - services - remote - collector
    return {
        "timers": timers,
        "native_finite_applets": native,
        "distinct_registry_workers": workers,
        "in_process_applets": in_process,
        "scheduled_program_services": services,
        "loopback_remote_listeners": remote,
        "timer_collector": collector,
        "total_graph_actors": total,
    }


def _validate_mixed(raw: dict[str, object], args: argparse.Namespace) -> dict[str, object]:
    if raw.get("schema") != "execute.graph-mixed-lifecycle.raw/1" or raw.get("trial_type") != "measurement":
        raise ValueError("mixed graph probe returned an unknown raw schema")
    if raw.get("status") != "passed":
        raise ValueError(f"mixed graph probe did not pass: {raw.get('status')!r}")
    parameters = raw.get("parameters")
    if not isinstance(parameters, dict):
        raise ValueError("mixed graph probe omitted parameters")
    if parameters.get("total_graph_actors") != args.nodes:
        raise ValueError("mixed graph probe returned an unexpected actor count")
    if parameters.get("active_window_ms") != args.window_ms:
        raise ValueError("mixed graph probe returned an unexpected active window")
    layout = raw.get("actor_layout")
    expected_layout = _mixed_actor_layout(args.nodes)
    if not isinstance(layout, dict):
        raise ValueError("mixed graph probe omitted its actor layout")
    for key, value in expected_layout.items():
        if layout.get(key) != value:
            raise ValueError(f"mixed graph probe returned unexpected {key}: {layout.get(key)!r}")
    graph = raw.get("graph")
    if not isinstance(graph, dict):
        raise ValueError("mixed graph probe omitted graph admission receipts")
    if graph.get("node_count") != args.nodes or graph.get("edge_count") != expected_layout["timers"]:
        raise ValueError("mixed graph probe did not create the requested nodes and timer edges")
    if graph.get("max_parallel_admission") != args.nodes or graph.get("max_node_admission") != args.nodes:
        raise ValueError("mixed graph probe changed its explicit actor admission bound")
    if graph.get("lossless_timer_edge_capacity") != 1:
        raise ValueError("mixed graph timer fan-in was not bounded by capacity-one lossless edges")
    if graph.get("listener_reader_admission") != expected_layout["loopback_remote_listeners"]:
        raise ValueError("mixed graph listener-reader admission does not match the remote listener count")
    correctness = raw.get("correctness")
    if not isinstance(correctness, dict):
        raise ValueError("mixed graph probe omitted correctness receipts")
    expected_checks = (
        "all_native_finite_nodes_emitted",
        "all_worker_nodes_emitted",
        "all_in_process_nodes_emitted",
        "all_program_services_emitted",
        "all_remote_listeners_emitted",
        "all_timer_nodes_emitted",
        "timer_events_consumed",
        "remote_requests_complete",
        "worker_pool_capacity_respected",
        "workers_alive_before_registry_drop",
        "workers_reaped_after_registry_drop",
        "graph_metrics_match_native_plus_worker_starts",
        "threads_returned_to_baseline_plus_two",
        "cancel_join_and_helper_threads_succeeded",
    )
    for key in expected_checks:
        if correctness.get(key) is not True:
            raise ValueError(f"mixed graph probe failed {key}")
    outputs = raw.get("outputs")
    if not isinstance(outputs, dict):
        raise ValueError("mixed graph probe omitted output receipts")
    distinct = outputs.get("distinct_nodes_with_terminal_output")
    if not isinstance(distinct, dict):
        raise ValueError("mixed graph probe omitted per-backend output counts")
    for key, count in (
        ("native", expected_layout["native_finite_applets"]),
        ("worker", expected_layout["distinct_registry_workers"]),
        ("in_process", expected_layout["in_process_applets"]),
        ("service_event", expected_layout["scheduled_program_services"]),
        ("remote_event", expected_layout["loopback_remote_listeners"]),
    ):
        if distinct.get(key) != count:
            raise ValueError(f"mixed graph probe returned an incomplete {key} output count")
    if outputs.get("distinct_timer_sources_with_output") != expected_layout["timers"]:
        raise ValueError("mixed graph probe did not observe output from every distinct timer node")
    remote_fixture = raw.get("remote_fixture")
    if not isinstance(remote_fixture, dict):
        raise ValueError("mixed graph probe omitted loopback server receipts")
    if remote_fixture.get("requests") != 2 * expected_layout["loopback_remote_listeners"]:
        raise ValueError("mixed graph probe did not serve every discovery and event request")
    workers = raw.get("workers")
    if not isinstance(workers, dict):
        raise ValueError("mixed graph probe omitted registry worker receipts")
    pids = workers.get("worker_pids")
    if not isinstance(pids, list) or len(pids) != expected_layout["distinct_registry_workers"]:
        raise ValueError("mixed graph probe did not preserve one process receipt per worker identity")
    if any(type(pid) is not int or pid <= 0 for pid in pids) or len(set(pids)) != len(pids):
        raise ValueError("mixed graph probe returned invalid or duplicate worker process ids")
    resources = raw.get("resources")
    if not isinstance(resources, dict) or not isinstance(resources.get("peak_sampled"), dict):
        raise ValueError("mixed graph probe omitted sampled process-local resources")
    if type(resources.get("launched_processes_reported_by_graph_metrics")) is not int:
        raise ValueError("mixed graph probe omitted its process-launch count")
    return {
        "actor_count": args.nodes,
        "actor_layout": expected_layout,
        "active_window_ms": args.window_ms,
        "distinct_backend_nodes_with_output": distinct,
        "timer_events_consumed": outputs.get("timer_events_consumed_by_capacity_one_lossless_collector"),
        "loopback_requests": remote_fixture,
        "registry_worker_reap": {
            "count": len(pids),
            "all_reaped_after_registry_drop": workers.get("all_reaped_after_registry_drop"),
            "ownership": workers.get("ownership"),
        },
        "process_local_resources": resources,
        "timings_us": raw.get("timings_us"),
        "scope_limits": raw.get("scope_limits"),
    }


def _retain_mixed_failure(
    output_path: Path,
    raw_path: Path,
    args: argparse.Namespace,
    test_output: str,
    watchdog_cleanup: dict[str, object] | None = None,
) -> Path:
    failure_path = output_path.with_name(f"{output_path.stem}-failure.json")
    if failure_path.exists() or failure_path.is_symlink():
        raise FileExistsError(f"refusing to overwrite failure evidence: {failure_path}")
    try:
        raw: object = json.loads(raw_path.read_text()) if raw_path.is_file() else None
    except (OSError, json.JSONDecodeError) as error:
        raw = {"read_error": f"{type(error).__name__}: {error}"}
    record = {
        "schema": "execute.graph-mixed-lifecycle.failure/1",
        "status": "failed_unverified",
        "claim": "none",
        "created_utc": datetime.now(timezone.utc).isoformat(),
        "test_timeout_s": args.overall_timeout_s,
        "test_binary_sha256": sha256_file(args.test_binary.resolve()),
        "source_files_sha256": source_hashes("mixed"),
        "environment": environment_record(),
        "raw_checkpoint": raw,
        "watchdog_cleanup": watchdog_cleanup,
        "test_output_tail": test_output[-12_000:],
        "note": "This is retained failure evidence only; it is not a successful measurement report.",
    }
    with failure_path.open("x", encoding="utf-8") as destination:
        destination.write(json.dumps(record, sort_keys=True, indent=2) + "\n")
    return failure_path


def _worker_group_ids_from_checkpoint(raw_path: Path) -> list[int]:
    """Find only fixture workers recorded under this wrapper's private temp dir."""
    try:
        raw = json.loads(raw_path.read_text())
    except (OSError, json.JSONDecodeError):
        return []
    workers = raw.get("workers") if isinstance(raw, dict) else None
    marker_paths = workers.get("marker_paths") if isinstance(workers, dict) else None
    if not isinstance(marker_paths, list):
        return []
    temporary_root = raw_path.parent.resolve()
    group_ids: set[int] = set()
    for marker in marker_paths:
        if not isinstance(marker, str):
            continue
        try:
            path = Path(marker).resolve(strict=True)
            path.relative_to(temporary_root)
            lines = path.read_text(encoding="ascii").splitlines()
        except (OSError, ValueError):
            continue
        if not lines or any(not line.isdecimal() for line in lines):
            continue
        for line in lines:
            pid = int(line)
            if pid <= 1:
                continue
            try:
                command = Path(f"/proc/{pid}/cmdline").read_bytes().split(b"\0")
                decoded_command = [os.fsdecode(arg) for arg in command if arg]
                if not any(Path(arg).name == "json_lines_worker.py" for arg in decoded_command):
                    continue
                if os.fspath(path) not in decoded_command:
                    continue
                if os.getpgid(pid) == pid:
                    group_ids.add(pid)
            except (OSError, ProcessLookupError):
                continue
    return sorted(group_ids)


def _signal_worker_groups(group_ids: list[int], sig: signal.Signals) -> list[int]:
    signaled = []
    for pid in group_ids:
        try:
            if os.getpgid(pid) == pid:
                os.killpg(pid, sig)
                signaled.append(pid)
        except ProcessLookupError:
            continue
        except OSError:
            continue
    return signaled


def _terminate_process_group(
    process: subprocess.Popen,
    raw_path: Path,
) -> tuple[str, dict[str, object]]:
    """Stop the test process group and its separately grouped fixture workers."""
    test_group_term = False
    try:
        os.killpg(process.pid, signal.SIGTERM)
        test_group_term = True
    except ProcessLookupError:
        pass
    except OSError:
        process.kill()
    worker_group_ids = _worker_group_ids_from_checkpoint(raw_path)
    worker_term = _signal_worker_groups(worker_group_ids, signal.SIGTERM)
    test_group_kill = False
    try:
        output, _ = process.communicate(timeout=5)
    except subprocess.TimeoutExpired:
        try:
            os.killpg(process.pid, signal.SIGKILL)
            test_group_kill = True
        except ProcessLookupError:
            pass
        except OSError:
            process.kill()
        worker_kill = _signal_worker_groups(worker_group_ids, signal.SIGKILL)
        output, _ = process.communicate()
    else:
        worker_kill = _signal_worker_groups(worker_group_ids, signal.SIGKILL)
    return output or "", {
        "worker_group_pids_observed": worker_group_ids,
        "worker_group_pids_sigterm_sent": worker_term,
        "worker_group_pids_sigkill_sent": worker_kill,
        "test_process_group_sigterm_sent": test_group_term,
        "test_process_group_sigkill_sent": test_group_kill,
    }


def run_probe(args: argparse.Namespace) -> dict[str, object]:
    if not args.final_release_build_confirmed:
        raise ValueError("pass --final-release-build-confirmed only after the final release build is ready")
    if platform.system() != "Linux":
        raise ValueError("the graph, mixed, and remote resource workloads require a Linux runner; macOS evidence does not certify Arch/Omarchy")
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
    elif args.kind == "mixed":
        if not 32 <= args.nodes <= 1000:
            raise ValueError("mixed graph --nodes must be in 32..1000")
        if not 100 <= args.window_ms <= 10_000:
            raise ValueError("mixed graph --window-ms must be in 100..10000")
    elif not 1 <= args.warmups <= 10_000 or not 1 <= args.samples <= 1_000:
        raise ValueError("remote warmups/samples exceed supported bounds (1..10000 / 1..1000)")
    elif not 1 <= args.timeout_ms <= 300_000:
        raise ValueError("remote timeouts exceed supported bounds")
    binary = _verify_binary(args.test_binary)
    kind = args.kind
    prefix = {
        "graph": "graph-lifecycle-",
        "mixed": "graph-mixed-lifecycle-",
        "remote": "remote-applet-network-",
    }[kind]
    output_path = validate_report_path(args.report, prefix)
    mixed_failure_path = output_path.with_name(f"{output_path.stem}-failure.json")
    if kind == "mixed" and (mixed_failure_path.exists() or mixed_failure_path.is_symlink()):
        raise FileExistsError(f"refusing to overwrite failure evidence: {mixed_failure_path}")
    if kind == "remote" and not args.read_only_or_idempotent_confirmed:
        raise ValueError("remote samples repeat the operation; confirm it is read-only or independently idempotent")
    env = os.environ.copy()
    with tempfile.TemporaryDirectory(prefix=f"execute-{kind}-stress-") as directory:
        raw_path = Path(directory) / "raw.json"
        if kind == "mixed":
            env["TMPDIR"] = str(Path(directory).resolve())
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
        elif kind == "mixed":
            env.update({
                "GRAPH_MIXED_STRESS_RAW_REPORT": str(raw_path),
                "GRAPH_MIXED_STRESS_NODES": str(args.nodes),
                "GRAPH_MIXED_STRESS_WINDOW_MS": str(args.window_ms),
            })
            test_name = "mixed_graph_lifecycle_stress"
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
        if kind == "mixed":
            process = subprocess.Popen(
                command,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                text=True,
                env=env,
                start_new_session=True,
            )
            try:
                output, _ = process.communicate(timeout=args.overall_timeout_s)
            except subprocess.TimeoutExpired as error:
                output, cleanup = _terminate_process_group(process, raw_path)
                failure_path = _retain_mixed_failure(
                    output_path, raw_path, args, output, watchdog_cleanup=cleanup
                )
                raise RuntimeError(
                    f"mixed stress test exceeded {args.overall_timeout_s}s; process group stopped and failure evidence retained at {failure_path}"
                ) from error
            completed = subprocess.CompletedProcess(command, process.returncode, output)
        else:
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
            if kind == "mixed":
                failure_path = _retain_mixed_failure(
                    output_path, raw_path, args, completed.stdout
                )
                raise RuntimeError(
                    f"mixed stress test exited {completed.returncode}; failure evidence retained at {failure_path}\n{completed.stdout[-8000:]}"
                )
            raise RuntimeError(
                f"{kind} stress test exited {completed.returncode}; no report imported\n{completed.stdout[-8000:]}"
            )
        if not raw_path.is_file():
            if kind == "mixed":
                failure_path = _retain_mixed_failure(
                    output_path, raw_path, args, completed.stdout
                )
                raise RuntimeError(
                    f"mixed stress test did not create its raw report; failure evidence retained at {failure_path}"
                )
            raise RuntimeError(f"{kind} stress test did not create its raw report")
        try:
            raw = json.loads(raw_path.read_text())
        except (OSError, json.JSONDecodeError) as error:
            if kind == "mixed":
                failure_path = _retain_mixed_failure(
                    output_path,
                    raw_path,
                    args,
                    f"{completed.stdout}\nraw report import failed: {error}",
                )
                raise RuntimeError(
                    f"mixed raw report could not be imported; failure evidence retained at {failure_path}"
                ) from error
            raise
        if kind == "graph":
            report_summary = _validate_graph(raw, args)
        elif kind == "mixed":
            try:
                report_summary = _validate_mixed(raw, args)
            except ValueError as error:
                failure_path = _retain_mixed_failure(
                    output_path,
                    raw_path,
                    args,
                    f"{completed.stdout}\nreport validation failed: {error}",
                )
                raise RuntimeError(
                    f"mixed report validation failed; failure evidence retained at {failure_path}: {error}"
                ) from error
        else:
            report_summary = _validate_remote(raw, args)
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
                else (
                    "One mixed Linux RuntimeGraphExecutor graph with bounded timer, native, registry-worker, in-process, scheduled-service, and loopback-listener counts; process-local RSS/thread sample only."
                    if kind == "mixed"
                    else "One explicitly configured remote applet endpoint, measured from this Linux client; server resources are not measured."
                )
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
    mixed = subparsers.add_parser(
        "mixed",
        help="mixed RuntimeGraphExecutor actors with bounded native, worker, program-service, and loopback-listener counts",
    )
    mixed.add_argument("--test-binary", required=True, type=Path)
    mixed.add_argument("--report", required=True, type=Path)
    mixed.add_argument("--final-release-build-confirmed", action="store_true")
    mixed.add_argument("--nodes", type=int, default=32, help="total graph actors, 32..1000; default is the 32-actor smoke probe")
    mixed.add_argument("--window-ms", type=int, default=1_000, help="active cancellation window, 100..10000")
    mixed.add_argument("--overall-timeout-s", type=int, default=600, help="hard wrapper watchdog for graph start, active window, cancel, and reaping")
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
