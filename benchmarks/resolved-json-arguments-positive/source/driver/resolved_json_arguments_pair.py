#!/usr/bin/env python3
"""Prepare and run the frozen paired literal-argument runtime benchmark.

Build each arm once, copy its release library-test executable to an immutable
evidence path, and write the standard Cargo artifact receipt. ``prepare`` then
freezes source/receipt/binary identities before ``run`` performs the fixed
serial 3-warmup-pair and 48-measured-pair schedule. This driver never builds.
"""
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
TEST_NAME = "runtime::resolved_json_arguments_bench::repeated_number_add_runtime_samples"
HARNESS_RELATIVE = "src/runtime/resolved_json_arguments_bench.rs"
WORKLOAD_RELATIVE = "benchmarks/workloads/resolved-json-arguments-repeat-1000.program"
EXPECTED_HARNESS_SHA256 = "627d8466f0c0ef8264e483d456118bb7eb6f4e14757a752fbd226efecf917003"
EXPECTED_WORKLOAD_SHA256 = "a2a95cfb03e55164e459467b088eeffd1d8034fb7023a7e3dfcd806274177b1d"
EXPECTED_OUTPUT_SHA256 = hashlib.sha256(b"3").hexdigest()
EXPECTED_INPUTS = {
    "number_add_arguments": {"left": 1, "right": 2},
    "program_defaults": {"sum": 0},
    "repeat_count": 1000,
}
EXPECTED_INPUT_SHA256 = hashlib.sha256(
    json.dumps(EXPECTED_INPUTS, sort_keys=True, separators=(",", ":")).encode("utf-8")
).hexdigest()
WARMUP_PAIRS = 3
MEASURED_PAIRS = 48
INNER_WARMUPS = 0
INNER_SAMPLES = 3
INVOCATION_TIMEOUT_SECONDS = 120
MAX_TOTAL_BUDGET_SECONDS = 3 * 60 * 60
BASE_COMMIT = "b824aeaa7bccaa4d3d75d3fc6a363a99138287d4"
BUILD_CONFIG_FILES = {
    "Cargo.toml",
    "Cargo.lock",
    "build.rs",
    "rust-toolchain",
    "rust-toolchain.toml",
    "rustfmt.toml",
}
SHA256_RE = re.compile(r"^[0-9a-f]{64}$")
COMMIT_RE = re.compile(r"^[0-9a-f]{40}$")
MAX_JSON_BYTES = 4_000_000


class PairError(RuntimeError):
    """A frozen-input, execution, or benchmark-contract failure."""


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
    encoded = (json.dumps(value, sort_keys=True, indent=2, allow_nan=False) + "\n").encode("utf-8")
    if exclusive:
        with path.open("xb") as handle:
            handle.write(encoded)
            handle.flush()
            os.fsync(handle.fileno())
        return
    temporary = path.with_name(path.name + ".tmp")
    temporary.write_bytes(encoded)
    temporary.replace(path)


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
        raise PairError(f"cannot inspect source checkout {root}: {error}") from error
    if result.returncode:
        raise PairError(result.stderr.decode("utf-8", errors="replace").strip())
    return result.stdout.decode("utf-8").strip()


def _is_build_input(relative: str) -> bool:
    return (
        relative.startswith(("src/", "tests/", "vendor/", ".cargo/"))
        or relative.startswith("manifests/")
        or relative == WORKLOAD_RELATIVE
        or relative in BUILD_CONFIG_FILES
    )


def source_fingerprint(root: Path) -> dict[str, Any]:
    root = root.expanduser().resolve(strict=True)
    listed = subprocess.run(
        ["git", "-C", str(root), "ls-files", "--cached", "--others", "--exclude-standard", "-z"],
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        timeout=20,
        check=False,
    )
    if listed.returncode:
        raise PairError("cannot list build inputs: " + listed.stderr.decode("utf-8", errors="replace"))
    paths = sorted(
        path.decode("utf-8") for path in listed.stdout.split(b"\0") if path and _is_build_input(path.decode("utf-8"))
    )
    files: dict[str, str] = {}
    digest = hashlib.sha256()
    for relative in paths:
        path = root / relative
        if not path.is_file():
            raise PairError(f"build input is missing: {relative}")
        file_hash = sha256_file(path)
        files[relative] = file_hash
        digest.update(relative.encode("utf-8"))
        digest.update(b"\0")
        digest.update(file_hash.encode("ascii"))
        digest.update(b"\n")
    if HARNESS_RELATIVE not in files or WORKLOAD_RELATIVE not in files:
        raise PairError(f"source checkout is missing {HARNESS_RELATIVE} or {WORKLOAD_RELATIVE}")
    return {"sha256": digest.hexdigest(), "files": files}


