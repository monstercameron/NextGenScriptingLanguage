#!/usr/bin/env python3
"""Preregister and run the frozen exact payload-size paired benchmark."""
from __future__ import annotations

import argparse
import hashlib
import json
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


FORMAT_VERSION = 1
TEST_NAME = "graph::transport::payload_size_perf_harness::payload_size_raw_samples_with_exact_oracle"
HARNESS_FORMAT = "execute.graph.payload_size_perf.v1"
HARNESS_METHOD = (
    "In-process payload_size calls only; immutable payload references and all case data are prepared before timing. "
    "Each sample has exactly 4096 calls and retains its raw duration and checksum. Exact serde_json::to_vec byte "
    "lengths are checked before and after each timed sample. Three unsorted samples are retained in execution order; "
    "no aggregation is performed."
)
CASE_NAMES = (
    "scalar_integer",
    "escape_heavy_unicode_control_text",
    "nested_record",
    "text_4k",
)
REPORT_FIELDS = {
    "format",
    "method",
    "calls_per_sample",
    "raw_samples",
    "warmup_calls_per_case",
    "cases",
}
CASE_FIELDS = {
    "name",
    "payload_kind",
    "input_payload_sha256",
    "serialized_bytes",
    "calls_per_sample",
    "warmup_calls",
    "samples_in_execution_order",
}
SAMPLE_FIELDS = {"sample_index", "duration_ns", "calls", "checksum"}
CALLS_PER_SAMPLE = 4096
WARMUP_CALLS_PER_CASE = 4096
RAW_SAMPLES = 3
WARMUP_PAIRS = 3
MEASURED_PAIRS = 48
INVOCATION_TIMEOUT_SECONDS = 60
MAX_JSON_BYTES = 4_000_000
U64_MAX = (1 << 64) - 1
SHA256_RE = re.compile(r"^[0-9a-f]{64}$")
COMMIT_RE = re.compile(r"^[0-9a-f]{40}$")
BUILD_INPUT_ROOTS = ("src/", "vendor/", ".cargo/")
BUILD_CONFIG_FILES = {
    "Cargo.toml",
    "Cargo.lock",
    "build.rs",
    "rust-toolchain",
    "rust-toolchain.toml",
    "rustfmt.toml",
}


class PairError(RuntimeError):
    """A preregistration, identity, execution, or harness-contract failure."""


def utc_now() -> str:
    return datetime.now(timezone.utc).isoformat(timespec="milliseconds").replace("+00:00", "Z")


