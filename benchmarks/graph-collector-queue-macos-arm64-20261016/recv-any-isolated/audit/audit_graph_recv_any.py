#!/usr/bin/env python3
"""Independent, read-only audit for the frozen single-queue recv_any trial.

Run --source-only while a trial is in progress. Full mode refuses to open any
attempt artifacts until the atomically-written paired report says "complete".
It reads each finished invocation's raw report one at a time and recomputes
event, process, journal, paired-schedule, and timing-summary oracles without
importing the frozen runner.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import statistics
import subprocess
import sys
from typing import Any, Iterable, Mapping


CAMPAIGN = Path(__file__).resolve().parents[2]
EVIDENCE = CAMPAIGN / "evidence/graph-recv-any-single-queue"
ATTEMPT_ROOT = EVIDENCE / "paired-run-attempt-1"
PREREG = EVIDENCE / "paired-preregistration.json"
SOURCE_FREEZE = EVIDENCE / "paired-runner-source-freeze.json"
BASELINE_ROOT = CAMPAIGN / "work/graph-recv-any-collector-baseline-a884"
CANDIDATE_ROOT = CAMPAIGN / "work/graph-recv-any-collector-queue-a884"
RUNNER = CAMPAIGN / "work/graph-recv-any-e2e-paired-runner/graph_recv_any_e2e_pair.py"
DRIVER = CAMPAIGN / "work/build_graph_recv_any_e2e_pair.py"
HARNESS = EVIDENCE / "graph_recv_any_e2e_bench-v2.rs"
CANONICAL_HARNESS_PATH = BASELINE_ROOT / "tests/graph_recv_any_e2e_bench.rs"

PINS = {
    "preregistration": "540991bbea87aafa25b2eee5a064d5db733d355d2fa89efa016bc4c6309edb68",
    "source_freeze": "5a21e7995a35dec654ce44983ba34c8cf0b1731b6c5af5dda2a251da4427464c",
    "runner": "98ddece53f24fc68a2afd1e9117791de1b119f1e673b728b4c3a42d2add3ad6e",
    "build_driver": "e9be14febfa53f3c744773ef8581d813cd69e14583284abea70cef831a49964e",
    "harness": "c38f59198e056576b69481e10bfa0cf2ac95124095869550a0d863ceb2bb18c1",
    "shared_collector_patch": "7d411b9b0fc3a8a37fbd589381720016916b6edabec43be06414e198cd39bca4",
    "candidate_queue_patch": "9135c5ee1ea8b350e6e50acb76eef19d5603a044f6dbcf51271bde83088b0400",
    "baseline_transport": "bfc03a7028b2871a4823ed0a130e3b735ca8dc535cac9c12cd39d876174dad9c",
    "candidate_transport": "ac546546ac93266b675ba0f37418961b3bc1672d6cd34c68974aeaf1f6c8ee45",
}

BASE_COMMIT = "a884f6561fd43f45e43b25a3cc2233be17ee3944"
REPORT_FORMAT = "execute.graph.recv_any_e2e.v1"
PAIRED_FORMAT = "execute.graph.recv_any_e2e.paired_run.v1"
TEST_NAME = "bounded_native_stream_recv_any_has_exact_end_to_end_samples"
METHOD = (
    "wall-clock GraphSession start through run_to_completion; real finite cat NativeStream, typed JSON decoding, "
    "two bounded lossless fanout edges, two custom pass-through receivers using recv_any on one queue each, and "
    "terminal collection; all exact payload/order/hash validation follows the timed region"
)
RECORDS = 4096
TEXT_BYTES = 256
EDGE_CAPACITY = 8
TERMINAL_CAPACITY = 16
GENERATION = 17
REVISION = 29
INNER_SAMPLES = 3
WARMUP_PAIRS = 3
MEASURED_PAIRS = 48
TOTAL_PAIRS = WARMUP_PAIRS + MEASURED_PAIRS
TOTAL_ATTEMPTS = TOTAL_PAIRS * 2
ATTEMPT_TIMEOUT_SECONDS = 120
MAX_REPORT_BYTES = 64 * 1024 * 1024
MAX_JOURNAL_BYTES = 1024 * 1024
MAX_INTEGER = (1 << 64) - 1
SHA256_RE = re.compile(r"^[0-9a-f]{64}$")
BUILD_INPUT_ROOTS = ("src/", "vendor/", ".cargo/")
BUILD_CONFIG_FILES = {
    "Cargo.toml", "Cargo.lock", "build.rs", "rust-toolchain", "rust-toolchain.toml", "rustfmt.toml"
}


class AuditError(RuntimeError):
    pass


def require(ok: bool, message: str) -> None:
    if not ok:
        raise AuditError(message)


def sha_bytes(value: bytes) -> str:
    return hashlib.sha256(value).hexdigest()


def sha_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def canonical(value: Any) -> bytes:
    return json.dumps(
        value, sort_keys=True, separators=(",", ":"), ensure_ascii=False, allow_nan=False
    ).encode("utf-8")


def _unique_object(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    value: dict[str, Any] = {}
    for key, item in pairs:
        require(key not in value, f"duplicate JSON key {key!r}")
        value[key] = item
    return value


def _reject_constant(value: str) -> None:
    raise AuditError(f"non-finite JSON constant {value!r}")


def strict_json_bytes(payload: bytes, label: str, limit: int = MAX_REPORT_BYTES) -> Any:
    require(len(payload) <= limit, f"{label} exceeds {limit} byte audit limit")
    try:
        return json.loads(
            payload.decode("utf-8"),
            object_pairs_hook=_unique_object,
            parse_constant=_reject_constant,
        )
    except (UnicodeDecodeError, json.JSONDecodeError, ValueError) as error:
        raise AuditError(f"{label} is not strict UTF-8 JSON: {error}") from error


def strict_json(path: Path, label: str, limit: int = MAX_REPORT_BYTES) -> Any:
    try:
        return strict_json_bytes(path.read_bytes(), label, limit)
    except OSError as error:
        raise AuditError(f"cannot read {label}: {error}") from error


def exact_int(value: Any, label: str, minimum: int = 0, maximum: int = MAX_INTEGER) -> int:
    require(type(value) is int and minimum <= value <= maximum, f"{label} is not an integer in range")
    return value


def git(root: Path, *args: str) -> bytes:
    try:
        result = subprocess.run(
            ["git", "-C", str(root), *args],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=30,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        raise AuditError(f"cannot inspect source worktree {root}: {error}") from error
    require(result.returncode == 0, result.stderr.decode("utf-8", errors="replace").strip())
    return result.stdout


def build_fingerprint(root: Path) -> dict[str, Any]:
    paths = sorted(
        item.decode("utf-8")
        for item in git(root, "ls-files", "-z").split(b"\0")
        if item
        and (
            item.decode("utf-8").startswith(BUILD_INPUT_ROOTS)
            or item.decode("utf-8") in BUILD_CONFIG_FILES
        )
    )
    files: dict[str, str] = {}
    digest = hashlib.sha256()
    for relative in paths:
        path = root / relative
        require(path.is_file(), f"tracked build input is absent: {relative}")
        content_hash = sha_file(path)
        files[relative] = content_hash
        digest.update(relative.encode("utf-8"))
        digest.update(b"\0")
        digest.update(content_hash.encode("ascii"))
        digest.update(b"\n")
    require(files, f"no tracked build inputs in {root}")
    return {"sha256": digest.hexdigest(), "files": files}


def normalized_status(root: Path) -> list[str]:
    output = git(root, "status", "--porcelain", "--untracked-files=all").decode("utf-8")
    return sorted(output.splitlines())


def read_artifact_row(path: Path) -> dict[str, Any]:
    matches: list[dict[str, Any]] = []
    try:
        with path.open("rb") as stream:
            for line_no, line in enumerate(stream, start=1):
                if not line.strip():
                    continue
                row = strict_json_bytes(line, f"Cargo JSON line {line_no}", 2_000_000)
                if (
                    isinstance(row, dict)
                    and row.get("reason") == "compiler-artifact"
                    and isinstance(row.get("target"), dict)
                    and row["target"].get("name") == "graph_recv_any_e2e_bench"
                ):
                    matches.append(row)
    except OSError as error:
        raise AuditError(f"cannot read Cargo build output {path}: {error}") from error
    require(len(matches) == 1, f"expected one benchmark Cargo artifact in {path}, found {len(matches)}")
    return matches[0]


def verify_source_build_inputs(prereg: Mapping[str, Any]) -> dict[str, Any]:
    require(sha_file(PREREG) == PINS["preregistration"], "paired preregistration hash differs from the fixed pin")
    require(sha_file(SOURCE_FREEZE) == PINS["source_freeze"], "paired runner source-freeze hash differs from pin")
    require(sha_file(RUNNER) == PINS["runner"], "paired runner changed after freeze")
    require(sha_file(DRIVER) == PINS["build_driver"], "build driver changed after freeze")
    require(sha_file(HARNESS) == PINS["harness"], "canonical benchmark harness changed after freeze")

    freeze = strict_json(SOURCE_FREEZE, "source freeze", 2_000_000)
    require(freeze.get("schema") == "execute.graph-recv-any-single-queue-paired-runner-source-freeze/2", "wrong freeze schema")
    require(freeze["arms"]["shared_harness_sha256"] == PINS["harness"], "freeze does not pin the benchmark harness")
    require(freeze["paired_runner"]["source_sha256"] == PINS["runner"], "freeze does not pin the paired runner")
    require(freeze["paired_runner"]["build_driver_sha256"] == PINS["build_driver"], "freeze does not pin build driver")

    require(prereg.get("schema_version") == 1, "wrong preregistration schema version")
    protocol = prereg.get("protocol")
    require(isinstance(protocol, dict), "preregistration protocol is missing")
    expected_protocol = {
        "test_name": TEST_NAME,
        "harness_format": REPORT_FORMAT,
        "harness_sha256": PINS["harness"],
        "baseline_commit": BASE_COMMIT,
        "warmup_pairs": WARMUP_PAIRS,
        "measured_pairs": MEASURED_PAIRS,
        "inner_harness_warmups": 0,
        "inner_harness_samples": INNER_SAMPLES,
        "invocation_timeout_seconds": ATTEMPT_TIMEOUT_SECONDS,
        "retry_policy": "no retries; run both sides of the current pair, then stop after any failed pair and record all remaining planned slots as not attempted",
    }
    for key, expected in expected_protocol.items():
        require(protocol.get(key) == expected, f"preregistered {key} does not match fixed design")
    require(protocol["workload"] == {
        "record_count": RECORDS,
        "text_bytes_per_record": TEXT_BYTES,
        "producer": "/bin/cat through NativeStream to natural EOF",
        "consumers": 2,
        "edge_capacity": EDGE_CAPACITY,
        "edge_policy": "lossless",
    }, "preregistered finite workload differs from the fixed design")
    require(protocol["pair_order"] == "alternating AB/BA across three warmup pairs and 48 measured pairs; first measured pair is BA", "pair schedule differs")
    require(protocol["invocation_environment"] == {
        "EXECUTE_GRAPH_RECV_ANY_E2E_REPORT": "absolute attempt-local raw.json",
        "EXECUTE_GRAPH_RECV_ANY_E2E_WARMUPS": "0",
        "EXECUTE_GRAPH_RECV_ANY_E2E_SAMPLES": "3",
        "LC_ALL": "C",
        "TZ": "UTC",
    }, "inner benchmark environment differs")

    inputs = prereg.get("inputs")
    require(isinstance(inputs, dict) and set(inputs) == {"harness", "baseline", "candidate", "build_provenance"}, "preregistration inputs schema differs")
    harness = inputs["harness"]
    require(
        harness == {
            "path": str(CANONICAL_HARNESS_PATH),
            "sha256": PINS["harness"],
            "baseline_copy": str(CANONICAL_HARNESS_PATH),
            "candidate_copy": str(CANDIDATE_ROOT / "tests/graph_recv_any_e2e_bench.rs"),
        },
        "canonical harness preregistration differs",
    )

    sides: dict[str, Any] = {}
    expected_status = sorted([" M src/graph/transport.rs", "?? tests/graph_recv_any_e2e_bench.rs"])
    for side, root, transport_pin in (
        ("baseline", BASELINE_ROOT, PINS["baseline_transport"]),
        ("candidate", CANDIDATE_ROOT, PINS["candidate_transport"]),
    ):
        identity = inputs[side]
        require(Path(identity["root"]).resolve(strict=True) == root.resolve(strict=True), f"{side} root changed")
        require(git(root, "rev-parse", "HEAD").decode("ascii").strip() == BASE_COMMIT, f"{side} base commit changed")
        require(normalized_status(root) == expected_status, f"{side} worktree changes differ from frozen source composition")
        require(identity["commit"] == BASE_COMMIT and identity["source_stable"] is True, f"{side} preregistration source identity is not stable")
        require(identity["transport_sha256"] == transport_pin, f"{side} transport hash differs from its composition pin")
        source = root / "src/graph/transport.rs"
        require(sha_file(source) == transport_pin, f"{side} transport source changed")
        harness_copy = root / "tests/graph_recv_any_e2e_bench.rs"
        require(sha_file(harness_copy) == PINS["harness"], f"{side} harness copy changed")
        fp = build_fingerprint(root)
        require(fp == identity["source_fingerprint"], f"{side} full tracked source/build fingerprint differs from preregistration")
        sides[side] = {"root": str(root), "transport_sha256": transport_pin, "build_input_count": len(fp["files"]), "source_fingerprint_sha256": fp["sha256"]}
    base_files = inputs["baseline"]["source_fingerprint"]["files"]
    candidate_files = inputs["candidate"]["source_fingerprint"]["files"]
    require(base_files.keys() == candidate_files.keys(), "build input path sets differ across arms")
    require({key for key in base_files if base_files[key] != candidate_files[key]} == {"src/graph/transport.rs"}, "build inputs differ outside intended transport source")

    patches = {
        EVIDENCE / "collector-progress.patch": PINS["shared_collector_patch"],
        EVIDENCE / "graph-recv-any-single-queue-product-v2.patch": PINS["candidate_queue_patch"],
    }
    for path, digest in patches.items():
        require(sha_file(path) == digest, f"source composition patch changed: {path.name}")

    provenance = inputs["build_provenance"]
    env_ref = provenance["environment"]
    env_path = Path(env_ref["path"])
    require(sha_file(env_path) == env_ref["sha256"], "captured build environment changed")
    build_env = strict_json(env_path, "captured build environment", 2_000_000)
    require(build_env.get("captured_before_clean_and_build") is True, "compiler environment was not captured before clean/build")
    require(isinstance(build_env.get("rustc_vv"), str) and build_env["rustc_vv"].startswith("rustc "), "captured rustc version is missing")
    require(isinstance(build_env.get("cargo_version"), str) and build_env["cargo_version"].startswith("cargo "), "captured Cargo version is missing")
    selected_env = build_env.get("selected_build_environment")
    require(isinstance(selected_env, dict), "selected build environment is missing")
    require(selected_env.get("CARGO_TARGET_DIR") == str(CAMPAIGN / "work/perf-target-candidate"), "build target directory differs from preregistration")
    require(selected_env.get("CARGO_BUILD_JOBS") == "4", "build parallelism differs from captured configuration")
    require(provenance["driver"] == {"path": str(DRIVER), "sha256": PINS["build_driver"]}, "build driver receipt differs")
    require(sha_file(Path(provenance["driver"]["path"])) == PINS["build_driver"], "pinned build driver bytes differ")

    built: dict[str, Any] = {}
    for side in ("baseline", "candidate"):
        identity = inputs[side]
        receipt_path = Path(identity["receipt_path"])
        require(sha_file(receipt_path) == identity["receipt_sha256"], f"{side} binary receipt changed")
        receipt = strict_json(receipt_path, f"{side} binary receipt", 2_000_000)
        require(receipt.get("source_stable") is True and receipt.get("attempt") == 1, f"{side} build receipt source/attempt differs")
        require(receipt.get("source_sha256") == identity["transport_sha256"], f"{side} build receipt source hash differs")
        require(receipt.get("build_metadata") == build_env, f"{side} build metadata differs from captured environment")
        require(identity.get("build_metadata") == build_env, f"{side} preregistration build metadata differs")
        require(receipt.get("saved_binary") == identity.get("saved_binary") and receipt.get("sha256") == identity.get("binary_sha256"), f"{side} saved binary receipt fields differ")
        binary = Path(receipt["saved_binary"])
        require(binary.is_file() and os.access(binary, os.X_OK), f"{side} immutable binary copy absent/not executable")
        require(sha_file(binary) == receipt["sha256"], f"{side} saved binary checksum differs")
        artifact_log = EVIDENCE / f"{side}-build-attempt-1.jsonl"
        artifact = read_artifact_row(artifact_log)
        require(receipt.get("artifact") == artifact == identity.get("artifact"), f"{side} Cargo artifact receipt differs from build JSONL")
        target = artifact.get("target", {})
        profile = artifact.get("profile", {})
        require(
            artifact.get("fresh") is False
            and target.get("name") == "graph_recv_any_e2e_bench"
            and target.get("kind") == ["test"]
            and target.get("test") is True
            and profile.get("test") is True
            and profile.get("opt_level") == "3"
            and profile.get("debug_assertions") is False,
            f"{side} Cargo row is not a newly compiled optimized test binary",
        )
        require(Path(artifact["manifest_path"]).resolve(strict=True) == (Path(identity["root"]) / "Cargo.toml"), f"{side} artifact manifest path mismatch")
        require(Path(target["src_path"]).resolve(strict=True) == Path(identity["harness_path"]), f"{side} artifact test source path mismatch")
        require(Path(artifact["executable"]).is_relative_to(Path(build_env["selected_build_environment"]["CARGO_TARGET_DIR"]) / "release/deps"), f"{side} Cargo executable is outside shared release deps")
        require(Path(artifact["executable"]) in {Path(filename) for filename in artifact.get("filenames", [])}, f"{side} executable absent from Cargo artifact filenames")
        test_list = EVIDENCE / f"{side}-test-list.txt"
        require(TEST_NAME in test_list.read_text(encoding="utf-8"), f"{side} test-list receipt omits benchmark")
        for label, binding in provenance["logs"][side].items():
            path = Path(binding["path"])
            require(sha_file(path) == binding["sha256"], f"{side} {label} build receipt log changed")
        built[side] = {
            "binary_path": str(binary),
            "binary_sha256": receipt["sha256"],
            "artifact_fresh": artifact["fresh"],
            "target_name": target["name"],
            "opt_level": profile["opt_level"],
            "rustc_vv_sha256": sha_bytes(build_env["rustc_vv"].encode()),
            "cargo_version": build_env["cargo_version"].strip(),
        }
    require(inputs["baseline"]["build_metadata"] == inputs["candidate"]["build_metadata"], "compiler/build environments differ across arms")
    require(built["baseline"]["rustc_vv_sha256"] == built["candidate"]["rustc_vv_sha256"], "compiler identity differs across arms")

    return {
        "status": "source_provenance_verified",
        "preregistration_sha256": PINS["preregistration"],
        "runner_sha256": PINS["runner"],
        "build_driver_sha256": PINS["build_driver"],
        "harness_sha256": PINS["harness"],
        "source_arms": sides,
        "builds": built,
        "attempt_artifacts_read": False,
    }


def expected_fixture() -> bytes:
    return b"".join(canonical({"id": i, "text": "x" * TEXT_BYTES}) + b"\n" for i in range(1, RECORDS + 1))


def expected_manifest_hash() -> str:
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
    return sha_bytes(canonical(manifest))


def expected_graph_hash() -> str:
    template = "graph graph_recv_any_e2e\nstream from bench.jsonl_source with path __FIXTURE__ as producer\nshow each line\nend graph\n"
    return sha_bytes(template.encode())


def expected_config() -> dict[str, Any]:
    typed_bytes = sum(
        len(canonical({"Record": {"id": {"Integer": i}, "text": {"String": "x" * TEXT_BYTES}}}))
        for i in range(1, RECORDS + 1)
    )
    result_bytes = typed_bytes * 2 + len(canonical({"Integer": 0}))
    return {
        "inner_warmups": 0,
        "raw_samples": INNER_SAMPLES,
        "record_count": RECORDS,
        "text_bytes_per_record": TEXT_BYTES,
        "edge_capacity": EDGE_CAPACITY,
        "edge_policy": "lossless",
        "terminal_capacity": TERMINAL_CAPACITY,
        "generation": GENERATION,
        "revision": REVISION,
        "max_collected_events": RECORDS * 2 + 1,
        "max_message_bytes": 4096,
        "expected_launched_processes": 1,
        "max_collected_bytes": result_bytes,
    }


def expected_attempt_metadata(global_index: int, position: int, side: str, identity: Mapping[str, Any]) -> dict[str, Any]:
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


def expected_order(pair_index: int) -> tuple[str, str]:
    return ("baseline", "candidate") if pair_index % 2 == 0 else ("candidate", "baseline")


def normalize_identity(value: Mapping[str, Any]) -> dict[str, Any]:
    return {key: item for key, item in value.items() if key != "sequence"}


def verify_event_identity(value: Any, node: str, port: str, input_port: Any, label: str) -> int:
    require(isinstance(value, dict) and set(value) == {"port", "input", "generation", "revision", "sequence", "sensitivity", "remote"}, f"{label}: event identity shape differs")
    require(value["port"] == {"node": node, "port": port} and value["input"] == input_port, f"{label}: port/input identity differs")
    require(exact_int(value["generation"], f"{label} generation") == GENERATION, f"{label}: generation differs")
    require(exact_int(value["revision"], f"{label} revision") == REVISION, f"{label}: revision differs")
    require(value["sensitivity"] == ["unknown"] and value["remote"] is None, f"{label}: sensitivity/remote metadata differs")
    return exact_int(value["sequence"], f"{label} sequence", minimum=1)


def verify_sample(verification: Any, fixture_hash: str) -> dict[str, Any]:
    require(isinstance(verification, dict) and set(verification) == {
        "terminal_event_count", "source_status_event_count", "source_status_identity", "consumers"
    }, "sample verification shape differs")
    require(exact_int(verification["terminal_event_count"], "terminal event count") == 2 * RECORDS + 1, "terminal event count is not exactly 8193")
    require(exact_int(verification["source_status_event_count"], "source status count") == 1, "source status count is not one")
    status_seq = verify_event_identity(verification["source_status_identity"], "producer", "status", None, "source status")
    consumers = verification["consumers"]
    require(isinstance(consumers, list) and len(consumers) == 2, "exactly two consumer receipts are required")
    source_sequences: list[int] | None = None
    output_sequences: set[int] = set()
    normalized_consumers: list[dict[str, Any]] = []
    for idx, node in enumerate(("consumer-0", "consumer-1")):
        consumer = consumers[idx]
        require(isinstance(consumer, dict) and set(consumer) == {
            "consumer", "input_event_count", "input_sequence_sha256", "input_source_port", "input_target_port",
            "input_sensitivity", "input_remote_metadata", "input_event_identities", "output_event_count",
            "output_payload_sha256", "output_event_identities",
        }, f"{node}: consumer receipt shape differs")
        require(consumer["consumer"] == node, "consumer order/identity differs")
        require(exact_int(consumer["input_event_count"], "consumer input count") == RECORDS, f"{node}: input count differs")
        require(exact_int(consumer["output_event_count"], "consumer output count") == RECORDS, f"{node}: output count differs")
        source_port = {"node": "producer", "port": "stdout"}
        target_port = {"node": node, "port": "events"}
        require(consumer["input_source_port"] == source_port and consumer["input_target_port"] == target_port, f"{node}: fanout ports differ")
        require(consumer["input_sensitivity"] == ["unknown"] and consumer["input_remote_metadata"] is None, f"{node}: input metadata differs")

        inputs = consumer["input_event_identities"]
        require(isinstance(inputs, list) and len(inputs) == RECORDS, f"{node}: full source event identities absent")
        sequences: list[int] = []
        normalized_inputs: list[dict[str, Any]] = []
        previous = 0
        for event_idx, item in enumerate(inputs):
            require(isinstance(item, dict) and set(item) == {
                "sequence", "generation", "revision", "source_port_matches", "input_port_matches",
                "sensitivity_unknown_only", "remote_absent",
            }, f"{node}: input identity {event_idx} shape differs")
            sequence = exact_int(item["sequence"], "input sequence", minimum=1)
            require(sequence > previous, f"{node}: source stream sequence is not strictly increasing")
            previous = sequence
            require(
                exact_int(item["generation"], "input generation") == GENERATION
                and exact_int(item["revision"], "input revision") == REVISION
                and item["source_port_matches"] is True
                and item["input_port_matches"] is True
                and item["sensitivity_unknown_only"] is True
                and item["remote_absent"] is True,
                f"{node}: source identity metadata differs at row {event_idx}",
            )
            sequences.append(sequence)
            normalized_inputs.append({key: item[key] for key in item if key != "sequence"})
        seq_digest = sha_bytes(b"".join(value.to_bytes(8, "little") for value in sequences))
        require(consumer["input_sequence_sha256"] == seq_digest, f"{node}: source event sequence digest differs")
        if source_sequences is None:
            source_sequences = sequences
        else:
            require(sequences == source_sequences, "fanout consumers did not receive the same source events in order")

        outputs = consumer["output_event_identities"]
        require(isinstance(outputs, list) and len(outputs) == RECORDS, f"{node}: output event identities absent")
        normalized_outputs: list[dict[str, Any]] = []
        source_sequence_set = set(sequences)
        previous = 0
        for event_idx, item in enumerate(outputs):
            seq = verify_event_identity(item, node, "stdout", None, f"{node} output {event_idx}")
            require(seq > previous and seq > sequences[event_idx], f"{node}: output order or input/output sequence order differs")
            require(seq not in source_sequence_set and seq not in output_sequences, "an event sequence was reused across distinct emissions")
            previous = seq
            output_sequences.add(seq)
            normalized_outputs.append(normalize_identity(item))
        require(consumer["output_payload_sha256"] == fixture_hash, f"{node}: ordered full output payload checksum differs from fixture")
        normalized_consumers.append({
            "consumer": node,
            "input_event_count": RECORDS,
            "input_source_port": source_port,
            "input_target_port": target_port,
            "input_sensitivity": ["unknown"],
            "input_remote_metadata": None,
            "input_event_identities_without_sequences": normalized_inputs,
            "output_event_count": RECORDS,
            "output_payload_sha256": consumer["output_payload_sha256"],
            "output_event_identities_without_sequences": normalized_outputs,
        })
    require(source_sequences is not None and status_seq > source_sequences[-1], "source status does not follow all source records")
    require(status_seq not in set(source_sequences) and status_seq not in output_sequences, "source status reuses an emission sequence")
    return {
        "terminal_event_count": 2 * RECORDS + 1,
        "source_status_event_count": 1,
        "source_status_identity_without_sequence": normalize_identity(verification["source_status_identity"]),
        "consumers": normalized_consumers,
    }


def verify_journal(path: Path, samples: list[Mapping[str, Any]], fixture_hash: str) -> str:
    payload = path.read_bytes()
    require(len(payload) <= MAX_JOURNAL_BYTES, "sample journal exceeds frozen bound")
    require(payload.endswith(b"\n"), "sample journal lacks final newline")
    lines = payload.splitlines()
    require(len(lines) == INNER_SAMPLES + 1, "sample journal does not contain prepare plus three raw sample records")
    rows = [strict_json_bytes(line, f"journal row {idx}", 64 * 1024) for idx, line in enumerate(lines)]
    require(rows[0] == {
        "status": "prepared", "planned_warmups": 0, "planned_samples": INNER_SAMPLES,
        "record_count": RECORDS, "input_fixture_sha256": fixture_hash,
    }, "journal preparation row differs")
    for idx, (row, sample) in enumerate(zip(rows[1:], samples)):
        require(isinstance(sample, dict), "sample is not an object")
        evidence = sample["evidence"]
        expected = {
            "phase": "sample", "index": idx, "status": "ok", "duration_ns": sample["duration_ns"],
            "dropped_messages": 0, "launched_processes": 1, "input_fixture_sha256": fixture_hash,
            "verification_sha256": sha_bytes(canonical(evidence["verification"])),
        }
        require(row == expected, f"journal sample row {idx} differs from independent raw oracle")
    return sha_bytes(payload)


def verify_raw_invocation(raw_path: Path) -> dict[str, Any]:
    raw_bytes = raw_path.read_bytes()
    raw = strict_json_bytes(raw_bytes, f"raw report {raw_path}")
    require(isinstance(raw, dict) and set(raw) == {
        "format", "method", "configuration", "identity", "warmups", "raw_samples_in_execution_order"
    }, "raw harness report top-level schema differs")
    require(raw["format"] == REPORT_FORMAT and raw["method"] == METHOD, "raw harness method/format differs")
    cfg = expected_config()
    require(raw["configuration"] == cfg, "raw harness configuration/limits differ")
    fixture = expected_fixture()
    fixture_hash = sha_bytes(fixture)
    fixture_path = raw_path.with_name(raw_path.stem + ".input.jsonl").resolve()
    journal_path = raw_path.with_name(raw_path.stem + ".journal.jsonl").resolve()
    identity = raw["identity"]
    require(isinstance(identity, dict) and set(identity) == {
        "harness_sha256", "source_manifest_sha256", "graph_template_sha256", "input_fixture_path",
        "input_fixture_bytes", "input_fixture_sha256", "expected_payload_sha256_per_consumer", "sample_journal_path",
    }, "raw harness identity shape differs")
    require(identity == {
        "harness_sha256": PINS["harness"],
        "source_manifest_sha256": expected_manifest_hash(),
        "graph_template_sha256": expected_graph_hash(),
        "input_fixture_path": str(fixture_path),
        "input_fixture_bytes": len(fixture),
        "input_fixture_sha256": fixture_hash,
        "expected_payload_sha256_per_consumer": fixture_hash,
        "sample_journal_path": str(journal_path),
    }, "raw harness source/fixture identity differs")
    fixture_path = raw_path.with_name(raw_path.stem + ".input.jsonl")
    require(fixture_path.read_bytes() == fixture, "retained deterministic JSONL fixture differs byte-for-byte")
    require(raw["warmups"] == [], "inner harness ran warmups")
    samples = raw["raw_samples_in_execution_order"]
    require(isinstance(samples, list) and len(samples) == INNER_SAMPLES, "raw report does not contain exactly three samples")
    durations: list[int] = []
    normalized_hashes: list[str] = []
    for idx, sample in enumerate(samples):
        require(isinstance(sample, dict) and set(sample) == {"sample_index", "duration_ns", "evidence"}, f"sample {idx} shape differs")
        require(exact_int(sample["sample_index"], "sample index") == idx, "raw sample order/index differs")
        duration = exact_int(sample["duration_ns"], "sample duration", minimum=1)
        evidence = sample["evidence"]
        require(isinstance(evidence, dict) and set(evidence) == {"duration_ns", "dropped_messages", "launched_processes", "verification"}, f"sample {idx} evidence shape differs")
        require(exact_int(evidence["duration_ns"], "sample evidence duration", minimum=1) == duration, "sample duration fields disagree")
        require(exact_int(evidence["dropped_messages"], "dropped messages") == 0, "sample reports a dropped event")
        require(exact_int(evidence["launched_processes"], "launched process count") == 1, "sample did not launch exactly one /bin/cat producer")
        normalized = verify_sample(evidence["verification"], fixture_hash)
        normalized_hashes.append(sha_bytes(canonical(normalized)))
        durations.append(duration)
    require(len(set(normalized_hashes)) == 1, "full normalized event oracle differs within the invocation")
    journal_hash = verify_journal(journal_path, samples, fixture_hash)
    oracle = {
        "configuration": cfg,
        "source_manifest_sha256": expected_manifest_hash(),
        "graph_template_sha256": expected_graph_hash(),
        "harness_sha256": PINS["harness"],
        "fixture_bytes": len(fixture),
        "fixture_sha256": fixture_hash,
        "normalized_sample_oracle_sha256": normalized_hashes,
    }
    return {
        "raw_sha256": sha_bytes(raw_bytes),
        "fixture_sha256": fixture_hash,
        "fixture_bytes": len(fixture),
        "journal_sha256": journal_hash,
        "raw_duration_ns": durations,
        "median_duration_ns": int(statistics.median(durations)),
        "oracle": oracle,
    }


def stable_snapshot(snapshot: Any, identity: Mapping[str, Any], label: str) -> None:
    require(isinstance(snapshot, dict) and snapshot.get("stable") is True, f"{label} source/binary snapshot is unstable")
    require(snapshot.get("commit") == identity["commit"], f"{label} commit differs")
    require(snapshot.get("transport_sha256") == identity["transport_sha256"], f"{label} transport hash differs")
    require(snapshot.get("source_fingerprint") == identity["source_fingerprint"], f"{label} full source fingerprint differs")
    require(snapshot.get("harness_sha256") == PINS["harness"], f"{label} harness hash differs")
    require(snapshot.get("binary_sha256") == identity["binary_sha256"], f"{label} saved binary hash differs")
    require(snapshot.get("receipt_sha256") == identity["receipt_sha256"], f"{label} receipt hash differs")


def exact_test_output(stdout: bytes, stderr: bytes) -> bool:
    output = (stdout + b"\n" + stderr).decode("utf-8", errors="replace")
    return (
        "running 1 test" in output
        and output.count(f"test {TEST_NAME} ... ok") == 1
        and f"test {TEST_NAME} ... ok" in output
        and re.search(r"test result: ok\. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in [0-9.]+s", output) is not None
    )


def verify_attempt(
    report_root: Path,
    row: Mapping[str, Any],
    global_index: int,
    position: int,
    side: str,
    identity: Mapping[str, Any],
) -> dict[str, Any]:
    metadata = expected_attempt_metadata(global_index, position, side, identity)
    require({key: row.get(key) for key in metadata} == metadata, "attempt report row metadata/schedule differs")
    require(row.get("successful") is True and row.get("returncode") == 0 and row.get("timed_out") is False, "attempt did not finish successfully without timeout")
    require(row.get("execution_error") is None and row.get("validation_error") is None, "attempt contains execution or oracle error")
    exact_int(row.get("elapsed_wall_ns"), "attempt wall duration", minimum=1)
    require(row.get("raw_report_present") is True and row.get("input_fixture_present") is True and row.get("sample_journal_present") is True, "attempt omitted raw output/fixture/journal")

    attempt_dir = report_root / "attempts" / metadata["attempt_id"]
    require(Path(row.get("attempt_dir", "")).resolve() == attempt_dir.resolve(), "attempt directory receipt path differs")
    require(set(item.name for item in attempt_dir.iterdir()) == {
        "started.json", "finished.json", "raw.json", "raw.input.jsonl", "raw.journal.jsonl", "stdout.bin", "stderr.bin"
    }, f"attempt {metadata['attempt_id']} has missing/extra artifacts")
    started_path = attempt_dir / "started.json"
    finished_path = attempt_dir / "finished.json"
    started = strict_json(started_path, f"{metadata['attempt_id']} start receipt", 2_000_000)
    finished = strict_json(finished_path, f"{metadata['attempt_id']} finish receipt", 2_000_000)
    require(finished == dict(row), "paired report attempt summary differs from retained finished receipt")
    require({key: started.get(key) for key in metadata} == metadata, "start receipt metadata/schedule differs")
    binary = identity["saved_binary"]
    expected_command = [binary, "--exact", TEST_NAME, "--ignored", "--nocapture", "--test-threads=1"]
    require(started.get("command") == expected_command, "attempt ran a different binary/command")
    require(started.get("timeout_seconds") == ATTEMPT_TIMEOUT_SECONDS, "attempt timeout differs from preregistration")
    raw_path = attempt_dir / "raw.json"
    expected_environment = {
        "EXECUTE_GRAPH_RECV_ANY_E2E_REPORT": str(raw_path.resolve()),
        "EXECUTE_GRAPH_RECV_ANY_E2E_WARMUPS": "0",
        "EXECUTE_GRAPH_RECV_ANY_E2E_SAMPLES": "3",
        "LC_ALL": "C",
        "TZ": "UTC",
    }
    require(started.get("environment") == expected_environment, "attempt selected environment differs")
    require(started.get("raw_report_path") == str(raw_path.resolve()), "attempt report path is not fresh/attempt-local")
    stable_snapshot(started.get("identity_before"), identity, "pre-invocation")
    stable_snapshot(finished.get("identity_before"), identity, "finished pre-invocation")
    stable_snapshot(finished.get("identity_after"), identity, "post-invocation")

    stdout = (attempt_dir / "stdout.bin").read_bytes()
    stderr = (attempt_dir / "stderr.bin").read_bytes()
    require(exact_test_output(stdout, stderr), "attempt output does not show exactly the one expected passing ignored test")
    require(sha_bytes(stdout) == row.get("stdout_sha256") and sha_bytes(stderr) == row.get("stderr_sha256"), "stdout/stderr hashes differ from retained finished receipt")
    raw_summary = verify_raw_invocation(raw_path)
    require(raw_summary["raw_sha256"] == row.get("raw_report_sha256"), "raw report hash differs from finished receipt")
    require(raw_summary["fixture_sha256"] == row.get("input_fixture_sha256"), "fixture hash differs from finished receipt")
    require(raw_summary["journal_sha256"] == row.get("sample_journal_sha256"), "journal hash differs from finished receipt")
    require(raw_summary["raw_duration_ns"] == row.get("raw_duration_ns"), "raw duration samples differ from finished receipt")
    require(raw_summary["median_duration_ns"] == row.get("median_duration_ns"), "raw median differs from finished receipt")
    require(raw_summary["oracle"] == row.get("oracle"), "finished receipt oracle differs from independent oracle")
    return raw_summary


def expected_pairs(attempt_rows: list[Mapping[str, Any]]) -> tuple[list[dict[str, Any]], list[dict[str, Any]], dict[str, Any]]:
    pairs: list[dict[str, Any]] = []
    deltas: list[int] = []
    candidate_losses: list[int] = []
    candidate_improvements: list[int] = []
    ties: list[int] = []
    for idx in range(TOTAL_PAIRS):
        order = expected_order(idx)
        # The order is intentionally returned for independent attempt indexing.
        pair_rows = {
            row["side"]: row for row in attempt_rows
            if row.get("global_pair_index") == idx
        }
        require(set(pair_rows) == {"baseline", "candidate"}, f"pair {idx} lacks one arm")
        baseline = pair_rows["baseline"]
        candidate = pair_rows["candidate"]
        require(baseline["oracle"] == candidate["oracle"], f"pair {idx} cross-arm exact oracle differs")
        base_median = baseline["median_duration_ns"]
        candidate_median = candidate["median_duration_ns"]
        delta = candidate_median - base_median
        phase = "warmup" if idx < WARMUP_PAIRS else "measurement"
        phase_index = idx + 1 if phase == "warmup" else idx - WARMUP_PAIRS + 1
        pair = {
            "global_pair_index": idx,
            "phase": phase,
            "phase_pair_index": phase_index,
            "order": list(order),
            "baseline_attempt_id": baseline["attempt_id"],
            "candidate_attempt_id": candidate["attempt_id"],
            "valid_for_timing_comparison": True,
            "full_oracle_equal": True,
            "cases": {
                "whole_graph_session": {
                    "baseline_raw_duration_ns": baseline["raw_duration_ns"],
                    "candidate_raw_duration_ns": candidate["raw_duration_ns"],
                    "baseline_median_duration_ns": base_median,
                    "candidate_median_duration_ns": candidate_median,
                    "candidate_minus_baseline_ns": delta,
                    "candidate_over_baseline": candidate_median / base_median,
                    "candidate_change_percent": (candidate_median / base_median - 1.0) * 100.0,
                },
            },
        }
        pairs.append(pair)
        if phase == "measurement":
            deltas.append(delta)
            (candidate_losses if delta > 0 else candidate_improvements if delta < 0 else ties).append(idx)
    measured_deltas = [
        {"global_pair_index": idx + WARMUP_PAIRS, "candidate_minus_baseline_ns": pairs[idx + WARMUP_PAIRS]["cases"]["whole_graph_session"]["candidate_minus_baseline_ns"]}
        for idx in range(MEASURED_PAIRS)
    ]
    summary = {
        "metric": "whole_graph_session median duration per invocation; candidate minus baseline",
        "valid_measured_pair_count": MEASURED_PAIRS,
        "paired_deltas_in_order": measured_deltas,
        "median_paired_delta_ns": statistics.median(deltas),
        "candidate_loss_pair_indices": candidate_losses,
        "candidate_improvement_pair_indices": candidate_improvements,
        "tie_pair_indices": ties,
    }
    return pairs[:WARMUP_PAIRS], pairs[WARMUP_PAIRS:], summary


def audit_complete_report(path: Path, prereg: Mapping[str, Any]) -> dict[str, Any]:
    # One small atomic summary read comes first. If it still says running, do not
    # touch the live attempts tree at all.
    report = strict_json(path, "paired report", 32 * 1024 * 1024)
    require(isinstance(report, dict), "paired report root is not an object")
    require(report.get("status") == "complete", "paired trial is not complete; attempt artifacts were not opened")
    require(set(report) == {
        "schema_version", "format", "created_at", "status", "preregistration_path", "preregistration_sha256",
        "protocol", "host", "disk_preflight", "initial_identity", "attempts", "warmup_pairs", "measured_pairs",
        "losses", "unattempted_slots", "post_run_identity", "paired_summary", "completed_at", "planned_pair_count",
        "planned_attempt_count", "attempt_count", "successful_attempt_count", "failed_attempt_count", "unattempted_attempt_count",
    }, "paired report top-level fields differ")
    require(report.get("format") == PAIRED_FORMAT and report.get("schema_version") == 1, "paired report format/schema differs")
    require(report.get("preregistration_path") == str(PREREG.resolve()), "paired report names another preregistration")
    require(report.get("preregistration_sha256") == PINS["preregistration"], "paired report preregistration hash differs")
    require(report.get("protocol") == prereg["protocol"], "paired report protocol differs from preregistration")
    require(report.get("planned_pair_count") == TOTAL_PAIRS and report.get("planned_attempt_count") == TOTAL_ATTEMPTS, "planned pair/attempt counts differ")
    require(report.get("attempt_count") == TOTAL_ATTEMPTS and report.get("successful_attempt_count") == TOTAL_ATTEMPTS, "actual attempt/success counts differ")
    require(report.get("failed_attempt_count") == 0 and report.get("unattempted_attempt_count") == 0, "trial has failed or lost attempts")
    require(report.get("losses") == [] and report.get("unattempted_slots") == [], "trial retained losses/unattempted slots")
    identities = prereg["inputs"]
    for which in ("initial_identity", "post_run_identity"):
        snapshots = report.get(which)
        require(isinstance(snapshots, dict) and set(snapshots) == {"baseline", "candidate"}, f"{which} does not contain both arms")
        for side in ("baseline", "candidate"):
            stable_snapshot(snapshots[side], identities[side], which)

    rows = report.get("attempts")
    require(isinstance(rows, list) and len(rows) == TOTAL_ATTEMPTS, "report has not retained all 102 invocation summaries")
    expected_sequence: list[tuple[int, int, str]] = []
    for idx in range(TOTAL_PAIRS):
        expected_sequence.extend((idx, pos, side) for pos, side in enumerate(expected_order(idx), start=1))
    raw_summaries: dict[str, dict[str, Any]] = {}
    for row, (idx, pos, side) in zip(rows, expected_sequence):
        summary = verify_attempt(path.parent, row, idx, pos, side, identities[side])
        raw_summaries[row["attempt_id"]] = summary

    warmups, measurements, summary = expected_pairs(rows)
    require(report.get("warmup_pairs") == warmups, "retained warmup pairing/order/oracles differ from independent recomputation")
    require(report.get("measured_pairs") == measurements, "retained measurement pairing/order/oracles differ from independent recomputation")
    require(report.get("paired_summary") == summary, "paired summary/loss classification differs from independent recomputation")
    require(report.get("attempt_count") == len(rows), "attempt count field disagrees with raw attempt rows")

    fixture = expected_fixture()
    fixture_hash = sha_bytes(fixture)
    return {
        "status": "independent_complete_audit_passed",
        "report_path": str(path.resolve()),
        "report_sha256": sha_file(path),
        "preregistration_sha256": PINS["preregistration"],
        "attempts": TOTAL_ATTEMPTS,
        "warmup_pairs": WARMUP_PAIRS,
        "measured_pairs": MEASURED_PAIRS,
        "raw_samples_per_attempt": INNER_SAMPLES,
        "graph_sessions_audited": TOTAL_ATTEMPTS * INNER_SAMPLES,
        "terminal_events_per_graph_session": RECORDS * 2 + 1,
        "expected_fixture_bytes": len(fixture),
        "expected_fixture_sha256": fixture_hash,
        "dropped_messages": 0,
        "producer_launches_per_graph_session": 1,
        "candidate_loss_count": len(summary["candidate_loss_pair_indices"]),
        "candidate_improvement_count": len(summary["candidate_improvement_pair_indices"]),
        "tie_count": len(summary["tie_pair_indices"]),
        "median_paired_delta_ns": summary["median_paired_delta_ns"],
        "attempt_artifacts_read": True,
        "limitations": [
            "Timing is whole finite GraphSession execution, including start and terminal collection, not an isolated recv_any microbenchmark.",
            "Native producer completion is asserted by the frozen harness as process status Integer(0); the per-sample raw report records one launch and one status identity rather than repeating its payload.",
            "This is one host/run; it does not establish Bash-versus-Rust superiority or broad workload performance.",
        ],
    }


def main(argv: Iterable[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source-only", action="store_true", help="verify frozen source/build provenance without opening any trial attempts")
    parser.add_argument("--report", type=Path, default=ATTEMPT_ROOT / "paired-report.json", help="completed paired report")
    parser.add_argument("--output", type=Path, help="write the audit receipt to a new path instead of stdout")
    args = parser.parse_args(argv)
    try:
        prereg = strict_json(PREREG, "paired preregistration", 8 * 1024 * 1024)
        source = verify_source_build_inputs(prereg)
        if args.source_only:
            result = source
        else:
            result = audit_complete_report(args.report.expanduser().resolve(strict=True), prereg)
            result["source_provenance"] = source
        encoded = json.dumps(result, sort_keys=True, indent=2, ensure_ascii=False, allow_nan=False) + "\n"
        if args.output:
            output = args.output.expanduser().absolute()
            output.parent.mkdir(parents=True, exist_ok=True)
            with output.open("x", encoding="utf-8") as stream:
                stream.write(encoded)
                stream.flush()
                os.fsync(stream.fileno())
        else:
            sys.stdout.write(encoded)
        return 0
    except (AuditError, OSError, KeyError, TypeError, ValueError, json.JSONDecodeError) as error:
        print(f"independent audit failed: {type(error).__name__}: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