def _valid_sha(value: Any) -> bool:
    return isinstance(value, str) and SHA256_RE.fullmatch(value) is not None


def _arm_from_receipt(side: str, receipt_path: Path) -> dict[str, Any]:
    receipt_path = receipt_path.expanduser().resolve(strict=True)
    receipt = load_json(receipt_path, f"{side} compile receipt")
    required = {"artifact", "saved_binary", "sha256", "source_sha256", "source_stable", "attempt"}
    if not isinstance(receipt, dict) or set(receipt) != required:
        raise PairError(f"{side} compile receipt has an unexpected schema")
    if receipt["source_stable"] is not True or type(receipt["attempt"]) is not int or receipt["attempt"] != 1:
        raise PairError(f"{side} receipt must attest to its single stable build")
    artifact = receipt["artifact"]
    if not isinstance(artifact, dict):
        raise PairError(f"{side} receipt artifact must be a Cargo compiler-artifact row")
    target = artifact.get("target")
    profile = artifact.get("profile")
    if not isinstance(target, dict) or not isinstance(profile, dict):
        raise PairError(f"{side} compiler artifact is missing target/profile metadata")
    kinds = target.get("kind")
    if (
        artifact.get("reason") != "compiler-artifact"
        or artifact.get("fresh") is not False
        or target.get("name") != "flow"
        or target.get("test") is not True
        or not isinstance(kinds, list)
        or "lib" not in kinds
        or profile.get("test") is not True
        or profile.get("opt_level") != "3"
        or profile.get("debug_assertions") is not False
    ):
        raise PairError(f"{side} artifact is not a freshly compiled optimized release library-test binary")
    try:
        manifest = Path(artifact["manifest_path"]).resolve(strict=True)
        source = Path(target["src_path"]).resolve(strict=True)
        root = manifest.parent
    except (KeyError, OSError, TypeError, ValueError) as error:
        raise PairError(f"{side} artifact has invalid source paths: {error}") from error
    if manifest != (root / "Cargo.toml").resolve(strict=True) or source != (root / "src/lib.rs").resolve(strict=True):
        raise PairError(f"{side} artifact does not identify a library target in one checkout")
    if _git(root, "rev-parse", "HEAD") != BASE_COMMIT:
        raise PairError(f"{side} source checkout is not at the frozen common base {BASE_COMMIT}")
    expected_package = f"path+{root.as_uri()}#execute@"
    if not isinstance(artifact.get("package_id"), str) or not artifact["package_id"].startswith(expected_package):
        raise PairError(f"{side} Cargo package identity does not match its checkout")
    executable = artifact.get("executable")
    filenames = artifact.get("filenames")
    if not isinstance(executable, str) or not isinstance(filenames, list) or executable not in filenames:
        raise PairError(f"{side} Cargo artifact does not name its test executable")
    compiler_binary = Path(executable)
    if compiler_binary.parent.name != "deps" or compiler_binary.parent.parent.name != "release":
        raise PairError(f"{side} compiler artifact is not under a release/deps directory")
    binary = Path(receipt["saved_binary"]).expanduser().resolve(strict=True)
    expected_binary = receipt_path.parent / f"{side}-test-binary"
    if binary != expected_binary.resolve(strict=True):
        raise PairError(f"{side} binary must be the immutable evidence copy {expected_binary}")
    if binary == Path(executable).expanduser().resolve():
        raise PairError(f"{side} saved binary must be distinct from Cargo's reusable target artifact")
    binary_hash = receipt["sha256"]
    if not _valid_sha(binary_hash) or sha256_file(binary) != binary_hash or not os.access(binary, os.X_OK):
        raise PairError(f"{side} saved binary is missing, non-executable, or differs from its receipt")
    sources = source_fingerprint(root)
    runtime_hash = sources["files"].get("src/runtime.rs")
    if not _valid_sha(receipt["source_sha256"]) or receipt["source_sha256"] != runtime_hash:
        raise PairError(f"{side} receipt source_sha256 must identify this arm's src/runtime.rs")
    if sources["files"][HARNESS_RELATIVE] != EXPECTED_HARNESS_SHA256:
        raise PairError(f"{side} benchmark harness source differs from the frozen harness")
    if sources["files"][WORKLOAD_RELATIVE] != EXPECTED_WORKLOAD_SHA256:
        raise PairError(f"{side} fixed workload source differs from the registered workload")
    return {
        "side": side,
        "root": str(root),
        "commit": BASE_COMMIT,
        "source_fingerprint": sources,
        "runtime_source_sha256": runtime_hash,
        "harness_source_sha256": sources["files"][HARNESS_RELATIVE],
        "workload_source_sha256": sources["files"][WORKLOAD_RELATIVE],
        "receipt": str(receipt_path),
        "receipt_sha256": sha256_file(receipt_path),
        "receipt_source_sha256": receipt["source_sha256"],
        "artifact": artifact,
        "binary": str(binary),
        "binary_sha256": binary_hash,
    }