def sha256_bytes(payload: bytes) -> str:
    return hashlib.sha256(payload).hexdigest()


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def _unique_object(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            raise ValueError(f"duplicate JSON key: {key}")
        result[key] = value
    return result


def _reject_constant(value: str) -> None:
    raise ValueError(f"non-finite JSON constant: {value}")


def parse_json(payload: bytes, label: str) -> Any:
    if len(payload) > MAX_JSON_BYTES:
        raise PairError(f"{label} exceeds {MAX_JSON_BYTES} bytes")
    try:
        return json.loads(
            payload.decode("utf-8"),
            object_pairs_hook=_unique_object,
            parse_constant=_reject_constant,
        )
    except (UnicodeDecodeError, json.JSONDecodeError, ValueError) as error:
        raise PairError(f"{label} is not strict UTF-8 JSON: {error}") from error


def load_json(path: Path, label: str) -> Any:
    try:
        payload = path.expanduser().resolve(strict=True).read_bytes()
    except OSError as error:
        raise PairError(f"cannot read {label}: {error}") from error
    return parse_json(payload, label)


def write_json(path: Path, value: Any, *, exclusive: bool = False) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    if exclusive and (path.exists() or path.is_symlink()):
        raise PairError(f"refusing to overwrite existing file: {path}")
    temporary = path.with_name(path.name + ".tmp")
    temporary.write_text(json.dumps(value, sort_keys=True, indent=2, allow_nan=False) + "\n", encoding="utf-8")
    temporary.replace(path)


def _git(root: Path, *args: str) -> bytes:
    try:
        result = subprocess.run(
            ["git", "-C", str(root), *args],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=20,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        raise PairError(f"cannot inspect Git checkout {root}: {error}") from error
    if result.returncode:
        raise PairError(result.stderr.decode("utf-8", errors="replace").strip())
    return result.stdout


def _is_build_input(path: str) -> bool:
    return path.startswith(BUILD_INPUT_ROOTS) or path in BUILD_CONFIG_FILES


def source_fingerprint(root: Path) -> dict[str, Any]:
    root = root.expanduser().resolve(strict=True)
    tracked = _git(root, "ls-files", "-z")
    paths = sorted(
        item.decode("utf-8")
        for item in tracked.split(b"\0")
        if item and _is_build_input(item.decode("utf-8"))
    )
    files: dict[str, str] = {}
    digest = hashlib.sha256()
    for relative in paths:
        path = root / relative
        if not path.is_file():
            raise PairError(f"tracked build input is missing: {relative}")
        file_hash = sha256_file(path)
        files[relative] = file_hash
        digest.update(relative.encode("utf-8"))
        digest.update(b"\0")
        digest.update(file_hash.encode("ascii"))
        digest.update(b"\n")
    if not files:
        raise PairError(f"no tracked build inputs found under {root}")
    return {"sha256": digest.hexdigest(), "files": files}


def _commit(root: Path) -> str:
    return _git(root, "rev-parse", "HEAD").decode("ascii").strip()


def _valid_hash(value: Any) -> bool:
    return isinstance(value, str) and SHA256_RE.fullmatch(value) is not None


def _validate_source_root(
    root: Path,
    expected_commit: str,
    expected_transport_hash: str,
    size_harness: bytes,
) -> dict[str, Any]:
    root = root.expanduser().resolve(strict=True)
    if _commit(root) != expected_commit:
        raise PairError(f"source root {root} does not have expected HEAD {expected_commit}")
    changed = _git(root, "diff", "--name-only").decode("utf-8").splitlines()
    staged = _git(root, "diff", "--cached", "--name-only").decode("utf-8").splitlines()
    untracked = [
        item.decode("utf-8")
        for item in _git(root, "ls-files", "--others", "--exclude-standard", "-z").split(b"\0")
        if item
    ]
    if staged or untracked or any(path != "src/graph/transport.rs" for path in changed):
        raise PairError(f"unexpected source-tree changes in {root}: {changed}, {staged}, {untracked}")
    transport = root / "src/graph/transport.rs"
    if not transport.is_file():
        raise PairError(f"missing frozen transport source: {transport}")
    transport_hash = sha256_file(transport)
    if transport_hash != expected_transport_hash:
        raise PairError(f"transport source hash mismatch at {transport}")
    if size_harness not in transport.read_bytes():
        raise PairError(f"frozen size harness bytes are absent from {transport}")
    return {
        "root": str(root),
        "commit": expected_commit,
        "transport_sha256": transport_hash,
        "fingerprint": source_fingerprint(root),
    }


def _receipt_identity(receipt_path: Path, label: str, freeze: Mapping[str, Any], size_harness: bytes) -> dict[str, Any]:
    receipt_path = receipt_path.expanduser().resolve(strict=True)
    receipt = load_json(receipt_path, f"{label} binary receipt")
    if not isinstance(receipt, dict) or not isinstance(receipt.get("artifact"), dict):
        raise PairError(f"{label} binary receipt has an unexpected schema")
    artifact = receipt["artifact"]
    target = artifact.get("target")
    profile = artifact.get("profile")
    if not isinstance(target, dict) or not isinstance(profile, dict):
        raise PairError(f"{label} receipt lacks Cargo target/profile metadata")
    kinds = target.get("kind")
    if (
        artifact.get("reason") != "compiler-artifact"
        or target.get("test") is not True
        or not isinstance(kinds, list)
        or "lib" not in kinds
        or profile.get("test") is not True
        or profile.get("opt_level") != "3"
        or profile.get("debug_assertions") is not False
        or artifact.get("fresh") is not False
        or receipt.get("source_stable") is not True
    ):
        raise PairError(f"{label} receipt is not a fresh optimized release library-test artifact")
    try:
        manifest = Path(artifact["manifest_path"]).expanduser().resolve(strict=True)
        root = manifest.parent
        source_file = Path(target["src_path"]).expanduser().resolve(strict=True)
    except (KeyError, OSError, TypeError, ValueError) as error:
        raise PairError(f"{label} receipt has invalid checkout paths: {error}") from error
    if manifest != (root / "Cargo.toml").resolve(strict=True) or source_file != (root / "src/lib.rs").resolve(strict=True):
        raise PairError(f"{label} receipt manifest/source paths do not identify one checkout")
    expected_transport = freeze.get("source_paths", {}).get(str(root / "src/graph/transport.rs"))
    if not _valid_hash(expected_transport) or receipt.get("source_sha256") != expected_transport:
        raise PairError(f"{label} receipt transport hash differs from source freeze")
    source = _validate_source_root(root, str(freeze["base_commit"]), expected_transport, size_harness)

    try:
        binary_path = Path(receipt["saved_binary"]).expanduser().resolve(strict=True)
    except (KeyError, OSError, TypeError, ValueError) as error:
        raise PairError(f"{label} saved binary path is invalid: {error}") from error
    binary_hash = receipt.get("sha256")
    if not _valid_hash(binary_hash) or sha256_file(binary_path) != binary_hash:
        raise PairError(f"{label} saved binary digest differs from its receipt")
    if not binary_path.is_file() or not os.access(binary_path, os.X_OK):
        raise PairError(f"{label} saved test binary is not executable")
    return {
        "label": label,
        "receipt_path": str(receipt_path),
        "receipt_sha256": sha256_file(receipt_path),
        "root": source["root"],
        "commit": source["commit"],
        "transport_sha256": source["transport_sha256"],
        "source_fingerprint": source["fingerprint"],
        "binary": str(binary_path),
        "binary_sha256": binary_hash,
    }


def _protocol() -> dict[str, Any]:
    return {
        "test_name": TEST_NAME,
        "harness_format": HARNESS_FORMAT,
        "warmup_pairs": WARMUP_PAIRS,
        "measured_pairs": MEASURED_PAIRS,
        "pair_order": "alternating baseline/candidate then candidate/baseline across all pairs; first measured pair is candidate/baseline",
        "calls_per_sample": CALLS_PER_SAMPLE,
        "warmup_calls_per_case": WARMUP_CALLS_PER_CASE,
        "raw_samples_per_case": RAW_SAMPLES,
        "case_order": list(CASE_NAMES),
        "invocation_timeout_seconds": INVOCATION_TIMEOUT_SECONDS,
        "invocation_environment": {
            "EXECUTE_PAYLOAD_SIZE_PERF_REPORT": "unique absolute path per attempt",
            "LC_ALL": "C",
            "TZ": "UTC",
        },
        "metric": "median of three raw duration_ns samples per case; paired candidate minus baseline; positive is a slowdown",
        "cross_arm_gate": "all four input payload hashes, exact serialized byte counts, and sample checksums must match",
        "no_result_dependent_retries": True,
        "no_timeout_retries": True,
    }


def prepare(args: argparse.Namespace) -> None:
    freeze_path = Path(args.source_freeze).expanduser().resolve(strict=True)
    freeze = load_json(freeze_path, "source freeze")
    if not isinstance(freeze, dict) or freeze.get("identical_harness") is not True:
        raise PairError("source freeze does not attest to identical harness bytes")
    base_commit = freeze.get("base_commit")
    if not isinstance(base_commit, str) or COMMIT_RE.fullmatch(base_commit) is None:
        raise PairError("source freeze does not identify a full baseline commit")
    harness_path = Path(args.harness_source).expanduser().resolve(strict=True)
    size_harness = harness_path.read_bytes()
    harness_hash = sha256_bytes(size_harness)
    if freeze.get("size_harness_sha256") != harness_hash:
        raise PairError("size harness source SHA-256 differs from source freeze")
    baseline = _receipt_identity(Path(args.baseline_receipt), "baseline", freeze, size_harness)
    candidate = _receipt_identity(Path(args.candidate_receipt), "candidate", freeze, size_harness)
    if baseline["root"] == candidate["root"] or baseline["binary"] == candidate["binary"]:
        raise PairError("baseline and candidate must have distinct roots and saved binaries")
    if baseline["transport_sha256"] == candidate["transport_sha256"]:
        raise PairError("baseline and candidate source bytes are unexpectedly identical")

    script = Path(__file__).resolve(strict=True)
    preregistration = {
        "schema_version": FORMAT_VERSION,
        "protocol": _protocol(),
        "inputs": {
            "source_freeze": {"path": str(freeze_path), "sha256": sha256_file(freeze_path)},
            "size_harness": {"path": str(harness_path), "sha256": harness_hash},
            "baseline": baseline,
            "candidate": candidate,
        },
        "driver": {"path": str(script), "sha256": sha256_file(script)},
        "prepared_at": utc_now(),
    }
    destination = Path(args.preregistration).expanduser().absolute()
    write_json(destination, preregistration, exclusive=True)
    print(json.dumps({"preregistration": str(destination), "sha256": sha256_file(destination)}, sort_keys=True))


def validate_harness_report(payload: bytes) -> dict[str, Any]:
    report = parse_json(payload, "payload-size harness report")
    if not isinstance(report, dict) or set(report) != REPORT_FIELDS:
        raise PairError("harness report has an unexpected top-level schema")
    if report["format"] != HARNESS_FORMAT or report["method"] != HARNESS_METHOD:
        raise PairError("harness report format/method is unexpected")
    for key, expected in (
        ("calls_per_sample", CALLS_PER_SAMPLE),
        ("raw_samples", RAW_SAMPLES),
        ("warmup_calls_per_case", WARMUP_CALLS_PER_CASE),
    ):
        if type(report[key]) is not int or report[key] != expected:
            raise PairError(f"harness report has unexpected {key}")
    cases = report["cases"]
    if not isinstance(cases, list) or len(cases) != len(CASE_NAMES):
        raise PairError("harness report must contain exactly four cases")

    durations_by_case: dict[str, list[int]] = {}
    oracle_by_case: dict[str, dict[str, Any]] = {}
    for case, expected_name in zip(cases, CASE_NAMES):
        if not isinstance(case, dict) or set(case) != CASE_FIELDS:
            raise PairError("case record has an unexpected schema")
        if case["name"] != expected_name or case["payload_kind"] != "typed_value":
            raise PairError(f"unexpected case identity: {case.get('name')}")
        input_hash = case["input_payload_sha256"]
        if not _valid_hash(input_hash):
            raise PairError(f"case {expected_name} has invalid input SHA-256")
        size = case["serialized_bytes"]
        if type(size) is not int or not 0 < size <= U64_MAX // CALLS_PER_SAMPLE:
            raise PairError(f"case {expected_name} has invalid serialized byte count")
        for key, expected in (
            ("calls_per_sample", CALLS_PER_SAMPLE),
            ("warmup_calls", WARMUP_CALLS_PER_CASE),
        ):
            if type(case[key]) is not int or case[key] != expected:
                raise PairError(f"case {expected_name} has unexpected {key}")
        samples = case["samples_in_execution_order"]
        if not isinstance(samples, list) or len(samples) != RAW_SAMPLES:
            raise PairError(f"case {expected_name} must contain exactly three raw samples")
        durations: list[int] = []
        checksums: list[int] = []
        for index, sample in enumerate(samples):
            if not isinstance(sample, dict) or set(sample) != SAMPLE_FIELDS:
                raise PairError(f"case {expected_name} sample has an unexpected schema")
            if type(sample["sample_index"]) is not int or sample["sample_index"] != index:
                raise PairError(f"case {expected_name} sample order is invalid")
            duration = sample["duration_ns"]
            if type(duration) is not int or not 0 < duration <= U64_MAX:
                raise PairError(f"case {expected_name} sample has invalid duration_ns")
            if type(sample["calls"]) is not int or sample["calls"] != CALLS_PER_SAMPLE:
                raise PairError(f"case {expected_name} sample has unexpected calls")
            checksum = sample["checksum"]
            if type(checksum) is not int or checksum != size * CALLS_PER_SAMPLE:
                raise PairError(f"case {expected_name} sample checksum disagrees with byte oracle")
            durations.append(duration)
            checksums.append(checksum)
        durations_by_case[expected_name] = durations
        oracle_by_case[expected_name] = {
            "input_payload_sha256": input_hash,
            "serialized_bytes": size,
            "sample_checksums": checksums,
        }
    return {
        "median_duration_ns": {name: statistics.median(durations_by_case[name]) for name in CASE_NAMES},
        "raw_duration_ns": durations_by_case,
        "oracle": oracle_by_case,
    }


def pair_order(global_pair_index: int) -> tuple[str, str]:
    return ("baseline", "candidate") if global_pair_index % 2 == 0 else ("candidate", "baseline")


def run_attempt(identity: Mapping[str, Any], attempt_dir: Path, metadata: Mapping[str, Any]) -> dict[str, Any]:
    attempt_dir.mkdir(parents=True, exist_ok=False)
    raw_path = attempt_dir / "raw.json"
    binary = Path(identity["binary"])
    transport = Path(identity["root"]) / "src/graph/transport.rs"
    expected_binary_hash = str(identity["binary_sha256"])
    expected_transport_hash = str(identity["transport_sha256"])
    binary_before = sha256_file(binary) if binary.is_file() else None
    transport_before = sha256_file(transport) if transport.is_file() else None
    command = [str(binary), "--exact", TEST_NAME, "--ignored", "--nocapture", "--test-threads=1"]
    environment = os.environ.copy()
    environment.update(
        {
            "EXECUTE_PAYLOAD_SIZE_PERF_REPORT": str(raw_path.resolve()),
            "LC_ALL": "C",
            "TZ": "UTC",
        }
    )
    recorded_environment = {
        "EXECUTE_PAYLOAD_SIZE_PERF_REPORT": str(raw_path.resolve()),
        "LC_ALL": "C",
        "TZ": "UTC",
    }
    write_json(
        attempt_dir / "started.json",
        {
            **dict(metadata),
            "started_at": utc_now(),
            "command": command,
            "timeout_seconds": INVOCATION_TIMEOUT_SECONDS,
            "environment": recorded_environment,
            "binary_sha256_before": binary_before,
            "transport_sha256_before": transport_before,
            "raw_report_path": str(raw_path.resolve()),
        },
        exclusive=True,
    )

    start_ns = time.monotonic_ns()
    returncode: int | None = None
    timed_out = False
    execution_error: str | None = None
    stdout = b""
    stderr = b""
    if binary_before != expected_binary_hash:
        execution_error = "saved binary SHA-256 mismatched before invocation"
    elif transport_before != expected_transport_hash:
        execution_error = "frozen transport SHA-256 mismatched before invocation"
    else:
        try:
            completed = subprocess.run(
                command,
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                env=environment,
                timeout=INVOCATION_TIMEOUT_SECONDS,
                check=False,
            )
            returncode = completed.returncode
            stdout, stderr = completed.stdout, completed.stderr
        except subprocess.TimeoutExpired as error:
            timed_out = True
            stdout, stderr = error.stdout or b"", error.stderr or b""
        except OSError as error:
            execution_error = f"{type(error).__name__}: {error}"
            stderr = execution_error.encode("utf-8", errors="replace")
    elapsed_wall_ns = time.monotonic_ns() - start_ns
    binary_after = sha256_file(binary) if binary.is_file() else None
    transport_after = sha256_file(transport) if transport.is_file() else None
    if binary_after != expected_binary_hash:
        execution_error = "saved binary SHA-256 changed during or after invocation"
    if transport_after != expected_transport_hash:
        execution_error = "frozen transport SHA-256 changed during or after invocation"
    (attempt_dir / "stdout.bin").write_bytes(stdout)
    (attempt_dir / "stderr.bin").write_bytes(stderr)

    parsed: dict[str, Any] | None = None
    raw_hash: str | None = None
    validation_error: str | None = None
    if raw_path.is_file():
        raw_bytes = raw_path.read_bytes()
        raw_hash = sha256_bytes(raw_bytes)
        try:
            parsed = validate_harness_report(raw_bytes)
        except PairError as error:
            validation_error = str(error)
    else:
        validation_error = "harness did not create a raw report"
    successful = returncode == 0 and not timed_out and execution_error is None and validation_error is None
    finished = {
        **dict(metadata),
        "finished_at": utc_now(),
        "returncode": returncode,
        "timed_out": timed_out,
        "execution_error": execution_error,
        "elapsed_wall_ns": elapsed_wall_ns,
        "binary_sha256_before": binary_before,
        "binary_sha256_after": binary_after,
        "transport_sha256_before": transport_before,
        "transport_sha256_after": transport_after,
        "raw_report_present": raw_path.is_file(),
        "raw_report_sha256": raw_hash,
        "validation_error": validation_error,
        "successful": successful,
        "median_duration_ns": parsed["median_duration_ns"] if parsed else None,
        "raw_duration_ns": parsed["raw_duration_ns"] if parsed else None,
        "oracle": parsed["oracle"] if parsed else None,
        "stdout_sha256": sha256_bytes(stdout),
        "stderr_sha256": sha256_bytes(stderr),
        "attempt_dir": str(attempt_dir.resolve()),
    }
    write_json(attempt_dir / "finished.json", finished, exclusive=True)
    return finished


def _pair_result(pair_index: int, baseline: Mapping[str, Any], candidate: Mapping[str, Any]) -> dict[str, Any]:
    oracle_equal = (
        baseline.get("oracle") == candidate.get("oracle")
        if baseline.get("successful") is True and candidate.get("successful") is True
        else False
    )
    valid = baseline.get("successful") is True and candidate.get("successful") is True and oracle_equal
    result: dict[str, Any] = {
        "pair_index": pair_index,
        "baseline_attempt_id": baseline.get("attempt_id"),
        "candidate_attempt_id": candidate.get("attempt_id"),
        "valid_for_timing_comparison": valid,
        "input_hash_checksum_and_size_equal": oracle_equal,
        "cases": {},
    }
    if not valid:
        result["failure_reasons"] = {
            "baseline": baseline.get("validation_error") or baseline.get("execution_error"),
            "candidate": candidate.get("validation_error") or candidate.get("execution_error"),
            "oracle_mismatch": baseline.get("successful") is True and candidate.get("successful") is True and not oracle_equal,
        }
        return result
    for name in CASE_NAMES:
        baseline_ns = int(baseline["median_duration_ns"][name])
        candidate_ns = int(candidate["median_duration_ns"][name])
        result["cases"][name] = {
            "baseline_median_duration_ns": baseline_ns,
            "candidate_median_duration_ns": candidate_ns,
            "candidate_minus_baseline_ns": candidate_ns - baseline_ns,
            "baseline_raw_duration_ns": baseline["raw_duration_ns"][name],
            "candidate_raw_duration_ns": candidate["raw_duration_ns"][name],
        }
    return result


def _summary(pairs: Sequence[Mapping[str, Any]]) -> dict[str, Any]:
    summary: dict[str, Any] = {}
    for name in CASE_NAMES:
        all_deltas: list[dict[str, Any]] = []
        valid_deltas: list[int] = []
        for pair in pairs:
            if pair.get("valid_for_timing_comparison") is True:
                delta = int(pair["cases"][name]["candidate_minus_baseline_ns"])
                valid_deltas.append(delta)
                all_deltas.append({"pair_index": pair["pair_index"], "candidate_minus_baseline_ns": delta})
            else:
                all_deltas.append({"pair_index": pair["pair_index"], "candidate_minus_baseline_ns": None})
        summary[name] = {
            "valid_pair_count": len(valid_deltas),
            "paired_deltas_in_order": all_deltas,
            "median_paired_delta_ns": statistics.median(valid_deltas) if valid_deltas else None,
            "candidate_loss_pair_indices": [item["pair_index"] for item in all_deltas if item["candidate_minus_baseline_ns"] is not None and item["candidate_minus_baseline_ns"] > 0],
            "candidate_improvement_pair_indices": [item["pair_index"] for item in all_deltas if item["candidate_minus_baseline_ns"] is not None and item["candidate_minus_baseline_ns"] < 0],
            "tie_pair_indices": [item["pair_index"] for item in all_deltas if item["candidate_minus_baseline_ns"] == 0],
        }
    return summary


def _verify_preregistration(path: Path) -> dict[str, Any]:
    prereg = load_json(path, "preregistration")
    if not isinstance(prereg, dict) or prereg.get("schema_version") != FORMAT_VERSION:
        raise PairError("preregistration has an unexpected schema")
    driver = prereg.get("driver", {})
    if driver.get("sha256") != sha256_file(Path(__file__).resolve(strict=True)):
        raise PairError("runner source changed after preregistration")
    if prereg.get("protocol") != _protocol():
        raise PairError("preregistered protocol differs from this runner")
    inputs = prereg.get("inputs", {})
    freeze_ref = inputs.get("source_freeze", {})
    harness_ref = inputs.get("size_harness", {})
    freeze_path = Path(freeze_ref.get("path", "")).expanduser().resolve(strict=True)
    harness_path = Path(harness_ref.get("path", "")).expanduser().resolve(strict=True)
    if sha256_file(freeze_path) != freeze_ref.get("sha256"):
        raise PairError("source freeze changed after preregistration")
    size_harness = harness_path.read_bytes()
    if sha256_file(harness_path) != harness_ref.get("sha256"):
        raise PairError("size harness changed after preregistration")
    freeze = load_json(freeze_path, "source freeze")
    for side in ("baseline", "candidate"):
        expected = inputs.get(side, {})
        current = _receipt_identity(Path(expected.get("receipt_path", "")), side, freeze, size_harness)
        if current != expected:
            raise PairError(f"{side} receipt/source identity changed after preregistration")
    return prereg


def run(args: argparse.Namespace) -> int:
    prereg_path = Path(args.preregistration).expanduser().resolve(strict=True)
    prereg = _verify_preregistration(prereg_path)
    report_dir = Path(args.report_dir).expanduser().absolute()
    if report_dir.exists() or report_dir.is_symlink():
        raise PairError(f"refusing to overwrite report directory: {report_dir}")
    report_dir.mkdir(parents=True, exist_ok=False)
    inputs = prereg["inputs"]
    initial_identity = {
        side: {
            "root": inputs[side]["root"],
            "commit": inputs[side]["commit"],
            "transport_sha256": sha256_file(Path(inputs[side]["root"]) / "src/graph/transport.rs"),
            "source_fingerprint": source_fingerprint(Path(inputs[side]["root"])),
            "binary_sha256": sha256_file(Path(inputs[side]["binary"])),
            "receipt_sha256": sha256_file(Path(inputs[side]["receipt_path"])),
        }
        for side in ("baseline", "candidate")
    }
    report: dict[str, Any] = {
        "schema_version": FORMAT_VERSION,
        "format": "execute.graph.payload_size_paired_run.v1",
        "created_at": utc_now(),
        "status": "running",
        "preregistration_path": str(prereg_path),
        "preregistration_sha256": sha256_file(prereg_path),
        "protocol": prereg["protocol"],
        "host": {"system": platform.system(), "release": platform.release(), "machine": platform.machine(), "python": sys.version},
        "initial_identity": initial_identity,
        "attempts": [],
        "warmup_pairs": [],
        "measured_pairs": [],
    }
    report_path = report_dir / "paired-report.json"
    write_json(report_path, report, exclusive=True)
    overall_failed = False

    for global_pair in range(WARMUP_PAIRS + MEASURED_PAIRS):
        phase = "warmup" if global_pair < WARMUP_PAIRS else "measurement"
        phase_index = global_pair + 1 if phase == "warmup" else global_pair - WARMUP_PAIRS + 1
        order = pair_order(global_pair)
        results: dict[str, dict[str, Any]] = {}
        for position, side in enumerate(order, start=1):
            identity = inputs[side]
            attempt_id = f"{phase}-{phase_index:02d}-{position}-{side}"
            metadata = {
                "attempt_id": attempt_id,
                "phase": phase,
                "phase_pair_index": phase_index,
                "global_pair_index": global_pair,
                "position_in_pair": position,
                "side": side,
                "commit": identity["commit"],
                "transport_sha256": identity["transport_sha256"],
                "source_fingerprint_sha256": identity["source_fingerprint"]["sha256"],
                "binary_sha256": identity["binary_sha256"],
            }
            attempt_dir = report_dir / "attempts" / attempt_id
            try:
                result = run_attempt(identity, attempt_dir, metadata)
            except (PairError, OSError) as error:
                result = {
                    **metadata,
                    "finished_at": utc_now(),
                    "successful": False,
                    "returncode": None,
                    "timed_out": False,
                    "execution_error": f"{type(error).__name__}: {error}",
                    "attempt_dir": str(attempt_dir.resolve()),
                }
            report["attempts"].append(result)
            results[side] = result
            if result.get("successful") is not True:
                overall_failed = True
            write_json(report_path, report)

        pair = {
            "pair_index": phase_index,
            "global_pair_index": global_pair,
            "order": list(order),
            **_pair_result(phase_index, results["baseline"], results["candidate"]),
        }
        if pair["valid_for_timing_comparison"] is not True:
            overall_failed = True
        report["warmup_pairs" if phase == "warmup" else "measured_pairs"].append(pair)
        write_json(report_path, report)

    harness = Path(inputs["size_harness"]["path"]).read_bytes()
    post_identity: dict[str, Any] = {}
    for side in ("baseline", "candidate"):
        expected = inputs[side]
        try:
            source = _validate_source_root(
                Path(expected["root"]),
                expected["commit"],
                expected["transport_sha256"],
                harness,
            )
            binary_hash = sha256_file(Path(expected["binary"]))
            receipt_hash = sha256_file(Path(expected["receipt_path"]))
            stable = (
                source["fingerprint"] == expected["source_fingerprint"]
                and binary_hash == expected["binary_sha256"]
                and receipt_hash == expected["receipt_sha256"]
            )
            post_identity[side] = {
                "transport_sha256": source["transport_sha256"],
                "source_fingerprint": source["fingerprint"],
                "binary_sha256": binary_hash,
                "receipt_sha256": receipt_hash,
                "stable": stable,
            }
            overall_failed |= not stable
        except (PairError, OSError) as error:
            post_identity[side] = {"stable": False, "error": f"{type(error).__name__}: {error}"}
            overall_failed = True

    report["post_run_identity"] = post_identity
    report["paired_summary"] = _summary(report["measured_pairs"])
    report["completed_at"] = utc_now()
    report["attempt_count"] = len(report["attempts"])
    report["successful_attempt_count"] = sum(item.get("successful") is True for item in report["attempts"])
    report["failed_attempt_count"] = report["attempt_count"] - report["successful_attempt_count"]
    valid_measured = all(pair.get("valid_for_timing_comparison") is True for pair in report["measured_pairs"])
    report["status"] = "complete" if not overall_failed and valid_measured else "failed"
    write_json(report_path, report)
    print(json.dumps({"report": str(report_path), "status": report["status"]}, sort_keys=True))
    return 0 if report["status"] == "complete" else 1


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    prepare_parser = commands.add_parser("prepare", help="freeze receipts, source, harness, and schedule before any timing")
    prepare_parser.add_argument("--source-freeze", required=True)
    prepare_parser.add_argument("--baseline-receipt", required=True)
    prepare_parser.add_argument("--candidate-receipt", required=True)
    prepare_parser.add_argument("--harness-source", required=True)
    prepare_parser.add_argument("--preregistration", required=True)
    prepare_parser.set_defaults(function=prepare)
    run_parser = commands.add_parser("run", help="run the preregistered 3-warmup/48-pair sequence")
    run_parser.add_argument("--preregistration", required=True)
    run_parser.add_argument("--report-dir", required=True)
    run_parser.set_defaults(function=run)
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    args = _parser().parse_args(argv)
    try:
        result = args.function(args)
        return int(result or 0)
    except (PairError, OSError, ValueError) as error:
        print(f"payload-size paired runner: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
