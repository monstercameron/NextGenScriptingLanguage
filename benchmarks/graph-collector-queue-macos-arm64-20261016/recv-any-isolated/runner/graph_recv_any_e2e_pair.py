#!/usr/bin/env python3
"""Preregister and run the frozen finite graph recv_any E2E comparison."""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import shutil
import statistics
import subprocess
import sys
import time
from datetime import datetime, timezone
from typing import Any, Mapping, Sequence


SCHEMA_VERSION = 1
TEST_NAME = "bounded_native_stream_recv_any_has_exact_end_to_end_samples"
REPORT_FORMAT = "execute.graph.recv_any_e2e.v1"
METHOD = (
    "wall-clock GraphSession start through run_to_completion; real finite cat NativeStream, typed JSON decoding, "
    "two bounded lossless fanout edges, two custom pass-through receivers using recv_any on one queue each, and "
    "terminal collection; all exact payload/order/hash validation follows the timed region"
)
CAMPAIGN = Path(__file__).resolve().parents[2]
BASELINE_ROOT = CAMPAIGN / "work/graph-recv-any-collector-baseline-a884"
CANDIDATE_ROOT = CAMPAIGN / "work/graph-recv-any-collector-queue-a884"
SHARED_TARGET_DIR = CAMPAIGN / "work/perf-target-candidate"
BASELINE_COMMIT = "a884f6561fd43f45e43b25a3cc2233be17ee3944"
HARNESS_RELATIVE = "tests/graph_recv_any_e2e_bench.rs"
HARNESS_SHA256 = "c38f59198e056576b69481e10bfa0cf2ac95124095869550a0d863ceb2bb18c1"
EXPECTED_TRANSPORT_SHA256 = {
    "baseline": "bfc03a7028b2871a4823ed0a130e3b735ca8dc535cac9c12cd39d876174dad9c",
    "candidate": "ac546546ac93266b675ba0f37418961b3bc1672d6cd34c68974aeaf1f6c8ee45",
}
COLLECTOR_PATCH_SHA256 = "7d411b9b0fc3a8a37fbd589381720016916b6edabec43be06414e198cd39bca4"
QUEUE_PATCH_SHA256 = "9135c5ee1ea8b350e6e50acb76eef19d5603a044f6dbcf51271bde83088b0400"
RECORD_COUNT = 4096
TEXT_BYTES = 256
EDGE_CAPACITY = 8
TERMINAL_CAPACITY = 16
GENERATION = 17
REVISION = 29
INNER_WARMUPS = 0
INNER_SAMPLES = 3
WARMUP_PAIRS = 3
MEASURED_PAIRS = 48
TOTAL_PAIRS = WARMUP_PAIRS + MEASURED_PAIRS
TOTAL_ATTEMPTS = TOTAL_PAIRS * 2
INVOCATION_TIMEOUT_SECONDS = 120
MIN_FREE_BYTES = 3 * 1024**3
MAX_RAW_JSON_BYTES = 64 * 1024 * 1024
MAX_JOURNAL_BYTES = 1024 * 1024
U64_MAX = (1 << 64) - 1
COMMIT_RE = re.compile(r"^[0-9a-f]{40}$")
SHA256_RE = re.compile(r"^[0-9a-f]{64}$")
BUILD_INPUT_ROOTS = ("src/", "vendor/", ".cargo/")
BUILD_CONFIG_FILES = {
    "Cargo.toml",
    "Cargo.lock",
    "build.rs",
    "rust-toolchain",
    "rust-toolchain.toml",
    "rustfmt.toml",
}
BUILD_ENVIRONMENT_RELATIVE = "evidence/graph-recv-any-single-queue/build-environment-attempt-1.json"
BUILD_DRIVER_RELATIVE = "work/build_graph_recv_any_e2e_pair.py"
BUILD_METADATA_FIELDS = {
    "rustc_vv",
    "cargo_version",
    "selected_build_environment",
    "captured_before_clean_and_build",
}
REPORT_FIELDS = {
    "format",
    "method",
    "configuration",
    "identity",
    "warmups",
    "raw_samples_in_execution_order",
}
CONFIGURATION = {
    "inner_warmups": INNER_WARMUPS,
    "raw_samples": INNER_SAMPLES,
    "record_count": RECORD_COUNT,
    "text_bytes_per_record": TEXT_BYTES,
    "edge_capacity": EDGE_CAPACITY,
    "edge_policy": "lossless",
    "terminal_capacity": TERMINAL_CAPACITY,
    "generation": GENERATION,
    "revision": REVISION,
    "max_collected_events": RECORD_COUNT * 2 + 1,
    "max_message_bytes": 4096,
    "expected_launched_processes": 1,
}
SAMPLE_FIELDS = {"sample_index", "duration_ns", "evidence"}
EVIDENCE_FIELDS = {"duration_ns", "dropped_messages", "launched_processes", "verification"}
VERIFICATION_FIELDS = {"terminal_event_count", "source_status_event_count", "source_status_identity", "consumers"}
CONSUMER_FIELDS = {
    "consumer",
    "input_event_count",
    "input_sequence_sha256",
    "input_source_port",
    "input_target_port",
    "input_sensitivity",
    "input_remote_metadata",
    "input_event_identities",
    "output_event_count",
    "output_payload_sha256",
    "output_event_identities",
}
RECEIVED_IDENTITY_FIELDS = {
    "sequence",
    "generation",
    "revision",
    "source_port_matches",
    "input_port_matches",
    "sensitivity_unknown_only",
    "remote_absent",
}
EVENT_IDENTITY_FIELDS = {"port", "input", "generation", "revision", "sequence", "sensitivity", "remote"}


class PairError(RuntimeError):
    """A preregistration, source, execution, or harness-contract failure."""


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


def canonical_json(value: Any) -> bytes:
    return json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=False, allow_nan=False).encode("utf-8")


