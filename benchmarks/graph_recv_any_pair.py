#!/usr/bin/env python3
"""Paired hosted runner for the frozen graph recv_any queue benchmark.

This driver never builds code. The workflow builds one immutable test binary
per source checkout, records its digest, then invokes the exact ignored test
serially in alternating baseline/candidate order.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import re
import statistics
import subprocess
import sys
import time
from datetime import datetime, timezone
from typing import Any, Mapping, Sequence

try:
    import resource
except ImportError:  # pragma: no cover - hosted runner is Ubuntu
    resource = None


FORMAT_VERSION = 1
BENCHMARK_NAME = "recv_any_preloaded_bounded_queue_throughput"
TEST_NAME = (
    "graph::transport::input_cursor_tests::recv_any_preloaded_bench::"
    "recv_any_preloaded_bounded_queue_throughput"
)
EVENTS_PER_CASE = 54_208
SAMPLES_PER_CASE = 3
EXPECTED_CHECKSUM = EVENTS_PER_CASE * (EVENTS_PER_CASE + 1) // 2
ORDER_ORACLE = (
    "sorted input-name round robin; per-input edge round robin; FIFO within each edge"
)
CASE_SPECS = tuple(
    (source_count, input_count, f"sources={source_count},inputs={input_count}")
    for source_count in (1, 16, 847)
    for input_count in (1, 8)
)
SAMPLE_FIELDS = {
    "case",
    "source_count",
    "input_count",
    "messages_per_source",
    "sample_index",
    "events",
    "duration_ns",
    "ns_per_event",
    "checksum",
    "order_oracle",
}
CASE_FIELDS = {"case", "p50_ns_per_event", "samples"}
REPORT_FIELDS = {"format_version", "harness", "cases"}
SOURCE_CONFIG_FILES = {
    "Cargo.toml",
    "Cargo.lock",
    "build.rs",
    ".cargo/config.toml",
    "rust-toolchain",
    "rust-toolchain.toml",
    "rustfmt.toml",
}
EXPECTED_PATCH_SHA256 = "749f639c07957e20966981ca43ea824c078781d7f1a588264fa9252ff0162efe"
EXPECTED_HARNESS_SHA256 = "87eb3b72e27757d3f71977fa9d47d6adf5ff028504c3cb3da858656c350ce891"
HARNESS_MARKER = b"    mod recv_any_preloaded_bench {"
COMMIT_RE = re.compile(r"^[0-9a-f]{40}$")
SHA256_RE = re.compile(r"^[0-9a-f]{64}$")
MAX_REPORT_BYTES = 1_000_000


class BenchmarkError(RuntimeError):
    """A source identity, artifact, invocation, or benchmark-contract failure."""


def utc_now() -> str:
    return datetime.now(timezone.utc).isoformat(timespec="milliseconds").replace("+00:00", "Z")


def sha256_bytes(payload: bytes) -> str:
    return hashlib.sha256(payload).hexdigest()


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for block in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def require_sha256(value: str, expected: str, label: str) -> None:
    if not SHA256_RE.fullmatch(expected) or value != expected:
        raise BenchmarkError(f"{label} SHA-256 mismatch: expected {expected}, got {value}")


def harness_source_suffix(transport_source: bytes) -> bytes:
    matches = [index for index in range(len(transport_source)) if transport_source.startswith(HARNESS_MARKER, index)]
    if len(matches) != 1:
        raise BenchmarkError(
            f"expected one frozen benchmark module marker in transport source, found {len(matches)}"
        )
    return transport_source[matches[0] :]


def verify_harness_source(transport_source: bytes, expected_hash: str = EXPECTED_HARNESS_SHA256) -> str:
    suffix = harness_source_suffix(transport_source)
    actual_hash = sha256_bytes(suffix)
    require_sha256(actual_hash, expected_hash, "benchmark harness source suffix")
    return actual_hash


def is_tracked_build_input(relative_path: str) -> bool:
    return (
        relative_path.startswith("src/")
        or relative_path.startswith("vendor/")
        or relative_path.startswith(".cargo/")
        or relative_path in SOURCE_CONFIG_FILES
    )


def _unique_object(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            raise ValueError(f"duplicate JSON key: {key}")
        result[key] = value
    return result


def _reject_json_constant(value: str) -> None:
    raise ValueError(f"non-finite JSON number: {value}")


def parse_json_bytes(payload: bytes, label: str) -> Any:
    if len(payload) > MAX_REPORT_BYTES:
        raise BenchmarkError(f"{label} exceeds {MAX_REPORT_BYTES} bytes")
    try:
        return json.loads(
            payload.decode("utf-8"),
            object_pairs_hook=_unique_object,
            parse_constant=_reject_json_constant,
        )
    except (UnicodeDecodeError, json.JSONDecodeError, ValueError) as error:
        raise BenchmarkError(f"{label} is not strict UTF-8 JSON: {error}") from error


def _is_number(value: Any) -> bool:
    if not isinstance(value, (int, float)) or isinstance(value, bool):
        return False
    try:
        return math.isfinite(float(value))
    except (OverflowError, ValueError):
        return False


def validate_benchmark_json(payload: bytes) -> dict[str, float]:
    report = parse_json_bytes(payload, "benchmark JSON")
    if not isinstance(report, dict) or set(report) != REPORT_FIELDS:
        raise BenchmarkError("benchmark JSON has an unexpected top-level schema")
    if (
        not isinstance(report["format_version"], int)
        or isinstance(report["format_version"], bool)
        or report["format_version"] != FORMAT_VERSION
    ):
        raise BenchmarkError("benchmark JSON format_version must be integer 1")
    if report["harness"] != BENCHMARK_NAME:
        raise BenchmarkError("benchmark JSON identifies a different harness")
    cases = report["cases"]
    if not isinstance(cases, list) or len(cases) != len(CASE_SPECS):
        raise BenchmarkError("benchmark JSON must contain exactly the six declared cases")

    metrics: dict[str, float] = {}
    for case_record, (source_count, input_count, case_name) in zip(cases, CASE_SPECS):
        if not isinstance(case_record, dict) or set(case_record) != CASE_FIELDS:
            raise BenchmarkError(f"case {case_name} has an unexpected schema")
        if case_record["case"] != case_name:
            raise BenchmarkError(f"case identity/order mismatch; expected {case_name}")
        p50 = case_record["p50_ns_per_event"]
        if not _is_number(p50) or p50 <= 0:
            raise BenchmarkError(f"case {case_name} has an invalid p50")
        samples = case_record["samples"]
        if not isinstance(samples, list) or len(samples) != SAMPLES_PER_CASE:
            raise BenchmarkError(f"case {case_name} must contain exactly three raw samples")

        durations: list[int] = []
        for sample_index, sample in enumerate(samples):
            if not isinstance(sample, dict) or set(sample) != SAMPLE_FIELDS:
                raise BenchmarkError(f"case {case_name} sample {sample_index} has an unexpected schema")
            expected = {
                "case": case_name,
                "source_count": source_count,
                "input_count": input_count,
                "messages_per_source": EVENTS_PER_CASE // source_count,
                "sample_index": sample_index,
                "events": EVENTS_PER_CASE,
                "checksum": EXPECTED_CHECKSUM,
                "order_oracle": ORDER_ORACLE,
            }
            for key, expected_value in expected.items():
                actual = sample[key]
                if isinstance(expected_value, int) and isinstance(actual, bool):
                    raise BenchmarkError(f"case {case_name} sample {sample_index} has invalid {key}")
                if actual != expected_value:
                    raise BenchmarkError(
                        f"case {case_name} sample {sample_index} has {key}={actual!r}; "
                        f"expected {expected_value!r}"
                    )
            duration = sample["duration_ns"]
            if not isinstance(duration, int) or isinstance(duration, bool) or duration <= 0:
                raise BenchmarkError(f"case {case_name} sample {sample_index} has invalid duration_ns")
            per_event = sample["ns_per_event"]
            if not _is_number(per_event) or per_event <= 0:
                raise BenchmarkError(f"case {case_name} sample {sample_index} has invalid ns_per_event")
            if not math.isclose(per_event, duration / EVENTS_PER_CASE, rel_tol=1e-9, abs_tol=1e-9):
                raise BenchmarkError(f"case {case_name} sample {sample_index} ns_per_event disagrees with duration")
            durations.append(duration)

        expected_p50 = statistics.median(durations) / EVENTS_PER_CASE
        if not math.isclose(float(p50), expected_p50, rel_tol=1e-9, abs_tol=1e-9):
            raise BenchmarkError(f"case {case_name} p50 disagrees with its three raw samples")
        metrics[case_name] = float(p50)
    return metrics


def _git(root: Path, *arguments: str) -> str:
    try:
        result = subprocess.run(
            ["git", "-C", str(root), *arguments],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=20,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        raise BenchmarkError(f"cannot inspect Git checkout {root}: {error}") from error
    if result.returncode != 0:
        detail = result.stderr.decode("utf-8", errors="replace").strip()
        raise BenchmarkError(f"Git command failed in {root}: {detail}")
    return result.stdout.decode("utf-8").strip()


def source_fingerprint(root: Path) -> dict[str, Any]:
    root = root.resolve(strict=True)
    raw_paths = subprocess.run(
        ["git", "-C", str(root), "ls-files", "-z"],
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        timeout=20,
        check=False,
    )
    if raw_paths.returncode != 0:
        raise BenchmarkError(
            "cannot list tracked source files: "
            + raw_paths.stderr.decode("utf-8", errors="replace").strip()
        )
    paths = sorted(
        relative
        for raw_path in raw_paths.stdout.split(b"\0")
        if raw_path
        for relative in (raw_path.decode("utf-8"),)
        if is_tracked_build_input(relative)
    )
    if not paths:
        raise BenchmarkError(f"no tracked Rust source or build configuration found in {root}")
    files: dict[str, str] = {}
    digest = hashlib.sha256()
    for relative in paths:
        path = root / relative
        if not path.is_file():
            raise BenchmarkError(f"tracked source file is missing: {relative}")
        file_hash = sha256_file(path)
        files[relative] = file_hash
        digest.update(relative.encode("utf-8"))
        digest.update(b"\0")
        digest.update(file_hash.encode("ascii"))
        digest.update(b"\n")
    return {"sha256": digest.hexdigest(), "files": files}


def select_test_binary(
    messages: Sequence[Mapping[str, Any]],
    manifest_path: Path,
    target_source_path: Path,
    target_dir: Path,
) -> Path:
    manifest = manifest_path.expanduser().resolve(strict=True)
    target_source = target_source_path.expanduser().resolve(strict=True)
    build_target = target_dir.expanduser().resolve(strict=True)
    expected_binary_dir = (build_target / "release" / "deps").resolve(strict=True)
    selected: list[Path] = []

    for message in messages:
        if message.get("reason") != "compiler-artifact":
            continue
        target = message.get("target")
        profile = message.get("profile")
        if not isinstance(target, dict) or not isinstance(profile, dict):
            continue
        if target.get("test") is not True or "lib" not in target.get("kind", []):
            continue
        if (
            profile.get("test") is not True
            or profile.get("opt_level") != "3"
            or profile.get("debug_assertions") is not False
            or message.get("fresh") is not False
        ):
            continue
        try:
            artifact_manifest = Path(message["manifest_path"]).resolve(strict=True)
            artifact_source = Path(target["src_path"]).resolve(strict=True)
            executable = Path(message["executable"]).resolve(strict=True)
            executable.relative_to(expected_binary_dir)
        except (KeyError, OSError, TypeError, ValueError):
            continue
        if artifact_manifest != manifest or artifact_source != target_source:
            continue
        if not executable.is_file() or not os.access(executable, os.X_OK):
            continue
        selected.append(executable)

    if len(selected) != 1:
        raise BenchmarkError(
            "Cargo JSON must identify exactly one fresh release library-test artifact "
            "for this checkout; found "
            + str(len(selected))
        )
    return selected[0]


def select_test_binary_from_jsonl(
    cargo_json_path: Path,
    manifest_path: Path,
    target_source_path: Path,
    target_dir: Path,
) -> Path:
    messages: list[dict[str, Any]] = []
    try:
        lines = cargo_json_path.expanduser().resolve(strict=True).read_text(encoding="utf-8").splitlines()
    except (OSError, UnicodeDecodeError) as error:
        raise BenchmarkError(f"cannot read Cargo JSON output: {error}") from error
    for line_number, line in enumerate(lines, start=1):
        if not line.strip():
            continue
        try:
            message = json.loads(
                line,
                object_pairs_hook=_unique_object,
                parse_constant=_reject_json_constant,
            )
        except (json.JSONDecodeError, ValueError) as error:
            raise BenchmarkError(f"malformed Cargo JSON line {line_number}: {error}") from error
        if not isinstance(message, dict):
            raise BenchmarkError(f"Cargo JSON line {line_number} is not an object")
        messages.append(message)
    return select_test_binary(messages, manifest_path, target_source_path, target_dir)


def validate_checkout(root: Path, expected_commit: str) -> tuple[Path, dict[str, Any]]:
    if not COMMIT_RE.fullmatch(expected_commit):
        raise BenchmarkError("source commit must be a full lowercase 40-character SHA")
    root = root.expanduser().resolve(strict=True)
    actual_commit = _git(root, "rev-parse", "HEAD")
    if actual_commit != expected_commit:
        raise BenchmarkError(f"checkout at {root} is {actual_commit}, expected {expected_commit}")
    modified = _git(root, "diff", "--name-only").splitlines()
    staged = _git(root, "diff", "--cached", "--name-only").splitlines()
    untracked_raw = subprocess.run(
        ["git", "-C", str(root), "ls-files", "--others", "--exclude-standard", "-z"],
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        timeout=20,
        check=False,
    )
    if untracked_raw.returncode != 0:
        raise BenchmarkError("cannot inspect untracked source files")
    untracked = [path.decode("utf-8") for path in untracked_raw.stdout.split(b"\0") if path]
    if staged or untracked or any(path != "src/graph/transport.rs" for path in modified):
        raise BenchmarkError(
            "source checkout has changes beyond the frozen benchmark patch: "
            + json.dumps({"modified": modified, "staged": staged, "untracked": untracked}, sort_keys=True)
        )
    return root, source_fingerprint(root)


def _write_json(path: Path, value: Any, *, exclusive: bool = False) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    payload = (json.dumps(value, sort_keys=True, indent=2, allow_nan=False) + "\n").encode("utf-8")
    if exclusive and path.exists():
        raise BenchmarkError(f"refusing to overwrite existing file: {path}")
    temporary = path.with_name(path.name + ".tmp")
    temporary.write_bytes(payload)
    temporary.replace(path)


def _load_object(path: Path, label: str) -> dict[str, Any]:
    value = parse_json_bytes(path.read_bytes(), label)
    if not isinstance(value, dict):
        raise BenchmarkError(f"{label} must be a JSON object")
    return value


def _verify_binary(path: Path, target_dir: Path, expected_hash: str | None = None) -> tuple[Path, str]:
    binary = path.expanduser().resolve(strict=True)
    target = target_dir.expanduser().resolve(strict=True)
    expected_parent = (target / "release" / "deps").resolve(strict=True)
    try:
        binary.relative_to(expected_parent)
    except ValueError as error:
        raise BenchmarkError(f"test binary is not inside the fresh release/deps directory: {binary}") from error
    if not binary.is_file() or not os.access(binary, os.X_OK):
        raise BenchmarkError(f"test binary is not executable: {binary}")
    digest = sha256_file(binary)
    if expected_hash is not None:
        require_sha256(digest, expected_hash, f"binary {binary}")
    return binary, digest


def create_build_receipt(args: argparse.Namespace) -> None:
    root, sources = validate_checkout(Path(args.root), args.commit)
    target_dir = Path(args.target_dir).expanduser().resolve(strict=True)
    if not args.fresh_target_dir_confirmed:
        raise BenchmarkError("the workflow must confirm that this target directory was initially absent")
    patch = Path(args.patch).expanduser().resolve(strict=True)
    patch_hash = sha256_file(patch)
    require_sha256(patch_hash, EXPECTED_PATCH_SHA256, "frozen benchmark patch")
    harness_hash = verify_harness_source((root / "src/graph/transport.rs").read_bytes())
    binary, binary_hash = _verify_binary(Path(args.binary), target_dir)
    receipt_path = Path(args.receipt).expanduser().absolute()
    receipt = {
        "schema_version": FORMAT_VERSION,
        "created_at": utc_now(),
        "commit": args.commit,
        "root": str(root),
        "source_fingerprint": sources,
        "patch_sha256": patch_hash,
        "harness_source_sha256": harness_hash,
        "target_dir": str(target_dir),
        "fresh_target_dir_confirmed_by_workflow": True,
        "binary": str(binary),
        "binary_sha256": binary_hash,
        "build_command": "cargo test --locked --release --lib --no-run --message-format=json",
    }
    _write_json(receipt_path, receipt, exclusive=True)
    print(json.dumps({"receipt": str(receipt_path), "binary_sha256": binary_hash}, sort_keys=True))


def _verify_build_receipt(
    side: str,
    receipt_path: Path,
    source_root: Path,
    expected_commit: str,
    expected_patch_hash: str,
) -> dict[str, Any]:
    receipt = _load_object(receipt_path, f"{side} build receipt")
    required = {
        "schema_version",
        "created_at",
        "commit",
        "root",
        "source_fingerprint",
        "patch_sha256",
        "harness_source_sha256",
        "target_dir",
        "fresh_target_dir_confirmed_by_workflow",
        "binary",
        "binary_sha256",
        "build_command",
    }
    if set(receipt) != required or receipt.get("schema_version") != FORMAT_VERSION:
        raise BenchmarkError(f"{side} build receipt has an unexpected schema")
    if receipt["commit"] != expected_commit or receipt["root"] != str(source_root):
        raise BenchmarkError(f"{side} build receipt is bound to a different commit or source checkout")
    if receipt["build_command"] != "cargo test --locked --release --lib --no-run --message-format=json":
        raise BenchmarkError(f"{side} build receipt has an unexpected build command")
    if receipt["fresh_target_dir_confirmed_by_workflow"] is not True:
        raise BenchmarkError(f"{side} build receipt does not confirm a fresh target directory")
    require_sha256(str(receipt["patch_sha256"]), expected_patch_hash, f"{side} patch")
    actual_harness_hash = verify_harness_source(
        (source_root / "src/graph/transport.rs").read_bytes()
    )
    if receipt["harness_source_sha256"] != actual_harness_hash:
        raise BenchmarkError(f"{side} build receipt identifies a different harness source")
    current_sources = source_fingerprint(source_root)
    if current_sources != receipt["source_fingerprint"]:
        raise BenchmarkError(f"{side} source tree changed after its build receipt was created")
    target_dir = Path(receipt["target_dir"]).resolve(strict=True)
    binary, digest = _verify_binary(
        Path(receipt["binary"]), target_dir, str(receipt["binary_sha256"])
    )
    return {
        "commit": expected_commit,
        "root": str(source_root),
        "source_sha256": current_sources["sha256"],
        "source_files": len(current_sources["files"]),
        "source_fingerprint": current_sources,
        "target_dir": str(target_dir),
        "binary": str(binary),
        "binary_sha256": digest,
        "receipt": str(receipt_path.resolve(strict=True)),
    }


def interleaved_order(index: int) -> tuple[str, str]:
    return ("baseline", "candidate") if index % 2 == 0 else ("candidate", "baseline")


def _child_cpu_seconds() -> dict[str, float] | None:
    if resource is None:
        return None
    try:
        usage = resource.getrusage(resource.RUSAGE_CHILDREN)
    except (AttributeError, OSError):
        return None
    return {"user": float(usage.ru_utime), "system": float(usage.ru_stime)}


def run_attempt(
    command: Sequence[str],
    attempt_dir: Path,
    metadata: Mapping[str, Any],
    timeout_seconds: float,
) -> dict[str, Any]:
    attempt_dir.mkdir(parents=True, exist_ok=False)
    raw_path = attempt_dir / "raw.json"
    environment = os.environ.copy()
    environment["LC_ALL"] = "C"
    environment["TZ"] = "UTC"
    environment["FLOW_RECV_ANY_BENCH_JSON"] = str(raw_path.resolve())
    started = {
        **dict(metadata),
        "started_at": utc_now(),
        "command": list(command),
        "timeout_seconds": timeout_seconds,
        "environment": {"LC_ALL": "C", "TZ": "UTC"},
        "raw_json_path": str(raw_path.resolve()),
    }
    _write_json(attempt_dir / "started.json", started, exclusive=True)
    cpu_before = _child_cpu_seconds()
    started_ns = time.monotonic_ns()
    returncode: int | None = None
    timed_out = False
    execution_error: str | None = None
    stdout = b""
    stderr = b""
    try:
        completed = subprocess.run(
            list(command),
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env=environment,
            timeout=timeout_seconds,
            check=False,
        )
        returncode = completed.returncode
        stdout = completed.stdout
        stderr = completed.stderr
    except subprocess.TimeoutExpired as error:
        timed_out = True
        stdout = error.stdout or b""
        stderr = error.stderr or b""
    except OSError as error:
        execution_error = f"{type(error).__name__}: {error}"
        stderr = execution_error.encode("utf-8", errors="replace")
    elapsed_ns = time.monotonic_ns() - started_ns
    cpu_after = _child_cpu_seconds()
    cpu_delta = None
    if cpu_before is not None and cpu_after is not None:
        cpu_delta = {
            key: max(0.0, cpu_after[key] - cpu_before[key])
            for key in ("user", "system")
        }
    (attempt_dir / "stdout.bin").write_bytes(stdout)
    (attempt_dir / "stderr.bin").write_bytes(stderr)

    metrics: dict[str, float] | None = None
    validation_error: str | None = None
    raw_hash: str | None = None
    if raw_path.is_file():
        raw = raw_path.read_bytes()
        raw_hash = sha256_bytes(raw)
        try:
            metrics = validate_benchmark_json(raw)
        except BenchmarkError as error:
            validation_error = str(error)
    else:
        validation_error = "benchmark did not produce raw.json"

    succeeded = returncode == 0 and not timed_out and execution_error is None and validation_error is None
    result = {
        **dict(metadata),
        "started_at": started["started_at"],
        "finished_at": utc_now(),
        "elapsed_wall_ns": elapsed_ns,
        "returncode": returncode,
        "timed_out": timed_out,
        "execution_error": execution_error,
        "cpu_seconds": cpu_delta,
        "stdout_sha256": sha256_bytes(stdout),
        "stderr_sha256": sha256_bytes(stderr),
        "raw_json_present": raw_path.is_file(),
        "raw_json_sha256": raw_hash,
        "validation_error": validation_error,
        "successful": succeeded,
        "case_p50_ns_per_event": metrics,
        "attempt_dir": str(attempt_dir.resolve()),
    }
    _write_json(attempt_dir / "finished.json", result, exclusive=True)
    return result


def _comparison(pair_index: int, baseline: Mapping[str, Any], candidate: Mapping[str, Any]) -> dict[str, Any]:
    valid = baseline.get("successful") is True and candidate.get("successful") is True
    result: dict[str, Any] = {
        "pair_index": pair_index,
        "baseline_attempt": baseline["attempt_id"],
        "candidate_attempt": candidate["attempt_id"],
        "valid": valid,
        "cases": {},
    }
    if not valid:
        result["failure_reasons"] = {
            "baseline": baseline.get("validation_error") or baseline.get("execution_error") or (
                None if baseline.get("successful") else f"returncode={baseline.get('returncode')} timed_out={baseline.get('timed_out')}"
            ),
            "candidate": candidate.get("validation_error") or candidate.get("execution_error") or (
                None if candidate.get("successful") else f"returncode={candidate.get('returncode')} timed_out={candidate.get('timed_out')}"
            ),
        }
        return result
    for _, _, case_name in CASE_SPECS:
        base_value = float(baseline["case_p50_ns_per_event"][case_name])
        candidate_value = float(candidate["case_p50_ns_per_event"][case_name])
        result["cases"][case_name] = {
            "baseline_p50_ns_per_event": base_value,
            "candidate_p50_ns_per_event": candidate_value,
            "candidate_minus_baseline_ns_per_event": candidate_value - base_value,
            "candidate_over_baseline": candidate_value / base_value,
            "candidate_change_percent": (candidate_value / base_value - 1.0) * 100.0,
        }
    return result


def _summary(pairs: Sequence[Mapping[str, Any]]) -> dict[str, Any]:
    cases: dict[str, Any] = {}
    for _, _, case_name in CASE_SPECS:
        values = [pair["cases"][case_name] for pair in pairs if pair.get("valid") is True]
        deltas = [float(value["candidate_minus_baseline_ns_per_event"]) for value in values]
        ratios = [float(value["candidate_over_baseline"]) for value in values]
        cases[case_name] = {
            "valid_pair_count": len(values),
            "paired_deltas_ns_per_event": deltas,
            "median_paired_delta_ns_per_event": statistics.median(deltas) if deltas else None,
            "paired_candidate_over_baseline_ratios": ratios,
            "median_candidate_over_baseline": statistics.median(ratios) if ratios else None,
            "candidate_losses": sum(delta > 0 for delta in deltas),
            "candidate_improvements": sum(delta < 0 for delta in deltas),
            "ties": sum(delta == 0 for delta in deltas),
        }
    return cases


def run_pairs(args: argparse.Namespace) -> int:
    if args.repetitions not in (12, 24, 48):
        raise BenchmarkError("repetitions must be one of 12, 24, or 48")
    if not 1 <= args.timeout_seconds <= 300:
        raise BenchmarkError("per-invocation timeout must be between 1 and 300 seconds")
    if not COMMIT_RE.fullmatch(args.baseline_commit) or not COMMIT_RE.fullmatch(args.candidate_commit):
        raise BenchmarkError("baseline and candidate commits must be full lowercase 40-character SHAs")
    if args.baseline_commit == args.candidate_commit:
        raise BenchmarkError("baseline and candidate commits must be different")

    patch_path = Path(args.patch).expanduser().resolve(strict=True)
    patch_hash = sha256_file(patch_path)
    require_sha256(patch_hash, EXPECTED_PATCH_SHA256, "frozen benchmark patch")
    baseline_root, _ = validate_checkout(Path(args.baseline_root), args.baseline_commit)
    candidate_root, _ = validate_checkout(Path(args.candidate_root), args.candidate_commit)
    baseline = _verify_build_receipt(
        "baseline", Path(args.baseline_receipt), baseline_root, args.baseline_commit, patch_hash
    )
    candidate = _verify_build_receipt(
        "candidate", Path(args.candidate_receipt), candidate_root, args.candidate_commit, patch_hash
    )
    if baseline["target_dir"] == candidate["target_dir"]:
        raise BenchmarkError("baseline and candidate must use separate fresh Cargo target directories")
    if baseline["binary"] == candidate["binary"]:
        raise BenchmarkError("baseline and candidate must use distinct built test binary paths")

    report_dir = Path(args.report_dir).expanduser().absolute()
    if report_dir.exists() or report_dir.is_symlink():
        raise BenchmarkError(f"refusing to overwrite report directory: {report_dir}")
    report_dir.mkdir(parents=True)
    report_path = report_dir / "paired-report.json"
    report: dict[str, Any] = {
        "schema_version": FORMAT_VERSION,
        "benchmark": BENCHMARK_NAME,
        "test_name": TEST_NAME,
        "created_at": utc_now(),
        "status": "running",
        "contract": {
            "events_per_case": EVENTS_PER_CASE,
            "raw_samples_per_case": SAMPLES_PER_CASE,
            "case_order": [case_name for _, _, case_name in CASE_SPECS],
            "checksum_per_case": EXPECTED_CHECKSUM,
            "warmup_pairs": 3,
            "measured_repetitions": args.repetitions,
            "invocation_timeout_seconds": args.timeout_seconds,
            "paired_order": "alternating AB then BA; all invocations are sequential",
            "metric": "candidate p50 ns/event minus baseline p50 ns/event; positive values are losses",
            "patch_sha256": patch_hash,
            "harness_source_sha256": EXPECTED_HARNESS_SHA256,
        },
        "host": {
            "system": platform.system(),
            "release": platform.release(),
            "machine": platform.machine(),
            "python": sys.version,
        },
        "baseline": baseline,
        "candidate": candidate,
        "attempts": [],
        "measured_pairs": [],
    }
    _write_json(report_path, report, exclusive=True)

    identities = {"baseline": baseline, "candidate": candidate}
    binaries = {side: identity["binary"] for side, identity in identities.items()}
    total_pairs = 3 + args.repetitions
    overall_failed = False
    for pair_number in range(total_pairs):
        phase = "warmup" if pair_number < 3 else "measurement"
        phase_index = pair_number + 1 if phase == "warmup" else pair_number - 2
        order = interleaved_order(pair_number if phase == "warmup" else phase_index - 1)
        pair_results: dict[str, dict[str, Any]] = {}
        for position, side in enumerate(order, start=1):
            identity = identities[side]
            attempt_id = f"{phase}-{phase_index:03d}-{position}-{side}"
            metadata = {
                "attempt_id": attempt_id,
                "phase": phase,
                "pair_index": phase_index,
                "position_in_pair": position,
                "side": side,
                "commit": identity["commit"],
                "source_sha256": identity["source_sha256"],
                "binary_sha256": identity["binary_sha256"],
                "binary": identity["binary"],
            }
            command = [
                binaries[side],
                "--exact",
                TEST_NAME,
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ]
            attempt_dir = report_dir / "attempts" / attempt_id
            result = run_attempt(command, attempt_dir, metadata, args.timeout_seconds)
            report["attempts"].append(result)
            pair_results[side] = result
            if not result["successful"]:
                overall_failed = True
            _write_json(report_path, report)

        if phase == "measurement":
            pair_record = {
                "pair_index": phase_index,
                "order": list(order),
                **_comparison(phase_index, pair_results["baseline"], pair_results["candidate"]),
            }
            report["measured_pairs"].append(pair_record)
            _write_json(report_path, report)

    post_state: dict[str, Any] = {}
    for side, identity in identities.items():
        try:
            post_root, post_source = validate_checkout(Path(identity["root"]), identity["commit"])
            post_binary, post_binary_hash = _verify_binary(
                Path(identity["binary"]), Path(identity["target_dir"]), identity["binary_sha256"]
            )
            stable = post_source["sha256"] == identity["source_sha256"] and post_binary_hash == identity["binary_sha256"]
            post_state[side] = {
                "root": str(post_root),
                "source_sha256": post_source["sha256"],
                "binary": str(post_binary),
                "binary_sha256": post_binary_hash,
                "stable": stable,
            }
            if not stable:
                overall_failed = True
        except (BenchmarkError, OSError) as error:
            post_state[side] = {"stable": False, "error": str(error)}
            overall_failed = True

    report["post_run_identity_check"] = post_state
    report["paired_summary"] = _summary(report["measured_pairs"])
    report["completed_at"] = utc_now()
    report["status"] = "failed" if overall_failed else "complete"
    report["attempt_count"] = len(report["attempts"])
    report["successful_attempt_count"] = sum(item["successful"] for item in report["attempts"])
    report["failed_attempt_count"] = report["attempt_count"] - report["successful_attempt_count"]
    _write_json(report_path, report)
    print(json.dumps({"report": str(report_path), "status": report["status"]}, sort_keys=True))
    return 0 if not overall_failed else 1


def select_binary_command(args: argparse.Namespace) -> None:
    binary = select_test_binary_from_jsonl(
        Path(args.cargo_json),
        Path(args.manifest_path),
        Path(args.target_source_path),
        Path(args.target_dir),
    )
    print(binary)


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)

    receipt = commands.add_parser("build-receipt", help="bind a fresh test binary to its exact source checkout")
    receipt.add_argument("--root", required=True)
    receipt.add_argument("--commit", required=True)
    receipt.add_argument("--target-dir", required=True)
    receipt.add_argument("--binary", required=True)
    receipt.add_argument("--patch", required=True)
    receipt.add_argument("--receipt", required=True)
    receipt.add_argument("--fresh-target-dir-confirmed", action="store_true")
    receipt.set_defaults(function=create_build_receipt)

    select = commands.add_parser("select-binary", help="select the exact fresh release library-test artifact")
    select.add_argument("--cargo-json", required=True)
    select.add_argument("--manifest-path", required=True)
    select.add_argument("--target-source-path", required=True)
    select.add_argument("--target-dir", required=True)
    select.set_defaults(function=select_binary_command)

    run = commands.add_parser("run", help="run fixed paired baseline/candidate invocations")
    run.add_argument("--baseline-root", required=True)
    run.add_argument("--candidate-root", required=True)
    run.add_argument("--baseline-commit", required=True)
    run.add_argument("--candidate-commit", required=True)
    run.add_argument("--baseline-receipt", required=True)
    run.add_argument("--candidate-receipt", required=True)
    run.add_argument("--patch", required=True)
    run.add_argument("--repetitions", required=True, type=int, choices=(12, 24, 48))
    run.add_argument("--timeout-seconds", required=True, type=int)
    run.add_argument("--report-dir", required=True)
    run.set_defaults(function=run_pairs)
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    args = _parser().parse_args(argv)
    try:
        result = args.function(args)
        return int(result or 0)
    except (BenchmarkError, OSError, ValueError, KeyError) as error:
        print(f"graph recv_any benchmark: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
