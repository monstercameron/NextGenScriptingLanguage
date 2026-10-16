#!/usr/bin/env python3
"""Preregister and run the frozen local graph emit paired benchmark."""
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

try:
    import resource
except ImportError:  # pragma: no cover - Windows has no resource module
    resource = None


FORMAT_VERSION = 1
TEST_NAME = "graph::transport::emit_perf_harness::emit_raw_samples_with_full_oracle"
HARNESS_FORMAT = "execute.graph.emit_perf.v2"
EVENT_FIELDS = {
    "repetition",
    "warmup",
    "duration_ns",
    "input_payloads_sha256",
    "delivered",
    "dropped",
    "exact_correctness_oracle",
}
CASE_FIELDS = {
    "name",
    "payload_kind",
    "sink_count",
    "events",
    "remote_metadata",
    "queue_policy",
    "queue_capacity_per_sink",
    "queue_storage_preallocated",
    "input_payloads_sha256",
    "samples_in_execution_order",
}
ORACLE_FIELDS = {"sink", "event_count", "canonical_stream_sha256", "representative_event"}
REPORT_FIELDS = {"format", "method", "warmup_repetitions", "measured_repetitions", "cases"}
CASES = (
    {"name": "single_sink_integer", "payload_kind": "integer", "sink_count": 1, "events": 256, "remote_metadata": False},
    {"name": "fanout_8_integer", "payload_kind": "integer", "sink_count": 8, "events": 128, "remote_metadata": False},
    {"name": "fanout_4_text", "payload_kind": "text", "sink_count": 4, "events": 16, "remote_metadata": True},
    {"name": "fanout_4_typed_record", "payload_kind": "typed_record", "sink_count": 4, "events": 16, "remote_metadata": True},
)
WARMUP_PAIRS = 3
MEASURED_PAIRS = 48
INNER_WARMUPS = 0
INNER_SAMPLES = 3
INVOCATION_TIMEOUT_SECONDS = 60
HARNESS_SHA256 = "098db9038e8e8cc684eb6533d3fb60aaa33184f3e16cc9e9a028ac2c860a48f3"
COMMIT_RE = re.compile(r"^[0-9a-f]{40}$")
SHA256_RE = re.compile(r"^[0-9a-f]{64}$")
MAX_JSON_BYTES = 4_000_000
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
    """A frozen-input, execution, or harness-contract failure."""


def utc_now() -> str:
    return datetime.now(timezone.utc).isoformat(timespec="milliseconds").replace("+00:00", "Z")


