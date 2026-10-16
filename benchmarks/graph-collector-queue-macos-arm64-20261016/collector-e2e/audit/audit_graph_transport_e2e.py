#!/usr/bin/env python3
"""Independent auditor for the frozen graph-transport paired E2E run.

This program refuses to inspect per-attempt raw reports unless the outer report
is complete. It does not run Cargo, invoke benchmark binaries, or change inputs.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import re
import statistics
import subprocess
import sys
from datetime import datetime, timezone
from typing import Any

CAMPAIGN = Path(__file__).resolve().parents[2]
DEFAULT_REPORT = CAMPAIGN / "evidence/graph-transport-e2e/paired-run-attempt-1/paired-report.json"
DEFAULT_PREREG = CAMPAIGN / "evidence/graph-transport-e2e/paired-preregistration.json"
DEFAULT_PATCH = CAMPAIGN / "work/graph-collector-sleep-candidate-v2.patch"
EXPECTED_PREREG_SHA256 = "c7341933548a6ac0fbd2396519597a04b326220580b9e9dc5d7aa78b86279b52"
EXPECTED_DRIVER_SHA256 = "8d39ec34c761f5a186ea45d8978c9a8a66e0cadf37149d3d4bb8dbcb9cfbd1b8"
EXPECTED_PATCH_SHA256 = "7d411b9b0fc3a8a37fbd589381720016916b6edabec43be06414e198cd39bca4"
EXPECTED_HARNESS_SHA256 = "e3a8c29881fe904b6b68b9e4a97485704a919dcac03aff0be2cc1998456b0d1e"
EXPECTED_COMMIT = "81bf9ba766a775cccf059007b19f5def9b9bfb03"
RECORD_COUNT = 4096
TEXT_BYTES = 256
EDGE_CAPACITY = 8
TERMINAL_CAPACITY = 16
GENERATION = 17
REVISION = 29
WARMUP_PAIRS = 3
MEASURED_PAIRS = 48
TOTAL_PAIRS = WARMUP_PAIRS + MEASURED_PAIRS
TOTAL_ATTEMPTS = TOTAL_PAIRS * 2
INNER_SAMPLES = 3
TEST_NAME = "bounded_native_stream_fanout_has_exact_end_to_end_samples"
METHOD = (
    "wall-clock GraphSession start through run_to_completion; real finite cat NativeStream, typed JSON decoding, "
    "two bounded lossless fanout edges, two custom pass-through receivers and terminal collection; all exact "
    "payload/order/hash validation follows the timed region"
)
SHA_RE = re.compile(r"^[0-9a-f]{64}$")
EVENT_FIELDS = {"port", "input", "generation", "revision", "sequence", "sensitivity", "remote"}
RECEIVED_FIELDS = {
    "sequence", "generation", "revision", "source_port_matches", "input_port_matches",
    "sensitivity_unknown_only", "remote_absent",
}


class AuditError(RuntimeError):
    pass


def require(ok: bool, message: str) -> None:
    if not ok:
        raise AuditError(message)


def sha_bytes(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def sha_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def parse_constant(value: str) -> None:
    raise AuditError(f"non-finite JSON constant is forbidden: {value}")


def unique_object(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        require(key not in result, f"duplicate JSON key: {key}")
        result[key] = value
    return result


def load_json(path: Path, label: str, max_bytes: int = 64 * 1024 * 1024) -> Any:
    raw = path.read_bytes()
    require(len(raw) <= max_bytes, f"{label} exceeds {max_bytes} bytes")
    try:
        return json.loads(raw, object_pairs_hook=unique_object, parse_constant=parse_constant)
    except (json.JSONDecodeError, UnicodeDecodeError) as error:
        raise AuditError(f"{label} is not strict UTF-8 JSON: {error}") from error


def canonical_json(value: Any) -> bytes:
    return json.dumps(
        value, sort_keys=True, separators=(",", ":"), ensure_ascii=False, allow_nan=False
    ).encode("utf-8")


def sha_u64_sequence(values: list[int]) -> str:
    digest = hashlib.sha256()
    for value in values:
        require(type(value) is int and 0 < value <= (1 << 64) - 1, "sequence is not a positive u64")
        digest.update(value.to_bytes(8, "little"))
    return digest.hexdigest()


def expected_fixture() -> bytes:
    return b"".join(
        canonical_json({"id": record_id, "text": "x" * TEXT_BYTES}) + b"\n"
        for record_id in range(1, RECORD_COUNT + 1)
    )


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
    return sha_bytes(canonical_json(manifest))


def expected_graph_hash() -> str:
    template = (
        "graph graph_transport_e2e\n"
        "stream from bench.jsonl_source with path __FIXTURE__ as producer\n"
        "show each line\n"
        "end graph\n"
    )
    return sha_bytes(template.encode("utf-8"))


def expected_collected_payload_bytes() -> int:
    payload_bytes = sum(
        len(canonical_json({"Record": {
            "id": {"Integer": record_id},
            "text": {"String": "x" * TEXT_BYTES},
        }}))
        for record_id in range(1, RECORD_COUNT + 1)
    )
    return payload_bytes * 2 + len(canonical_json({"Integer": 0}))


def exact_int(value: Any, label: str, minimum: int = 0) -> int:
    require(type(value) is int and value >= minimum, f"{label} must be an integer >= {minimum}")
    return value


def normalize_identity(value: dict[str, Any]) -> dict[str, Any]:
    return {key: item for key, item in value.items() if key != "sequence"}


def validate_event_identity(
    item: Any, port: dict[str, str], expected_input: dict[str, str] | None, label: str
) -> int:
    require(isinstance(item, dict) and set(item) == EVENT_FIELDS, f"{label} schema changed")
    require(item["port"] == port, f"{label} port changed")
    require(item["input"] == expected_input, f"{label} input port changed")
    require(exact_int(item["generation"], f"{label} generation") == GENERATION, f"{label} generation changed")
    require(exact_int(item["revision"], f"{label} revision") == REVISION, f"{label} revision changed")
    require(item["sensitivity"] == ["unknown"] and item["remote"] is None, f"{label} sensitivity/remote changed")
    return exact_int(item["sequence"], f"{label} sequence", 1)


def validate_verification(value: Any, fixture_hash: str) -> dict[str, Any]:
    require(
        isinstance(value, dict)
        and set(value) == {"terminal_event_count", "source_status_event_count", "source_status_identity", "consumers"},
        "verification schema changed",
    )
    require(exact_int(value["terminal_event_count"], "terminal event count") == RECORD_COUNT * 2 + 1,
            "terminal event count is not 8193")
    require(exact_int(value["source_status_event_count"], "source status count") == 1,
            "producer status count is not one")
    status_sequence = validate_event_identity(
        value["source_status_identity"], {"node": "producer", "port": "status"}, None, "producer status"
    )
    consumers = value["consumers"]
    require(isinstance(consumers, list) and len(consumers) == 2, "expected exactly two consumers")
    shared_inputs: list[int] | None = None
    normalized_consumers: list[dict[str, Any]] = []
    emission_sequences: set[int] = set()
    for index, expected_consumer in enumerate(("consumer-0", "consumer-1")):
        consumer = consumers[index]
        expected_fields = {
            "consumer", "input_event_count", "input_sequence_sha256", "input_source_port",
            "input_target_port", "input_sensitivity", "input_remote_metadata", "input_event_identities",
            "output_event_count", "output_payload_sha256", "output_event_identities",
        }
        require(isinstance(consumer, dict) and set(consumer) == expected_fields,
                f"{expected_consumer} schema changed")
        require(consumer["consumer"] == expected_consumer, "consumer ordering changed")
        require(exact_int(consumer["input_event_count"], "input event count") == RECORD_COUNT,
                f"{expected_consumer} input count changed")
        require(exact_int(consumer["output_event_count"], "output event count") == RECORD_COUNT,
                f"{expected_consumer} output count changed")
        expected_source = {"node": "producer", "port": "stdout"}
        expected_target = {"node": expected_consumer, "port": "events"}
        require(consumer["input_source_port"] == expected_source, "input source port changed")
        require(consumer["input_target_port"] == expected_target, "input target port changed")
        require(consumer["input_sensitivity"] == ["unknown"] and consumer["input_remote_metadata"] is None,
                "input metadata changed")
        inputs = consumer["input_event_identities"]
        require(isinstance(inputs, list) and len(inputs) == RECORD_COUNT,
                f"{expected_consumer} input identity list is partial")
        input_sequences: list[int] = []
        normalized_inputs: list[dict[str, Any]] = []
        previous = 0
        for i, item in enumerate(inputs):
            require(isinstance(item, dict) and set(item) == RECEIVED_FIELDS,
                    f"{expected_consumer} input identity {i} schema changed")
            sequence = exact_int(item["sequence"], "input sequence", 1)
            require(sequence > previous, f"{expected_consumer} input sequence order changed")
            previous = sequence
            require(exact_int(item["generation"], "input generation") == GENERATION, "input generation changed")
            require(exact_int(item["revision"], "input revision") == REVISION, "input revision changed")
            require(
                item["source_port_matches"] is True
                and item["input_port_matches"] is True
                and item["sensitivity_unknown_only"] is True
                and item["remote_absent"] is True,
                f"{expected_consumer} input port/sensitivity/remote identity changed",
            )
            input_sequences.append(sequence)
            normalized_inputs.append({key: field for key, field in item.items() if key != "sequence"})
        require(consumer["input_sequence_sha256"] == sha_u64_sequence(input_sequences),
                f"{expected_consumer} input sequence checksum mismatch")
        if shared_inputs is None:
            shared_inputs = input_sequences
        else:
            require(input_sequences == shared_inputs, "fanout consumers received different source event sequences")

        outputs = consumer["output_event_identities"]
        require(isinstance(outputs, list) and len(outputs) == RECORD_COUNT,
                f"{expected_consumer} output identity list is partial")
        previous = 0
        normalized_outputs: list[dict[str, Any]] = []
        for i, item in enumerate(outputs):
            sequence = validate_event_identity(
                item, {"node": expected_consumer, "port": "stdout"}, None, f"{expected_consumer} output {i}"
            )
            require(sequence > previous and sequence > input_sequences[i],
                    f"{expected_consumer} output order or causal order changed")
            require(sequence not in emission_sequences, "output sequence reused across distinct emissions")
            emission_sequences.add(sequence)
            previous = sequence
            normalized_outputs.append(normalize_identity(item))
        require(consumer["output_payload_sha256"] == fixture_hash,
                f"{expected_consumer} payload differs from exact fixture bytes")
        normalized_consumers.append({
            "consumer": expected_consumer,
            "input_event_count": RECORD_COUNT,
            "input_source_port": expected_source,
            "input_target_port": expected_target,
            "input_sensitivity": ["unknown"],
            "input_remote_metadata": None,
            "input_event_identities_without_sequences": normalized_inputs,
            "output_event_count": RECORD_COUNT,
            "output_payload_sha256": fixture_hash,
            "output_event_identities_without_sequences": normalized_outputs,
        })
    require(shared_inputs is not None and status_sequence > shared_inputs[-1],
            "producer status does not follow the source stream")
    require(status_sequence not in emission_sequences, "producer status sequence duplicates an output")
    require(len(set(shared_inputs)) == RECORD_COUNT, "source event sequences are not unique")
    require(not (set(shared_inputs) & emission_sequences), "sequence reused across input/output emissions")
    require(status_sequence not in set(shared_inputs), "producer status sequence duplicates a source event")
    return {
        "terminal_event_count": RECORD_COUNT * 2 + 1,
        "source_status_event_count": 1,
        "source_status_identity_without_sequence": normalize_identity(value["source_status_identity"]),
        "consumers": normalized_consumers,
    }


def validate_attempt_raw(raw_path: Path, attempt_dir: Path, arm: dict[str, Any]) -> dict[str, Any]:
    report = load_json(raw_path, "attempt raw report")
    require(isinstance(report, dict) and set(report) == {
        "format", "method", "configuration", "identity", "warmups", "raw_samples_in_execution_order"
    }, "raw harness report top-level schema changed")
    require(report["format"] == "execute.graph.transport_e2e.v1" and report["method"] == METHOD,
            "raw harness method/format differs from frozen protocol")
    fixture = expected_fixture()
    fixture_hash = sha_bytes(fixture)
    expected_config = {
        "inner_warmups": 0,
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
        "max_collected_bytes": expected_collected_payload_bytes(),
    }
    require(report["configuration"] == expected_config, "raw harness configuration changed")
    expected_identity = {
        "harness_sha256": EXPECTED_HARNESS_SHA256,
        "source_manifest_sha256": expected_manifest_hash(),
        "graph_template_sha256": expected_graph_hash(),
        "input_fixture_path": str(raw_path.with_name(raw_path.stem + ".input.jsonl").resolve()),
        "input_fixture_bytes": len(fixture),
        "input_fixture_sha256": fixture_hash,
        "expected_payload_sha256_per_consumer": fixture_hash,
        "sample_journal_path": str(raw_path.with_name(raw_path.stem + ".journal.jsonl").resolve()),
    }
    require(report["identity"] == expected_identity, "raw harness fixture/source identity changed")
    fixture_path = Path(expected_identity["input_fixture_path"])
    require(fixture_path.read_bytes() == fixture, "retained fixture differs byte-for-byte from deterministic fixture")
    require(report["warmups"] == [], "raw harness unexpectedly performed inner warmups")
    samples = report["raw_samples_in_execution_order"]
    require(isinstance(samples, list) and len(samples) == INNER_SAMPLES, "expected three ordered raw samples")
    durations: list[int] = []
    normalized_hashes: list[str] = []
    for index, sample in enumerate(samples):
        require(isinstance(sample, dict) and set(sample) == {"sample_index", "duration_ns", "evidence"},
                f"raw sample {index} schema changed")
        require(exact_int(sample["sample_index"], "sample index") == index, "raw sample order changed")
        duration = exact_int(sample["duration_ns"], "duration", 1)
        evidence = sample["evidence"]
        require(isinstance(evidence, dict) and set(evidence) == {
            "duration_ns", "dropped_messages", "launched_processes", "verification"
        }, f"raw sample {index} evidence schema changed")
        require(exact_int(evidence["duration_ns"], "evidence duration", 1) == duration,
                "sample duration duplicates disagree")
        require(exact_int(evidence["dropped_messages"], "dropped messages") == 0,
                "sample dropped graph messages")
        require(exact_int(evidence["launched_processes"], "launched processes") == 1,
                "sample did not launch exactly one native producer")
        normalized = validate_verification(evidence["verification"], fixture_hash)
        normalized_hashes.append(sha_bytes(canonical_json(normalized)))
        durations.append(duration)
    require(len(set(normalized_hashes)) == 1, "within-attempt full normalized oracle differs across samples")

    journal_path = raw_path.with_name(raw_path.stem + ".journal.jsonl")
    journal = journal_path.read_bytes()
    require(len(journal) <= 1024 * 1024 and journal.endswith(b"\n"), "sample journal framing/size invalid")
    rows = [load_json_line(line, f"journal row {i}") for i, line in enumerate(journal.splitlines())]
    require(len(rows) == INNER_SAMPLES + 1, "journal must contain preparation row plus three samples")
    require(rows[0] == {
        "status": "prepared", "planned_warmups": 0, "planned_samples": INNER_SAMPLES,
        "record_count": RECORD_COUNT, "input_fixture_sha256": fixture_hash,
    }, "journal preparation row differs")
    for i, (row, sample) in enumerate(zip(rows[1:], samples)):
        evidence = sample["evidence"]
        expected = {
            "phase": "sample",
            "index": i,
            "status": "ok",
            "duration_ns": sample["duration_ns"],
            "dropped_messages": 0,
            "launched_processes": 1,
            "input_fixture_sha256": fixture_hash,
            "verification_sha256": sha_bytes(canonical_json(evidence["verification"])),
        }
        require(row == expected, f"journal row {i + 1} disagrees with raw sample")
    return {
        "raw_sha256": sha_file(raw_path),
        "fixture_sha256": fixture_hash,
        "fixture_bytes": len(fixture),
        "journal_sha256": sha_bytes(journal),
        "durations_ns": durations,
        "median_duration_ns": int(statistics.median(durations)),
        "normalized_oracle_hashes": normalized_hashes,
        "arm_oracle": {
            "configuration": expected_config,
            "source_manifest_sha256": expected_manifest_hash(),
            "graph_template_sha256": expected_graph_hash(),
            "harness_sha256": EXPECTED_HARNESS_SHA256,
            "fixture_bytes": len(fixture),
            "fixture_sha256": fixture_hash,
            "normalized_sample_oracle_sha256": normalized_hashes,
        },
    }


def load_json_line(line: bytes, label: str) -> Any:
    try:
        return json.loads(line, object_pairs_hook=unique_object, parse_constant=parse_constant)
    except (json.JSONDecodeError, UnicodeDecodeError) as error:
        raise AuditError(f"{label} is not strict JSON: {error}") from error


def source_fingerprint(root: Path, recorded: dict[str, Any]) -> dict[str, Any]:
    command = subprocess.run(
        ["git", "-C", str(root), "ls-files", "-z"],
        stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=False,
    )
    require(command.returncode == 0, f"git ls-files failed for {root}")
    included_names = {"Cargo.toml", "Cargo.lock", "build.rs", "rust-toolchain", "rust-toolchain.toml", "rustfmt.toml"}
    paths = []
    for raw in command.stdout.split(b"\0"):
        if not raw:
            continue
        rel = raw.decode("utf-8")
        if rel.startswith(("src/", "vendor/", ".cargo/")) or rel in included_names:
            paths.append(rel)
    files: dict[str, str] = {}
    digest = hashlib.sha256()
    for rel in sorted(paths):
        path = root / rel
        require(path.is_file(), f"tracked build input missing: {root}/{rel}")
        file_hash = sha_file(path)
        files[rel] = file_hash
        digest.update(rel.encode("utf-8") + b"\0" + file_hash.encode("ascii") + b"\n")
    fresh = {"files": files, "sha256": digest.hexdigest()}
    require(fresh == recorded, f"current source fingerprint differs from preregistration for {root}")
    return fresh


def git_text(root: Path, *args: str) -> str:
    result = subprocess.run(
        ["git", "-C", str(root), *args], stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=False,
    )
    require(result.returncode == 0, f"git command failed in {root}: {' '.join(args)}")
    return result.stdout.decode("utf-8").strip()


def verify_build_provenance(prereg: dict[str, Any], patch_path: Path) -> dict[str, Any]:
    inputs = prereg["inputs"]
    arms = {side: inputs[side] for side in ("baseline", "candidate")}
    require(prereg["driver"] == {
        "path": str((CAMPAIGN / "work/graph-transport-e2e-paired-runner/graph_transport_e2e_pair.py").resolve()),
        "sha256": EXPECTED_DRIVER_SHA256,
    }, "paired runner identity changed")
    require(sha_file(Path(prereg["driver"]["path"])) == EXPECTED_DRIVER_SHA256, "paired runner hash mismatch")
    require(sha_file(patch_path) == EXPECTED_PATCH_SHA256, "collector product patch SHA changed")
    require(sha_file(Path(inputs["harness"]["path"])) == EXPECTED_HARNESS_SHA256, "canonical harness hash mismatch")

    baselines: dict[str, Any] = {}
    for side, arm in arms.items():
        root = Path(arm["root"]).resolve(strict=True)
        require(git_text(root, "rev-parse", "HEAD") == EXPECTED_COMMIT, f"{side} commit changed")
        require(arm["commit"] == EXPECTED_COMMIT, f"{side} preregistered commit changed")
        require(sha_file(root / "tests/graph_transport_e2e_bench.rs") == EXPECTED_HARNESS_SHA256,
                f"{side} harness source changed")
        fingerprint = source_fingerprint(root, arm["source_fingerprint"])
        require(sha_file(root / "src/graph/transport.rs") == arm["transport_sha256"],
                f"{side} transport source differs from preregistration")
        diff_names = git_text(root, "diff", "--name-only", "HEAD").splitlines()
        staged_names = git_text(root, "diff", "--cached", "--name-only").splitlines()
        untracked = git_text(root, "ls-files", "--others", "--exclude-standard", "-z")
        untracked_names = [name for name in untracked.split("\0") if name]
        expected_diff = [] if side == "baseline" else ["src/graph/transport.rs"]
        require(sorted(diff_names) == expected_diff and not staged_names,
                f"{side} worktree changed beyond frozen source delta")
        require(untracked_names == ["tests/graph_transport_e2e_bench.rs"],
                f"{side} untracked files differ from frozen harness-only addition")
        if side == "candidate":
            diff = subprocess.run(
                ["git", "-C", str(root), "diff", "HEAD", "--binary", "--", "src/graph/transport.rs"],
                stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=False,
            )
            require(diff.returncode == 0 and sha_bytes(diff.stdout) == EXPECTED_PATCH_SHA256,
                    "candidate source diff no longer matches the frozen product patch")
        receipt_path = Path(arm["receipt_path"]).resolve(strict=True)
        require(sha_file(receipt_path) == arm["receipt_sha256"], f"{side} build receipt hash changed")
        receipt = load_json(receipt_path, f"{side} build receipt", 2_000_000)
        require(receipt["source_stable"] is True and receipt["source_sha256"] == arm["transport_sha256"],
                f"{side} build receipt does not identify frozen source")
        require(receipt["artifact"] == arm["artifact"], f"{side} prereg artifact differs from receipt")
        require(receipt["saved_binary"] == arm["saved_binary"] and receipt["sha256"] == arm["binary_sha256"],
                f"{side} receipt binary identity differs from preregistration")
        saved = Path(arm["saved_binary"]).resolve(strict=True)
        require(os.access(saved, os.X_OK) and sha_file(saved) == arm["binary_sha256"],
                f"{side} saved binary hash/executable check failed")
        artifact = arm["artifact"]
        require(artifact["fresh"] is False and artifact["reason"] == "compiler-artifact",
                f"{side} artifact was not newly compiled by the pinned Cargo build")
        require(artifact["target"]["name"] == "graph_transport_e2e_bench" and
                artifact["target"]["test"] is True and artifact["profile"]["test"] is True and
                artifact["profile"]["opt_level"] == "3" and artifact["profile"]["debug_assertions"] is False,
                f"{side} artifact is not the optimized ignored-test binary")
        require(Path(artifact["manifest_path"]).resolve() == root / "Cargo.toml" and
                Path(artifact["target"]["src_path"]).resolve() == root / "tests/graph_transport_e2e_bench.rs",
                f"{side} artifact paths do not correspond to the frozen source root")
        build_json = Path(prereg["inputs"]["build_provenance"]["logs"][side]["build_json"]["path"])
        require(sha_file(build_json) == prereg["inputs"]["build_provenance"]["logs"][side]["build_json"]["sha256"],
                f"{side} pinned cargo build log hash changed")
        matching = []
        build_finished_success = False
        for line in build_json.read_bytes().splitlines():
            if not line:
                continue
            row = load_json_line(line, f"{side} Cargo build JSONL")
            if row.get("reason") == "compiler-artifact" and row.get("target", {}).get("name") == "graph_transport_e2e_bench":
                matching.append(row)
            if row.get("reason") == "build-finished" and row.get("success") is True:
                build_finished_success = True
        require(matching == [artifact] and build_finished_success,
                f"{side} Cargo logs do not attest to the exact successful artifact")
        baselines[side] = {
            "commit": arm["commit"],
            "source_fingerprint_sha256": fingerprint["sha256"],
            "transport_sha256": arm["transport_sha256"],
            "receipt_sha256": arm["receipt_sha256"],
            "saved_binary_sha256": arm["binary_sha256"],
            "saved_binary_path": str(saved),
            "cargo_version": arm["build_metadata"]["cargo_version"],
            "rustc_vv_sha256": sha_bytes(arm["build_metadata"]["rustc_vv"].encode()),
            "target_triple": artifact["target"].get("target"),
            "artifact_path": artifact["executable"],
        }
    require(arms["baseline"]["source_fingerprint"]["files"].keys() ==
            arms["candidate"]["source_fingerprint"]["files"].keys(), "arm source input sets differ")
    changed = [
        name for name in arms["baseline"]["source_fingerprint"]["files"]
        if arms["baseline"]["source_fingerprint"]["files"][name] !=
        arms["candidate"]["source_fingerprint"]["files"][name]
    ]
    require(changed == ["src/graph/transport.rs"], f"unexpected source differences: {changed}")
    require(arms["baseline"]["binary_sha256"] != arms["candidate"]["binary_sha256"],
            "baseline and candidate saved binaries are not distinct")
    metadata = [arms[side]["build_metadata"] for side in ("baseline", "candidate")]
    require(metadata[0]["cargo_version"] == metadata[1]["cargo_version"] and
            metadata[0]["rustc_vv"] == metadata[1]["rustc_vv"],
            "baseline and candidate compiler/tool versions differ")
    return baselines


def check_snapshot(snapshot: dict[str, Any], arm: dict[str, Any]) -> None:
    require(snapshot.get("stable") is True, "attempt identity snapshot is not stable")
    require(snapshot.get("commit") == arm["commit"], "attempt source commit changed")
    require(snapshot.get("transport_sha256") == arm["transport_sha256"], "attempt transport source changed")
    require(snapshot.get("source_fingerprint") == arm["source_fingerprint"],
            "attempt source fingerprint differs from preregistration")
    require(snapshot.get("harness_sha256") == EXPECTED_HARNESS_SHA256, "attempt harness hash changed")
    require(snapshot.get("binary_sha256") == arm["binary_sha256"], "attempt binary hash changed")
    require(snapshot.get("receipt_sha256") == arm["receipt_sha256"], "attempt build receipt changed")


def expected_pair_order(index: int) -> list[str]:
    return ["baseline", "candidate"] if index % 2 == 0 else ["candidate", "baseline"]


def audit(report_path: Path, prereg_path: Path, patch_path: Path, output_path: Path) -> dict[str, Any]:
    # Gate before loading attempt reports. This prevents a live/partial run from
    # being mistaken for a complete sample set.
    report = load_json(report_path, "paired run report")
    require(report.get("status") == "complete", "paired report is not complete; raw audit deferred")
    prereg = load_json(prereg_path, "paired preregistration", 8 * 1024 * 1024)
    prereg_hash = sha_file(prereg_path)
    require(prereg_hash == EXPECTED_PREREG_SHA256, "preregistration hash differs from frozen protocol")
    require(report.get("preregistration_path") == str(prereg_path.resolve()), "report names another preregistration")
    require(report.get("preregistration_sha256") == prereg_hash, "report preregistration hash mismatch")
    require(report.get("protocol") == prereg.get("protocol"), "report protocol differs from frozen preregistration")
    require(report["protocol"]["warmup_pairs"] == WARMUP_PAIRS and
            report["protocol"]["measured_pairs"] == MEASURED_PAIRS and
            report["protocol"]["inner_harness_samples"] == INNER_SAMPLES,
            "preregistered fixed sample counts changed")
    require(report["protocol"]["pair_order"].startswith("alternating AB/BA"),
            "pair order description changed")
    provenance = verify_build_provenance(prereg, patch_path)
    report_dir = report_path.parent.resolve()

    initial_identity = report.get("initial_identity")
    post_identity = report.get("post_run_identity")
    require(isinstance(initial_identity, dict) and isinstance(post_identity, dict), "missing run identity snapshots")
    for side in ("baseline", "candidate"):
        arm = prereg["inputs"][side]
        check_snapshot(initial_identity.get(side, {}), arm)
        check_snapshot(post_identity.get(side, {}), arm)

    attempts = report.get("attempts")
    require(isinstance(attempts, list) and len(attempts) == TOTAL_ATTEMPTS, "attempt count is not 102")
    require(report.get("planned_pair_count") == TOTAL_PAIRS and report.get("planned_attempt_count") == TOTAL_ATTEMPTS,
            "planned run count changed")
    require(report.get("attempt_count") == TOTAL_ATTEMPTS and report.get("successful_attempt_count") == TOTAL_ATTEMPTS,
            "not all planned attempts succeeded")
    require(report.get("failed_attempt_count") == 0 and report.get("unattempted_attempt_count") == 0,
            "report has failed or unattempted slots")
    require(report.get("losses") == [] and report.get("unattempted_slots") == [],
            "report records retries, losses, or unattempted attempts")
    require(len(report.get("warmup_pairs", [])) == WARMUP_PAIRS and
            len(report.get("measured_pairs", [])) == MEASURED_PAIRS,
            "pair ledger does not contain exactly three warmups and 48 measured pairs")

    fixture = expected_fixture()
    fixture_hash = sha_bytes(fixture)
    validated: dict[str, dict[str, Any]] = {}
    arm_medians: dict[str, list[int]] = {"baseline": [], "candidate": []}
    expected_attempts: list[dict[str, Any]] = []
    pair_metrics: list[dict[str, Any]] = []
    raw_sha_by_attempt: dict[str, str] = {}
    attempt_index = 0
    all_pairs = report["warmup_pairs"] + report["measured_pairs"]
    for pair_index in range(TOTAL_PAIRS):
        pair = all_pairs[pair_index]
        order = expected_pair_order(pair_index)
        phase = "warmup" if pair_index < WARMUP_PAIRS else "measurement"
        phase_index = pair_index + 1 if phase == "warmup" else pair_index - WARMUP_PAIRS + 1
        require(pair.get("global_pair_index") == pair_index and pair.get("phase") == phase and
                pair.get("phase_pair_index") == phase_index and pair.get("order") == order,
                f"pair {pair_index} identity/order differs from fixed AB/BA protocol")
        results: dict[str, dict[str, Any]] = {}
        for position, side in enumerate(order, 1):
            arm = prereg["inputs"][side]
            attempt = attempts[attempt_index]
            attempt_index += 1
            expected_id = f"{phase}-{phase_index:02d}-{position}-{side}"
            require(attempt.get("attempt_id") == expected_id and attempt.get("phase") == phase and
                    attempt.get("phase_pair_index") == phase_index and attempt.get("global_pair_index") == pair_index and
                    attempt.get("position_in_pair") == position and attempt.get("side") == side,
                    f"attempt {expected_id} sequence/metadata changed")
            require(attempt.get("successful") is True and attempt.get("returncode") == 0 and
                    attempt.get("timed_out") is False and attempt.get("execution_error") is None and
                    attempt.get("validation_error") is None,
                    f"attempt {expected_id} did not fully succeed")
            attempt_dir = (report_dir / "attempts" / expected_id).resolve(strict=True)
            require(Path(attempt["attempt_dir"]).resolve() == attempt_dir, f"{expected_id} directory mismatch")
            started_path, finished_path = attempt_dir / "started.json", attempt_dir / "finished.json"
            started, finished = load_json(started_path, f"{expected_id} started"), load_json(finished_path, f"{expected_id} finished")
            for row in (started, finished):
                require(row.get("attempt_id") == expected_id and row.get("side") == side and
                        row.get("global_pair_index") == pair_index and row.get("position_in_pair") == position,
                        f"{expected_id} start/finish metadata mismatch")
            binary = arm["saved_binary"]
            require(started.get("command") == [
                binary, "--exact", TEST_NAME, "--ignored", "--nocapture", "--test-threads=1"
            ], f"{expected_id} executed a different binary/test")
            require(started.get("timeout_seconds") == 120, f"{expected_id} invocation timeout changed")
            expected_env = {
                "EXECUTE_GRAPH_TRANSPORT_E2E_REPORT": str((attempt_dir / "raw.json").resolve()),
                "EXECUTE_GRAPH_TRANSPORT_E2E_WARMUPS": "0",
                "EXECUTE_GRAPH_TRANSPORT_E2E_SAMPLES": "3",
                "LC_ALL": "C",
                "TZ": "UTC",
            }
            require(started.get("environment") == expected_env, f"{expected_id} environment differs from prereg")
            check_snapshot(started.get("identity_before", {}), arm)
            check_snapshot(finished.get("identity_before", {}), arm)
            check_snapshot(finished.get("identity_after", {}), arm)
            raw_path = attempt_dir / "raw.json"
            require(started.get("raw_report_path") == str(raw_path.resolve()), f"{expected_id} raw path mismatch")
            raw_hash = sha_file(raw_path)
            require(finished.get("raw_report_present") is True and finished.get("raw_report_sha256") == raw_hash,
                    f"{expected_id} raw report hash/presence mismatch")
            fixture_path = attempt_dir / "raw.input.jsonl"
            journal_path = attempt_dir / "raw.journal.jsonl"
            require(finished.get("input_fixture_present") is True and
                    finished.get("input_fixture_sha256") == sha_file(fixture_path) and
                    fixture_path.read_bytes() == fixture,
                    f"{expected_id} retained input fixture differs")
            require(finished.get("sample_journal_present") is True and
                    finished.get("sample_journal_sha256") == sha_file(journal_path),
                    f"{expected_id} sample journal hash/presence mismatch")
            stdout, stderr = (attempt_dir / "stdout.bin").read_bytes(), (attempt_dir / "stderr.bin").read_bytes()
            require(finished.get("stdout_sha256") == sha_bytes(stdout) and
                    finished.get("stderr_sha256") == sha_bytes(stderr), f"{expected_id} process output hashes mismatch")
            output = (stdout + b"\n" + stderr).decode("utf-8", errors="replace")
            require("running 1 test" in output and
                    f"test {TEST_NAME} ... ok" in output and
                    re.search(r"test result: ok\. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in [0-9.]+s", output),
                    f"{expected_id} did not report exactly one passing ignored test")

            raw_result = validate_attempt_raw(raw_path, attempt_dir, arm)
            raw_sha_by_attempt[expected_id] = raw_hash
            require(attempt.get("raw_duration_ns") == raw_result["durations_ns"] and
                    attempt.get("median_duration_ns") == raw_result["median_duration_ns"],
                    f"{expected_id} runner-stored duration summary disagrees with raw samples")
            require(attempt.get("oracle") == raw_result["arm_oracle"], f"{expected_id} stored oracle differs from raw")
            results[side] = raw_result
            if phase == "measurement":
                arm_medians[side].append(raw_result["median_duration_ns"])
            expected_attempts.append({
                "attempt_id": expected_id,
                "side": side,
                "phase": phase,
                "pair_index": pair_index,
                "durations_ns": raw_result["durations_ns"],
                "median_duration_ns": raw_result["median_duration_ns"],
                "raw_sha256": raw_hash,
                "normalized_oracle_hashes": raw_result["normalized_oracle_hashes"],
            })
        base, cand = results["baseline"], results["candidate"]
        require(base["arm_oracle"] == cand["arm_oracle"], f"full normalized oracles differ in pair {pair_index}")
        valid = pair.get("valid_for_timing_comparison") is True and pair.get("full_oracle_equal") is True
        require(valid, f"pair {pair_index} is not valid for timing")
        base_median, cand_median = base["median_duration_ns"], cand["median_duration_ns"]
        delta = cand_median - base_median
        ratio = cand_median / base_median
        case = pair.get("cases", {}).get("whole_graph_session")
        require(isinstance(case, dict), f"pair {pair_index} is missing wall-clock case")
        require(case.get("baseline_raw_duration_ns") == base["durations_ns"] and
                case.get("candidate_raw_duration_ns") == cand["durations_ns"] and
                case.get("baseline_median_duration_ns") == base_median and
                case.get("candidate_median_duration_ns") == cand_median and
                case.get("candidate_minus_baseline_ns") == delta and
                math.isclose(case.get("candidate_over_baseline", float("nan")), ratio, rel_tol=1e-12, abs_tol=1e-15),
                f"pair {pair_index} summary differs from independent raw recomputation")
        pair_metrics.append({
            "pair_index": pair_index,
            "phase": phase,
            "order": order,
            "baseline_median_ns": base_median,
            "candidate_median_ns": cand_median,
            "candidate_minus_baseline_ns": delta,
            "candidate_over_baseline": ratio,
            "full_oracle_equal": True,
        })

    measured = pair_metrics[WARMUP_PAIRS:]
    ratios = [pair["candidate_over_baseline"] for pair in measured]
    deltas = [pair["candidate_minus_baseline_ns"] for pair in measured]
    wins = [pair["pair_index"] for pair in measured if pair["candidate_over_baseline"] < 1.0]
    losses = [pair["pair_index"] for pair in measured if pair["candidate_over_baseline"] > 1.0]
    ties = [pair["pair_index"] for pair in measured if pair["candidate_over_baseline"] == 1.0]
    baseline_arm_median = statistics.median(arm_medians["baseline"])
    candidate_arm_median = statistics.median(arm_medians["candidate"])
    summary = report.get("paired_summary")
    require(isinstance(summary, dict) and summary.get("valid_measured_pair_count") == MEASURED_PAIRS,
            "paired summary does not contain 48 valid measured pairs")
    recorded_deltas = [
        {"global_pair_index": pair["pair_index"], "candidate_minus_baseline_ns": pair["candidate_minus_baseline_ns"]}
        for pair in measured
    ]
    require(summary.get("paired_deltas_in_order") == recorded_deltas, "paired delta ledger differs from raw recomputation")
    require(summary.get("median_paired_delta_ns") == statistics.median(deltas),
            "median paired delta differs from raw recomputation")
    require(summary.get("candidate_loss_pair_indices") == losses and
            summary.get("candidate_improvement_pair_indices") == wins and
            summary.get("tie_pair_indices") == ties,
            "reported wins/losses/ties differ from independent ratios")

    report_result = {
        "schema_version": 1,
        "status": "complete",
        "audited_at": datetime.now(timezone.utc).isoformat().replace("+00:00", "Z"),
        "audit_script_sha256": sha_file(Path(__file__)),
        "report_path": str(report_path.resolve()),
        "preregistration_path": str(prereg_path.resolve()),
        "preregistration_sha256": prereg_hash,
        "driver_sha256": EXPECTED_DRIVER_SHA256,
        "candidate_product_patch_sha256": EXPECTED_PATCH_SHA256,
        "harness_sha256": EXPECTED_HARNESS_SHA256,
        "method": METHOD,
        "timer_boundary_source_review": "Instant::now before GraphSession::start; elapsed captured immediately after run_to_completion; output and payload/order/metadata validation is afterward",
        "workload_oracle": {
            "records_per_consumer": RECORD_COUNT,
            "text_bytes_per_record": TEXT_BYTES,
            "terminal_events_per_sample": RECORD_COUNT * 2 + 1,
            "input_fixture_bytes": len(fixture),
            "input_fixture_sha256": sha_bytes(fixture),
            "all_samples": "exact payload hash, full input/output event identity arrays, fanout sequence equality, metadata, natural EOF/status, and no drops independently validated",
        },
        "build_provenance": provenance,
        "attempt_count": TOTAL_ATTEMPTS,
        "successful_attempt_count": TOTAL_ATTEMPTS,
        "failed_attempt_count": 0,
        "unattempted_attempt_count": 0,
        "warmup_pair_count": WARMUP_PAIRS,
        "measured_pair_count": MEASURED_PAIRS,
        "measured_candidate_wins": len(wins),
        "measured_candidate_losses": len(losses),
        "measured_ties": len(ties),
        "loss_pair_indices": losses,
        "win_pair_indices": wins,
        "tie_pair_indices": ties,
        "median_paired_candidate_over_baseline": statistics.median(ratios),
        "median_paired_candidate_minus_baseline_ns": statistics.median(deltas),
        "baseline_arm_median_of_48_invocation_medians_ns": baseline_arm_median,
        "candidate_arm_median_of_48_invocation_medians_ns": candidate_arm_median,
        "candidate_over_baseline_arm_median_ratio": candidate_arm_median / baseline_arm_median,
        "median_of_all_raw_baseline_sample_durations_ns": statistics.median([
            duration for item in expected_attempts if item["side"] == "baseline" and item["phase"] == "measurement"
            for duration in item["durations_ns"]
        ]),
        "median_of_all_raw_candidate_sample_durations_ns": statistics.median([
            duration for item in expected_attempts if item["side"] == "candidate" and item["phase"] == "measurement"
            for duration in item["durations_ns"]
        ]),
        "pairs": pair_metrics,
        "attempts": expected_attempts,
        "claim_limit": "one fixed 4,096-record NativeStream fanout graph on this host; not a whole-application or general runtime speedup claim",
    }
    require(not output_path.exists(), f"refusing to overwrite audit output: {output_path}")
    output_path.parent.mkdir(parents=True, exist_ok=True)
    output_path.write_text(json.dumps(report_result, sort_keys=True, indent=2, ensure_ascii=False, allow_nan=False) + "\n")
    return report_result


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--report", type=Path, default=DEFAULT_REPORT)
    parser.add_argument("--preregistration", type=Path, default=DEFAULT_PREREG)
    parser.add_argument("--product-patch", type=Path, default=DEFAULT_PATCH)
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    report_path = args.report.expanduser().resolve(strict=True)
    prereg_path = args.preregistration.expanduser().resolve(strict=True)
    patch_path = args.product_patch.expanduser().resolve(strict=True)
    output_path = args.output.expanduser().absolute() if args.output else report_path.parent / "independent-audit.json"
    result = audit(report_path, prereg_path, patch_path, output_path)
    print(json.dumps({
        "status": result["status"],
        "attempts": result["attempt_count"],
        "measured_pairs": result["measured_pair_count"],
        "candidate_wins": result["measured_candidate_wins"],
        "candidate_losses": result["measured_candidate_losses"],
        "median_paired_ratio": result["median_paired_candidate_over_baseline"],
        "output": str(output_path),
    }, sort_keys=True))
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (AuditError, OSError, KeyError, TypeError, ValueError, subprocess.SubprocessError) as error:
        print(f"independent graph E2E audit failed: {error}", file=sys.stderr)
        raise SystemExit(1)

