#!/usr/bin/env python3
"""Paired runner for the frozen graph named-receive queue benchmark.

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
BENCHMARK_NAME = "recv_named_preloaded_bounded_queue_throughput"
TEST_NAME = (
    "graph::transport::input_cursor_tests::recv_named_preloaded_bench::"
    "recv_named_preloaded_bounded_queue_throughput"
)
EVENTS_PER_CASE = 54_208
SAMPLES_PER_CASE = 3
EXPECTED_CHECKSUM = EVENTS_PER_CASE * (EVENTS_PER_CASE + 1) // 2
ORDER_ORACLE = "ascending named-input drain; per-input edge round robin; FIFO within each edge"
METHODS = ("recv", "recv_timeout_zero")
CASE_SPECS = tuple(
    (method, source_count, input_count, f"sources={source_count},inputs={input_count}")
    for method in METHODS
    for source_count in (1, 16, 847)
    for input_count in (1, 8)
)
SAMPLE_FIELDS = {
    "method",
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
CASE_FIELDS = {"method", "case", "p50_ns_per_event", "samples"}
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
EXPECTED_PATCH_SHA256_BY_SIDE = {
    "baseline": "ed64221aada6c33f33f8e7dcc2c8f0b79a2f518dbcafc4b7f895423b0d8d656e",
    "candidate": "2d580bf46e6e9bacc76dfcce3ce4cde41e2a9350f6add4c614002e51080763dd",
}
EXPECTED_TRANSPORT_SHA256_BY_SIDE = {
    "baseline": "48eeff362dc4fda512386967ba20b6f184bd7d2fb8fb3e34f672e242acbbe180",
    "candidate": "451d42902b672a8e97e3f761112979782610e658def3161a345046e016579369",
}
EXPECTED_BUILD_WORKFLOW_SHA256 = "7b1b329338624c16e771b86b1c37034d45016dfe9f52b6429f29d82b0063b2b7"
EXPECTED_HARNESS_SHA256 = "1faae3e5100c515bd752b55b569f5cd5bec04a304c0d53fbc18ee78fd1648521"
HARNESS_MARKER = b"    mod recv_named_preloaded_bench {"
SOURCE_FREEZE_BASE_COMMIT = "c6f77a0cb199aedec564440a6a5b4b6170317831"
WARMUP_PAIRS = 3
MEASURED_PAIRS = 48
INVOCATION_TIMEOUT_SECONDS = 120
MAX_TOTAL_BUDGET_SECONDS = 3 * 60 * 60
ORACLE_SAMPLE_FIELDS = (
    "method",
    "case",
    "source_count",
    "input_count",
    "messages_per_source",
    "sample_index",
    "events",
    "checksum",
    "order_oracle",
)
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


def validate_benchmark_json(payload: bytes) -> dict[str, Any]:
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
        raise BenchmarkError("benchmark JSON must contain exactly the twelve declared cases")

    metrics: dict[str, float] = {}
    oracle: list[dict[str, Any]] = []
    for case_record, (method, source_count, input_count, case_name) in zip(cases, CASE_SPECS):
        if not isinstance(case_record, dict) or set(case_record) != CASE_FIELDS:
            raise BenchmarkError(f"case {method}/{case_name} has an unexpected schema")
        if case_record["method"] != method or case_record["case"] != case_name:
            raise BenchmarkError(f"case identity/order mismatch; expected {method}/{case_name}")
        p50 = case_record["p50_ns_per_event"]
        if not _is_number(p50) or p50 <= 0:
            raise BenchmarkError(f"case {method}/{case_name} has an invalid p50")
        samples = case_record["samples"]
        if not isinstance(samples, list) or len(samples) != SAMPLES_PER_CASE:
            raise BenchmarkError(f"case {method}/{case_name} must contain exactly three raw samples")

        durations: list[int] = []
        for sample_index, sample in enumerate(samples):
            if not isinstance(sample, dict) or set(sample) != SAMPLE_FIELDS:
                raise BenchmarkError(
                    f"case {method}/{case_name} sample {sample_index} has an unexpected schema"
                )
            expected = {
                "method": method,
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
                if isinstance(expected_value, int) and (
                    not isinstance(actual, int) or isinstance(actual, bool)
                ):
                    raise BenchmarkError(
                        f"case {method}/{case_name} sample {sample_index} has invalid {key}"
                    )
                if actual != expected_value:
                    raise BenchmarkError(
                        f"case {method}/{case_name} sample {sample_index} has {key}={actual!r}; "
                        f"expected {expected_value!r}"
                    )
            duration = sample["duration_ns"]
            if (
                not isinstance(duration, int)
                or isinstance(duration, bool)
                or not 0 < duration <= (1 << 64) - 1
            ):
                raise BenchmarkError(
                    f"case {method}/{case_name} sample {sample_index} has invalid duration_ns"
                )
            per_event = sample["ns_per_event"]
            if not _is_number(per_event) or per_event <= 0:
                raise BenchmarkError(
                    f"case {method}/{case_name} sample {sample_index} has invalid ns_per_event"
                )
            if not math.isclose(per_event, duration / EVENTS_PER_CASE, rel_tol=1e-9, abs_tol=1e-9):
                raise BenchmarkError(
                    f"case {method}/{case_name} sample {sample_index} ns_per_event disagrees with duration"
                )
            durations.append(duration)
            oracle.append({key: sample[key] for key in ORACLE_SAMPLE_FIELDS})

        expected_p50 = statistics.median(durations) / EVENTS_PER_CASE
        if not math.isclose(float(p50), expected_p50, rel_tol=1e-9, abs_tol=1e-9):
            raise BenchmarkError(
                f"case {method}/{case_name} p50 disagrees with its three raw samples"
            )
        metrics[f"{method}|{case_name}"] = float(p50)
    signature = sha256_bytes(
        json.dumps(oracle, separators=(",", ":"), sort_keys=True, allow_nan=False).encode("utf-8")
    )
    return {"metrics": metrics, "oracle_sha256": signature}


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


def verify_frozen_source_side(root: Path, side: str, freeze_path: Path) -> tuple[dict[str, Any], str]:
    if side not in EXPECTED_PATCH_SHA256_BY_SIDE:
        raise BenchmarkError(f"unknown source arm: {side}")
    freeze_path = freeze_path.expanduser().resolve(strict=True)
    freeze = _load_object(freeze_path, "source freeze")
    required = {
        "recorded_at",
        "base_commit",
        "harness_sha256",
        "identical_harness",
        "source_paths",
        "claim",
    }
    if set(freeze) != required:
        raise BenchmarkError("source-freeze record has an unexpected schema")
    if freeze["base_commit"] != SOURCE_FREEZE_BASE_COMMIT:
        raise BenchmarkError("source-freeze record identifies a different base commit")
    if freeze["identical_harness"] is not True:
        raise BenchmarkError("source-freeze record does not confirm identical harnesses")
    require_sha256(str(freeze["harness_sha256"]), EXPECTED_HARNESS_SHA256, "source-freeze harness")

    root = root.expanduser().resolve(strict=True)
    source_path = (root / "src/graph/transport.rs").resolve(strict=True)
    source_paths = freeze["source_paths"]
    if not isinstance(source_paths, dict):
        raise BenchmarkError("source-freeze source_paths must be an object")
    expected_source_hash = source_paths.get(str(source_path))
    if not isinstance(expected_source_hash, str):
        raise BenchmarkError(f"source freeze has no {side} source entry for {source_path}")
    require_sha256(
        expected_source_hash,
        EXPECTED_TRANSPORT_SHA256_BY_SIDE[side],
        f"{side} frozen transport source record",
    )
    actual_source_hash = sha256_file(source_path)
    require_sha256(actual_source_hash, expected_source_hash, f"{side} frozen transport source")
    harness_hash = verify_harness_source(source_path.read_bytes())
    require_sha256(harness_hash, EXPECTED_HARNESS_SHA256, f"{side} benchmark harness")
    freeze_hash = sha256_file(freeze_path)
    return freeze, freeze_hash


def _planned_slots_after(pair_number: int, current_position: int) -> list[dict[str, Any]]:
    phase = "warmup" if pair_number < WARMUP_PAIRS else "measurement"
    pair_index = pair_number + 1 if phase == "warmup" else pair_number - WARMUP_PAIRS + 1
    order = interleaved_order(pair_number if phase == "warmup" else pair_index - 1)
    return [
        {
            "phase": phase,
            "pair_index": pair_index,
            "position_in_pair": position,
            "side": side,
            "status": "not_run_total_budget_exhausted",
        }
        for position, side in enumerate(order, start=1)
        if position > current_position
    ]


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


def _verify_build_receipt(
    side: str,
    receipt_path: Path,
    source_root: Path,
    expected_commit: str,
    expected_patch_hash: str,
    expected_freeze_hash: str,
) -> dict[str, Any]:
    source_root = source_root.expanduser().resolve(strict=True)
    evidence_dir = receipt_path.expanduser().resolve(strict=True).parent
    campaign_root = evidence_dir.parent.parent
    receipt_path = receipt_path.expanduser().resolve(strict=True)
    expected_receipt_path = evidence_dir / f"{side}-binary-receipt.json"
    if receipt_path != expected_receipt_path:
        raise BenchmarkError(f"{side} receipt must be the frozen evidence receipt at {expected_receipt_path}")
    receipt = _load_object(receipt_path, f"{side} build receipt")
    if set(receipt) != {"artifact", "saved_binary", "sha256", "source_sha256", "source_stable", "attempt"}:
        raise BenchmarkError(f"{side} build receipt has an unexpected schema")
    if (
        receipt["source_stable"] is not True
        or not isinstance(receipt["attempt"], int)
        or isinstance(receipt["attempt"], bool)
        or receipt["attempt"] != 1
    ):
        raise BenchmarkError(f"{side} receipt does not describe its one stable frozen build")
    if _git(source_root, "rev-parse", "HEAD") != expected_commit:
        raise BenchmarkError(f"{side} receipt source checkout has a different commit")
    require_sha256(expected_patch_hash, EXPECTED_PATCH_SHA256_BY_SIDE[side], f"{side} frozen patch")
    require_sha256(str(receipt["source_sha256"]), EXPECTED_TRANSPORT_SHA256_BY_SIDE[side], f"{side} source")
    saved_binary_sha256 = receipt["sha256"]
    if not isinstance(saved_binary_sha256, str) or not SHA256_RE.fullmatch(saved_binary_sha256):
        raise BenchmarkError(f"{side} receipt saved binary SHA-256 is malformed")
    _, actual_freeze_hash = verify_frozen_source_side(source_root, side, evidence_dir / "source-freeze.json")
    require_sha256(actual_freeze_hash, expected_freeze_hash, f"{side} source freeze")
    actual_harness_hash = verify_harness_source((source_root / "src/graph/transport.rs").read_bytes())
    require_sha256(actual_harness_hash, EXPECTED_HARNESS_SHA256, f"{side} benchmark harness")
    current_transport_hash = sha256_file(source_root / "src/graph/transport.rs")
    require_sha256(current_transport_hash, str(receipt["source_sha256"]), f"{side} receipt source")
    current_sources = source_fingerprint(source_root)

    artifact = receipt["artifact"]
    if not isinstance(artifact, dict):
        raise BenchmarkError(f"{side} receipt artifact must be an object")
    target = artifact.get("target")
    profile = artifact.get("profile")
    if not isinstance(target, dict) or not isinstance(profile, dict):
        raise BenchmarkError(f"{side} receipt artifact is missing target/profile metadata")
    if artifact.get("reason") != "compiler-artifact" or artifact.get("fresh") is not False:
        raise BenchmarkError(f"{side} artifact was not freshly compiled by rustc after package clean")
    if target.get("name") != "flow" or "lib" not in target.get("kind", []) or target.get("test") is not True:
        raise BenchmarkError(f"{side} artifact is not the library test binary")
    if (
        profile.get("test") is not True
        or profile.get("opt_level") != "3"
        or profile.get("debug_assertions") is not False
    ):
        raise BenchmarkError(f"{side} artifact is not the optimized release test profile")
    if Path(str(artifact.get("manifest_path", ""))).resolve() != source_root / "Cargo.toml":
        raise BenchmarkError(f"{side} artifact was built from a different Cargo manifest")
    if Path(str(target.get("src_path", ""))).resolve() != source_root / "src/lib.rs":
        raise BenchmarkError(f"{side} artifact was built from a different library source")
    expected_package_prefix = f"path+{source_root.as_uri()}#execute@"
    if not isinstance(artifact.get("package_id"), str) or not artifact["package_id"].startswith(expected_package_prefix):
        raise BenchmarkError(f"{side} artifact package identity does not match its source checkout")

    shared_target = (campaign_root / "work/perf-target-candidate").resolve(strict=True)
    compiler_binary = Path(str(artifact.get("executable", ""))).resolve(strict=True)
    try:
        compiler_binary.relative_to(shared_target / "release/deps")
    except ValueError as error:
        raise BenchmarkError(f"{side} compiler artifact is outside the recorded sequential target") from error
    if not isinstance(artifact.get("filenames"), list) or str(compiler_binary) not in artifact["filenames"]:
        raise BenchmarkError(f"{side} compiler artifact filenames omit the selected test binary")

    saved_binary = (evidence_dir / f"{side}-test-binary").resolve(strict=True)
    if Path(str(receipt["saved_binary"])).resolve(strict=True) != saved_binary:
        raise BenchmarkError(f"{side} receipt points at a different immutable binary copy")
    if not saved_binary.is_file() or not os.access(saved_binary, os.X_OK):
        raise BenchmarkError(f"{side} saved test binary is not executable")
    binary_hash = sha256_file(saved_binary)
    require_sha256(binary_hash, saved_binary_sha256, f"{side} saved binary")

    build_script = campaign_root / "work/build_named_pair.py"
    require_sha256(sha256_file(build_script), EXPECTED_BUILD_WORKFLOW_SHA256, "sequential build workflow")
    provenance_files: dict[str, str] = {str(build_script): sha256_file(build_script)}
    for suffix in (
        "release-clean-attempt-1.log",
        "build-attempt-1.log",
        "build-attempt-1.jsonl",
        "test-list.txt",
    ):
        evidence_file = evidence_dir / f"{side}-{suffix}"
        payload = evidence_file.read_bytes()
        provenance_files[str(evidence_file)] = sha256_bytes(payload)
        if suffix == "release-clean-attempt-1.log":
            if re.search(rb"Removed\s+\d+ files", payload) is None:
                raise BenchmarkError(f"{side} sequential build log does not confirm cargo clean")
        elif suffix == "build-attempt-1.log":
            text = payload.decode("utf-8", errors="replace")
            if "Compiling execute v" not in text or "Finished `release` profile [optimized] target(s) in" not in text:
                raise BenchmarkError(f"{side} build log does not confirm a successful release compile")
        elif suffix == "test-list.txt":
            text = payload.decode("utf-8", errors="strict")
            if f"{TEST_NAME}: test" not in text:
                raise BenchmarkError(f"{side} immutable binary test list omits the benchmark")

    cargo_json_path = evidence_dir / f"{side}-build-attempt-1.jsonl"
    artifact_matches: list[dict[str, Any]] = []
    try:
        for line_number, line in enumerate(cargo_json_path.read_text(encoding="utf-8").splitlines(), start=1):
            if not line.strip():
                continue
            message = json.loads(
                line,
                object_pairs_hook=_unique_object,
                parse_constant=_reject_json_constant,
            )
            if isinstance(message, dict) and message.get("reason") == "compiler-artifact":
                if message == artifact:
                    artifact_matches.append(message)
    except (OSError, UnicodeDecodeError, json.JSONDecodeError, ValueError) as error:
        raise BenchmarkError(f"{side} Cargo JSON artifact log is invalid: {error}") from error
    if len(artifact_matches) != 1:
        raise BenchmarkError(f"{side} receipt artifact does not match exactly one Cargo JSON row")

    return {
        "commit": expected_commit,
        "root": str(source_root),
        "source_sha256": current_sources["sha256"],
        "transport_sha256": current_transport_hash,
        "source_files": len(current_sources["files"]),
        "source_fingerprint": current_sources,
        "shared_target_dir": str(shared_target),
        "binary": str(saved_binary),
        "binary_sha256": binary_hash,
        "compiler_artifact_path": str(compiler_binary),
        "compiler_artifact_fresh": artifact["fresh"],
        "receipt": str(receipt_path),
        "receipt_sha256": sha256_file(receipt_path),
        "patch_sha256": expected_patch_hash,
        "source_freeze_sha256": actual_freeze_hash,
        "build_provenance_files": provenance_files,
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
    environment["FLOW_RECV_NAMED_BENCH_JSON"] = str(raw_path.resolve())
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
    oracle_sha256: str | None = None
    validation_error: str | None = None
    raw_hash: str | None = None
    if raw_path.is_file():
        raw = raw_path.read_bytes()
        raw_hash = sha256_bytes(raw)
        try:
            validated = validate_benchmark_json(raw)
            metrics = validated["metrics"]
            oracle_sha256 = validated["oracle_sha256"]
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
        "oracle_sha256": oracle_sha256,
        "attempt_dir": str(attempt_dir.resolve()),
    }
    _write_json(attempt_dir / "finished.json", result, exclusive=True)
    return result


def _comparison(pair_index: int, baseline: Mapping[str, Any], candidate: Mapping[str, Any]) -> dict[str, Any]:
    both_successful = baseline.get("successful") is True and candidate.get("successful") is True
    oracle_match = (
        both_successful
        and isinstance(baseline.get("oracle_sha256"), str)
        and baseline.get("oracle_sha256") == candidate.get("oracle_sha256")
    )
    valid = both_successful and oracle_match
    result: dict[str, Any] = {
        "pair_index": pair_index,
        "baseline_attempt": baseline["attempt_id"],
        "candidate_attempt": candidate["attempt_id"],
        "valid": valid,
        "cross_arm_oracle_match": oracle_match,
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
            "cross_arm_oracle": None if oracle_match else "baseline/candidate sample metadata differ",
        }
        return result
    for method, _, _, case_name in CASE_SPECS:
        metric_key = f"{method}|{case_name}"
        base_value = float(baseline["case_p50_ns_per_event"][metric_key])
        candidate_value = float(candidate["case_p50_ns_per_event"][metric_key])
        result["cases"][metric_key] = {
            "baseline_p50_ns_per_event": base_value,
            "candidate_p50_ns_per_event": candidate_value,
            "candidate_minus_baseline_ns_per_event": candidate_value - base_value,
            "candidate_over_baseline": candidate_value / base_value,
            "candidate_change_percent": (candidate_value / base_value - 1.0) * 100.0,
        }
    return result


def _summary(pairs: Sequence[Mapping[str, Any]]) -> dict[str, Any]:
    cases: dict[str, Any] = {}
    for method, _, _, case_name in CASE_SPECS:
        metric_key = f"{method}|{case_name}"
        values = [pair["cases"][metric_key] for pair in pairs if pair.get("valid") is True]
        deltas = [float(value["candidate_minus_baseline_ns_per_event"]) for value in values]
        ratios = [float(value["candidate_over_baseline"]) for value in values]
        cases[metric_key] = {
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
    if args.timeout_seconds != INVOCATION_TIMEOUT_SECONDS:
        raise BenchmarkError(f"invocation timeout is fixed at {INVOCATION_TIMEOUT_SECONDS} seconds")
    if not 1 <= args.total_budget_seconds <= MAX_TOTAL_BUDGET_SECONDS:
        raise BenchmarkError("total budget must be between 1 second and the 3 hour maximum")
    if not COMMIT_RE.fullmatch(args.baseline_commit) or not COMMIT_RE.fullmatch(args.candidate_commit):
        raise BenchmarkError("baseline and candidate commits must be full lowercase 40-character SHAs")
    if args.baseline_commit != SOURCE_FREEZE_BASE_COMMIT or args.candidate_commit != SOURCE_FREEZE_BASE_COMMIT:
        raise BenchmarkError("both source arms must remain based on the frozen c6f77a0 commit")

    baseline_root, _ = validate_checkout(Path(args.baseline_root), args.baseline_commit)
    candidate_root, _ = validate_checkout(Path(args.candidate_root), args.candidate_commit)
    if baseline_root == candidate_root:
        raise BenchmarkError("baseline and candidate must be distinct source checkouts")
    baseline_freeze, baseline_freeze_hash = verify_frozen_source_side(
        baseline_root, "baseline", Path(args.source_freeze)
    )
    candidate_freeze, candidate_freeze_hash = verify_frozen_source_side(
        candidate_root, "candidate", Path(args.source_freeze)
    )
    if baseline_freeze != candidate_freeze or baseline_freeze_hash != candidate_freeze_hash:
        raise BenchmarkError("baseline and candidate do not share the same frozen source record")
    baseline_patch_hash = sha256_file(Path(args.baseline_patch).expanduser().resolve(strict=True))
    candidate_patch_hash = sha256_file(Path(args.candidate_patch).expanduser().resolve(strict=True))
    require_sha256(
        baseline_patch_hash,
        EXPECTED_PATCH_SHA256_BY_SIDE["baseline"],
        "frozen baseline patch",
    )
    require_sha256(
        candidate_patch_hash,
        EXPECTED_PATCH_SHA256_BY_SIDE["candidate"],
        "frozen candidate patch",
    )
    baseline = _verify_build_receipt(
        "baseline",
        Path(args.baseline_receipt),
        baseline_root,
        args.baseline_commit,
        baseline_patch_hash,
        baseline_freeze_hash,
    )
    candidate = _verify_build_receipt(
        "candidate",
        Path(args.candidate_receipt),
        candidate_root,
        args.candidate_commit,
        candidate_patch_hash,
        candidate_freeze_hash,
    )
    if baseline["source_sha256"] == candidate["source_sha256"]:
        raise BenchmarkError("baseline and candidate source fingerprints are identical")
    if baseline["shared_target_dir"] != candidate["shared_target_dir"]:
        raise BenchmarkError("baseline and candidate receipts do not identify the sequential shared target")
    if baseline["binary"] == candidate["binary"]:
        raise BenchmarkError("baseline and candidate must use distinct immutable saved binary copies")

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
            "methods": list(METHODS),
            "case_order": [f"{method}|{case_name}" for method, _, _, case_name in CASE_SPECS],
            "checksum_per_case": EXPECTED_CHECKSUM,
            "warmup_pairs": WARMUP_PAIRS,
            "measured_pairs": MEASURED_PAIRS,
            "invocation_timeout_seconds": args.timeout_seconds,
            "total_budget_seconds": args.total_budget_seconds,
            "maximum_total_budget_seconds": MAX_TOTAL_BUDGET_SECONDS,
            "paired_order": "3 warmup pairs then 48 measured pairs; AB/BA alternation resets per phase; sequential invocations",
            "metric": "candidate p50 ns/event minus baseline p50 ns/event; positive values are losses",
            "source_freeze_sha256": baseline_freeze_hash,
            "baseline_patch_sha256": baseline_patch_hash,
            "candidate_patch_sha256": candidate_patch_hash,
            "harness_source_sha256": EXPECTED_HARNESS_SHA256,
            "build_provenance_policy": (
                "pinned sequential cargo clean/build workflow; Cargo fresh=false after clean; "
                "distinct immutable saved binaries with full hashes"
            ),
            "shared_target_dir": baseline["shared_target_dir"],
            "saved_binary_sha256s": {
                "baseline": baseline["binary_sha256"],
                "candidate": candidate["binary_sha256"],
            },
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
        "warmup_pairs": [],
        "measured_pairs": [],
        "unrun_attempts": [],
    }
    _write_json(report_path, report, exclusive=True)

    identities = {"baseline": baseline, "candidate": candidate}
    binaries = {side: identity["binary"] for side, identity in identities.items()}
    total_pairs = WARMUP_PAIRS + MEASURED_PAIRS
    overall_failed = False
    run_started = time.monotonic()
    run_deadline = run_started + args.total_budget_seconds
    budget_exhausted = False
    for pair_number in range(total_pairs):
        phase = "warmup" if pair_number < WARMUP_PAIRS else "measurement"
        phase_index = pair_number + 1 if phase == "warmup" else pair_number - WARMUP_PAIRS + 1
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
            remaining_seconds = run_deadline - time.monotonic()
            if remaining_seconds <= 0:
                report["unrun_attempts"].append(
                    {**metadata, "status": "not_run_total_budget_exhausted"}
                )
                report["unrun_attempts"].extend(_planned_slots_after(pair_number, position))
                budget_exhausted = True
                overall_failed = True
                break
            command = [
                binaries[side],
                "--exact",
                TEST_NAME,
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ]
            attempt_dir = report_dir / "attempts" / attempt_id
            effective_timeout = min(float(args.timeout_seconds), remaining_seconds)
            metadata["budget_limited_timeout"] = effective_timeout < args.timeout_seconds
            result = run_attempt(command, attempt_dir, metadata, effective_timeout)
            report["attempts"].append(result)
            pair_results[side] = result
            if not result["successful"]:
                overall_failed = True
            _write_json(report_path, report)

        if len(pair_results) != 2:
            pair_record = {
                "pair_index": phase_index,
                "order": list(order),
                "valid": False,
                "status": "incomplete_total_budget_exhausted",
                "attempt_ids_by_side": {
                    side: result["attempt_id"] for side, result in pair_results.items()
                },
            }
        else:
            pair_record = {
                "order": list(order),
                **_comparison(phase_index, pair_results["baseline"], pair_results["candidate"]),
            }
            if not pair_record["valid"]:
                overall_failed = True
        pair_record["phase"] = phase
        pair_list = report["warmup_pairs"] if phase == "warmup" else report["measured_pairs"]
        pair_list.append(pair_record)
        _write_json(report_path, report)
        if budget_exhausted:
            for later_pair_number in range(pair_number + 1, total_pairs):
                later_phase = "warmup" if later_pair_number < WARMUP_PAIRS else "measurement"
                later_phase_index = (
                    later_pair_number + 1
                    if later_phase == "warmup"
                    else later_pair_number - WARMUP_PAIRS + 1
                )
                later_order = interleaved_order(
                    later_pair_number
                    if later_phase == "warmup"
                    else later_phase_index - 1
                )
                report["unrun_attempts"].extend(
                    {
                        "phase": later_phase,
                        "pair_index": later_phase_index,
                        "position_in_pair": position,
                        "side": side,
                        "status": "not_run_total_budget_exhausted",
                    }
                    for position, side in enumerate(later_order, start=1)
                )
                later_pair = {
                    "phase": later_phase,
                    "pair_index": later_phase_index,
                    "order": list(later_order),
                    "valid": False,
                    "status": "not_run_total_budget_exhausted",
                }
                later_list = report["warmup_pairs"] if later_phase == "warmup" else report["measured_pairs"]
                later_list.append(later_pair)
            break

    post_state: dict[str, Any] = {}
    for side, identity in identities.items():
        try:
            post_root, post_source = validate_checkout(Path(identity["root"]), identity["commit"])
            post_receipt = _verify_build_receipt(
                side,
                Path(identity["receipt"]),
                post_root,
                identity["commit"],
                identity["patch_sha256"],
                baseline_freeze_hash,
            )
            stable = (
                post_source == identity["source_fingerprint"]
                and post_receipt["source_fingerprint"] == identity["source_fingerprint"]
                and post_receipt["binary"] == identity["binary"]
                and post_receipt["binary_sha256"] == identity["binary_sha256"]
                and post_receipt["receipt_sha256"] == identity["receipt_sha256"]
                and post_receipt["build_provenance_files"] == identity["build_provenance_files"]
            )
            post_state[side] = {
                "root": str(post_root),
                "source_sha256": post_receipt["source_sha256"],
                "binary": post_receipt["binary"],
                "binary_sha256": post_receipt["binary_sha256"],
                "receipt_sha256": post_receipt["receipt_sha256"],
                "build_provenance_files": post_receipt["build_provenance_files"],
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
    report["elapsed_total_seconds"] = time.monotonic() - run_started
    report["status"] = (
        "budget_exhausted"
        if budget_exhausted
        else "failed"
        if overall_failed
        else "complete"
    )
    report["attempt_count"] = len(report["attempts"])
    report["planned_attempt_count"] = 2 * total_pairs
    report["unrun_attempt_count"] = len(report["unrun_attempts"])
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

    select = commands.add_parser("select-binary", help="select the exact fresh release library-test artifact")
    select.add_argument("--cargo-json", required=True)
    select.add_argument("--manifest-path", required=True)
    select.add_argument("--target-source-path", required=True)
    select.add_argument("--target-dir", required=True)
    select.set_defaults(function=select_binary_command)

    run = commands.add_parser("run", help="run fixed paired baseline/candidate invocations")
    run.add_argument("--baseline-root", required=True)
    run.add_argument("--candidate-root", required=True)
    run.add_argument("--source-freeze", required=True)
    run.add_argument("--baseline-commit", required=True)
    run.add_argument("--candidate-commit", required=True)
    run.add_argument("--baseline-receipt", required=True)
    run.add_argument("--candidate-receipt", required=True)
    run.add_argument("--baseline-patch", required=True)
    run.add_argument("--candidate-patch", required=True)
    run.add_argument(
        "--timeout-seconds",
        type=int,
        default=INVOCATION_TIMEOUT_SECONDS,
        help="fixed at 120 seconds per invocation",
    )
    run.add_argument(
        "--total-budget-seconds",
        type=int,
        default=MAX_TOTAL_BUDGET_SECONDS,
        help="hard cap for the full 3 warmup + 48 measured paired run (maximum 10800)",
    )
    run.add_argument("--report-dir", required=True)
    run.set_defaults(function=run_pairs)
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    args = _parser().parse_args(argv)
    try:
        result = args.function(args)
        return int(result or 0)
    except (BenchmarkError, OSError, ValueError, KeyError) as error:
        print(f"graph named-receive benchmark: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