def _protocol() -> dict[str, Any]:
    return {
        "test_name": TEST_NAME,
        "warmup_pairs": WARMUP_PAIRS,
        "measured_pairs": MEASURED_PAIRS,
        "pair_order": "global pairs alternate AB/BA, beginning AB; measured pairs are balanced and begin BA",
        "samples_per_invocation": INNER_SAMPLES,
        "invocation_warmups": INNER_WARMUPS,
        "invocation_timeout_seconds": INVOCATION_TIMEOUT_SECONDS,
        "total_budget_seconds": MAX_TOTAL_BUDGET_SECONDS,
        "no_retries": True,
        "invocation_environment": {
            "EXECUTE_JSON_ARGUMENTS_BENCH_WARMUPS": str(INNER_WARMUPS),
            "EXECUTE_JSON_ARGUMENTS_BENCH_SAMPLES": str(INNER_SAMPLES),
            "LC_ALL": "C",
            "TZ": "UTC",
        },
        "metric": (
            "within-invocation median of 3 raw runtime::run durations; per-pair candidate minus baseline; "
            "report median of 48 paired deltas"
        ),
        "correctness": (
            "every warmup and measured invocation must report exact output b'3', 1,000 completed calls, "
            "zero child launches, and the frozen workload/input/output hashes"
        ),
        "interpretation_limit": (
            "fixed local paired microbenchmark; no confidence interval, cold-start, sustained-load, "
            "or general workload claim"
        ),
    }


def _validate_arm_pair(baseline: Mapping[str, Any], candidate: Mapping[str, Any]) -> None:
    if baseline["root"] == candidate["root"] or baseline["binary"] == candidate["binary"]:
        raise PairError("baseline and candidate must use distinct source roots and saved binaries")
    if baseline["source_fingerprint"]["files"].keys() != candidate["source_fingerprint"]["files"].keys():
        raise PairError("paired source roots have different build-input path sets")
    changed = {
        path
        for path, digest in baseline["source_fingerprint"]["files"].items()
        if candidate["source_fingerprint"]["files"][path] != digest
    }
    allowed_changes = {"src/runtime.rs", "tests/structured_tasks.rs"}
    if changed != allowed_changes:
        raise PairError(
            "paired source roots must differ only in the runtime implementation and its reviewed integration "
            f"regression; found {sorted(changed)}"
        )
    if baseline["binary_sha256"] == candidate["binary_sha256"]:
        raise PairError("baseline and candidate saved binaries are byte-identical")
    for key in ("harness_source_sha256", "workload_source_sha256"):
        if baseline[key] != candidate[key]:
            raise PairError(f"paired arms do not share identical {key}")