def _unique_object(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            raise ValueError(f"duplicate JSON key: {key}")
        result[key] = value
    return result


def _reject_constant(value: str) -> None:
    raise ValueError(f"non-finite JSON constant: {value}")


def parse_json(payload: bytes, label: str, max_bytes: int = MAX_RAW_JSON_BYTES) -> Any:
    if len(payload) > max_bytes:
        raise PairError(f"{label} exceeds {max_bytes} bytes")
    try:
        return json.loads(
            payload.decode("utf-8"),
            object_pairs_hook=_unique_object,
            parse_constant=_reject_constant,
        )
    except (UnicodeDecodeError, json.JSONDecodeError, ValueError) as error:
        raise PairError(f"{label} is not strict UTF-8 JSON: {error}") from error


def load_json(path: Path, label: str, max_bytes: int = MAX_RAW_JSON_BYTES) -> Any:
    try:
        return parse_json(path.expanduser().resolve(strict=True).read_bytes(), label, max_bytes)
    except OSError as error:
        raise PairError(f"cannot read {label}: {error}") from error


def _write_json_exclusive(path: Path, value: Any) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    try:
        with path.open("x", encoding="utf-8") as stream:
            json.dump(value, stream, sort_keys=True, indent=2, ensure_ascii=False, allow_nan=False)
            stream.write("\n")
            stream.flush()
            os.fsync(stream.fileno())
    except FileExistsError as error:
        raise PairError(f"refusing to overwrite existing file: {path}") from error


def _write_json_replace(path: Path, value: Any) -> None:
    temporary = path.with_name(path.name + ".tmp")
    with temporary.open("w", encoding="utf-8") as stream:
        json.dump(value, stream, sort_keys=True, indent=2, ensure_ascii=False, allow_nan=False)
        stream.write("\n")
        stream.flush()
        os.fsync(stream.fileno())
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


def _commit(root: Path) -> str:
    return _git(root, "rev-parse", "HEAD").decode("ascii").strip()


def _is_build_input(relative: str) -> bool:
    return relative.startswith(BUILD_INPUT_ROOTS) or relative in BUILD_CONFIG_FILES


def source_fingerprint(root: Path) -> dict[str, Any]:
    root = root.expanduser().resolve(strict=True)
    raw_paths = _git(root, "ls-files", "-z")
    paths = sorted(
        item.decode("utf-8")
        for item in raw_paths.split(b"\0")
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


def _inspect_source(root: Path, side: str, harness: bytes) -> dict[str, Any]:
    root = root.expanduser().resolve(strict=True)
    work_root = (CAMPAIGN / "work").resolve(strict=True)
    if not root.is_relative_to(work_root):
        raise PairError(f"{side} source root must be under the campaign work directory: {root}")
    commit = _commit(root)
    if commit != BASELINE_COMMIT:
        raise PairError(f"{side} source root has unexpected HEAD {commit}; expected {BASELINE_COMMIT}")
    changed = _git(root, "diff", "--name-only").decode("utf-8").splitlines()
    staged = _git(root, "diff", "--cached", "--name-only").decode("utf-8").splitlines()
    untracked = [
        item.decode("utf-8")
        for item in _git(root, "ls-files", "--others", "--exclude-standard", "-z").split(b"\0")
        if item
    ]
    expected_changed = ["src/graph/transport.rs"]
    if sorted(changed) != expected_changed or staged or untracked != [HARNESS_RELATIVE]:
        raise PairError(
            f"unexpected {side} worktree state: changed={changed}, staged={staged}, untracked={untracked}"
        )
    harness_path = root / HARNESS_RELATIVE
    if not harness_path.is_file() or harness_path.read_bytes() != harness:
        raise PairError(f"{side} does not contain the exact frozen integration harness")
    transport_path = root / "src/graph/transport.rs"
    transport_hash = sha256_file(transport_path)
    return {
        "root": str(root),
        "commit": commit,
        "transport_path": str(transport_path),
        "transport_sha256": transport_hash,
        "harness_path": str(harness_path),
        "harness_sha256": sha256_bytes(harness),
        "source_fingerprint": source_fingerprint(root),
    }


def _valid_hash(value: Any) -> bool:
    return isinstance(value, str) and SHA256_RE.fullmatch(value) is not None


def _validated_build_metadata(value: Any, label: str) -> dict[str, Any]:
    if not isinstance(value, dict) or set(value) != BUILD_METADATA_FIELDS:
        raise PairError(f"{label} build metadata has an unexpected schema")
    if not isinstance(value["rustc_vv"], str) or not value["rustc_vv"].strip():
        raise PairError(f"{label} build metadata lacks rustc -Vv output")
    if not isinstance(value["cargo_version"], str) or not value["cargo_version"].strip():
        raise PairError(f"{label} build metadata lacks cargo --version output")
    environment = value["selected_build_environment"]
    if not isinstance(environment, dict) or not all(
        isinstance(key, str) and isinstance(item, str) for key, item in environment.items()
    ):
        raise PairError(f"{label} selected build environment must map strings to strings")
    required_environment = {
        "PATH",
        "CARGO_HOME",
        "RUSTUP_HOME",
        "RUSTUP_TOOLCHAIN",
        "CARGO_BUILD_JOBS",
        "CARGO_TARGET_DIR",
    }
    if not required_environment.issubset(environment) or not value["captured_before_clean_and_build"]:
        raise PairError(f"{label} build metadata is incomplete or was captured too late")
    if type(value["captured_before_clean_and_build"]) is not bool:
        raise PairError(f"{label} build metadata capture marker must be boolean")
    if environment["CARGO_TARGET_DIR"] != str(SHARED_TARGET_DIR):
        raise PairError(f"{label} CARGO_TARGET_DIR differs from the frozen shared target path")
    return value


def _validate_build_metadata_pair(
    baseline: Mapping[str, Any], candidate: Mapping[str, Any], recorded: Mapping[str, Any]
) -> None:
    baseline = _validated_build_metadata(dict(baseline), "baseline")
    candidate = _validated_build_metadata(dict(candidate), "candidate")
    recorded = _validated_build_metadata(dict(recorded), "captured environment")

    if baseline != candidate:
        raise PairError("baseline and candidate toolchain or selected build environment differ")
    if baseline != recorded:
        raise PairError("build receipt metadata differs from the captured build environment")


def _build_provenance(baseline: Mapping[str, Any], candidate: Mapping[str, Any]) -> dict[str, Any]:
    if baseline["build_attempt"] != candidate["build_attempt"]:
        raise PairError("baseline and candidate build attempts differ")
    attempt = baseline["build_attempt"]
    environment_path = (CAMPAIGN / BUILD_ENVIRONMENT_RELATIVE).resolve(strict=True)
    recorded_metadata = _validated_build_metadata(
        load_json(environment_path, "captured build environment", 2_000_000), "captured environment"
    )
    _validate_build_metadata_pair(baseline["build_metadata"], candidate["build_metadata"], recorded_metadata)

    def pin_file(path: Path, label: str) -> dict[str, str]:
        path = path.resolve(strict=True)
        if not path.is_file():
            raise PairError(f"{label} is not a regular file: {path}")
        return {"path": str(path), "sha256": sha256_file(path)}

    logs: dict[str, dict[str, dict[str, str]]] = {}
    for side in ("baseline", "candidate"):
        logs[side] = {}
        for label, pattern in (
            ("clean", "{side}-release-clean-attempt-{attempt}.log"),
            ("build_json", "{side}-build-attempt-{attempt}.jsonl"),
            ("build_stderr", "{side}-build-attempt-{attempt}.log"),
            ("test_list", "{side}-test-list.txt"),
        ):
            name = pattern.format(side=side, attempt=attempt)
            logs[side][label] = pin_file(CAMPAIGN / "evidence/graph-recv-any-single-queue" / name, f"{side} {label} build evidence")
    return {
        "environment": pin_file(environment_path, "captured build environment"),
        "driver": pin_file(CAMPAIGN / BUILD_DRIVER_RELATIVE, "build driver"),
        "logs": logs,
    }


def _artifact_from_build_log(path: Path, side: str) -> dict[str, Any]:
    matching_rows: list[dict[str, Any]] = []
    try:
        with path.open("rb") as stream:
            for line_number, line in enumerate(stream, start=1):
                if not line.strip():
                    continue
                row = parse_json(line, f"{side} Cargo build JSONL line {line_number}", 2_000_000)
                if not isinstance(row, dict):
                    raise PairError(f"{side} Cargo build JSONL line {line_number} is not an object")
                target = row.get("target")
                if (
                    row.get("reason") == "compiler-artifact"
                    and isinstance(target, dict)
                    and target.get("name") == "graph_recv_any_e2e_bench"
                ):
                    matching_rows.append(row)
    except OSError as error:
        raise PairError(f"cannot read {side} pinned Cargo build JSONL: {error}") from error
    if len(matching_rows) != 1:
        raise PairError(f"{side} pinned Cargo build JSONL must contain one integration-test artifact row")
    return matching_rows[0]


def _lexical_absolute_path(value: Any, label: str) -> Path:
    if not isinstance(value, str) or not value:
        raise PairError(f"{label} path must be a nonempty string")
    path = Path(value).expanduser()
    if not path.is_absolute() or Path(os.path.normpath(str(path))) != path:
        raise PairError(f"{label} path must be a normalized absolute path")
    return path


def _validate_artifact_receipt(
    side: str,
    artifact: Any,
    pinned_artifact: Mapping[str, Any],
    build_metadata: Mapping[str, Any],
    expected_root: Path,
    saved_binary: Path,
    binary_hash: Any,
) -> None:
    if not isinstance(artifact, dict) or artifact != pinned_artifact:
        raise PairError(f"{side} receipt artifact differs from the pinned Cargo build JSONL row")
    target = artifact.get("target")
    profile = artifact.get("profile")
    if not isinstance(target, dict) or not isinstance(profile, dict):
        raise PairError(f"{side} Cargo artifact lacks target/profile metadata")
    kinds = target.get("kind")
    if (
        artifact.get("reason") != "compiler-artifact"
        or target.get("name") != "graph_recv_any_e2e_bench"
        or not isinstance(kinds, list)
        or "test" not in kinds
        or target.get("test") is not True
        or profile.get("test") is not True
        or profile.get("opt_level") != "3"
        or profile.get("debug_assertions") is not False
        or artifact.get("fresh") is not False
    ):
        raise PairError(f"{side} receipt is not a newly built optimized integration-test binary")
    manifest_path = Path(str(artifact.get("manifest_path", ""))).expanduser().resolve(strict=True)
    source_path = Path(str(target.get("src_path", ""))).expanduser().resolve(strict=True)
    expected_target = expected_root / "Cargo.toml"
    if manifest_path != expected_target or source_path != expected_root / HARNESS_RELATIVE:
        raise PairError(f"{side} receipt paths do not identify the frozen integration test")

    artifact_binary = _lexical_absolute_path(artifact.get("executable"), f"{side} Cargo artifact executable")
    environment = build_metadata["selected_build_environment"]
    target_directory = _lexical_absolute_path(environment.get("CARGO_TARGET_DIR"), f"{side} Cargo target directory")
    release_dependencies = target_directory / "release" / "deps"
    if not artifact_binary.is_relative_to(release_dependencies):
        raise PairError(f"{side} Cargo artifact executable is outside its configured release deps directory")
    filenames = artifact.get("filenames")
    if not isinstance(filenames, list) or not any(
        _lexical_absolute_path(filename, f"{side} Cargo artifact filename") == artifact_binary
        for filename in filenames
    ):
        raise PairError(f"{side} Cargo artifact executable is absent from its filenames list")
    if saved_binary == artifact_binary or saved_binary.is_relative_to(release_dependencies):
        raise PairError(f"{side} saved binary must be separate from the reusable Cargo artifact path")
    if not saved_binary.is_file() or not os.access(saved_binary, os.X_OK):
        raise PairError(f"{side} saved test binary is missing or not executable")
    if not _valid_hash(binary_hash) or sha256_file(saved_binary) != binary_hash:
        raise PairError(f"{side} saved binary does not match the build receipt digest")


def _receipt_identity(receipt_path: Path, side: str, root: Path, harness: bytes) -> dict[str, Any]:
    receipt_path = receipt_path.expanduser().resolve(strict=True)
    receipt = load_json(receipt_path, f"{side} build receipt", 2_000_000)
    expected_fields = {
        "artifact", "saved_binary", "sha256", "source_sha256", "source_stable", "attempt", "build_metadata"
    }
    if not isinstance(receipt, dict) or set(receipt) != expected_fields:
        raise PairError(f"{side} build receipt has an unexpected schema")
    source = _inspect_source(root, side, harness)
    if receipt.get("source_stable") is not True:
        raise PairError(f"{side} build receipt does not attest to stable source")
    if receipt.get("source_sha256") != source["transport_sha256"]:
        raise PairError(f"{side} build receipt source hash differs from frozen transport source")
    if type(receipt.get("attempt")) is not int or receipt["attempt"] < 1:
        raise PairError(f"{side} build receipt has an invalid build attempt")
    build_metadata = _validated_build_metadata(receipt.get("build_metadata"), side)
    artifact = receipt.get("artifact")
    expected_root = Path(source["root"])
    saved_binary = Path(str(receipt.get("saved_binary", ""))).expanduser().resolve(strict=True)
    binary_hash = receipt.get("sha256")
    build_log = CAMPAIGN / "evidence/graph-recv-any-single-queue" / f"{side}-build-attempt-{receipt['attempt']}.jsonl"
    pinned_artifact = _artifact_from_build_log(build_log, side)
    _validate_artifact_receipt(
        side, artifact, pinned_artifact, build_metadata, expected_root, saved_binary, binary_hash
    )
    return {
        "side": side,
        **source,
        "receipt_path": str(receipt_path),
        "receipt_sha256": sha256_file(receipt_path),
        "artifact": artifact,
        "saved_binary": str(saved_binary),
        "binary_sha256": binary_hash,
        "source_stable": True,
        "build_attempt": receipt["attempt"],
        "build_metadata": build_metadata,
    }


def expected_fixture_bytes() -> bytes:
    return b"".join(
        canonical_json({"id": record_id, "text": "x" * TEXT_BYTES}) + b"\n"
        for record_id in range(1, RECORD_COUNT + 1)
    )


def _typed_payload_bytes(record_id: int) -> bytes:
    return canonical_json(
        {"Record": {"id": {"Integer": record_id}, "text": {"String": "x" * TEXT_BYTES}}}
    )


def expected_collected_payload_bytes() -> int:
    payload_bytes = sum(len(_typed_payload_bytes(record_id)) for record_id in range(1, RECORD_COUNT + 1))
    return payload_bytes * 2 + len(canonical_json({"Integer": 0}))


def _manifest_sha256() -> str:
    manifest = {
        "name": "bench.jsonl_source",
        "binary": "/bin/cat",
        "accepts_stdin": False,
        "parameters": {"path": {"type": "path", "required": True}},
        "arguments": [{"parameter": "path"}],
        "contract": {
            "version": 1,
            "output": {
                "type": "record",
                "encoding": "json",
                "fields": {"id": {"type": "integer"}, "text": {"type": "text"}},
                "required": ["id", "text"],
            },
        },
    }
    return sha256_bytes(canonical_json(manifest))


def _graph_template_sha256() -> str:
    template = (
        "graph graph_recv_any_e2e\n"
        "stream from bench.jsonl_source with path __FIXTURE__ as producer\n"
        "show each line\n"
        "end graph\n"
    )
    return sha256_bytes(template.encode("utf-8"))


def _protocol() -> dict[str, Any]:
    return {
        "test_name": TEST_NAME,
        "harness_format": REPORT_FORMAT,
        "harness_sha256": HARNESS_SHA256,
        "baseline_commit": BASELINE_COMMIT,
        "build_target": {
            "cargo_target_dir": str(SHARED_TARGET_DIR),
            "policy": "shared target; clean only execute release outputs between arms and save baseline binary before candidate clean",
        },
        "source_composition": {
            "baseline_transport_sha256": EXPECTED_TRANSPORT_SHA256["baseline"],
            "candidate_transport_sha256": EXPECTED_TRANSPORT_SHA256["candidate"],
            "shared_collector_patch_sha256": COLLECTOR_PATCH_SHA256,
            "candidate_queue_patch_sha256": QUEUE_PATCH_SHA256,
        },
        "workload": {
            "record_count": RECORD_COUNT,
            "text_bytes_per_record": TEXT_BYTES,
            "producer": "/bin/cat through NativeStream to natural EOF",
            "consumers": 2,
            "edge_capacity": EDGE_CAPACITY,
            "edge_policy": "lossless",
        },
        "warmup_pairs": WARMUP_PAIRS,
        "measured_pairs": MEASURED_PAIRS,
        "pair_order": "alternating AB/BA across three warmup pairs and 48 measured pairs; first measured pair is BA",
        "inner_harness_warmups": INNER_WARMUPS,
        "inner_harness_samples": INNER_SAMPLES,
        "invocation_timeout_seconds": INVOCATION_TIMEOUT_SECONDS,
        "minimum_free_bytes_before_run": MIN_FREE_BYTES,
        "invocation_environment": {
            "EXECUTE_GRAPH_RECV_ANY_E2E_REPORT": "absolute attempt-local raw.json",
            "EXECUTE_GRAPH_RECV_ANY_E2E_WARMUPS": str(INNER_WARMUPS),
            "EXECUTE_GRAPH_RECV_ANY_E2E_SAMPLES": str(INNER_SAMPLES),
            "LC_ALL": "C",
            "TZ": "UTC",
        },
        "metric": "median of three raw GraphSession wall durations per invocation; paired candidate minus baseline; positive is slower",
        "cross_arm_oracle": "fixture bytes/hash, frozen configuration, source/graph/harness hashes, and all full exact payload/order/metadata identity oracles must match; only scheduler-assigned global sequence numbers and absolute per-attempt paths are normalized away after within-run sequence validation",
        "retry_policy": "no retries; run both sides of the current pair, then stop after any failed pair and record all remaining planned slots as not attempted",
    }


def _validate_source_pair(baseline: Mapping[str, Any], candidate: Mapping[str, Any]) -> None:
    if baseline["root"] != str(BASELINE_ROOT.resolve(strict=True)):
        raise PairError("baseline source root is not the prepared collector baseline worktree")
    if candidate["root"] != str(CANDIDATE_ROOT.resolve(strict=True)):
        raise PairError("candidate source root is not the prepared queue worktree")
    if baseline["root"] == candidate["root"]:
        raise PairError("baseline and candidate source roots must differ")
    if baseline["commit"] != candidate["commit"] or baseline["commit"] != BASELINE_COMMIT:
        raise PairError("baseline and candidate must share the frozen a884 commit")
    if baseline["transport_sha256"] != EXPECTED_TRANSPORT_SHA256["baseline"]:
        raise PairError("baseline transport source does not match the shared collector patch pin")
    if candidate["transport_sha256"] != EXPECTED_TRANSPORT_SHA256["candidate"]:
        raise PairError("candidate transport source does not match the collector plus queue patch pin")
    base_files = baseline["source_fingerprint"]["files"]
    candidate_files = candidate["source_fingerprint"]["files"]
    if base_files.keys() != candidate_files.keys():
        raise PairError("baseline and candidate tracked build input sets differ")
    changed = {path for path in base_files if base_files[path] != candidate_files[path]}
    if changed != {"src/graph/transport.rs"}:
        raise PairError(f"candidate build inputs differ outside the intended transport source: {sorted(changed)}")
    if baseline["transport_sha256"] == candidate["transport_sha256"]:
        raise PairError("candidate transport source is byte-identical to baseline")


def prepare(args: argparse.Namespace) -> None:
    harness_path = Path(args.harness_source).expanduser().resolve(strict=True)
    harness = harness_path.read_bytes()
    if sha256_bytes(harness) != HARNESS_SHA256:
        raise PairError("canonical E2E harness bytes do not match the frozen SHA-256")
    candidate_root = Path(args.candidate_root).expanduser().resolve(strict=True)
    if candidate_root != CANDIDATE_ROOT.resolve(strict=True):
        raise PairError("candidate root differs from the prepared queue worktree")
    baseline = _receipt_identity(Path(args.baseline_receipt), "baseline", BASELINE_ROOT, harness)
    candidate = _receipt_identity(Path(args.candidate_receipt), "candidate", candidate_root, harness)
    _validate_source_pair(baseline, candidate)
    build_provenance = _build_provenance(baseline, candidate)
    harness_identity = {
        "path": str(harness_path),
        "sha256": HARNESS_SHA256,
        "baseline_copy": baseline["harness_path"],
        "candidate_copy": candidate["harness_path"],
    }
    script = Path(__file__).resolve(strict=True)
    prereg = {
        "schema_version": SCHEMA_VERSION,
        "protocol": _protocol(),
        "inputs": {
            "harness": harness_identity,
            "baseline": baseline,
            "candidate": candidate,
            "build_provenance": build_provenance,
        },
        "driver": {"path": str(script), "sha256": sha256_file(script)},
        "prepared_at": utc_now(),
    }
    destination = Path(args.preregistration).expanduser().absolute()
    _write_json_exclusive(destination, prereg)
    print(json.dumps({"preregistration": str(destination), "sha256": sha256_file(destination)}, sort_keys=True))


def _require(condition: bool, message: str) -> None:
    if not condition:
        raise PairError(message)


def _integer(value: Any, label: str, *, minimum: int = 0, maximum: int = U64_MAX) -> int:
    if type(value) is not int or not minimum <= value <= maximum:
        raise PairError(f"{label} must be an integer in [{minimum}, {maximum}]")
    return value


def _validate_event_identity(
    value: Any,
    *,
    expected_port: Mapping[str, str],
    expected_input: Mapping[str, str] | None,
    label: str,
) -> int:
    if not isinstance(value, dict) or set(value) != EVENT_IDENTITY_FIELDS:
        raise PairError(f"{label} has an unexpected event identity schema")
    if value["port"] != dict(expected_port) or value["input"] != (dict(expected_input) if expected_input else None):
        raise PairError(f"{label} port identity changed")
    if _integer(value["generation"], f"{label} generation") != GENERATION:
        raise PairError(f"{label} generation changed")
    if _integer(value["revision"], f"{label} revision") != REVISION:
        raise PairError(f"{label} revision changed")
    if value["sensitivity"] != ["unknown"] or value["remote"] is not None:
        raise PairError(f"{label} sensitivity or remote metadata changed")
    return _integer(value["sequence"], f"{label} sequence", minimum=1)


def _normalize_event_identity(value: Mapping[str, Any]) -> dict[str, Any]:
    return {key: item for key, item in value.items() if key != "sequence"}


def validate_verification(verification: Any, input_sha256: str, record_count: int = RECORD_COUNT) -> dict[str, Any]:
    if not isinstance(verification, dict) or set(verification) != VERIFICATION_FIELDS:
        raise PairError("sample verification has an unexpected schema")
    if _integer(verification["terminal_event_count"], "terminal event count") != record_count * 2 + 1:
        raise PairError("sample has a partial or extra terminal event set")
    if _integer(verification["source_status_event_count"], "source status count") != 1:
        raise PairError("sample does not have exactly one producer status event")
    status_sequence = _validate_event_identity(
        verification["source_status_identity"],
        expected_port={"node": "producer", "port": "status"},
        expected_input=None,
        label="producer status",
    )
    consumers = verification["consumers"]
    if not isinstance(consumers, list) or len(consumers) != 2:
        raise PairError("sample must contain exactly two consumers")
    all_emission_sequences: set[int] = set()
    normalized_consumers: list[dict[str, Any]] = []
    shared_input_sequences: list[int] | None = None
    for index, expected_consumer in enumerate(("consumer-0", "consumer-1")):
        consumer = consumers[index]
        if not isinstance(consumer, dict) or set(consumer) != CONSUMER_FIELDS:
            raise PairError(f"{expected_consumer} report has an unexpected schema")
        if consumer["consumer"] != expected_consumer:
            raise PairError("consumer order or identity changed")
        if _integer(consumer["input_event_count"], "input event count") != record_count:
            raise PairError(f"{expected_consumer} received a partial stream")
        if _integer(consumer["output_event_count"], "output event count") != record_count:
            raise PairError(f"{expected_consumer} returned a partial stream")
        if consumer["input_source_port"] != {"node": "producer", "port": "stdout"}:
            raise PairError(f"{expected_consumer} input source port changed")
        if consumer["input_target_port"] != {"node": expected_consumer, "port": "events"}:
            raise PairError(f"{expected_consumer} input target port changed")
        if consumer["input_sensitivity"] != ["unknown"] or consumer["input_remote_metadata"] is not None:
            raise PairError(f"{expected_consumer} input metadata changed")

        inputs = consumer["input_event_identities"]
        if not isinstance(inputs, list) or len(inputs) != record_count:
            raise PairError(f"{expected_consumer} lacks full input identity receipts")
        input_sequences: list[int] = []
        previous = 0
        normalized_inputs: list[dict[str, Any]] = []
        for event_index, item in enumerate(inputs):
            if not isinstance(item, dict) or set(item) != RECEIVED_IDENTITY_FIELDS:
                raise PairError(f"{expected_consumer} input identity {event_index} has an unexpected schema")
            sequence = _integer(item["sequence"], "input sequence", minimum=1)
            if sequence <= previous:
                raise PairError(f"{expected_consumer} input sequence order changed")
            previous = sequence
            if (
                _integer(item["generation"], "input generation") != GENERATION
                or _integer(item["revision"], "input revision") != REVISION
                or item["source_port_matches"] is not True
                or item["input_port_matches"] is not True
                or item["sensitivity_unknown_only"] is not True
                or item["remote_absent"] is not True
            ):
                raise PairError(f"{expected_consumer} input metadata/port identity changed at {event_index}")
            input_sequences.append(sequence)
            all_emission_sequences.add(sequence)
            normalized_inputs.append({key: item[key] for key in item if key != "sequence"})
        expected_sequence_hash = sha256_bytes(b"".join(value.to_bytes(8, "little") for value in input_sequences))
        if consumer["input_sequence_sha256"] != expected_sequence_hash:
            raise PairError(f"{expected_consumer} input sequence checksum is incorrect")
        if shared_input_sequences is None:
            shared_input_sequences = input_sequences
        elif input_sequences != shared_input_sequences:
            raise PairError("recv_any consumers received different source event sequences")

        outputs = consumer["output_event_identities"]
        if not isinstance(outputs, list) or len(outputs) != record_count:
            raise PairError(f"{expected_consumer} lacks full output identity receipts")
        previous = 0
        normalized_outputs: list[dict[str, Any]] = []
        for event_index, item in enumerate(outputs):
            sequence = _validate_event_identity(
                item,
                expected_port={"node": expected_consumer, "port": "stdout"},
                expected_input=None,
                label=f"{expected_consumer} output {event_index}",
            )
            if sequence <= previous or sequence <= input_sequences[event_index]:
                raise PairError(f"{expected_consumer} output order or input-to-output ordering changed")
            previous = sequence
            if sequence in all_emission_sequences:
                raise PairError("graph reused an event sequence across distinct emissions")
            all_emission_sequences.add(sequence)
            normalized_outputs.append(_normalize_event_identity(item))
        if consumer["output_payload_sha256"] != input_sha256:
            raise PairError(f"{expected_consumer} full output payload checksum differs from the fixture")
        normalized_consumers.append(
            {
                "consumer": expected_consumer,
                "input_event_count": record_count,
                "input_source_port": consumer["input_source_port"],
                "input_target_port": consumer["input_target_port"],
                "input_sensitivity": consumer["input_sensitivity"],
                "input_remote_metadata": consumer["input_remote_metadata"],
                "input_event_identities_without_sequences": normalized_inputs,
                "output_event_count": record_count,
                "output_payload_sha256": consumer["output_payload_sha256"],
                "output_event_identities_without_sequences": normalized_outputs,
            }
        )
    if shared_input_sequences is None or status_sequence <= shared_input_sequences[-1]:
        raise PairError("producer status does not follow the complete source stream")
    if status_sequence in all_emission_sequences:
        raise PairError("producer status sequence duplicates another event")
    all_emission_sequences.add(status_sequence)
    return {
        "terminal_event_count": record_count * 2 + 1,
        "source_status_event_count": 1,
        "source_status_identity_without_sequence": _normalize_event_identity(verification["source_status_identity"]),
        "consumers": normalized_consumers,
    }


def _expected_identity_paths(report_path: Path) -> tuple[Path, Path]:
    stem = report_path.stem
    return (
        report_path.with_name(f"{stem}.input.jsonl"),
        report_path.with_name(f"{stem}.journal.jsonl"),
    )


def validate_journal(
    payload: bytes,
    samples: Sequence[Mapping[str, Any]],
    input_sha256: str,
) -> dict[str, Any]:
    if len(payload) > MAX_JOURNAL_BYTES:
        raise PairError(f"sample journal exceeds {MAX_JOURNAL_BYTES} bytes")
    if not payload.endswith(b"\n"):
        raise PairError("sample journal is missing its final newline")
    lines = payload.splitlines()
    if len(lines) != INNER_SAMPLES + 1:
        raise PairError("sample journal must contain one preparation record and three sample records")
    rows = [parse_json(line, f"sample journal row {index}", 64 * 1024) for index, line in enumerate(lines)]
    prepared = rows[0]
    if not isinstance(prepared, dict) or set(prepared) != {
        "status", "planned_warmups", "planned_samples", "record_count", "input_fixture_sha256"
    }:
        raise PairError("sample journal preparation record has an unexpected schema")
    if prepared != {
        "status": "prepared",
        "planned_warmups": 0,
        "planned_samples": INNER_SAMPLES,
        "record_count": RECORD_COUNT,
        "input_fixture_sha256": input_sha256,
    }:
        raise PairError("sample journal preparation record disagrees with preregistration")
    normalized_rows: list[dict[str, Any]] = [prepared]
    for index, (row, sample) in enumerate(zip(rows[1:], samples)):
        if not isinstance(row, dict) or set(row) != {
            "phase", "index", "status", "duration_ns", "dropped_messages", "launched_processes",
            "input_fixture_sha256", "verification_sha256"
        }:
            raise PairError(f"sample journal row {index + 1} has an unexpected schema")
        evidence = sample["evidence"]
        expected = {
            "phase": "sample",
            "index": index,
            "status": "ok",
            "duration_ns": sample["duration_ns"],
            "dropped_messages": 0,
            "launched_processes": 1,
            "input_fixture_sha256": input_sha256,
            "verification_sha256": sha256_bytes(canonical_json(evidence["verification"])),
        }
        if row != expected:
            raise PairError(f"sample journal row {index + 1} disagrees with its full raw sample")
        normalized_rows.append(row)
    return {"sha256": sha256_bytes(payload), "row_count": len(rows), "records": normalized_rows}


def validate_harness_report(report_path: Path, attempt_dir: Path) -> dict[str, Any]:
    raw = report_path.read_bytes()
    report = parse_json(raw, "harness report")
    if not isinstance(report, dict) or set(report) != REPORT_FIELDS:
        raise PairError("harness report has an unexpected top-level schema")
    if report["format"] != REPORT_FORMAT or report["method"] != METHOD:
        raise PairError("harness report format or timed method differs from the frozen protocol")
    expected_payload_bytes = expected_collected_payload_bytes()
    expected_configuration = {
        **CONFIGURATION,
        "max_collected_bytes": expected_payload_bytes,
    }
    if report["configuration"] != expected_configuration:
        raise PairError("harness report configuration differs from the preregistered workload")
    identity = report["identity"]
    expected_keys = {
        "harness_sha256",
        "source_manifest_sha256",
        "graph_template_sha256",
        "input_fixture_path",
        "input_fixture_bytes",
        "input_fixture_sha256",
        "expected_payload_sha256_per_consumer",
        "sample_journal_path",
    }
    if not isinstance(identity, dict) or set(identity) != expected_keys:
        raise PairError("harness report identity has an unexpected schema")
    report_path = report_path.resolve(strict=True)
    fixture_path, journal_path = _expected_identity_paths(report_path)
    fixture_path = fixture_path.resolve(strict=False)
    journal_path = journal_path.resolve(strict=False)
    expected_fixture = expected_fixture_bytes()
    expected_fixture_hash = sha256_bytes(expected_fixture)
    if identity != {
        "harness_sha256": HARNESS_SHA256,
        "source_manifest_sha256": _manifest_sha256(),
        "graph_template_sha256": _graph_template_sha256(),
        "input_fixture_path": str(fixture_path),
        "input_fixture_bytes": len(expected_fixture),
        "input_fixture_sha256": expected_fixture_hash,
        "expected_payload_sha256_per_consumer": expected_fixture_hash,
        "sample_journal_path": str(journal_path),
    }:
        raise PairError("harness report source/configuration/fixture identity differs from the frozen oracle")
    try:
        fixture = fixture_path.read_bytes()
    except OSError as error:
        raise PairError(f"harness did not retain its input fixture: {error}") from error
    if fixture != expected_fixture:
        raise PairError("retained input fixture differs byte-for-byte from deterministic input")

    if report["warmups"] != []:
        raise PairError("harness report must contain zero inner warmups")
    samples = report["raw_samples_in_execution_order"]
    if not isinstance(samples, list) or len(samples) != INNER_SAMPLES:
        raise PairError("harness report must contain exactly three raw samples")
    durations: list[int] = []
    normalized_oracle_hashes: list[str] = []
    for index, sample in enumerate(samples):
        if not isinstance(sample, dict) or set(sample) != SAMPLE_FIELDS:
            raise PairError(f"raw sample {index} has an unexpected schema")
        if _integer(sample["sample_index"], "sample index") != index:
            raise PairError("raw sample order changed")
        duration = _integer(sample["duration_ns"], "duration_ns", minimum=1)
        evidence = sample["evidence"]
        if not isinstance(evidence, dict) or set(evidence) != EVIDENCE_FIELDS:
            raise PairError(f"raw sample {index} evidence has an unexpected schema")
        if evidence["duration_ns"] != duration:
            raise PairError(f"raw sample {index} duration duplicates disagree")
        if _integer(evidence["dropped_messages"], "dropped message count") != 0:
            raise PairError(f"raw sample {index} reports dropped graph events")
        if _integer(evidence["launched_processes"], "launched process count") != 1:
            raise PairError(f"raw sample {index} did not launch exactly one producer process")
        normalized = validate_verification(evidence["verification"], expected_fixture_hash)
        normalized_oracle_hashes.append(sha256_bytes(canonical_json(normalized)))
        durations.append(duration)

    if len(set(normalized_oracle_hashes)) != 1:
        raise PairError("full exact oracle differs between raw samples within this invocation")
    try:
        journal_bytes = journal_path.read_bytes()
    except OSError as error:
        raise PairError(f"harness did not retain its sample journal: {error}") from error
    journal = validate_journal(journal_bytes, samples, expected_fixture_hash)
    oracle = {
        "configuration": expected_configuration,
        "source_manifest_sha256": _manifest_sha256(),
        "graph_template_sha256": _graph_template_sha256(),
        "harness_sha256": HARNESS_SHA256,
        "fixture_bytes": len(expected_fixture),
        "fixture_sha256": expected_fixture_hash,
        "normalized_sample_oracle_sha256": normalized_oracle_hashes,
    }
    return {
        "raw_sha256": sha256_bytes(raw),
        "fixture_path": str(fixture_path),
        "fixture_bytes": len(fixture),
        "fixture_sha256": expected_fixture_hash,
        "journal": journal,
        "raw_duration_ns": durations,
        "median_duration_ns": int(statistics.median(durations)),
        "oracle": oracle,
    }


def _test_output_ok(stdout: bytes, stderr: bytes) -> bool:
    output = (stdout + b"\n" + stderr).decode("utf-8", errors="replace")
    return (
        "running 1 test" in output
        and f"test {TEST_NAME} ... ok" in output
        and re.search(
            r"test result: ok\. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in [0-9.]+s",
            output,
        )
        is not None
    )


def _snapshot(identity: Mapping[str, Any]) -> dict[str, Any]:
    result: dict[str, Any] = {"captured_at": utc_now()}
    root = Path(identity["root"])
    try:
        source = _inspect_source(root, identity["side"], Path(identity["harness_path"]).read_bytes())
        result.update(
            {
                "commit": source["commit"],
                "transport_sha256": source["transport_sha256"],
                "source_fingerprint": source["source_fingerprint"],
                "harness_sha256": source["harness_sha256"],
            }
        )
    except (PairError, OSError) as error:
        result["source_error"] = f"{type(error).__name__}: {error}"
    binary = Path(identity["saved_binary"])
    receipt = Path(identity["receipt_path"])
    try:
        result["binary_sha256"] = sha256_file(binary)
    except OSError as error:
        result["binary_error"] = f"{type(error).__name__}: {error}"
    try:
        result["receipt_sha256"] = sha256_file(receipt)
    except OSError as error:
        result["receipt_error"] = f"{type(error).__name__}: {error}"
    result["stable"] = (
        result.get("commit") == identity["commit"]
        and result.get("transport_sha256") == identity["transport_sha256"]
        and result.get("source_fingerprint") == identity["source_fingerprint"]
        and result.get("harness_sha256") == HARNESS_SHA256
        and result.get("binary_sha256") == identity["binary_sha256"]
        and result.get("receipt_sha256") == identity["receipt_sha256"]
        and "source_error" not in result
        and "binary_error" not in result
        and "receipt_error" not in result
    )
    return result


def _attempt_failure_reason(result: Mapping[str, Any]) -> str | None:
    if result.get("successful") is True:
        return None
    for field in ("execution_error", "validation_error"):
        value = result.get(field)
        if value:
            return str(value)
    return f"returncode={result.get('returncode')} timed_out={result.get('timed_out')}"


def run_attempt(identity: Mapping[str, Any], attempt_dir: Path, metadata: Mapping[str, Any]) -> dict[str, Any]:
    attempt_dir.mkdir(parents=True, exist_ok=False)
    raw_path = attempt_dir / "raw.json"
    binary = Path(identity["saved_binary"])
    command = [str(binary), "--exact", TEST_NAME, "--ignored", "--nocapture", "--test-threads=1"]
    environment = os.environ.copy()
    invocation_environment = {
        "EXECUTE_GRAPH_RECV_ANY_E2E_REPORT": str(raw_path.resolve()),
        "EXECUTE_GRAPH_RECV_ANY_E2E_WARMUPS": str(INNER_WARMUPS),
        "EXECUTE_GRAPH_RECV_ANY_E2E_SAMPLES": str(INNER_SAMPLES),
        "LC_ALL": "C",
        "TZ": "UTC",
    }
    environment.update(invocation_environment)
    before = _snapshot(identity)
    started = {
        **dict(metadata),
        "started_at": utc_now(),
        "command": command,
        "timeout_seconds": INVOCATION_TIMEOUT_SECONDS,
        "environment": invocation_environment,
        "identity_before": before,
        "raw_report_path": str(raw_path.resolve()),
    }
    _write_json_exclusive(attempt_dir / "started.json", started)

    start_ns = time.monotonic_ns()
    returncode: int | None = None
    timed_out = False
    execution_error: str | None = None
    stdout = b""
    stderr = b""
    if before.get("stable") is not True:
        execution_error = "source/binary/build receipt identity changed before invocation"
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
            stdout = completed.stdout
            stderr = completed.stderr
        except subprocess.TimeoutExpired as error:
            timed_out = True
            stdout = error.stdout or b""
            stderr = error.stderr or b""
        except OSError as error:
            execution_error = f"{type(error).__name__}: {error}"
            stderr = execution_error.encode("utf-8", errors="replace")
    elapsed_wall_ns = time.monotonic_ns() - start_ns
    after = _snapshot(identity)
    if after.get("stable") is not True:
        execution_error = "source/binary/build receipt identity changed during or after invocation"
    stdout_path = attempt_dir / "stdout.bin"
    stderr_path = attempt_dir / "stderr.bin"
    stdout_path.write_bytes(stdout)
    stderr_path.write_bytes(stderr)

    raw_summary: dict[str, Any] | None = None
    validation_error: str | None = None
    if raw_path.is_file():
        try:
            raw_summary = validate_harness_report(raw_path, attempt_dir)
        except (PairError, OSError, ValueError) as error:
            validation_error = f"{type(error).__name__}: {error}"
    else:
        validation_error = "harness did not create a final raw report"
    if not _test_output_ok(stdout, stderr):
        output_error = "test process did not report exactly one passing ignored benchmark test"
        validation_error = f"{validation_error}; {output_error}" if validation_error else output_error
    successful = returncode == 0 and not timed_out and execution_error is None and validation_error is None
    raw_fixture_path = attempt_dir / "raw.input.jsonl"
    raw_journal_path = attempt_dir / "raw.journal.jsonl"
    finished = {
        **dict(metadata),
        "finished_at": utc_now(),
        "returncode": returncode,
        "timed_out": timed_out,
        "execution_error": execution_error,
        "validation_error": validation_error,
        "elapsed_wall_ns": elapsed_wall_ns,
        "identity_before": before,
        "identity_after": after,
        "successful": successful,
        "raw_report_present": raw_path.is_file(),
        "raw_report_sha256": sha256_file(raw_path) if raw_path.is_file() else None,
        "input_fixture_present": raw_fixture_path.is_file(),
        "input_fixture_sha256": sha256_file(raw_fixture_path) if raw_fixture_path.is_file() else None,
        "sample_journal_present": raw_journal_path.is_file(),
        "sample_journal_sha256": sha256_file(raw_journal_path) if raw_journal_path.is_file() else None,
        "stdout_sha256": sha256_bytes(stdout),
        "stderr_sha256": sha256_bytes(stderr),
        "raw_duration_ns": raw_summary["raw_duration_ns"] if raw_summary else None,
        "median_duration_ns": raw_summary["median_duration_ns"] if raw_summary else None,
        "oracle": raw_summary["oracle"] if raw_summary else None,
        "attempt_dir": str(attempt_dir.resolve()),
    }
    _write_json_exclusive(attempt_dir / "finished.json", finished)
    return finished


def pair_order(global_pair_index: int) -> tuple[str, str]:
    return ("baseline", "candidate") if global_pair_index % 2 == 0 else ("candidate", "baseline")


def _pair_result(global_index: int, results: Mapping[str, Mapping[str, Any]]) -> dict[str, Any]:
    baseline = results["baseline"]
    candidate = results["candidate"]
    oracle_equal = (
        baseline.get("oracle") == candidate.get("oracle")
        if baseline.get("successful") is True and candidate.get("successful") is True
        else False
    )
    valid = baseline.get("successful") is True and candidate.get("successful") is True and oracle_equal
    phase = "warmup" if global_index < WARMUP_PAIRS else "measurement"
    phase_index = global_index + 1 if phase == "warmup" else global_index - WARMUP_PAIRS + 1
    result: dict[str, Any] = {
        "global_pair_index": global_index,
        "phase": phase,
        "phase_pair_index": phase_index,
        "order": list(pair_order(global_index)),
        "baseline_attempt_id": baseline.get("attempt_id"),
        "candidate_attempt_id": candidate.get("attempt_id"),
        "valid_for_timing_comparison": valid,
        "full_oracle_equal": oracle_equal,
        "cases": {},
    }
    if not valid:
        result["failure_reasons"] = {
            "baseline": _attempt_failure_reason(baseline),
            "candidate": _attempt_failure_reason(candidate),
            "oracle_mismatch": baseline.get("successful") is True and candidate.get("successful") is True and not oracle_equal,
        }
        return result
    base_raw = baseline["raw_duration_ns"]
    candidate_raw = candidate["raw_duration_ns"]
    base_median = int(statistics.median(base_raw))
    candidate_median = int(statistics.median(candidate_raw))
    result["cases"]["whole_graph_session"] = {
        "baseline_raw_duration_ns": base_raw,
        "candidate_raw_duration_ns": candidate_raw,
        "baseline_median_duration_ns": base_median,
        "candidate_median_duration_ns": candidate_median,
        "candidate_minus_baseline_ns": candidate_median - base_median,
        "candidate_over_baseline": candidate_median / base_median,
        "candidate_change_percent": (candidate_median / base_median - 1.0) * 100.0,
    }
    return result


def _summary(pairs: Sequence[Mapping[str, Any]]) -> dict[str, Any]:
    valid_deltas: list[int] = []
    deltas_in_order: list[dict[str, Any]] = []
    for pair in pairs:
        if pair.get("phase") != "measurement":
            continue
        if pair.get("valid_for_timing_comparison") is True:
            delta = int(pair["cases"]["whole_graph_session"]["candidate_minus_baseline_ns"])
            valid_deltas.append(delta)
            deltas_in_order.append({"global_pair_index": pair["global_pair_index"], "candidate_minus_baseline_ns": delta})
        else:
            deltas_in_order.append({"global_pair_index": pair["global_pair_index"], "candidate_minus_baseline_ns": None})
    return {
        "metric": "whole_graph_session median duration per invocation; candidate minus baseline",
        "valid_measured_pair_count": len(valid_deltas),
        "paired_deltas_in_order": deltas_in_order,
        "median_paired_delta_ns": statistics.median(valid_deltas) if valid_deltas else None,
        "candidate_loss_pair_indices": [item["global_pair_index"] for item in deltas_in_order if item["candidate_minus_baseline_ns"] is not None and item["candidate_minus_baseline_ns"] > 0],
        "candidate_improvement_pair_indices": [item["global_pair_index"] for item in deltas_in_order if item["candidate_minus_baseline_ns"] is not None and item["candidate_minus_baseline_ns"] < 0],
        "tie_pair_indices": [item["global_pair_index"] for item in deltas_in_order if item["candidate_minus_baseline_ns"] == 0],
    }


def _verify_preregistration(path: Path) -> dict[str, Any]:
    prereg = load_json(path, "paired preregistration", 8 * 1024 * 1024)
    if not isinstance(prereg, dict) or prereg.get("schema_version") != SCHEMA_VERSION:
        raise PairError("paired preregistration has an unexpected schema")
    if prereg.get("protocol") != _protocol():
        raise PairError("preregistered protocol differs from this runner")
    driver = prereg.get("driver", {})
    script = Path(__file__).resolve(strict=True)
    if driver != {"path": str(script), "sha256": sha256_file(script)}:
        raise PairError("runner source changed after preregistration")
    inputs = prereg.get("inputs")
    if not isinstance(inputs, dict) or set(inputs) != {"harness", "baseline", "candidate", "build_provenance"}:
        raise PairError("preregistration inputs have an unexpected schema")
    harness_ref = inputs["harness"]
    harness_path = Path(harness_ref.get("path", "")).expanduser().resolve(strict=True)
    harness = harness_path.read_bytes()
    if harness_ref != {
        "path": str(harness_path),
        "sha256": HARNESS_SHA256,
        "baseline_copy": str(BASELINE_ROOT / HARNESS_RELATIVE),
        "candidate_copy": str(Path(inputs["candidate"]["root"]) / HARNESS_RELATIVE),
    } or sha256_bytes(harness) != HARNESS_SHA256:
        raise PairError("canonical or per-arm harness identity changed after preregistration")
    regenerated: dict[str, dict[str, Any]] = {}
    for side in ("baseline", "candidate"):
        expected = inputs[side]
        current = _receipt_identity(Path(expected["receipt_path"]), side, Path(expected["root"]), harness)
        if current != expected:
            raise PairError(f"{side} source/build receipt identity changed after preregistration")
        regenerated[side] = current
    _validate_source_pair(regenerated["baseline"], regenerated["candidate"])
    if _build_provenance(regenerated["baseline"], regenerated["candidate"]) != inputs["build_provenance"]:
        raise PairError("captured build environment, driver, or logs changed after preregistration")
    return prereg


def _attempt_metadata(global_index: int, position: int, side: str, identity: Mapping[str, Any]) -> dict[str, Any]:
    phase = "warmup" if global_index < WARMUP_PAIRS else "measurement"
    phase_index = global_index + 1 if phase == "warmup" else global_index - WARMUP_PAIRS + 1
    return {
        "attempt_id": f"{phase}-{phase_index:02d}-{position}-{side}",
        "phase": phase,
        "phase_pair_index": phase_index,
        "global_pair_index": global_index,
        "position_in_pair": position,
        "side": side,
        "commit": identity["commit"],
        "transport_sha256": identity["transport_sha256"],
        "source_fingerprint_sha256": identity["source_fingerprint"]["sha256"],
        "binary_sha256": identity["binary_sha256"],
        "receipt_sha256": identity["receipt_sha256"],
    }


def _unattempted_slot(global_index: int, position: int, side: str, reason: str, identity: Mapping[str, Any]) -> dict[str, Any]:
    metadata = _attempt_metadata(global_index, position, side, identity)
    return {
        **metadata,
        "status": "not_attempted",
        "reason": reason,
        "retry_count": 0,
    }


def check_disk_space(report_parent: Path) -> dict[str, int]:
    report_parent.mkdir(parents=True, exist_ok=True)
    usage = shutil.disk_usage(report_parent)
    if usage.free < MIN_FREE_BYTES:
        raise PairError(
            f"raw evidence preflight requires {MIN_FREE_BYTES} free bytes, found {usage.free} at {report_parent}"
        )
    return {"total_bytes": usage.total, "used_bytes": usage.used, "free_bytes": usage.free}


def run(args: argparse.Namespace) -> int:
    prereg_path = Path(args.preregistration).expanduser().resolve(strict=True)
    prereg = _verify_preregistration(prereg_path)
    report_dir = Path(args.report_dir).expanduser().absolute()
    if report_dir.exists() or report_dir.is_symlink():
        raise PairError(f"refusing to overwrite report directory: {report_dir}")
    disk_preflight = check_disk_space(report_dir.parent)
    report_dir.mkdir(parents=False, exist_ok=False)
    inputs = prereg["inputs"]
    identities = {side: inputs[side] for side in ("baseline", "candidate")}
    initial_identity = {side: _snapshot(identity) for side, identity in identities.items()}
    report_path = report_dir / "paired-report.json"
    report: dict[str, Any] = {
        "schema_version": SCHEMA_VERSION,
        "format": "execute.graph.recv_any_e2e.paired_run.v1",
        "created_at": utc_now(),
        "status": "running",
        "preregistration_path": str(prereg_path),
        "preregistration_sha256": sha256_file(prereg_path),
        "protocol": prereg["protocol"],
        "host": {"system": platform.system(), "release": platform.release(), "machine": platform.machine(), "python": sys.version},
        "disk_preflight": disk_preflight,
        "initial_identity": initial_identity,
        "attempts": [],
        "warmup_pairs": [],
        "measured_pairs": [],
        "losses": [],
        "unattempted_slots": [],
    }
    _write_json_exclusive(report_path, report)
    overall_failed = any(value.get("stable") is not True for value in initial_identity.values())
    stop_reason: str | None = "initial_identity_changed" if overall_failed else None

    for global_index in range(TOTAL_PAIRS):
        if stop_reason is not None:
            for position, side in enumerate(pair_order(global_index), start=1):
                loss = _unattempted_slot(global_index, position, side, stop_reason, identities[side])
                report["unattempted_slots"].append(loss)
                report["losses"].append(loss)
            continue
        results: dict[str, dict[str, Any]] = {}
        for position, side in enumerate(pair_order(global_index), start=1):
            identity = identities[side]
            metadata = _attempt_metadata(global_index, position, side, identity)
            attempt_dir = report_dir / "attempts" / metadata["attempt_id"]
            try:
                result = run_attempt(identity, attempt_dir, metadata)
            except (PairError, OSError, ValueError, TypeError, KeyError) as error:
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
                failure = {**metadata, "status": "attempt_failed", "reason": _attempt_failure_reason(result)}
                report["losses"].append(failure)
            _write_json_replace(report_path, report)

        pair = _pair_result(global_index, results)
        key = "warmup_pairs" if global_index < WARMUP_PAIRS else "measured_pairs"
        report[key].append(pair)
        if pair["valid_for_timing_comparison"] is not True:
            overall_failed = True
            stop_reason = "aborted_after_failed_pair_no_retries"
        _write_json_replace(report_path, report)
        if stop_reason is not None:
            for remaining_index in range(global_index + 1, TOTAL_PAIRS):
                for position, side in enumerate(pair_order(remaining_index), start=1):
                    loss = _unattempted_slot(remaining_index, position, side, stop_reason, identities[side])
                    report["unattempted_slots"].append(loss)
                    report["losses"].append(loss)
            break

    post_identity = {side: _snapshot(identity) for side, identity in identities.items()}
    if any(value.get("stable") is not True for value in post_identity.values()):
        overall_failed = True
    report["post_run_identity"] = post_identity
    report["paired_summary"] = _summary(report["warmup_pairs"] + report["measured_pairs"])
    report["completed_at"] = utc_now()
    report["planned_pair_count"] = TOTAL_PAIRS
    report["planned_attempt_count"] = TOTAL_ATTEMPTS
    report["attempt_count"] = len(report["attempts"])
    report["successful_attempt_count"] = sum(item.get("successful") is True for item in report["attempts"])
    report["failed_attempt_count"] = report["attempt_count"] - report["successful_attempt_count"]
    report["unattempted_attempt_count"] = len(report["unattempted_slots"])
    complete = (
        not overall_failed
        and report["attempt_count"] == TOTAL_ATTEMPTS
        and len(report["warmup_pairs"]) == WARMUP_PAIRS
        and len(report["measured_pairs"]) == MEASURED_PAIRS
        and all(pair.get("valid_for_timing_comparison") is True for pair in report["warmup_pairs"] + report["measured_pairs"])
        and all(value.get("stable") is True for value in post_identity.values())
    )
    report["status"] = "complete" if complete else "failed"
    _write_json_replace(report_path, report)
    print(json.dumps({"report": str(report_path), "status": report["status"]}, sort_keys=True))
    return 0 if complete else 1


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    prepare_parser = commands.add_parser("prepare", help="freeze both source trees, build receipts, harness, and pair schedule")
    prepare_parser.add_argument("--candidate-root", required=True)
    prepare_parser.add_argument("--baseline-receipt", required=True)
    prepare_parser.add_argument("--candidate-receipt", required=True)
    prepare_parser.add_argument("--harness-source", required=True)
    prepare_parser.add_argument("--preregistration", required=True)
    prepare_parser.set_defaults(function=prepare)
    run_parser = commands.add_parser("run", help="run 3 warmup and 48 measured alternating pairs")
    run_parser.add_argument("--preregistration", required=True)
    run_parser.add_argument("--report-dir", required=True)
    run_parser.set_defaults(function=run)
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    args = _parser().parse_args(argv)
    try:
        result = args.function(args)
    except (PairError, OSError, ValueError, TypeError, KeyError) as error:
        print(f"paired recv_any E2E benchmark failed: {type(error).__name__}: {error}", file=sys.stderr)
        return 1
    return int(result or 0)


if __name__ == "__main__":
    raise SystemExit(main())