def sha256_bytes(value: bytes) -> str:
    return hashlib.sha256(value).hexdigest()


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for block in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def _unique_object(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            raise ValueError(f"duplicate JSON key: {key}")
        result[key] = value
    return result


def _reject_json_constant(value: str) -> None:
    raise ValueError(f"non-finite JSON constant: {value}")


def load_json(path: Path, label: str) -> Any:
    try:
        payload = path.expanduser().resolve(strict=True).read_bytes()
    except OSError as error:
        raise PairError(f"cannot read {label}: {error}") from error
    if len(payload) > MAX_JSON_BYTES:
        raise PairError(f"{label} exceeds {MAX_JSON_BYTES} bytes")
    try:
        return json.loads(
            payload.decode("utf-8"),
            object_pairs_hook=_unique_object,
            parse_constant=_reject_json_constant,
        )
    except (UnicodeDecodeError, json.JSONDecodeError, ValueError) as error:
        raise PairError(f"{label} is not strict UTF-8 JSON: {error}") from error


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


def is_build_input(path: str) -> bool:
    return path.startswith(BUILD_INPUT_ROOTS) or path in BUILD_CONFIG_FILES


def source_fingerprint(root: Path) -> dict[str, Any]:
    root = root.expanduser().resolve(strict=True)
    raw_paths = _git(root, "ls-files", "-z")
    paths = sorted(
        raw_path.decode("utf-8")
        for raw_path in raw_paths.split(b"\0")
        if raw_path and is_build_input(raw_path.decode("utf-8"))
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


def _validate_source_root(root: Path, expected_commit: str, expected_transport_hash: str, harness: bytes) -> dict[str, Any]:
    root = root.expanduser().resolve(strict=True)
    if _commit(root) != expected_commit:
        raise PairError(f"source root {root} does not have expected HEAD {expected_commit}")
    changed = _git(root, "diff", "--name-only").decode("utf-8").splitlines()
    staged = _git(root, "diff", "--cached", "--name-only").decode("utf-8").splitlines()
    untracked = _git(root, "ls-files", "--others", "--exclude-standard", "-z").split(b"\0")
    untracked = [path.decode("utf-8") for path in untracked if path]
    if staged or untracked or any(path != "src/graph/transport.rs" for path in changed):
        raise PairError(f"unexpected source-tree changes in {root}: {changed}, {staged}, {untracked}")
    transport = root / "src/graph/transport.rs"
    transport_bytes = transport.read_bytes()
    actual_transport_hash = sha256_bytes(transport_bytes)
    if actual_transport_hash != expected_transport_hash:
        raise PairError(f"frozen transport source hash mismatch at {transport}")
    if harness not in transport_bytes:
        raise PairError(f"frozen benchmark harness bytes are absent from {transport}")
    return {
        "root": str(root),
        "commit": expected_commit,
        "transport_sha256": actual_transport_hash,
        "fingerprint": source_fingerprint(root),
    }


def _valid_hash(value: Any) -> bool:
    return isinstance(value, str) and SHA256_RE.fullmatch(value) is not None


def _protocol() -> dict[str, Any]:
    return {
        "test_name": TEST_NAME,
        "harness_format": HARNESS_FORMAT,
        "harness_sha256": HARNESS_SHA256,
        "warmup_pairs": WARMUP_PAIRS,
        "measured_pairs": MEASURED_PAIRS,
        "pair_order": "alternating AB/BA across the three warmup pairs and 48 measured pairs; first measured pair is BA",
        "inner_harness_warmups": INNER_WARMUPS,
        "inner_harness_samples": INNER_SAMPLES,
        "invocation_timeout_seconds": INVOCATION_TIMEOUT_SECONDS,
        "invocation_environment": {
            "EXECUTE_EMIT_PERF_WARMUPS": str(INNER_WARMUPS),
            "EXECUTE_EMIT_PERF_SAMPLES": str(INNER_SAMPLES),
            "LC_ALL": "C",
            "TZ": "UTC",
        },
        "metric": "median of the three inner duration_ns samples per case; paired candidate minus baseline; positive is a candidate slowdown",
        "cross_arm_correctness": "all four cases must match input payload SHA-256 and each sink event count, length-prefixed full-stream SHA-256, and representative full envelope; harness also checks every envelope by value and exact serialized bytes",
        "no_result_dependent_retries": True,
        "no_timeout_retries": True,
    }


def _receipt_identity(receipt_path: Path, label: str, freeze: Mapping[str, Any], harness: bytes) -> dict[str, Any]:
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
    ):
        raise PairError(f"{label} receipt is not a fresh optimized release library-test artifact")
    try:
        manifest = Path(artifact["manifest_path"]).expanduser().resolve(strict=True)
        root = manifest.parent
        target_source = Path(target["src_path"]).expanduser().resolve(strict=True)
    except (KeyError, OSError, TypeError, ValueError) as error:
        raise PairError(f"{label} receipt has invalid checkout paths: {error}") from error
    if manifest != (root / "Cargo.toml").resolve(strict=True) or target_source != (root / "src/lib.rs").resolve(strict=True):
        raise PairError(f"{label} receipt manifest/source paths do not identify one checkout")

    expected_transport = freeze.get("source_paths", {}).get(str(root / "src/graph/transport.rs"))
    if not _valid_hash(expected_transport) or receipt.get("source_sha256") != expected_transport:
        raise PairError(f"{label} receipt transport hash differs from source freeze")
    source = _validate_source_root(root, str(freeze["base_commit"]), expected_transport, harness)

    binary_path = Path(receipt.get("saved_binary", "")).expanduser().resolve(strict=True)
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


def prepare(args: argparse.Namespace) -> None:
    freeze_path = Path(args.source_freeze).expanduser().resolve(strict=True)
    freeze = load_json(freeze_path, "source freeze")
    if not isinstance(freeze, dict) or freeze.get("identical_harness") is not True:
        raise PairError("source freeze does not attest to an identical harness")
    if not COMMIT_RE.fullmatch(str(freeze.get("base_commit", ""))):
        raise PairError("source freeze does not identify a full baseline commit")
    harness_path = Path(args.harness_source).expanduser().resolve(strict=True)
    harness = harness_path.read_bytes()
    harness_hash = sha256_bytes(harness)
    if harness_hash != HARNESS_SHA256 or freeze.get("harness_sha256") != harness_hash:
        raise PairError("frozen harness source SHA-256 does not match the preregistered digest")
    baseline = _receipt_identity(Path(args.baseline_receipt), "baseline", freeze, harness)
    candidate = _receipt_identity(Path(args.candidate_receipt), "candidate", freeze, harness)
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
            "harness_source": {"path": str(harness_path), "sha256": harness_hash},
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
    if len(payload) > MAX_JSON_BYTES:
        raise PairError(f"harness report exceeds {MAX_JSON_BYTES} bytes")
    try:
        report = json.loads(
            payload.decode("utf-8"),
            object_pairs_hook=_unique_object,
            parse_constant=_reject_json_constant,
        )
    except (UnicodeDecodeError, json.JSONDecodeError, ValueError) as error:
        raise PairError(f"harness report is not strict UTF-8 JSON: {error}") from error
    if not isinstance(report, dict) or set(report) != REPORT_FIELDS:
        raise PairError("harness report has an unexpected top-level schema")
    if report["format"] != HARNESS_FORMAT or not isinstance(report["method"], str):
        raise PairError("harness report format/method is unexpected")
    if (
        type(report["warmup_repetitions"]) is not int
        or report["warmup_repetitions"] != INNER_WARMUPS
        or type(report["measured_repetitions"]) is not int
        or report["measured_repetitions"] != INNER_SAMPLES
    ):
        raise PairError("harness report has unexpected inner warmup/sample counts")
    cases = report["cases"]
    if not isinstance(cases, list) or len(cases) != len(CASES):
        raise PairError("harness report must contain exactly the four frozen cases")
    metrics: dict[str, float] = {}
    oracles: dict[str, Any] = {}
    for record, expected_case in zip(cases, CASES):
        if not isinstance(record, dict) or set(record) != CASE_FIELDS:
            raise PairError("case record has an unexpected schema")
        for key, expected_value in expected_case.items():
            if type(record.get(key)) is not type(expected_value) or record.get(key) != expected_value:
                raise PairError(f"case {expected_case['name']} has unexpected {key}")
        if (
            record.get("queue_policy") != "lossless"
            or record.get("queue_capacity_per_sink") != expected_case["events"]
            or record.get("queue_storage_preallocated") is not True
            or not _valid_hash(record.get("input_payloads_sha256"))
        ):
            raise PairError(f"case {expected_case['name']} has invalid queue/input metadata")
        samples = record["samples_in_execution_order"]
        if not isinstance(samples, list) or len(samples) != INNER_SAMPLES:
            raise PairError(f"case {expected_case['name']} must contain exactly three raw samples")
        durations: list[int] = []
        canonical_oracle: Any = None
        for repetition, sample in enumerate(samples):
            if not isinstance(sample, dict) or set(sample) != EVENT_FIELDS:
                raise PairError(f"case {expected_case['name']} sample has an unexpected schema")
            sample_contract = {
                "repetition": repetition,
                "warmup": False,
                "input_payloads_sha256": record["input_payloads_sha256"],
                "delivered": expected_case["events"] * expected_case["sink_count"],
                "dropped": 0,
            }
            if any(
                type(sample.get(key)) is not type(expected_value) or sample.get(key) != expected_value
                for key, expected_value in sample_contract.items()
            ):
                raise PairError(f"case {expected_case['name']} sample {repetition} violates its contract")
            duration = sample["duration_ns"]
            if not isinstance(duration, int) or isinstance(duration, bool) or not 0 < duration <= (1 << 64) - 1:
                raise PairError(f"case {expected_case['name']} sample {repetition} has invalid duration_ns")
            durations.append(duration)
            oracle = sample["exact_correctness_oracle"]
            if not isinstance(oracle, list) or len(oracle) != expected_case["sink_count"]:
                raise PairError(f"case {expected_case['name']} sample {repetition} has invalid sink oracle")
            for sink_index, sink_record in enumerate(oracle):
                if not isinstance(sink_record, dict) or set(sink_record) != ORACLE_FIELDS:
                    raise PairError(f"case {expected_case['name']} sample {repetition} has invalid sink record")
                if (
                    sink_record["sink"] != f"sink_{sink_index:02}.in"
                    or type(sink_record["event_count"]) is not int
                    or sink_record["event_count"] != expected_case["events"]
                    or not _valid_hash(sink_record["canonical_stream_sha256"])
                    or not isinstance(sink_record["representative_event"], dict)
                ):
                    raise PairError(f"case {expected_case['name']} sample {repetition} has invalid sink oracle fields")
            if canonical_oracle is None:
                canonical_oracle = oracle
            elif canonical_oracle != oracle:
                raise PairError(f"case {expected_case['name']} oracle differs between raw samples")
        metrics[expected_case["name"]] = float(statistics.median(durations))
        oracles[expected_case["name"]] = {
            "input_payloads_sha256": record["input_payloads_sha256"],
            "sinks": canonical_oracle,
        }
    return {"case_median_duration_ns": metrics, "oracle": oracles}


def interleaved_order(global_pair_index: int) -> tuple[str, str]:
    return ("baseline", "candidate") if global_pair_index % 2 == 0 else ("candidate", "baseline")


def _child_cpu() -> dict[str, float] | None:
    if resource is None:
        return None
    try:
        usage = resource.getrusage(resource.RUSAGE_CHILDREN)
    except (AttributeError, OSError):
        return None
    return {"user_seconds": float(usage.ru_utime), "system_seconds": float(usage.ru_stime)}


def run_attempt(binary_info: Mapping[str, Any], attempt_dir: Path, metadata: Mapping[str, Any]) -> dict[str, Any]:
    attempt_dir.mkdir(parents=True, exist_ok=False)
    raw_path = attempt_dir / "raw.json"
    binary = Path(binary_info["binary"])
    expected_binary_hash = str(binary_info["binary_sha256"])
    before_binary_hash = sha256_file(binary) if binary.is_file() else None
    command = [
        str(binary),
        "--exact",
        TEST_NAME,
        "--ignored",
        "--nocapture",
        "--test-threads=1",
    ]
    environment = os.environ.copy()
    environment.update(
        {
            "EXECUTE_EMIT_PERF_REPORT": str(raw_path.resolve()),
            "EXECUTE_EMIT_PERF_WARMUPS": str(INNER_WARMUPS),
            "EXECUTE_EMIT_PERF_SAMPLES": str(INNER_SAMPLES),
            "LC_ALL": "C",
            "TZ": "UTC",
        }
    )
    started = {
        **dict(metadata),
        "started_at": utc_now(),
        "command": command,
        "timeout_seconds": INVOCATION_TIMEOUT_SECONDS,
        "binary_sha256_before": before_binary_hash,
        "raw_report_path": str(raw_path.resolve()),
        "environment": {
            "EXECUTE_EMIT_PERF_WARMUPS": str(INNER_WARMUPS),
            "EXECUTE_EMIT_PERF_SAMPLES": str(INNER_SAMPLES),
            "LC_ALL": "C",
            "TZ": "UTC",
        },
    }
    write_json(attempt_dir / "started.json", started, exclusive=True)
    cpu_before = _child_cpu()
    start_ns = time.monotonic_ns()
    returncode: int | None = None
    timed_out = False
    error: str | None = None
    stdout = b""
    stderr = b""
    if before_binary_hash != expected_binary_hash:
        error = "binary SHA-256 did not match preregistration before invocation"
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
        except subprocess.TimeoutExpired as exc:
            timed_out = True
            stdout, stderr = exc.stdout or b"", exc.stderr or b""
        except OSError as exc:
            error = f"{type(exc).__name__}: {exc}"
            stderr = error.encode("utf-8", errors="replace")
    elapsed_wall_ns = time.monotonic_ns() - start_ns
    cpu_after = _child_cpu()
    cpu_delta = None
    if cpu_before is not None and cpu_after is not None:
        cpu_delta = {
            key: max(0.0, cpu_after[key] - cpu_before[key])
            for key in ("user_seconds", "system_seconds")
        }
    after_binary_hash = sha256_file(binary) if binary.is_file() else None
    if after_binary_hash != expected_binary_hash:
        error = "binary SHA-256 changed during or after invocation"
    (attempt_dir / "stdout.bin").write_bytes(stdout)
    (attempt_dir / "stderr.bin").write_bytes(stderr)
    parsed: dict[str, Any] | None = None
    raw_hash: str | None = None
    validation_error: str | None = None
    if raw_path.is_file():
        raw = raw_path.read_bytes()
        raw_hash = sha256_bytes(raw)
        try:
            parsed = validate_harness_report(raw)
        except PairError as exc:
            validation_error = str(exc)
    else:
        validation_error = "harness did not create a raw report"
    successful = returncode == 0 and not timed_out and error is None and validation_error is None
    finished = {
        **dict(metadata),
        "finished_at": utc_now(),
        "returncode": returncode,
        "timed_out": timed_out,
        "execution_error": error,
        "elapsed_wall_ns": elapsed_wall_ns,
        "child_cpu_seconds": cpu_delta,
        "binary_sha256_before": before_binary_hash,
        "binary_sha256_after": after_binary_hash,
        "raw_report_present": raw_path.is_file(),
        "raw_report_sha256": raw_hash,
        "validation_error": validation_error,
        "successful": successful,
        "case_median_duration_ns": parsed["case_median_duration_ns"] if parsed else None,
        "oracle": parsed["oracle"] if parsed else None,
        "stdout_sha256": sha256_bytes(stdout),
        "stderr_sha256": sha256_bytes(stderr),
        "attempt_dir": str(attempt_dir.resolve()),
    }
    write_json(attempt_dir / "finished.json", finished, exclusive=True)
    return finished


def _pair_result(pair_index: int, baseline: Mapping[str, Any], candidate: Mapping[str, Any]) -> dict[str, Any]:
    oracle_equal = baseline.get("oracle") == candidate.get("oracle") if baseline.get("successful") and candidate.get("successful") else False
    valid = baseline.get("successful") is True and candidate.get("successful") is True and oracle_equal
    result: dict[str, Any] = {
        "pair_index": pair_index,
        "baseline_attempt_id": baseline["attempt_id"],
        "candidate_attempt_id": candidate["attempt_id"],
        "valid_for_timing_comparison": valid,
        "full_input_and_oracle_equal": oracle_equal,
        "cases": {},
    }
    if not valid:
        result["failure_reasons"] = {
            "baseline": baseline.get("validation_error") or baseline.get("execution_error") or (None if baseline.get("successful") else f"returncode={baseline.get('returncode')} timed_out={baseline.get('timed_out')}"),
            "candidate": candidate.get("validation_error") or candidate.get("execution_error") or (None if candidate.get("successful") else f"returncode={candidate.get('returncode')} timed_out={candidate.get('timed_out')}"),
            "oracle_mismatch": baseline.get("successful") is True and candidate.get("successful") is True and not oracle_equal,
        }
        return result
    for case in CASES:
        name = case["name"]
        base_value = float(baseline["case_median_duration_ns"][name])
        candidate_value = float(candidate["case_median_duration_ns"][name])
        result["cases"][name] = {
            "baseline_median_duration_ns": base_value,
            "candidate_median_duration_ns": candidate_value,
            "candidate_minus_baseline_ns": candidate_value - base_value,
            "candidate_over_baseline": candidate_value / base_value,
            "candidate_change_percent": (candidate_value / base_value - 1.0) * 100.0,
        }
    return result


def _summary(pairs: Sequence[Mapping[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for case in CASES:
        name = case["name"]
        deltas = [float(pair["cases"][name]["candidate_minus_baseline_ns"]) for pair in pairs if pair.get("valid_for_timing_comparison")]
        result[name] = {
            "valid_pair_count": len(deltas),
            "paired_candidate_minus_baseline_ns": deltas,
            "median_paired_delta_ns": statistics.median(deltas) if deltas else None,
            "candidate_losses": sum(delta > 0 for delta in deltas),
            "candidate_improvements": sum(delta < 0 for delta in deltas),
            "ties": sum(delta == 0 for delta in deltas),
        }
    return result


def _verify_preregistration(path: Path) -> dict[str, Any]:
    prereg = load_json(path, "preregistration")
    if not isinstance(prereg, dict) or prereg.get("schema_version") != FORMAT_VERSION:
        raise PairError("preregistration has an unexpected schema")
    driver = prereg.get("driver", {})
    if driver.get("sha256") != sha256_file(Path(__file__).resolve(strict=True)):
        raise PairError("driver source changed after preregistration")
    protocol = prereg.get("protocol", {})
    if protocol != _protocol():
        raise PairError("preregistration protocol differs from this runner")
    inputs = prereg.get("inputs", {})
    freeze_ref = inputs.get("source_freeze", {})
    if sha256_file(Path(freeze_ref.get("path", ""))) != freeze_ref.get("sha256"):
        raise PairError("source freeze changed after preregistration")
    harness_ref = inputs.get("harness_source", {})
    if sha256_file(Path(harness_ref.get("path", ""))) != HARNESS_SHA256:
        raise PairError("frozen harness source changed after preregistration")
    freeze = load_json(Path(freeze_ref["path"]), "source freeze")
    harness = Path(harness_ref["path"]).read_bytes()
    for side in ("baseline", "candidate"):
        identity = inputs.get(side, {})
        regenerated = _receipt_identity(Path(identity["receipt_path"]), side, freeze, harness)
        if regenerated != identity:
            raise PairError(f"{side} receipt/source identity changed after preregistration")
        current = _validate_source_root(Path(identity["root"]), identity["commit"], identity["transport_sha256"], harness)
        if current["fingerprint"] != identity["source_fingerprint"]:
            raise PairError(f"{side} source fingerprint changed after preregistration")
        binary = Path(identity["binary"])
        if sha256_file(binary) != identity["binary_sha256"] or not os.access(binary, os.X_OK):
            raise PairError(f"{side} saved binary changed after preregistration")
    return prereg


def run(args: argparse.Namespace) -> int:
    prereg_path = Path(args.preregistration).expanduser().resolve(strict=True)
    prereg = _verify_preregistration(prereg_path)
    report_dir = Path(args.report_dir).expanduser().absolute()
    if report_dir.exists() or report_dir.is_symlink():
        raise PairError(f"refusing to overwrite report directory: {report_dir}")
    report_dir.mkdir(parents=True)
    inputs = prereg["inputs"]
    initial_identity = {
        side: {
            "source_fingerprint": inputs[side]["source_fingerprint"],
            "binary_sha256": sha256_file(Path(inputs[side]["binary"])),
        }
        for side in ("baseline", "candidate")
    }
    report: dict[str, Any] = {
        "schema_version": FORMAT_VERSION,
        "format": "execute.graph.emit_paired_run.v1",
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
    total_pairs = WARMUP_PAIRS + MEASURED_PAIRS
    for global_pair in range(total_pairs):
        phase = "warmup" if global_pair < WARMUP_PAIRS else "measurement"
        phase_index = global_pair + 1 if phase == "warmup" else global_pair - WARMUP_PAIRS + 1
        order = interleaved_order(global_pair)
        pair_attempts: dict[str, dict[str, Any]] = {}
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
                "source_fingerprint_sha256": identity["source_fingerprint"]["sha256"],
                "binary_sha256": identity["binary_sha256"],
            }
            try:
                result = run_attempt(identity, report_dir / "attempts" / attempt_id, metadata)
            except (PairError, OSError) as exc:
                result = {
                    **metadata,
                    "finished_at": utc_now(),
                    "successful": False,
                    "execution_error": str(exc),
                    "attempt_dir": None,
                }
                overall_failed = True
            report["attempts"].append(result)
            pair_attempts[side] = result
            if result.get("successful") is not True:
                overall_failed = True
            write_json(report_path, report)
        pair = {
            "pair_index": phase_index,
            "global_pair_index": global_pair,
            "order": list(order),
            **_pair_result(phase_index, pair_attempts["baseline"], pair_attempts["candidate"]),
        }
        if pair.get("full_input_and_oracle_equal") is not True:
            overall_failed = True
        if phase == "warmup":
            report["warmup_pairs"].append(pair)
        else:
            report["measured_pairs"].append(pair)
        write_json(report_path, report)

    post_identity: dict[str, Any] = {}
    for side in ("baseline", "candidate"):
        identity = inputs[side]
        try:
            source_now = _validate_source_root(
                Path(identity["root"]),
                identity["commit"],
                identity["transport_sha256"],
                Path(inputs["harness_source"]["path"]).read_bytes(),
            )
            binary_now = sha256_file(Path(identity["binary"]))
            stable = source_now["fingerprint"] == identity["source_fingerprint"] and binary_now == identity["binary_sha256"]
            post_identity[side] = {
                "source_fingerprint": source_now["fingerprint"],
                "binary_sha256": binary_now,
                "stable": stable,
            }
            overall_failed |= not stable
        except (PairError, OSError) as exc:
            post_identity[side] = {"stable": False, "error": str(exc)}
            overall_failed = True
    report["post_run_identity"] = post_identity
    report["paired_summary"] = _summary(report["measured_pairs"])
    report["completed_at"] = utc_now()
    report["attempt_count"] = len(report["attempts"])
    report["successful_attempt_count"] = sum(item.get("successful") is True for item in report["attempts"])
    report["failed_attempt_count"] = report["attempt_count"] - report["successful_attempt_count"]
    report["status"] = "failed" if overall_failed or any(not item.get("valid_for_timing_comparison") for item in report["measured_pairs"]) else "complete"
    write_json(report_path, report)
    print(json.dumps({"report": str(report_path), "status": report["status"]}, sort_keys=True))
    return 0 if report["status"] == "complete" else 1


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    freeze = commands.add_parser("prepare", help="freeze source, binary, and protocol inputs before timing")
    freeze.add_argument("--source-freeze", required=True)
    freeze.add_argument("--baseline-receipt", required=True)
    freeze.add_argument("--candidate-receipt", required=True)
    freeze.add_argument("--harness-source", required=True)
    freeze.add_argument("--preregistration", required=True)
    freeze.set_defaults(function=prepare)
    execute = commands.add_parser("run", help="execute the preregistered 3-warmup/48-pair sequence")
    execute.add_argument("--preregistration", required=True)
    execute.add_argument("--report-dir", required=True)
    execute.set_defaults(function=run)
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    args = _parser().parse_args(argv)
    try:
        value = args.function(args)
        return int(value or 0)
    except (PairError, OSError, KeyError, ValueError, TypeError) as exc:
        print(f"emit paired benchmark: {exc}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