def prepare(args: argparse.Namespace) -> None:
    baseline = _arm_from_receipt("baseline", Path(args.baseline_receipt))
    candidate = _arm_from_receipt("candidate", Path(args.candidate_receipt))
    _validate_arm_pair(baseline, candidate)
    script = Path(__file__).resolve(strict=True)
    prereg = {
        "schema_version": FORMAT_VERSION,
        "protocol": _protocol(),
        "inputs": {"baseline": baseline, "candidate": candidate},
        "driver": {"path": str(script), "sha256": sha256_file(script)},
        "expected": {
            "input_sha256": EXPECTED_INPUT_SHA256,
            "output_sha256": EXPECTED_OUTPUT_SHA256,
            "completed_call_count": 1000,
            "child_launches": 0,
        },
        "prepared_at": utc_now(),
    }
    destination = Path(args.preregistration).expanduser().absolute()
    write_json(destination, prereg, exclusive=True)
    print(json.dumps({"preregistration": str(destination), "sha256": sha256_file(destination)}, sort_keys=True))


def _verify_preregistration(path: Path) -> dict[str, Any]:
    prereg = load_json(path, "preregistration")
    if (
        not isinstance(prereg, dict)
        or type(prereg.get("schema_version")) is not int
        or prereg.get("schema_version") != FORMAT_VERSION
    ):
        raise PairError("preregistration has an unexpected schema")
    if prereg.get("protocol") != _protocol():
        raise PairError("preregistration protocol differs from this frozen runner")
    driver = prereg.get("driver")
    if (
        not isinstance(driver, dict)
        or driver.get("path") != str(Path(__file__).resolve(strict=True))
        or driver.get("sha256") != sha256_file(Path(__file__).resolve(strict=True))
    ):
        raise PairError("paired runner source changed after preregistration")
    if prereg.get("expected") != {
        "input_sha256": EXPECTED_INPUT_SHA256,
        "output_sha256": EXPECTED_OUTPUT_SHA256,
        "completed_call_count": 1000,
        "child_launches": 0,
    }:
        raise PairError("preregistration has different workload or result oracles")
    inputs = prereg.get("inputs")
    if not isinstance(inputs, dict) or set(inputs) != {"baseline", "candidate"}:
        raise PairError("preregistration must bind exactly baseline and candidate inputs")
    current = {
        side: _arm_from_receipt(side, Path(inputs[side]["receipt"]))
        for side in ("baseline", "candidate")
    }
    for side in ("baseline", "candidate"):
        if current[side] != inputs[side]:
            raise PairError(f"{side} source, compile receipt, or saved binary changed after preregistration")
    _validate_arm_pair(current["baseline"], current["candidate"])
    return prereg


def interleaved_order(global_pair_index: int) -> tuple[str, str]:
    return ("baseline", "candidate") if global_pair_index % 2 == 0 else ("candidate", "baseline")


