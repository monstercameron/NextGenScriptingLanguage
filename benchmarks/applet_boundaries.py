#!/usr/bin/env python3
"""Run the prebuilt release applet-boundary probe and retain its raw samples.

This wrapper deliberately never invokes Cargo. A final release build must be
ready before it is run; measurements belong in a fresh
benchmarks/applet-boundaries-*.json file.
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
import subprocess
import sys
import tempfile


ROOT = Path(__file__).resolve().parents[1]
BENCHMARKS = ROOT / "benchmarks"
DEFAULT_SAMPLES = 40
DEFAULT_WARMUPS = 10
SAFE_ENVIRONMENT_NAMES = (
    "CARGO_BUILD_TARGET",
    "CARGO_PROFILE_RELEASE_CODEGEN_UNITS",
    "CARGO_PROFILE_RELEASE_LTO",
    "CARGO_PROFILE_RELEASE_OPT_LEVEL",
    "CARGO_TARGET_DIR",
    "CARGO_ENCODED_RUSTFLAGS",
    "LANG",
    "LC_ALL",
    "RUSTFLAGS",
    "RUSTUP_TOOLCHAIN",
    "TARGET",
    "TZ",
)


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for block in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def percentile_nearest_rank(samples: list[int | float], percentile: float) -> int | float:
    """Return nearest-rank percentile (rank = ceil(p * n), one-based)."""
    if not samples:
        raise ValueError("percentile requires at least one sample")
    if not 0 < percentile <= 1:
        raise ValueError("percentile must be in (0, 1]")
    ordered = sorted(samples)
    return ordered[math.ceil(percentile * len(ordered)) - 1]


def summarize(samples: list[int | float]) -> dict[str, int | float]:
    if not samples:
        raise ValueError("summary requires at least one sample")
    import statistics

    return {
        "sample_count": len(samples),
        "median": statistics.median(samples),
        "p95_nearest_rank": percentile_nearest_rank(samples, 0.95),
        "minimum": min(samples),
        "maximum": max(samples),
    }


def validate_report_path(path: Path, benchmark_directory: Path = BENCHMARKS) -> Path:
    directory = benchmark_directory.resolve()
    candidate = path if path.is_absolute() else Path.cwd() / path
    candidate = candidate.absolute()
    if candidate.parent.resolve() != directory:
        raise ValueError(f"report must be saved directly under {directory}")
    if not candidate.name.startswith("applet-boundaries-") or not candidate.name.endswith(".json"):
        raise ValueError("report name must match applet-boundaries-*.json")
    if candidate.exists() or candidate.is_symlink():
        raise FileExistsError(f"refusing to overwrite report: {candidate}")
    return candidate


def _tracked_source_files() -> list[Path]:
    candidates = [ROOT / "Cargo.toml", ROOT / "Cargo.lock"]
    candidates.extend((ROOT / "src").rglob("*.rs"))
    candidates.extend(
        [
            ROOT / "tests/applet_boundaries_bench.rs",
            ROOT / "benchmarks/applet_boundaries.py",
        ]
    )
    return sorted({path for path in candidates if path.is_file()})


def source_hashes() -> dict[str, str]:
    return {
        path.relative_to(ROOT).as_posix(): sha256_file(path)
        for path in _tracked_source_files()
    }


def combined_source_hash(files: dict[str, str]) -> str:
    digest = hashlib.sha256()
    for name, content_hash in sorted(files.items()):
        digest.update(name.encode("utf-8"))
        digest.update(b"\0")
        digest.update(content_hash.encode("ascii"))
        digest.update(b"\n")
    return digest.hexdigest()


def _command_version(command: str) -> str | None:
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


def _cpu_model() -> str | None:
    if platform.system() == "Darwin":
        command = shutil.which("sysctl")
        if command:
            result = subprocess.run(
                [command, "-n", "machdep.cpu.brand_string"],
                check=False,
                stdout=subprocess.PIPE,
                stderr=subprocess.DEVNULL,
                text=True,
                timeout=5,
            )
            if result.returncode == 0 and result.stdout.strip():
                return result.stdout.strip()
    return platform.processor() or None


def _git(args: list[str]) -> str | None:
    git = shutil.which("git")
    if git is None:
        return None
    result = subprocess.run(
        [git, "-C", str(ROOT), *args],
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        text=True,
        timeout=10,
    )
    return result.stdout.strip() if result.returncode == 0 else None


def environment_record() -> dict[str, object]:
    selected = {name: os.environ.get(name) for name in SAFE_ENVIRONMENT_NAMES}
    canonical = json.dumps(selected, sort_keys=True, separators=(",", ":")).encode()
    return {
        "os": platform.system(),
        "os_release": platform.release(),
        "platform": platform.platform(),
        "architecture": platform.machine(),
        "cpu_model": _cpu_model(),
        "logical_cpu_count": os.cpu_count(),
        "python": sys.version.splitlines()[0],
        "rustc": _command_version("rustc"),
        "cargo": _command_version("cargo"),
        "selected_build_environment": selected,
        "selected_environment_sha256": hashlib.sha256(canonical).hexdigest(),
        "path_sha256": hashlib.sha256(os.environ.get("PATH", "").encode()).hexdigest(),
    }


def _validate_raw_report(raw: dict[str, object], samples: int, warmups: int) -> None:
    if raw.get("schema") != "execute.applet-boundaries.raw/1":
        raise ValueError("benchmark test returned an unknown raw report schema")
    iterations = raw.get("iterations")
    if not isinstance(iterations, dict) or iterations.get("samples") != samples or iterations.get("warmups") != warmups:
        raise ValueError("benchmark test returned unexpected sample counts")
    backends = raw.get("backends")
    if not isinstance(backends, dict) or set(backends) != {"command", "in_process", "json_lines_worker"}:
        raise ValueError("benchmark test did not return all three backends")
    for name, result in backends.items():
        if not isinstance(result, dict):
            raise ValueError(f"invalid result for backend {name}")
        values = result.get("samples_ns")
        if not isinstance(values, list) or len(values) != samples or any(not isinstance(value, int) or value <= 0 for value in values):
            raise ValueError(f"backend {name} returned invalid raw samples")
    modes = ["command", "in_process", "json_lines_worker"]
    expected_order = [modes[round % len(modes):] + modes[:round % len(modes)] for round in range(warmups + samples)]
    if iterations.get("interleaved_order") != expected_order:
        raise ValueError("benchmark samples are missing the expected interleaved backend order")
    correctness = raw.get("correctness")
    if not isinstance(correctness, dict):
        raise ValueError("benchmark test omitted its correctness record")
    expected = modes
    if correctness.get("success_backend_checks") != expected or correctness.get("absence_envelope_backends") != expected:
        raise ValueError("benchmark test omitted backend correctness checks")
    expected_absence = {
        "value": None,
        "outcome": "absence",
        "kind": "absence",
        "effect_certainty": "known_response",
        "status": 1,
    }
    if correctness.get("absence_envelope_expected_value") != expected_absence:
        raise ValueError("benchmark test omitted the complete absence result envelope")
    launches = raw.get("launches")
    if not isinstance(launches, dict):
        raise ValueError("benchmark test omitted process launch counts")
    expected_one_call = {"command": 1, "in_process": 0, "json_lines_worker": 1}
    expected_timed = {"command": warmups + samples, "in_process": 0, "json_lines_worker": 1}
    for field, expected_counts in (
        ("success_correctness_call_by_backend", expected_one_call),
        ("absence_envelope_correctness_call_by_backend", expected_one_call),
        ("timed_calls_and_worker_launches_by_backend", expected_timed),
    ):
        counts = launches.get(field)
        if counts != expected_counts:
            raise ValueError(f"benchmark test returned invalid process launch counts in {field}")
    for name, result in backends.items():
        expected_sample_window = samples if name == "command" else 0
        if result.get("launches_timed_calls_total") != expected_timed[name]:
            raise ValueError(f"backend {name} reported invalid launches across timed calls")
        if result.get("timed_calls_including_warmups") != warmups + samples:
            raise ValueError(f"backend {name} reported invalid timed-call count")
        if result.get("launches_in_timed_sample_window") != expected_sample_window:
            raise ValueError(f"backend {name} reported invalid launches in the measured sample window")
    serialization = raw.get("worker_serialization")
    if not isinstance(serialization, dict):
        raise ValueError("benchmark test omitted worker serialization samples")
    for field in ("request_encode_samples_ns", "response_decode_samples_ns"):
        values = serialization.get(field)
        if not isinstance(values, list) or len(values) != samples or any(not isinstance(value, (int, float)) or value <= 0 for value in values):
            raise ValueError(f"invalid worker serialization samples in {field}")


def _hash_inputs(test_binary: Path, helper: Path) -> dict[str, object]:
    return {
        "runtime_test_binary_sha256": sha256_file(test_binary),
        "helper_binary_sha256": sha256_file(helper),
        "source_files_sha256": source_hashes(),
    }


def _verify_executable(path: Path, label: str) -> Path:
    resolved = path.expanduser().resolve(strict=True)
    if not resolved.is_file() or not os.access(resolved, os.X_OK):
        raise ValueError(f"{label} must be an executable regular file: {resolved}")
    return resolved


def run_benchmark(args: argparse.Namespace) -> dict[str, object]:
    if not args.final_release_build_confirmed:
        raise ValueError("pass --final-release-build-confirmed only after the final release build is ready")
    if not args.measurement_window_idle:
        raise ValueError("pass --measurement-window-idle only after builds and tests have stopped")
    if args.samples <= 0 or args.warmups <= 0:
        raise ValueError("samples and warmups must both be positive")
    report_path = validate_report_path(args.report)
    test_binary = _verify_executable(args.test_binary, "release benchmark test binary")
    helper = _verify_executable(args.helper, "execute-applet helper")
    before = _hash_inputs(test_binary, helper)
    source_digest = combined_source_hash(before["source_files_sha256"])
    started_at = datetime.now(timezone.utc).isoformat()

    with tempfile.TemporaryDirectory(prefix="execute-applet-boundaries-") as temporary:
        raw_path = Path(temporary) / "raw.json"
        environment = os.environ.copy()
        environment.update(
            {
                "APPLET_BOUNDARY_HELPER": str(helper),
                "APPLET_BOUNDARY_RAW_REPORT": str(raw_path),
                "APPLET_BOUNDARY_SAMPLES": str(args.samples),
                "APPLET_BOUNDARY_WARMUPS": str(args.warmups),
            }
        )
        command = [
            str(test_binary),
            "--ignored",
            "--exact",
            "applet_boundaries",
            "--nocapture",
        ]
        completed = subprocess.run(
            command,
            cwd=ROOT,
            env=environment,
            check=False,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        if completed.returncode != 0:
            raise RuntimeError(
                "release benchmark probe failed\n"
                + completed.stdout.decode("utf-8", errors="replace")
                + completed.stderr.decode("utf-8", errors="replace")
            )
        if not raw_path.is_file():
            raise RuntimeError("release benchmark probe completed without writing its raw samples")
        raw = json.loads(raw_path.read_text(encoding="utf-8"))

    after = _hash_inputs(test_binary, helper)
    if before != after:
        raise RuntimeError("source or executable changed during the measurement; no report was written")
    _validate_raw_report(raw, args.samples, args.warmups)

    summaries = {
        name: summarize(result["samples_ns"])
        for name, result in raw["backends"].items()
    }
    serialization = raw["worker_serialization"]
    serialization_summaries = {
        "request_encode_ns": summarize(serialization["request_encode_samples_ns"]),
        "response_decode_ns": summarize(serialization["response_decode_samples_ns"]),
    }
    return {
        "schema": "execute.applet-boundaries/1",
        "measured_at_utc": started_at,
        "measurement_policy": {
            "release_build_confirmed": True,
            "measurement_window_idle_confirmed": True,
            "cargo_invoked": False,
            "test_or_build_concurrency_checked_by_harness": "explicit caller confirmation",
            "interleaved_backend_samples": True,
            "correctness_precedes_timing": True,
            "statistics": "median and nearest-rank p95; all raw nanosecond samples retained",
        },
        "source": {
            "git_revision": _git(["rev-parse", "HEAD"]),
            "working_tree_status": "dirty" if (_git(["status", "--porcelain"]) or "") else "clean",
            "combined_source_sha256": source_digest,
            "files_sha256": before["source_files_sha256"],
        },
        "executables": {
            "runtime_test_binary": str(test_binary),
            "runtime_test_binary_sha256": before["runtime_test_binary_sha256"],
            "helper_binary": str(helper),
            "helper_binary_sha256": before["helper_binary_sha256"],
        },
        "environment": environment_record(),
        "sample_counts": {"warmups_per_backend": args.warmups, "measured_per_backend": args.samples},
        "summary": {"backend_runtime_ns": summaries, "worker_serialization_ns": serialization_summaries},
        "raw": raw,
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--test-binary", type=Path, required=True, help="prebuilt release applet_boundaries_bench test executable")
    parser.add_argument("--helper", type=Path, required=True, help="prebuilt execute-applet helper from the same source revision")
    parser.add_argument("--report", type=Path, required=True, help="fresh benchmarks/applet-boundaries-<label>.json output path")
    parser.add_argument("--samples", type=int, default=DEFAULT_SAMPLES)
    parser.add_argument("--warmups", type=int, default=DEFAULT_WARMUPS)
    parser.add_argument(
        "--final-release-build-confirmed",
        action="store_true",
        help="confirm the root task declared this exact release build ready for measurement",
    )
    parser.add_argument(
        "--measurement-window-idle",
        action="store_true",
        help="confirm no build, test or other benchmark is running concurrently",
    )
    args = parser.parse_args()
    try:
        report = run_benchmark(args)
        destination = validate_report_path(args.report)
        payload = (json.dumps(report, indent=2, sort_keys=True) + "\n").encode("utf-8")
        descriptor = os.open(destination, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        with os.fdopen(descriptor, "wb") as output:
            output.write(payload)
            output.flush()
            os.fsync(output.fileno())
        print(f"Wrote {destination}")
        return 0
    except (FileExistsError, OSError, RuntimeError, ValueError, json.JSONDecodeError) as error:
        print(f"applet-boundary benchmark failed: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