def validate_raw_report(payload: bytes) -> dict[str, Any]:
    if len(payload) > MAX_JSON_BYTES:
        raise PairError(f"raw report exceeds {MAX_JSON_BYTES} bytes")
    try:
        report = json.loads(
            payload.decode("utf-8"),
            object_pairs_hook=_unique_object,
            parse_constant=_reject_json_constant,
        )
    except (UnicodeDecodeError, json.JSONDecodeError, ValueError) as error:
        raise PairError(f"raw report is not strict UTF-8 JSON: {error}") from error
    expected_fields = {
        "schema",
        "profile",
        "warmups_configured",
        "samples_configured",
        "workload",
        "warmups",
        "samples",
        "all_runs_valid",
    }
    if not isinstance(report, dict) or set(report) != expected_fields:
        raise PairError("raw report has an unexpected schema")
    expected_workload = {
        "path": WORKLOAD_RELATIVE,
        "source_sha256": EXPECTED_WORKLOAD_SHA256,
        "input_sha256": EXPECTED_INPUT_SHA256,
        "expected_output_sha256": EXPECTED_OUTPUT_SHA256,
    }
    if (
        report["schema"] != "execute.runtime-json-arguments.raw/1"
        or report["profile"] != "release"
        or type(report["warmups_configured"]) is not int
        or report["warmups_configured"] != INNER_WARMUPS
        or type(report["samples_configured"]) is not int
        or report["samples_configured"] != INNER_SAMPLES
        or report["workload"] != expected_workload
        or report["warmups"] != []
        or report["all_runs_valid"] is not True
    ):
        raise PairError("raw report does not match the registered release workload")
    samples = report["samples"]
    if not isinstance(samples, list) or len(samples) != INNER_SAMPLES:
        raise PairError("raw report must contain exactly three samples")
    durations: list[int] = []
    oracle = {
        "output_sha256": EXPECTED_OUTPUT_SHA256,
        "output_matches": True,
        "completed_call_count": 1000,
        "call_count_matches": True,
        "child_launches": 0,
        "no_child_launches": True,
        "valid": True,
    }
    for index, sample in enumerate(samples):
        if not isinstance(sample, dict) or set(sample) != {"index", "elapsed_ns", *oracle.keys()}:
            raise PairError(f"raw sample {index} has an unexpected schema")
        if type(sample["index"]) is not int or sample["index"] != index:
            raise PairError(f"raw sample {index} has the wrong index")
        duration = sample["elapsed_ns"]
        if type(duration) is not int or not 0 < duration < (1 << 128):
            raise PairError(f"raw sample {index} has invalid elapsed_ns")
        if any(sample[key] != value or type(sample[key]) is not type(value) for key, value in oracle.items()):
            raise PairError(f"raw sample {index} failed the exact output/call/child oracle")
        durations.append(duration)
    return {
        "workload": expected_workload,
        "samples": samples,
        "median_duration_ns": statistics.median(durations),
    }


def run_attempt(
    identity: Mapping[str, Any],
    attempt_dir: Path,
    metadata: Mapping[str, Any],
) -> dict[str, Any]:
    attempt_dir.mkdir(parents=True, exist_ok=False)
    raw_path = attempt_dir / "raw.json"
    binary = Path(str(identity["binary"]))
    expected_hash = str(identity["binary_sha256"])
    hash_before = sha256_file(binary) if binary.is_file() else None
    environment = os.environ.copy()
    environment.update({
        "EXECUTE_JSON_ARGUMENTS_BENCH_REPORT": str(raw_path.resolve()),
        "EXECUTE_JSON_ARGUMENTS_BENCH_WARMUPS": str(INNER_WARMUPS),
        "EXECUTE_JSON_ARGUMENTS_BENCH_SAMPLES": str(INNER_SAMPLES),
        "LC_ALL": "C",
        "TZ": "UTC",
    })
    command = [
        str(binary), "--exact", TEST_NAME, "--ignored", "--nocapture", "--test-threads=1"
    ]
    started = {
        **dict(metadata),
        "started_at": utc_now(),
        "command": command,
        "timeout_seconds": INVOCATION_TIMEOUT_SECONDS,
        "binary_sha256_before": hash_before,
        "raw_report_path": str(raw_path.resolve()),
        "environment": {
            "EXECUTE_JSON_ARGUMENTS_BENCH_WARMUPS": str(INNER_WARMUPS),
            "EXECUTE_JSON_ARGUMENTS_BENCH_SAMPLES": str(INNER_SAMPLES),
            "LC_ALL": "C",
            "TZ": "UTC",
        },
    }
    write_json(attempt_dir / "started.json", started, exclusive=True)
    returncode: int | None = None
    timed_out = False
    error: str | None = None
    stdout = b""
    stderr = b""
    start_ns = time.monotonic_ns()
    if hash_before != expected_hash or not os.access(binary, os.X_OK):
        error = "saved binary hash/executable check failed before invocation"
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
            returncode, stdout, stderr = completed.returncode, completed.stdout, completed.stderr
        except subprocess.TimeoutExpired as exception:
            timed_out = True
            stdout = exception.stdout or b""
            stderr = exception.stderr or b""
        except OSError as exception:
            error = f"{type(exception).__name__}: {exception}"
            stderr = error.encode("utf-8", errors="replace")
    wall_ns = time.monotonic_ns() - start_ns
    hash_after = sha256_file(binary) if binary.is_file() else None
    if hash_after != expected_hash:
        error = "saved binary changed during or after invocation"
    (attempt_dir / "stdout.bin").write_bytes(stdout)
    (attempt_dir / "stderr.bin").write_bytes(stderr)
    raw_hash: str | None = None
    parsed: dict[str, Any] | None = None
    validation_error: str | None = None
    if raw_path.is_file():
        raw_bytes = raw_path.read_bytes()
        raw_hash = sha256_bytes(raw_bytes)
        try:
            parsed = validate_raw_report(raw_bytes)
        except PairError as exception:
            validation_error = str(exception)
    else:
        validation_error = "test binary did not create its raw report"
    successful = returncode == 0 and not timed_out and error is None and validation_error is None
    result = {
        **dict(metadata),
        "finished_at": utc_now(),
        "returncode": returncode,
        "timed_out": timed_out,
        "execution_error": error,
        "wall_elapsed_ns": wall_ns,
        "binary_sha256_before": hash_before,
        "binary_sha256_after": hash_after,
        "raw_report_present": raw_path.is_file(),
        "raw_report_sha256": raw_hash,
        "validation_error": validation_error,
        "successful": successful,
        "sample_duration_ns": [sample["elapsed_ns"] for sample in parsed["samples"]] if parsed else None,
        "median_duration_ns": parsed["median_duration_ns"] if parsed else None,
        "stdout_sha256": sha256_bytes(stdout),
        "stderr_sha256": sha256_bytes(stderr),
        "attempt_dir": str(attempt_dir.resolve()),
    }
    write_json(attempt_dir / "finished.json", result, exclusive=True)
    return result


def pair_result(
    global_pair_index: int,
    phase_pair_index: int,
    order: Sequence[str],
    attempts: Mapping[str, Mapping[str, Any]],
) -> dict[str, Any]:
    baseline = attempts["baseline"]
    candidate = attempts["candidate"]
    valid = baseline.get("successful") is True and candidate.get("successful") is True
    result: dict[str, Any] = {
        "pair_index": phase_pair_index,
        "global_pair_index": global_pair_index,
        "order": list(order),
        "valid_for_timing": valid,
    }
    if valid:
        base_ns = float(baseline["median_duration_ns"])
        candidate_ns = float(candidate["median_duration_ns"])
        result["baseline_median_ns"] = base_ns
        result["candidate_median_ns"] = candidate_ns
        result["candidate_minus_baseline_ns"] = candidate_ns - base_ns
    return result


def _summary(pairs: Sequence[Mapping[str, Any]]) -> dict[str, Any]:
    deltas = [float(pair["candidate_minus_baseline_ns"]) for pair in pairs if pair.get("valid_for_timing") is True]
    return {
        "valid_pair_count": len(deltas),
        "paired_candidate_minus_baseline_ns": deltas,
        "median_paired_candidate_minus_baseline_ns": statistics.median(deltas) if deltas else None,
        "candidate_improvements": sum(delta < 0 for delta in deltas),
        "candidate_losses": sum(delta > 0 for delta in deltas),
        "ties": sum(delta == 0 for delta in deltas),
        "interpretation": (
            "descriptive paired microbenchmark result; not a confidence interval or general performance claim"
        ),
    }


def run(args: argparse.Namespace) -> int:
    prereg_path = Path(args.preregistration).expanduser().resolve(strict=True)
    prereg = _verify_preregistration(prereg_path)
    prereg_hash = sha256_file(prereg_path)
    report_dir = Path(args.report_dir).expanduser().absolute()
    if report_dir.exists() or report_dir.is_symlink():
        raise PairError(f"refusing to overwrite report directory: {report_dir}")
    report_dir.mkdir(parents=True)
    inputs = prereg["inputs"]
    report: dict[str, Any] = {
        "schema_version": FORMAT_VERSION,
        "format": "execute.runtime-json-arguments.paired/1",
        "created_at": utc_now(),
        "status": "running",
        "preregistration_path": str(prereg_path),
        "preregistration_sha256": sha256_file(prereg_path),
        "protocol": prereg["protocol"],
        "host": {
            "system": platform.system(),
            "release": platform.release(),
            "machine": platform.machine(),
            "python": sys.version,
        },
        "inputs": inputs,
        "attempts": [],
        "warmup_pairs": [],
        "measured_pairs": [],
    }
    report_path = report_dir / "paired-report.json"
    write_json(report_path, report, exclusive=True)
    overall_failed = False
    started_clock = time.monotonic()
    for global_pair in range(WARMUP_PAIRS + MEASURED_PAIRS):
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
                "source_fingerprint_sha256": identity["source_fingerprint"]["sha256"],
                "compile_receipt_sha256": identity["receipt_sha256"],
                "binary_sha256": identity["binary_sha256"],
            }
            if time.monotonic() - started_clock >= MAX_TOTAL_BUDGET_SECONDS:
                result = {
                    **metadata,
                    "finished_at": utc_now(),
                    "successful": False,
                    "execution_error": "fixed three-hour total budget expired; no retry or replacement run",
                    "attempt_dir": None,
                }
                overall_failed = True
            else:
                try:
                    result = run_attempt(identity, report_dir / "attempts" / attempt_id, metadata)
                except (PairError, OSError) as error:
                    result = {
                        **metadata,
                        "finished_at": utc_now(),
                        "successful": False,
                        "execution_error": str(error),
                        "attempt_dir": None,
                    }
                    overall_failed = True
            report["attempts"].append(result)
            pair_attempts[side] = result
            overall_failed |= result.get("successful") is not True
            write_json(report_path, report)
        pair = pair_result(global_pair, phase_index, order, pair_attempts)
        if not pair["valid_for_timing"]:
            overall_failed = True
        target = "warmup_pairs" if phase == "warmup" else "measured_pairs"
        report[target].append(pair)
        write_json(report_path, report)
    post_identity: dict[str, Any] = {}
    for side in ("baseline", "candidate"):
        try:
            current = _arm_from_receipt(side, Path(inputs[side]["receipt"]))
            stable = current == inputs[side]
            post_identity[side] = {
                "stable": stable,
                "source_fingerprint_sha256": current["source_fingerprint"]["sha256"],
                "binary_sha256": current["binary_sha256"],
                "receipt_sha256": current["receipt_sha256"],
            }
            overall_failed |= not stable
        except (PairError, OSError) as error:
            post_identity[side] = {"stable": False, "error": str(error)}
            overall_failed = True
    driver_path = Path(prereg["driver"]["path"])
    prereg_stable = sha256_file(prereg_path) == prereg_hash
    driver_stable = driver_path.is_file() and sha256_file(driver_path) == prereg["driver"]["sha256"]
    post_identity["preregistration_stable"] = prereg_stable
    post_identity["driver_stable"] = driver_stable
    overall_failed |= not prereg_stable or not driver_stable
    report["post_run_identity"] = post_identity
    report["paired_summary"] = _summary(report["measured_pairs"])
    report["completed_at"] = utc_now()
    report["attempt_count"] = len(report["attempts"])
    report["successful_attempt_count"] = sum(item.get("successful") is True for item in report["attempts"])
    report["failed_attempt_count"] = report["attempt_count"] - report["successful_attempt_count"]
    report["status"] = "failed" if overall_failed or len(report["measured_pairs"]) != MEASURED_PAIRS else "complete"
    write_json(report_path, report)
    print(json.dumps({"report": str(report_path), "status": report["status"]}, sort_keys=True))
    return 0 if report["status"] == "complete" else 1


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    freeze = commands.add_parser("prepare", help="freeze source, Cargo receipts, binaries, and protocol before timing")
    freeze.add_argument("--baseline-receipt", required=True)
    freeze.add_argument("--candidate-receipt", required=True)
    freeze.add_argument("--preregistration", required=True)
    freeze.set_defaults(function=prepare)
    execute = commands.add_parser("run", help="execute the frozen 3-warmup/48-measured-pair sequence")
    execute.add_argument("--preregistration", required=True)
    execute.add_argument("--report-dir", required=True)
    execute.set_defaults(function=run)
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    args = _parser().parse_args(argv)
    try:
        return int(args.function(args) or 0)
    except (PairError, OSError, KeyError, TypeError, ValueError) as error:
        print(f"literal-argument paired benchmark: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
