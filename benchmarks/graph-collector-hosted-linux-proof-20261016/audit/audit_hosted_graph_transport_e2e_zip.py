#!/usr/bin/env python3
"""Audit one fixed Linux graph-transport run directly from its Actions ZIP.

The raw JSON is large. This tool hashes every ZIP member without extraction,
then validates one raw report at a time in a temporary directory and deletes it
before reading the next one. It never launches a saved test binary or Cargo.
"""
from __future__ import annotations

import argparse
import hashlib
import importlib.util
import json
import lzma
import math
import os
from pathlib import Path, PurePosixPath
import re
import statistics
import subprocess
import sys
import tempfile
from datetime import datetime, timezone
from typing import Any
import zipfile


CAMPAIGN = Path(__file__).resolve().parents[2]
DEFAULT_BASELINE = CAMPAIGN / "work/graph-optimizations-integrated-81bf9ba"
DEFAULT_CANDIDATE = CAMPAIGN / "work/graph-collector-sleep-candidate"
PACKAGE = CAMPAIGN / "work/graph-transport-e2e-paired-runner"
PUBLICATION = CAMPAIGN / "work/graph-collector-hosted-replication-publication"
DEFAULT_HELPER = PUBLICATION / "work/build_graph_e2e_pair.py"
DEFAULT_PATCH = PUBLICATION / "work/graph-transport-e2e-paired-runner/graph-collector-candidate.patch.xz"
DEFAULT_HARNESS = PUBLICATION / "work/graph-transport-e2e-paired-runner/graph_transport_e2e_bench.rs"
DEFAULT_SAMPLE_AUDITOR = CAMPAIGN / "evidence/graph-transport-e2e/audit_graph_transport_e2e.py"

RUN_ID = "38878319193"
WORKFLOW_COMMIT = "7ce98bfbbba025f4c07f1bee991d32ff5d958e60"
BASE_COMMIT = "81bf9ba766a775cccf059007b19f5def9b9bfb03"
RUNNER_SHA256 = "8d39ec34c761f5a186ea45d8978c9a8a66e0cadf37149d3d4bb8dbcb9cfbd1b8"
RUNNER_TEST_SHA256 = "62f3047bf52fb72c1e49dd526bdad14d6779c7c706f3794be21599429c5f8585"
HELPER_SHA256 = "c2db1b86eaa046d0a9f1280faa92d44bbc380f122d98644c0c0eef7c69cf6b6b"
SAMPLE_AUDITOR_SHA256 = "bc5e3f4435a3da723242e8ad9b9b78b5f99031bcef2bf15a91012055130a11c3"
PATCH_SHA256 = "7d411b9b0fc3a8a37fbd589381720016916b6edabec43be06414e198cd39bca4"
PATCH_ARCHIVE_SHA256 = "ac1a15348ab47a7b102eb61be74042f265b8fcebf9b11278f84af1525aedcac0"
HARNESS_SHA256 = "e3a8c29881fe904b6b68b9e4a97485704a919dcac03aff0be2cc1998456b0d1e"
BASELINE_TRANSPORT_SHA256 = "049b2b13a6c82ba0c98f20b57a44e76c420d631815f1b673ba70d7902f24610c"
CANDIDATE_TRANSPORT_SHA256 = "bfc03a7028b2871a4823ed0a130e3b735ca8dc535cac9c12cd39d876174dad9c"
TEST_NAME = "bounded_native_stream_fanout_has_exact_end_to_end_samples"
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
INVOCATION_TIMEOUT = 120
MAX_RAW_BYTES = 64 * 1024 * 1024
MAX_JSON_BYTES = 8 * 1024 * 1024
MAX_SMALL_MEMBER_BYTES = 16 * 1024 * 1024
MAX_TOTAL_UNCOMPRESSED = 8 * 1024**3
BUILD_COMMAND = [
    "cargo", "test", "--locked", "--release",
    "--test", "graph_transport_e2e_bench",
    "--test", "graph_runtime",
    "--test", "graph_lifecycles",
    "--test", "graph_result_events",
    "--lib", "--no-run", "--message-format=json",
]
METHOD = (
    "wall-clock GraphSession start through run_to_completion; real finite cat NativeStream, typed JSON decoding, "
    "two bounded lossless fanout edges, two custom pass-through receivers and terminal collection; all exact "
    "payload/order/hash validation follows the timed region"
)
SHA_RE = re.compile(r"^[0-9a-f]{64}$")
COMMIT_RE = re.compile(r"^[0-9a-f]{40}$")
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


def load_json_bytes(raw: bytes, label: str, max_bytes: int = MAX_JSON_BYTES) -> Any:
    require(len(raw) <= max_bytes, f"{label} exceeds {max_bytes} bytes")
    try:
        return json.loads(raw, object_pairs_hook=unique_object, parse_constant=parse_constant)
    except (json.JSONDecodeError, UnicodeDecodeError) as error:
        raise AuditError(f"{label} is not strict UTF-8 JSON: {error}") from error


def canonical_json(value: Any) -> bytes:
    return json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=False, allow_nan=False).encode("utf-8")


def zip_sha256(path: Path) -> str:
    return sha_file(path)


class Archive:
    def __init__(self, path: Path):
        self.path = path
        self.zip = zipfile.ZipFile(path, "r")
        infos = self.zip.infolist()
        names = [item.filename for item in infos]
        require(len(names) == len(set(names)), "ZIP contains duplicate member names")
        self.infos = {item.filename: item for item in infos}
        self.names = set(names)
        total = sum(item.file_size for item in infos)
        require(total <= MAX_TOTAL_UNCOMPRESSED, f"ZIP expands beyond audit cap ({total} bytes)")
        for item in infos:
            path_parts = PurePosixPath(item.filename).parts
            require(not item.filename.startswith("/") and ".." not in path_parts,
                    f"unexpected non-relative archive member: {item.filename}")

    def find(self, suffix: str) -> str:
        matches = [name for name in self.names if name == suffix or name.endswith("/" + suffix)]
        require(len(matches) == 1, f"expected one ZIP member ending in {suffix!r}, found {len(matches)}")
        return matches[0]

    def read(self, name: str, limit: int = MAX_JSON_BYTES) -> bytes:
        info = self.infos[name]
        require(info.file_size <= limit, f"ZIP member {name} exceeds {limit} bytes")
        with self.zip.open(info, "r") as stream:
            result = bytearray()
            while True:
                block = stream.read(min(1024 * 1024, limit + 1 - len(result)))
                if not block:
                    break
                result.extend(block)
                require(len(result) <= limit, f"ZIP member {name} exceeds {limit} bytes")
            require(len(result) == info.file_size, f"short ZIP member read: {name}")
            return bytes(result)

    def hash_all(self, *, expected_fixture: bytes) -> tuple[list[dict[str, Any]], dict[str, str]]:
        ledger: list[dict[str, Any]] = []
        digests: dict[str, str] = {}
        total = 0
        for name in sorted(self.names):
            info = self.infos[name]
            if name.endswith("/raw.json") or name == "raw.json":
                require(info.file_size <= MAX_RAW_BYTES, f"raw report exceeds {MAX_RAW_BYTES} bytes: {name}")
            digest = hashlib.sha256()
            count = 0
            fixture_compare = name.endswith("/raw.input.jsonl") or name == "raw.input.jsonl"
            fixture_offset = 0
            with self.zip.open(info, "r") as stream:
                while True:
                    block = stream.read(1024 * 1024)
                    if not block:
                        break
                    count += len(block)
                    require(count <= info.file_size, f"ZIP member exceeds declared length: {name}")
                    digest.update(block)
                    if fixture_compare:
                        stop = fixture_offset + len(block)
                        require(stop <= len(expected_fixture) and block == expected_fixture[fixture_offset:stop],
                                f"archived deterministic fixture differs: {name}")
                        fixture_offset = stop
            require(count == info.file_size, f"ZIP member size mismatch: {name}")
            require((not fixture_compare) or fixture_offset == len(expected_fixture),
                    f"archived fixture length differs: {name}")
            value = digest.hexdigest()
            digests[name] = value
            total += count
            ledger.append({
                "name": name,
                "size": count,
                "compressed_size": info.compress_size,
                "crc32": f"{info.CRC:08x}",
                "sha256": value,
                "directory": info.is_dir(),
            })
        require(total <= MAX_TOTAL_UNCOMPRESSED, "ZIP uncompressed total changed during read")
        return ledger, digests


def expected_fixture() -> bytes:
    return b"".join(canonical_json({"id": i, "text": "x" * TEXT_BYTES}) + b"\n" for i in range(1, RECORD_COUNT + 1))


def expected_manifest_sha256() -> str:
    return sha_bytes(canonical_json({
        "name": "bench.jsonl_source",
        "binary": "/bin/cat",
        "accepts_stdin": False,
        "parameters": {"path": {"type": "path", "required": True}},
        "arguments": [{"parameter": "path"}],
        "contract": {"version": 1, "output": {
            "type": "record", "encoding": "json",
            "fields": {"id": {"type": "integer"}, "text": {"type": "text"}},
            "required": ["id", "text"],
        }},
    }))


def expected_graph_sha256() -> str:
    return sha_bytes(("graph graph_transport_e2e\n"
                      "stream from bench.jsonl_source with path __FIXTURE__ as producer\n"
                      "show each line\nend graph\n").encode())


def expected_payload_bytes() -> int:
    payload = sum(len(canonical_json({"Record": {
        "id": {"Integer": i}, "text": {"String": "x" * TEXT_BYTES}
    }})) for i in range(1, RECORD_COUNT + 1))
    return payload * 2 + len(canonical_json({"Integer": 0}))


def expected_protocol() -> dict[str, Any]:
    return {
        "test_name": TEST_NAME,
        "harness_format": "execute.graph.transport_e2e.v1",
        "harness_sha256": HARNESS_SHA256,
        "baseline_commit": BASE_COMMIT,
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
        "inner_harness_warmups": 0,
        "inner_harness_samples": INNER_SAMPLES,
        "invocation_timeout_seconds": INVOCATION_TIMEOUT,
        "minimum_free_bytes_before_run": 3 * 1024**3,
        "invocation_environment": {
            "EXECUTE_GRAPH_TRANSPORT_E2E_REPORT": "absolute attempt-local raw.json",
            "EXECUTE_GRAPH_TRANSPORT_E2E_WARMUPS": "0",
            "EXECUTE_GRAPH_TRANSPORT_E2E_SAMPLES": "3",
            "LC_ALL": "C",
            "TZ": "UTC",
        },
        "metric": "median of three raw GraphSession wall durations per invocation; paired candidate minus baseline; positive is slower",
        "cross_arm_oracle": "fixture bytes/hash, frozen configuration, source/graph/harness hashes, and all full exact payload/order/metadata identity oracles must match; only scheduler-assigned global sequence numbers and absolute per-attempt paths are normalized away after within-run sequence validation",
        "retry_policy": "no retries; run both sides of the current pair, then stop after any failed pair and record all remaining planned slots as not attempted",
    }


def exact_int(value: Any, label: str, minimum: int = 0) -> int:
    require(type(value) is int and value >= minimum, f"{label} must be an integer >= {minimum}")
    return value


def sha_u64_sequence(values: list[int]) -> str:
    digest = hashlib.sha256()
    for value in values:
        require(type(value) is int and 0 < value < (1 << 64), "sequence is not a positive u64")
        digest.update(value.to_bytes(8, "little"))
    return digest.hexdigest()


def normalize_identity(value: dict[str, Any]) -> dict[str, Any]:
    return {key: item for key, item in value.items() if key != "sequence"}


def validate_event_identity(item: Any, port: dict[str, str], input_port: dict[str, str] | None, label: str) -> int:
    require(isinstance(item, dict) and set(item) == EVENT_FIELDS, f"{label} identity schema changed")
    require(item["port"] == port and item["input"] == input_port, f"{label} port identity changed")
    require(exact_int(item["generation"], f"{label} generation") == GENERATION, f"{label} generation changed")
    require(exact_int(item["revision"], f"{label} revision") == REVISION, f"{label} revision changed")
    require(item["sensitivity"] == ["unknown"] and item["remote"] is None,
            f"{label} sensitivity/remote metadata changed")
    return exact_int(item["sequence"], f"{label} sequence", 1)


def validate_verification(value: Any, fixture_sha: str) -> dict[str, Any]:
    require(isinstance(value, dict) and set(value) == {
        "terminal_event_count", "source_status_event_count", "source_status_identity", "consumers"
    }, "verification schema changed")
    require(exact_int(value["terminal_event_count"], "terminal event count") == RECORD_COUNT * 2 + 1,
            "terminal event count is not 8193")
    require(exact_int(value["source_status_event_count"], "source status count") == 1,
            "producer status count is not one")
    status_seq = validate_event_identity(value["source_status_identity"],
        {"node": "producer", "port": "status"}, None, "producer status")
    consumers = value["consumers"]
    require(isinstance(consumers, list) and len(consumers) == 2, "expected two consumers")
    shared_inputs: list[int] | None = None
    normalized_consumers: list[dict[str, Any]] = []
    outputs_seen: set[int] = set()
    for index, consumer_id in enumerate(("consumer-0", "consumer-1")):
        consumer = consumers[index]
        keys = {
            "consumer", "input_event_count", "input_sequence_sha256", "input_source_port",
            "input_target_port", "input_sensitivity", "input_remote_metadata", "input_event_identities",
            "output_event_count", "output_payload_sha256", "output_event_identities",
        }
        require(isinstance(consumer, dict) and set(consumer) == keys, f"{consumer_id} schema changed")
        require(consumer["consumer"] == consumer_id, "consumer order changed")
        require(exact_int(consumer["input_event_count"], "input event count") == RECORD_COUNT,
                f"{consumer_id} input count changed")
        require(exact_int(consumer["output_event_count"], "output event count") == RECORD_COUNT,
                f"{consumer_id} output count changed")
        src = {"node": "producer", "port": "stdout"}
        dst = {"node": consumer_id, "port": "events"}
        require(consumer["input_source_port"] == src and consumer["input_target_port"] == dst,
                f"{consumer_id} edge endpoints changed")
        require(consumer["input_sensitivity"] == ["unknown"] and consumer["input_remote_metadata"] is None,
                f"{consumer_id} input metadata changed")
        inputs = consumer["input_event_identities"]
        require(isinstance(inputs, list) and len(inputs) == RECORD_COUNT,
                f"{consumer_id} has incomplete input identities")
        input_seq: list[int] = []
        normalized_inputs: list[dict[str, Any]] = []
        previous = 0
        for i, event in enumerate(inputs):
            require(isinstance(event, dict) and set(event) == RECEIVED_FIELDS,
                    f"{consumer_id} input {i} schema changed")
            seq = exact_int(event["sequence"], "input sequence", 1)
            require(seq > previous, f"{consumer_id} input order is not increasing")
            previous = seq
            require(exact_int(event["generation"], "input generation") == GENERATION and
                    exact_int(event["revision"], "input revision") == REVISION,
                    f"{consumer_id} input version changed")
            require(event["source_port_matches"] is True and event["input_port_matches"] is True and
                    event["sensitivity_unknown_only"] is True and event["remote_absent"] is True,
                    f"{consumer_id} input identity flags changed")
            input_seq.append(seq)
            normalized_inputs.append({k: v for k, v in event.items() if k != "sequence"})
        require(consumer["input_sequence_sha256"] == sha_u64_sequence(input_seq),
                f"{consumer_id} input sequence digest mismatch")
        if shared_inputs is None:
            shared_inputs = input_seq
        else:
            require(input_seq == shared_inputs, "fanout consumers got different source sequences")
        outputs = consumer["output_event_identities"]
        require(isinstance(outputs, list) and len(outputs) == RECORD_COUNT,
                f"{consumer_id} has incomplete output identities")
        normalized_outputs: list[dict[str, Any]] = []
        previous = 0
        for i, event in enumerate(outputs):
            seq = validate_event_identity(event, {"node": consumer_id, "port": "stdout"}, None,
                                          f"{consumer_id} output {i}")
            require(seq > previous and seq > input_seq[i], f"{consumer_id} output order/causality changed")
            require(seq not in outputs_seen, "distinct output emissions reused a sequence")
            previous = seq
            outputs_seen.add(seq)
            normalized_outputs.append(normalize_identity(event))
        require(consumer["output_payload_sha256"] == fixture_sha,
                f"{consumer_id} exact output payload digest differs from fixture")
        normalized_consumers.append({
            "consumer": consumer_id,
            "input_event_count": RECORD_COUNT,
            "input_source_port": src,
            "input_target_port": dst,
            "input_sensitivity": ["unknown"],
            "input_remote_metadata": None,
            "input_event_identities_without_sequences": normalized_inputs,
            "output_event_count": RECORD_COUNT,
            "output_payload_sha256": fixture_sha,
            "output_event_identities_without_sequences": normalized_outputs,
        })
    require(shared_inputs is not None, "missing shared source input stream")
    require(status_seq > shared_inputs[-1], "source status did not follow all source events")
    require(status_seq not in outputs_seen and not (set(shared_inputs) & outputs_seen),
            "event sequence reused across source, output, or status events")
    require(len(set(shared_inputs)) == RECORD_COUNT, "source event sequence is not unique")
    require(status_seq not in set(shared_inputs), "status reused a source event sequence")
    return {
        "terminal_event_count": RECORD_COUNT * 2 + 1,
        "source_status_event_count": 1,
        "source_status_identity_without_sequence": normalize_identity(value["source_status_identity"]),
        "consumers": normalized_consumers,
    }


def validate_raw_with_reference(raw_bytes: bytes, fixture: bytes, journal: bytes,
                                remote_raw_path: str, sample_auditor: Any) -> dict[str, Any]:
    """Run the frozen independent raw-oracle validator on one temporary sample."""
    original = load_json_bytes(raw_bytes, "raw harness report", MAX_RAW_BYTES)
    require(isinstance(original, dict) and isinstance(original.get("identity"), dict),
            "raw harness report/identity is malformed")
    remote = PurePosixPath(remote_raw_path)
    expected_fixture_path = str(remote.with_name("raw.input.jsonl"))
    expected_journal_path = str(remote.with_name("raw.journal.jsonl"))
    require(original["identity"].get("input_fixture_path") == expected_fixture_path,
            "harness report records a fixture path outside its attempt")
    require(original["identity"].get("sample_journal_path") == expected_journal_path,
            "harness report records a journal path outside its attempt")
    with tempfile.TemporaryDirectory(prefix="graph-e2e-zip-audit-") as temporary:
        root = Path(temporary)
        report_path = root / "raw.json"
        fixture_path = root / "raw.input.jsonl"
        journal_path = root / "raw.journal.jsonl"
        original["identity"]["input_fixture_path"] = str(fixture_path.resolve())
        original["identity"]["sample_journal_path"] = str(journal_path.resolve())
        report_path.write_bytes(json.dumps(original, ensure_ascii=False, allow_nan=False).encode("utf-8"))
        fixture_path.write_bytes(fixture)
        journal_path.write_bytes(journal)
        result = sample_auditor.validate_attempt_raw(report_path, root, {})
    # The reference validator saw only the temporary copy with local paths. Keep
    # the archived report digest from the ZIP member for every provenance claim.
    result["original_raw_sha256"] = sha_bytes(raw_bytes)
    result["remote_fixture_path"] = expected_fixture_path
    result["remote_journal_path"] = expected_journal_path
    return result


def strict_iso(value: Any, label: str) -> datetime:
    require(isinstance(value, str) and value, f"{label} timestamp is missing")
    try:
        parsed = datetime.fromisoformat(value.replace("Z", "+00:00"))
    except ValueError as error:
        raise AuditError(f"{label} timestamp is invalid: {value}") from error
    require(parsed.tzinfo is not None and parsed.utcoffset() is not None,
            f"{label} timestamp must include a timezone")
    return parsed


def validate_attempt_timestamps(started: dict[str, Any], finished: dict[str, Any], label: str) -> None:
    start = strict_iso(started.get("started_at"), f"{label} start")
    finish = strict_iso(finished.get("finished_at"), f"{label} finish")
    require(start <= finish, f"{label} finished before it started")


def git(root: Path, *args: str) -> str:
    proc = subprocess.run(["git", "-C", str(root), *args], stdin=subprocess.DEVNULL,
                          stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=False)
    require(proc.returncode == 0, f"git {' '.join(args)} failed for {root}: {proc.stderr.decode(errors='replace')}")
    return proc.stdout.decode("utf-8", errors="strict").strip()


def source_fingerprint(root: Path) -> dict[str, Any]:
    included = {"Cargo.toml", "Cargo.lock", "build.rs", "rust-toolchain", "rust-toolchain.toml", "rustfmt.toml"}
    raw = subprocess.run(["git", "-C", str(root), "ls-files", "-z"], stdin=subprocess.DEVNULL,
                         stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=False)
    require(raw.returncode == 0, f"git ls-files failed for {root}")
    paths = sorted(item.decode("utf-8") for item in raw.stdout.split(b"\0") if item and
                   (item.decode("utf-8").startswith(("src/", "vendor/", ".cargo/")) or item.decode("utf-8") in included))
    files: dict[str, str] = {}
    digest = hashlib.sha256()
    for relative in paths:
        path = root / relative
        require(path.is_file(), f"tracked build input is missing: {path}")
        value = sha_file(path)
        files[relative] = value
        digest.update(relative.encode() + b"\0" + value.encode("ascii") + b"\n")
    require(files, f"no tracked build inputs found under {root}")
    return {"files": files, "sha256": digest.hexdigest()}


def inspect_local_sources(baseline_root: Path, candidate_root: Path, harness: bytes,
                          prereg: dict[str, Any], patch_path: Path, helper_path: Path,
                          runner_path: Path, sample_auditor_path: Path) -> dict[str, Any]:
    require(sha_file(runner_path) == RUNNER_SHA256, "local paired runner is not the pinned source")
    require(sha_file(helper_path) == HELPER_SHA256, "local build helper is not the pinned source")
    require(sha_file(sample_auditor_path) == SAMPLE_AUDITOR_SHA256,
            "reference independent sample auditor changed")
    require(sha_file(DEFAULT_HARNESS) == HARNESS_SHA256 and harness == DEFAULT_HARNESS.read_bytes(),
            "local canonical harness differs from frozen source")
    require(sha_file(patch_path) == PATCH_ARCHIVE_SHA256, "compressed candidate patch differs from frozen archive")
    patch = lzma.decompress(patch_path.read_bytes())
    require(sha_bytes(patch) == PATCH_SHA256, "candidate patch content hash differs")
    roots = {"baseline": baseline_root.resolve(strict=True), "candidate": candidate_root.resolve(strict=True)}
    require(roots["baseline"] != roots["candidate"], "baseline and candidate share a source root")
    fingerprints: dict[str, Any] = {}
    summaries: dict[str, Any] = {}
    for side, root in roots.items():
        require(git(root, "rev-parse", "HEAD") == BASE_COMMIT, f"{side} checkout commit differs")
        changed = git(root, "diff", "--name-only", "HEAD").splitlines()
        staged = git(root, "diff", "--cached", "--name-only").splitlines()
        untracked = [name for name in git(root, "ls-files", "--others", "--exclude-standard", "-z").split("\0") if name]
        require(sorted(changed) == ([] if side == "baseline" else ["src/graph/transport.rs"]),
                f"{side} worktree has an unexpected tracked diff: {changed}")
        require(not staged and untracked == ["tests/graph_transport_e2e_bench.rs"],
                f"{side} worktree has unexpected staged/untracked files")
        require((root / "tests/graph_transport_e2e_bench.rs").read_bytes() == harness,
                f"{side} worktree harness differs")
        transport_hash = sha_file(root / "src/graph/transport.rs")
        require(transport_hash == (BASELINE_TRANSPORT_SHA256 if side == "baseline" else CANDIDATE_TRANSPORT_SHA256),
                f"{side} transport source SHA differs from frozen pin")
        fingerprints[side] = source_fingerprint(root)
        arm = prereg["inputs"][side]
        require(arm["root"] == str(root) or Path(arm["root"]).name == root.name,
                f"{side} preregistration root does not identify this source checkout")
        require(arm["transport_path"].endswith(f"/work/{root.name}/src/graph/transport.rs") and
                arm["harness_path"].endswith(f"/work/{root.name}/tests/graph_transport_e2e_bench.rs"),
                f"{side} preregistered source paths do not match the frozen checkout")
        require(arm["commit"] == BASE_COMMIT and arm["transport_sha256"] == transport_hash and
                arm["source_fingerprint"] == fingerprints[side] and
                arm["harness_sha256"] == HARNESS_SHA256,
                f"{side} preregistered source identity disagrees with local pinned files")
        summaries[side] = {"commit": BASE_COMMIT, "transport_sha256": transport_hash,
                           "source_fingerprint_sha256": fingerprints[side]["sha256"]}
    base_files = fingerprints["baseline"]["files"]
    cand_files = fingerprints["candidate"]["files"]
    require(base_files.keys() == cand_files.keys(), "tracked build-input file sets differ")
    changes = sorted(name for name in base_files if base_files[name] != cand_files[name])
    require(changes == ["src/graph/transport.rs"], f"unexpected tracked build-input differences: {changes}")
    diff = subprocess.run(["git", "-C", str(roots["candidate"]), "diff", "--binary", "HEAD", "--", "src/graph/transport.rs"],
                          stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=False)
    require(diff.returncode == 0 and sha_bytes(diff.stdout) == PATCH_SHA256,
            "candidate source diff does not match frozen collector patch")
    return {"arms": summaries, "changed_tracked_build_inputs": changes,
            "candidate_patch_sha256": PATCH_SHA256, "harness_sha256": HARNESS_SHA256}


def cargo_config_common_hash(fingerprint: dict[str, Any]) -> str:
    cargo_home = fingerprint["cargo_home"]
    cargo_path = PurePosixPath(cargo_home["path"])
    cargo_files = [{
        "relative_path": os.path.relpath(item["path"], str(cargo_path)),
        "present": item["present"], "sha256": item["sha256"], "symlink_target": item["symlink_target"],
    } for item in cargo_home["config_files"]]
    normalized: dict[str, list[dict[str, Any]]] = {}
    for side, root in sorted(fingerprint["source_roots"].items()):
        root_path = root["root"]
        normalized[side] = [{
            "relative_path": os.path.relpath(item["path"], root_path),
            "present": item["present"], "sha256": item["sha256"], "symlink_target": item["symlink_target"],
        } for item in root["config_files"]]
    values = list(normalized.values())
    require(len(values) == 2 and values[0] == values[1], "Cargo ancestor configs differ between arms")
    body = {"schema": fingerprint["schema"], "cargo_home_configs": cargo_files,
            "source_ancestor_configs": values[0]}
    return sha_bytes(canonical_json(body))


def validate_build_pair_result(summary: dict[str, Any], arm_results: dict[str, Any]) -> None:
    """Bind the pair receipt to the exact two per-arm results and one attempt."""
    require(set(summary) == {"schema", "completed_at", "attempt", "sides", "failed_sides", "no_retries"} and
            summary.get("schema") == "execute.graph.transport-e2e.hosted-build-pair/1" and
            summary.get("attempt") == 1 and summary.get("no_retries") is True and
            summary.get("failed_sides") == [] and
            set(summary.get("sides", {})) == {"baseline", "candidate"},
            "hosted build pair did not finish both source arms in one attempt without retries")
    strict_iso(summary.get("completed_at"), "build pair completion")
    require(summary["sides"] == arm_results,
            "build pair result does not match the per-arm build-result receipts")


def arm_build_member(side: str, filename: str) -> str:
    require(side in {"baseline", "candidate"}, f"unknown build arm: {side}")
    return f"{side}/{filename}"


def validate_host_tool_versions(tool_versions: str, baseline_metadata: dict[str, Any]) -> None:
    cargo_version = baseline_metadata["cargo_version"]
    rustc_vv = baseline_metadata["rustc_vv"]
    require(f"cargo {cargo_version.split()[1]}" in tool_versions and
            rustc_vv.splitlines()[0] in tool_versions,
            "host tool-version preflight differs from build receipts")


def verify_build_metadata(prereg: dict[str, Any], archive: Archive,
                          hashes: dict[str, Any]) -> dict[str, Any]:
    inputs = prereg["inputs"]
    arms = {side: inputs[side] for side in ("baseline", "candidate")}
    provenance = inputs["build_provenance"]
    require(provenance["driver"]["sha256"] == HELPER_SHA256 and
            provenance["driver"]["path"].endswith("/work/build_graph_e2e_pair.py"),
            "build provenance does not identify the pinned Linux build helper")
    require(provenance["environment"]["path"].endswith("/evidence/graph-transport-e2e/build-environment-attempt-1.json"),
            "build environment receipt path is outside the frozen namespace")
    env_name = archive.find("build-environment-attempt-1.json")
    env_bytes = archive.read(env_name, MAX_SMALL_MEMBER_BYTES)
    require(sha_bytes(env_bytes) == provenance["environment"]["sha256"], "build environment receipt hash mismatch")
    environment = load_json_bytes(env_bytes, "captured build environment")
    require(environment == arms["baseline"]["build_metadata"],
            "captured environment does not equal the preregistered baseline metadata")
    base_meta = arms["baseline"]["build_metadata"]
    candidate_meta = arms["candidate"]["build_metadata"]
    for side, metadata in (("baseline", base_meta), ("candidate", candidate_meta)):
        require(set(metadata) == {"rustc_vv", "cargo_version", "selected_build_environment", "captured_before_clean_and_build"},
                f"{side} build metadata schema changed")
        require(metadata["captured_before_clean_and_build"] is True,
                f"{side} toolchain metadata was captured after build start")
        require(isinstance(metadata["rustc_vv"], str) and "host: x86_64-unknown-linux-gnu\n" in metadata["rustc_vv"],
                f"{side} rustc metadata is not for Linux x86_64")
        require(isinstance(metadata["cargo_version"], str) and metadata["cargo_version"].startswith("cargo "),
                f"{side} Cargo version metadata is malformed")
        selected = metadata["selected_build_environment"]
        allowed = {"HOME", "PATH", "TMPDIR", "LANG", "CARGO_HOME", "RUSTUP_HOME", "RUSTUP_TOOLCHAIN",
                   "CARGO_BUILD_JOBS", "CARGO_INCREMENTAL", "CARGO_TARGET_DIR", "LC_ALL", "TZ",
                   "EXECUTE_CARGO_CONFIG_FINGERPRINT_SHA256"}
        required = {"HOME", "PATH", "CARGO_HOME", "RUSTUP_HOME", "RUSTUP_TOOLCHAIN", "CARGO_BUILD_JOBS",
                    "CARGO_INCREMENTAL", "CARGO_TARGET_DIR", "LC_ALL", "TZ", "EXECUTE_CARGO_CONFIG_FINGERPRINT_SHA256"}
        require(isinstance(selected, dict) and required <= selected.keys() and selected.keys() <= allowed,
                f"{side} selected build environment has untracked/missing variables")
        require(selected["RUSTUP_TOOLCHAIN"] == "stable" and selected["CARGO_BUILD_JOBS"] == "4" and
                selected["CARGO_INCREMENTAL"] == "0" and selected["LC_ALL"] == "C" and selected["TZ"] == "UTC",
                f"{side} fixed build environment changed")
        require(selected["CARGO_HOME"].rstrip("/") == selected["HOME"].rstrip("/") + "/.cargo" and
                selected["RUSTUP_HOME"].rstrip("/") == selected["HOME"].rstrip("/") + "/.rustup",
                f"{side} Rust home paths do not follow the captured HOME")
        require(selected["CARGO_TARGET_DIR"].endswith(f"/graph-transport-e2e-{side}"),
                f"{side} Cargo target is not dedicated to this arm")
    metadata_keys = set(base_meta["selected_build_environment"]) | set(candidate_meta["selected_build_environment"])
    require(base_meta["rustc_vv"] == candidate_meta["rustc_vv"] and
            base_meta["cargo_version"] == candidate_meta["cargo_version"],
            "arms were built with different compiler/tool versions")
    for key in metadata_keys:
        if key != "CARGO_TARGET_DIR":
            require(base_meta["selected_build_environment"].get(key) == candidate_meta["selected_build_environment"].get(key),
                    f"selected environment differs between arms at {key}")
    config_name = archive.find("cargo-configuration-fingerprint.json")
    config_doc = load_json_bytes(archive.read(config_name, MAX_SMALL_MEMBER_BYTES), "Cargo config fingerprint")
    fingerprint = config_doc["fingerprint"]
    fp_body = {k: v for k, v in fingerprint.items() if k != "sha256"}
    require(sha_bytes(canonical_json(fp_body)) == fingerprint["sha256"], "Cargo configuration fingerprint digest mismatch")
    common = cargo_config_common_hash(fingerprint)
    require(config_doc["shared_environment_sha256"] == common, "shared Cargo configuration hash mismatch")
    require(base_meta["selected_build_environment"]["EXECUTE_CARGO_CONFIG_FINGERPRINT_SHA256"] == common,
            "captured environment does not bind the Cargo configuration fingerprint")

    sources = load_json_bytes(archive.read(archive.find("source-freeze.json"), MAX_SMALL_MEMBER_BYTES), "source freeze")
    require(sources.get("schema") == "execute.graph.transport-e2e.hosted-source-freeze/1" and
            sources.get("base_commit") == BASE_COMMIT and
            sources.get("candidate_patch_archive_sha256") == PATCH_ARCHIVE_SHA256 and
            sources.get("candidate_patch_sha256") == PATCH_SHA256 and
            sources.get("shared_harness_sha256") == HARNESS_SHA256 and
            sources.get("allowed_arm_differences") == ["src/graph/transport.rs"],
            "source freeze does not match the pinned source/harness/patch")
    source_command = load_json_bytes(archive.read(archive.find("source-stage-command.json"), MAX_SMALL_MEMBER_BYTES),
                                     "source stage command receipt")
    require(source_command == sources, "source staging command output differs from frozen source receipt")
    for side in ("baseline", "candidate"):
        frozen = sources["arms"][side]
        arm = arms[side]
        for field in ("root", "commit", "transport_path", "transport_sha256", "harness_path", "harness_sha256", "source_fingerprint"):
            require(frozen[field] == arm[field], f"source freeze and preregistration disagree at {side}.{field}")

    build_results: dict[str, Any] = {}
    arm_result_documents: dict[str, Any] = {}
    for side in ("baseline", "candidate"):
        arm = arms[side]
        require(arm["build_attempt"] == 1 and
                arm["receipt_path"].endswith(f"/evidence/graph-transport-e2e/{side}-binary-receipt.json"),
                f"{side} build attempt/receipt path changed")
        receipt_name = archive.find(f"{side}-binary-receipt.json")
        receipt_bytes = archive.read(receipt_name, MAX_SMALL_MEMBER_BYTES)
        require(sha_bytes(receipt_bytes) == arm["receipt_sha256"], f"{side} binary receipt hash mismatch")
        receipt = load_json_bytes(receipt_bytes, f"{side} receipt")
        require(receipt["source_stable"] is True and receipt["source_sha256"] == arm["transport_sha256"] and
                receipt["artifact"] == arm["artifact"] and receipt["sha256"] == arm["binary_sha256"] and
                receipt["build_metadata"] == arm["build_metadata"], f"{side} receipt disagrees with preregistration")
        binary_name = archive.find(f"{side}-test-binary")
        require(hashes[binary_name] == arm["binary_sha256"], f"{side} saved binary SHA mismatch")
        require(arm["saved_binary"].endswith(f"/evidence/graph-transport-e2e/{side}-test-binary"),
                f"{side} saved-binary path is outside artifact evidence")
        artifact = arm["artifact"]
        target, profile = artifact.get("target", {}), artifact.get("profile", {})
        require(artifact.get("reason") == "compiler-artifact" and artifact.get("fresh") is False and
                target.get("name") == "graph_transport_e2e_bench" and target.get("test") is True and
                target.get("kind") == ["test"] and profile.get("test") is True and
                profile.get("opt_level") == "3" and profile.get("debug_assertions") is False,
                f"{side} Cargo artifact is not the pinned optimized test target")
        rustc_host = next((line.split(": ", 1)[1] for line in arm["build_metadata"]["rustc_vv"].splitlines()
                           if line.startswith("host: ")), None)
        require(rustc_host == "x86_64-unknown-linux-gnu", f"{side} rustc host triple is not Linux x86_64")
        root = PurePosixPath(arm["root"])
        require(PurePosixPath(artifact.get("manifest_path", "")) == root / "Cargo.toml" and
                PurePosixPath(target.get("src_path", "")) == root / "tests/graph_transport_e2e_bench.rs",
                f"{side} Cargo artifact source/manifest path changed")
        cargo_target = PurePosixPath(arm["build_metadata"]["selected_build_environment"]["CARGO_TARGET_DIR"])
        executable = PurePosixPath(artifact.get("executable", ""))
        require(executable == cargo_target / "release/deps" / executable.name and
                executable.as_posix() in artifact.get("filenames", []), f"{side} compiler executable path changed")
        require(receipt["saved_binary"] == arm["saved_binary"] and receipt["artifact"] == artifact,
                f"{side} receipt build artifact identity mismatch")
        json_name = archive.find(f"{side}-build-attempt-1.jsonl")
        log_item = provenance["logs"][side]["build_json"]
        require(log_item["sha256"] == hashes[json_name], f"{side} Cargo JSONL provenance hash mismatch")
        rows = []
        for line_number, line in enumerate(archive.read(json_name, MAX_SMALL_MEMBER_BYTES).splitlines(), start=1):
            if line.strip():
                rows.append(load_json_bytes(line, f"{side} Cargo JSONL line {line_number}", 2_000_000))
        matches = [row for row in rows if row.get("reason") == "compiler-artifact" and
                   row.get("target", {}).get("name") == "graph_transport_e2e_bench"]
        require(matches == [artifact] and any(row.get("reason") == "build-finished" and row.get("success") is True for row in rows),
                f"{side} Cargo log does not attest to exactly the receipt artifact and successful build")
        for label, suffix in (("clean", f"{side}-release-clean-attempt-1.log"),
                              ("build_stderr", f"{side}-build-attempt-1.log"),
                              ("test_list", f"{side}-test-list.txt")):
            member = archive.find(suffix)
            log_pin = provenance["logs"][side][label]
            require(log_pin["path"].endswith(f"/evidence/graph-transport-e2e/{suffix}") and
                    log_pin["sha256"] == hashes[member],
                    f"{side} {label} build evidence hash mismatch")
        test_list = archive.read(archive.find(f"{side}-test-list.txt"), MAX_SMALL_MEMBER_BYTES).decode("utf-8", "strict")
        require(any(line.strip() == f"{TEST_NAME}: test" for line in test_list.splitlines()),
                f"{side} saved binary test listing lacks the frozen benchmark")
        build_dir = archive.find(arm_build_member(side, "build-result.json"))
        result = load_json_bytes(archive.read(build_dir), f"{side} build result")
        require(result == {
            "side": side, "cargo_exit_status": 0, "timed_out": False,
            "target_directory_fresh": True, "source_stable": True,
            "artifact_selected": True, "receipt_written": True, "error": None,
        }, f"{side} build result does not attest fresh successful compile")
        arm_result_documents[side] = result
        target_freshness = load_json_bytes(archive.read(archive.find(arm_build_member(side, "target-freshness.json"))),
                                           f"{side} target freshness")
        require(target_freshness == {"path": str(cargo_target), "existed_before_build": False, "confirmed_fresh": True},
                f"{side} Cargo target was not fresh")
        cargo_status = archive.read(archive.find(arm_build_member(side, "cargo-exit-status.txt")), 1024)
        require(cargo_status == b"0\n", f"{side} Cargo build exit status was not zero")
        build_command = load_json_bytes(archive.read(archive.find(arm_build_member(side, "build-command.json"))),
                                        f"{side} build command")
        require(build_command["side"] == side and build_command["attempt"] == 1 and
                build_command["cwd"] == str(root) and build_command["argv"] == BUILD_COMMAND and
                build_command["build_metadata"] == arm["build_metadata"],
                f"{side} build command or source root differs from the registered build")
        stability = load_json_bytes(archive.read(archive.find(arm_build_member(side, "source-stability.json"))),
                                    f"{side} source stability")
        require(stability["stable"] is True and stability["before"] == sources["arms"][side] and
                stability["after"] == sources["arms"][side], f"{side} source changed around build")
        build_versions = archive.read(archive.find(arm_build_member(side, "tool-versions-before-build.txt")), MAX_SMALL_MEMBER_BYTES).decode("utf-8", "strict")
        require(f"cargo: {arm['build_metadata']['cargo_version']}" in build_versions and
                f"rustc:\n{arm['build_metadata']['rustc_vv']}" in build_versions,
                f"{side} recorded pre-build tool versions differ from receipt")
        build_results[side] = {"receipt_sha256": arm["receipt_sha256"],
                               "binary_sha256": arm["binary_sha256"],
                               "target_triple": rustc_host,
                               "cargo_version": arm["build_metadata"]["cargo_version"].strip(),
                               "rustc_host": rustc_host}
    require(arms["baseline"]["binary_sha256"] != arms["candidate"]["binary_sha256"],
            "baseline and candidate saved test binaries are identical")
    build_pair_result = load_json_bytes(archive.read(archive.find("build-pair-result.json"), 128 * 1024),
                                        "build pair result")
    validate_build_pair_result(build_pair_result, arm_result_documents)
    return {"arms": build_results, "cargo_configuration_sha256": common,
            "environment_receipt_sha256": sha_bytes(env_bytes),
            "source_freeze_sha256": hashes[archive.find("source-freeze.json")],
            "build_provenance": provenance}


def parse_request(data: bytes) -> dict[str, str]:
    try:
        text = data.decode("utf-8", "strict")
    except UnicodeDecodeError as error:
        raise AuditError(f"hosted request is not UTF-8: {error}") from error
    values: dict[str, str] = {}
    for line in text.splitlines():
        if not line:
            continue
        require("=" in line, f"malformed hosted request line: {line!r}")
        key, value = line.split("=", 1)
        require(key not in values, f"duplicate hosted request key: {key}")
        values[key] = value
    require(values.get("run_id") == RUN_ID and values.get("workflow_commit") == WORKFLOW_COMMIT,
            "hosted request does not identify the fixed dispatch/run commit")
    require(values.get("base_commit") == BASE_COMMIT and values.get("warmup_pairs") == "3" and
            values.get("measured_pairs") == "48" and values.get("samples_per_invocation") == "3" and
            values.get("invocation_timeout_seconds") == "120" and values.get("no_retries") == "true",
            "hosted request changed the fixed protocol")
    require(values.get("purpose") == "fixed hosted Linux replication of the audited finite graph transport E2E collector change",
            "hosted request purpose changed")
    require(values.get("run_attempt", "").isdigit(), "hosted request run_attempt is missing")
    require(values["run_attempt"] == "1", "unexpected rerun attempt for the frozen dispatch")
    return values


def expected_order(index: int) -> list[str]:
    return ["baseline", "candidate"] if index % 2 == 0 else ["candidate", "baseline"]


def check_snapshot(snapshot: Any, arm: dict[str, Any], label: str) -> None:
    expected = {
        "commit": arm["commit"],
        "transport_sha256": arm["transport_sha256"],
        "source_fingerprint": arm["source_fingerprint"],
        "harness_sha256": HARNESS_SHA256,
        "binary_sha256": arm["binary_sha256"],
        "receipt_sha256": arm["receipt_sha256"],
        "stable": True,
    }
    require(isinstance(snapshot, dict) and all(snapshot.get(k) == v for k, v in expected.items()),
            f"{label} identity snapshot changed")
    strict_iso(snapshot.get("captured_at"), f"{label} capture")


def expected_attempt_id(global_index: int, position: int, side: str) -> tuple[str, str, int]:
    phase = "warmup" if global_index < WARMUP_PAIRS else "measurement"
    phase_index = global_index + 1 if phase == "warmup" else global_index - WARMUP_PAIRS + 1
    return f"{phase}-{phase_index:02d}-{position}-{side}", phase, phase_index


def audit(zip_path: Path, output_path: Path, *, baseline_root: Path, candidate_root: Path,
          helper_path: Path, patch_path: Path, runner_path: Path, harness_path: Path,
          sample_auditor_path: Path) -> dict[str, Any]:
    require(zipfile.is_zipfile(zip_path), "input is not a ZIP artifact")
    archive = Archive(zip_path)
    request_name = archive.find("hosted-request.txt")
    request_bytes = archive.read(request_name, 64 * 1024)
    request = parse_request(request_bytes)
    attempt_number = request["run_attempt"]
    run_suffix = f"paired-runs/run-{RUN_ID}-{attempt_number}"
    report_name = archive.find(run_suffix + "/paired-report.json")
    prereg_name = archive.find("paired-preregistration.json")
    prereg_bytes = archive.read(prereg_name, MAX_JSON_BYTES)
    report_bytes = archive.read(report_name, MAX_JSON_BYTES)
    prereg = load_json_bytes(prereg_bytes, "paired preregistration")
    report = load_json_bytes(report_bytes, "paired report")
    require(report.get("status") == "complete", "paired report is not complete; sample audit deferred")
    prereg_hash = sha_bytes(prereg_bytes)
    require(report.get("preregistration_sha256") == prereg_hash,
            "paired report preregistration digest does not match archived preregistration")
    require(PurePosixPath(report.get("preregistration_path", "")).as_posix().endswith(
        "/evidence/graph-transport-e2e/paired-preregistration.json"),
        "paired report points to a preregistration outside the frozen evidence namespace")
    require(report.get("protocol") == expected_protocol() and prereg.get("protocol") == expected_protocol(),
            "report/preregistration protocol differs from fixed source-reviewed contract")
    require(prereg.get("schema_version") == 1 and prereg.get("driver", {}).get("sha256") == RUNNER_SHA256,
            "preregistration schema or paired runner identity changed")
    require(prereg.get("driver", {}).get("path", "").endswith("/work/graph-transport-e2e-paired-runner/graph_transport_e2e_pair.py"),
            "preregistration runner path changed")
    inputs = prereg.get("inputs")
    require(isinstance(inputs, dict) and set(inputs) == {"harness", "baseline", "candidate", "build_provenance"},
            "preregistration input schema changed")
    harness_bytes = harness_path.read_bytes()
    require(sha_bytes(harness_bytes) == HARNESS_SHA256 and sha_bytes(archive.read(archive.find("graph_transport_e2e_bench.rs"), 2_000_000)) == HARNESS_SHA256,
            "canonical/archived harness hash differs from frozen pin")
    harness_text = harness_bytes.decode("utf-8", "strict")
    run_one_start = harness_text.index("fn run_one(")
    run_one_end = harness_text.index("fn fresh_capture()", run_one_start)
    run_one_body = harness_text[run_one_start:run_one_end]
    timer_start = run_one_body.index("let started = Instant::now();")
    session_start = run_one_body.index("GraphSession::start(")
    timer_stop = run_one_body.index("started.elapsed()")
    post_timer_check = run_one_body.index("verify_outputs(")
    require(timer_start < session_start < timer_stop < post_timer_check,
            "frozen harness timer no longer brackets GraphSession execution before output checks")
    require('"binary": "/bin/cat"' in harness_text and
            "session.run_to_completion()" in run_one_body,
            "frozen harness no longer exercises finite cat graph completion")
    harness_ref = inputs["harness"]
    require(harness_ref.get("sha256") == HARNESS_SHA256 and
            harness_ref.get("path", "").endswith("/evidence/graph-transport-e2e/graph_transport_e2e_bench.rs") and
            harness_ref.get("baseline_copy", "").endswith("/work/graph-optimizations-integrated-81bf9ba/tests/graph_transport_e2e_bench.rs") and
            harness_ref.get("candidate_copy", "").endswith("/work/graph-collector-sleep-candidate/tests/graph_transport_e2e_bench.rs"),
            "preregistered harness copies do not point to the intended source roots")
    local_source = inspect_local_sources(baseline_root, candidate_root, harness_bytes, prereg,
                                         patch_path, helper_path, runner_path, sample_auditor_path)

    require(report.get("host", {}).get("system") == "Linux" and report.get("host", {}).get("machine") == "x86_64",
            "paired report is not from Linux x86_64")
    strict_iso(report.get("created_at"), "report creation")
    strict_iso(report.get("completed_at"), "report completion")
    require(report.get("format") == "execute.graph.transport_e2e.paired_run.v1" and
            report.get("schema_version") == 1, "paired report format/schema changed")
    require(report.get("initial_identity") and report.get("post_run_identity"), "report identity snapshots are missing")
    for side in ("baseline", "candidate"):
        arm = inputs[side]
        check_snapshot(report["initial_identity"].get(side), arm, f"initial {side}")
        check_snapshot(report["post_run_identity"].get(side), arm, f"post-run {side}")

    attempts = report.get("attempts")
    require(isinstance(attempts, list) and len(attempts) == TOTAL_ATTEMPTS,
            "report does not contain exactly 102 invocation attempts")
    require(report.get("planned_pair_count") == TOTAL_PAIRS and report.get("planned_attempt_count") == TOTAL_ATTEMPTS and
            report.get("attempt_count") == TOTAL_ATTEMPTS and report.get("successful_attempt_count") == TOTAL_ATTEMPTS and
            report.get("failed_attempt_count") == 0 and report.get("unattempted_attempt_count") == 0,
            "report counts do not show 102 successes with no failures/unattempted slots")
    require(report.get("losses") == [] and report.get("unattempted_slots") == [],
            "report records a failure, loss slot, or retry-related unattempted work")
    warmups, measured = report.get("warmup_pairs"), report.get("measured_pairs")
    require(isinstance(warmups, list) and len(warmups) == WARMUP_PAIRS and
            isinstance(measured, list) and len(measured) == MEASURED_PAIRS,
            "pair arrays do not contain exactly 3 warmups and 48 measurements")
    all_pairs = warmups + measured
    sample_auditor = sample_auditor_module(sample_auditor_path)
    fixture = sample_auditor.expected_fixture()
    fixture_hash = sha_bytes(fixture)
    report_dir = PurePosixPath(attempts[0]["attempt_dir"]).parents[1]
    require(report_dir.as_posix().endswith("/" + run_suffix), "attempt path does not match dispatched run id")
    prereg_remote = PurePosixPath(report["preregistration_path"])
    require(prereg_remote.parent == report_dir.parents[1],
            "run and preregistration paths do not share the same hosted evidence root")
    attempt_base_suffix = run_suffix + "/attempts/"
    expected_suffixes: set[str] = set()
    for global_index in range(TOTAL_PAIRS):
        for position, side in enumerate(expected_order(global_index), start=1):
            attempt_id, _, _ = expected_attempt_id(global_index, position, side)
            for name in ("started.json", "finished.json", "raw.json", "raw.input.jsonl",
                         "raw.journal.jsonl", "stdout.bin", "stderr.bin"):
                expected_suffixes.add(attempt_base_suffix + attempt_id + "/" + name)
    # ZIP upload may prepend a common-root folder. Compare each actual path by suffix.
    for suffix in expected_suffixes:
        archive.find(suffix)
    relevant_actual = {name for name in archive.names if attempt_base_suffix in name and not name.endswith("/")}
    require(len(relevant_actual) == len(expected_suffixes),
            "archive contains missing or extra attempt files beyond the 102x7 fixed layout")

    ledger, member_hashes = archive.hash_all(expected_fixture=fixture)
    build_summary = verify_build_metadata(prereg, archive, member_hashes)
    input_receipt_name = archive.find("input-verification.json")
    input_receipt = load_json_bytes(archive.read(input_receipt_name, MAX_SMALL_MEMBER_BYTES), "input verification receipt")
    require(input_receipt.get("schema") == "execute.graph.transport-e2e.hosted-inputs/1" and
            input_receipt.get("base_commit") == BASE_COMMIT and
            input_receipt.get("canonical_harness_sha256") == HARNESS_SHA256 and
            input_receipt.get("decoded_patch_hashes") == {"graph-collector-candidate.patch.xz": PATCH_SHA256} and
            input_receipt.get("hashes") == {
                "graph_transport_e2e_pair.py": RUNNER_SHA256,
                "test_graph_transport_e2e_pair.py": RUNNER_TEST_SHA256,
                "graph_transport_e2e_bench.rs": HARNESS_SHA256,
                "graph-collector-candidate.patch.xz": PATCH_ARCHIVE_SHA256,
            }, "static input verification receipt differs from frozen source pins")
    require(input_receipt["canonical_harness_copy"].endswith("/evidence/graph-transport-e2e/graph_transport_e2e_bench.rs"),
            "input verification canonical harness was saved outside evidence")
    input_command = load_json_bytes(archive.read(archive.find("input-verification-command.json"), MAX_SMALL_MEMBER_BYTES),
                                   "input verification command output")
    require(input_command == input_receipt, "input verification command output differs from its receipt")
    require(len([entry for entry in ledger if entry["name"].endswith("/raw.json")]) == TOTAL_ATTEMPTS,
            "archive raw member count does not equal 102")
    require(member_hashes[report_name] == sha_bytes(report_bytes) and
            member_hashes[prereg_name] == prereg_hash, "report/preregistration ZIP member hashes changed")
    require(member_hashes[request_name] == sha_bytes(request_bytes), "hosted request member digest mismatch")

    raw_results: dict[str, dict[str, Any]] = {}
    metrics: list[dict[str, Any]] = []
    arm_medians: dict[str, list[int]] = {"baseline": [], "candidate": []}
    attempt_cursor = 0
    for global_index, pair in enumerate(all_pairs):
        order = expected_order(global_index)
        phase = "warmup" if global_index < WARMUP_PAIRS else "measurement"
        phase_index = global_index + 1 if phase == "warmup" else global_index - WARMUP_PAIRS + 1
        require(pair.get("global_pair_index") == global_index and pair.get("phase") == phase and
                pair.get("phase_pair_index") == phase_index and pair.get("order") == order,
                f"pair {global_index} order/index differs from fixed AB/BA schedule")
        pair_results: dict[str, dict[str, Any]] = {}
        for position, side in enumerate(order, start=1):
            attempt_id, _, _ = expected_attempt_id(global_index, position, side)
            attempt = attempts[attempt_cursor]
            attempt_cursor += 1
            arm = inputs[side]
            require(attempt.get("attempt_id") == attempt_id and attempt.get("side") == side and
                    attempt.get("phase") == phase and attempt.get("phase_pair_index") == phase_index and
                    attempt.get("global_pair_index") == global_index and attempt.get("position_in_pair") == position,
                    f"attempt {attempt_id} metadata/order differs")
            require(attempt.get("successful") is True and attempt.get("returncode") == 0 and
                    attempt.get("timed_out") is False and attempt.get("execution_error") is None and
                    attempt.get("validation_error") is None,
                    f"attempt {attempt_id} was not a clean passing invocation")
            require(exact_int(attempt.get("elapsed_wall_ns"), f"{attempt_id} elapsed wall", 1) > 0,
                    f"attempt {attempt_id} has no invocation-level elapsed time")
            expected_metadata = {
                "attempt_id": attempt_id, "phase": phase, "phase_pair_index": phase_index,
                "global_pair_index": global_index, "position_in_pair": position, "side": side,
                "commit": arm["commit"], "transport_sha256": arm["transport_sha256"],
                "source_fingerprint_sha256": arm["source_fingerprint"]["sha256"],
                "binary_sha256": arm["binary_sha256"], "receipt_sha256": arm["receipt_sha256"],
            }
            require(all(attempt.get(k) == v for k, v in expected_metadata.items()),
                    f"attempt {attempt_id} source/build metadata differs")
            attempt_dir_remote = PurePosixPath(attempt["attempt_dir"])
            require(attempt_dir_remote == report_dir / "attempts" / attempt_id,
                    f"attempt {attempt_id} directory differs from run path")
            started_name = archive.find(attempt_base_suffix + attempt_id + "/started.json")
            finished_name = archive.find(attempt_base_suffix + attempt_id + "/finished.json")
            started = load_json_bytes(archive.read(started_name, MAX_SMALL_MEMBER_BYTES), f"{attempt_id} started")
            finished = load_json_bytes(archive.read(finished_name, MAX_SMALL_MEMBER_BYTES), f"{attempt_id} finished")
            validate_attempt_timestamps(started, finished, attempt_id)
            require(finished == attempt, f"{attempt_id} finished file differs from paired report attempt record")
            require(all(started.get(k) == v for k, v in expected_metadata.items()),
                    f"{attempt_id} started metadata differs")
            raw_member = archive.find(attempt_base_suffix + attempt_id + "/raw.json")
            raw_hash = member_hashes[raw_member]
            remote_raw = str(attempt_dir_remote / "raw.json")
            binary = arm["saved_binary"]
            require(started.get("command") == [binary, "--exact", TEST_NAME, "--ignored", "--nocapture", "--test-threads=1"],
                    f"{attempt_id} invoked a different saved binary/test")
            require(started.get("timeout_seconds") == INVOCATION_TIMEOUT and
                    started.get("raw_report_path") == remote_raw,
                    f"{attempt_id} timeout or report path changed")
            expected_env = {
                "EXECUTE_GRAPH_TRANSPORT_E2E_REPORT": remote_raw,
                "EXECUTE_GRAPH_TRANSPORT_E2E_WARMUPS": "0",
                "EXECUTE_GRAPH_TRANSPORT_E2E_SAMPLES": "3",
                "LC_ALL": "C", "TZ": "UTC",
            }
            require(started.get("environment") == expected_env, f"{attempt_id} invocation environment changed")
            check_snapshot(started.get("identity_before"), arm, f"{attempt_id} start snapshot")
            check_snapshot(finished.get("identity_before"), arm, f"{attempt_id} finish-before snapshot")
            check_snapshot(finished.get("identity_after"), arm, f"{attempt_id} finish-after snapshot")
            require(started.get("identity_before") == finished.get("identity_before"),
                    f"{attempt_id} start and finish-before identities differ")
            require(finished.get("raw_report_present") is True and finished.get("raw_report_sha256") == raw_hash,
                    f"{attempt_id} raw member hash differs from finished receipt")
            fixture_member = archive.find(attempt_base_suffix + attempt_id + "/raw.input.jsonl")
            journal_member = archive.find(attempt_base_suffix + attempt_id + "/raw.journal.jsonl")
            require(finished.get("input_fixture_present") is True and
                    finished.get("input_fixture_sha256") == member_hashes[fixture_member] == fixture_hash,
                    f"{attempt_id} fixture digest does not match deterministic input")
            require(finished.get("sample_journal_present") is True and
                    finished.get("sample_journal_sha256") == member_hashes[journal_member],
                    f"{attempt_id} sample journal digest differs from finished receipt")
            stdout_member = archive.find(attempt_base_suffix + attempt_id + "/stdout.bin")
            stderr_member = archive.find(attempt_base_suffix + attempt_id + "/stderr.bin")
            stdout = archive.read(stdout_member, 2 * 1024 * 1024)
            stderr = archive.read(stderr_member, 2 * 1024 * 1024)
            require(finished.get("stdout_sha256") == sha_bytes(stdout) and
                    finished.get("stderr_sha256") == sha_bytes(stderr),
                    f"{attempt_id} process output hash differs")
            output_text = (stdout + b"\n" + stderr).decode("utf-8", errors="replace")
            require("running 1 test" in output_text and f"test {TEST_NAME} ... ok" in output_text and
                    re.search(r"test result: ok\. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in [0-9.]+s", output_text),
                    f"{attempt_id} does not show exactly one passing ignored benchmark")
            raw_data = archive.read(raw_member, MAX_RAW_BYTES)
            fixture_data = archive.read(fixture_member, len(fixture) + 1)
            journal_data = archive.read(journal_member, 1024 * 1024)
            require(fixture_data == fixture, f"{attempt_id} fixture bytes differ from exact deterministic fixture")
            raw_result = validate_raw_with_reference(raw_data, fixture, journal_data, remote_raw, sample_auditor)
            require(raw_result["original_raw_sha256"] == raw_hash, f"{attempt_id} raw bytes changed during parsing")
            require(attempt.get("raw_duration_ns") == raw_result["durations_ns"] and
                    attempt.get("median_duration_ns") == raw_result["median_duration_ns"],
                    f"{attempt_id} runner duration summary differs from raw samples")
            require(attempt.get("oracle") == raw_result["arm_oracle"],
                    f"{attempt_id} runner oracle differs from independently checked raw oracle")
            raw_results[attempt_id] = raw_result
            pair_results[side] = raw_result
            if phase == "measurement":
                arm_medians[side].append(raw_result["median_duration_ns"])
        base, cand = pair_results["baseline"], pair_results["candidate"]
        require(base["arm_oracle"] == cand["arm_oracle"], f"pair {global_index} full oracle differs between arms")
        require(pair.get("valid_for_timing_comparison") is True and pair.get("full_oracle_equal") is True,
                f"pair {global_index} not marked valid despite required exact oracle")
        bmed, cmed = base["median_duration_ns"], cand["median_duration_ns"]
        delta, ratio = cmed - bmed, cmed / bmed
        cases = pair.get("cases", {}).get("whole_graph_session")
        require(isinstance(cases, dict) and cases.get("baseline_raw_duration_ns") == base["durations_ns"] and
                cases.get("candidate_raw_duration_ns") == cand["durations_ns"] and
                cases.get("baseline_median_duration_ns") == bmed and cases.get("candidate_median_duration_ns") == cmed and
                cases.get("candidate_minus_baseline_ns") == delta and
                math.isclose(cases.get("candidate_over_baseline", float("nan")), ratio, rel_tol=1e-12, abs_tol=1e-15) and
                math.isclose(cases.get("candidate_change_percent", float("nan")), (ratio - 1.0) * 100.0,
                             rel_tol=1e-12, abs_tol=1e-12),
                f"pair {global_index} recorded timing math differs from raw recomputation")
        metrics.append({"global_pair_index": global_index, "phase": phase, "phase_pair_index": phase_index,
                        "order": order, "baseline_median_ns": bmed, "candidate_median_ns": cmed,
                        "candidate_minus_baseline_ns": delta, "candidate_over_baseline": ratio,
                        "full_oracle_equal": True})
    require(attempt_cursor == TOTAL_ATTEMPTS,
            "not all raw attempts were validated exactly once")
    measured_metrics = metrics[WARMUP_PAIRS:]
    ratios = [row["candidate_over_baseline"] for row in measured_metrics]
    deltas = [row["candidate_minus_baseline_ns"] for row in measured_metrics]
    wins = [row["global_pair_index"] for row in measured_metrics if row["candidate_over_baseline"] < 1.0]
    losses = [row["global_pair_index"] for row in measured_metrics if row["candidate_over_baseline"] > 1.0]
    ties = [row["global_pair_index"] for row in measured_metrics if row["candidate_over_baseline"] == 1.0]
    summary = report.get("paired_summary")
    deltas_in_order = [{"global_pair_index": row["global_pair_index"],
                        "candidate_minus_baseline_ns": row["candidate_minus_baseline_ns"]}
                       for row in measured_metrics]
    require(isinstance(summary, dict) and summary.get("valid_measured_pair_count") == MEASURED_PAIRS and
            summary.get("paired_deltas_in_order") == deltas_in_order and
            summary.get("median_paired_delta_ns") == statistics.median(deltas) and
            summary.get("candidate_loss_pair_indices") == losses and
            summary.get("candidate_improvement_pair_indices") == wins and
            summary.get("tie_pair_indices") == ties,
            "runner aggregate summary differs from independent 48-pair recomputation")
    require(summary.get("metric") == "whole_graph_session median duration per invocation; candidate minus baseline",
            "paired summary metric label changed")

    # Verify every important host/build evidence file is covered by the archive ledger.
    require(member_hashes[archive.find("hosted-request.txt")] == sha_bytes(request_bytes), "hosted request hash mismatch")
    for path in ("host-tool-versions.txt", "host-info.txt", "input-verification-command.json",
                 "source-stage-command.json", "prepare-command.json", "build-pair-command.json",
                 "build-pair-result.json", "cargo-configuration-fingerprint.json"):
        archive.find(path)
    tool_versions = archive.read(archive.find("host-tool-versions.txt"), 64 * 1024).decode("utf-8", "strict")
    validate_host_tool_versions(tool_versions, inputs["baseline"]["build_metadata"])
    host_info = load_json_bytes(archive.read(archive.find("host-info.json"), 128 * 1024), "host info")
    require(host_info.get("runner_os") == "Linux" and host_info.get("runner_arch") == "X64" and
            host_info.get("python", "").strip() == tool_versions.splitlines()[0],
            "host metadata is not the pinned Linux x64 runner")
    build_pair_result = load_json_bytes(archive.read(archive.find("build-pair-result.json"), 128 * 1024), "build pair result")
    # verify_build_metadata also compares this receipt to both archived per-arm results.
    validate_build_pair_result(build_pair_result, {
        side: load_json_bytes(archive.read(archive.find(arm_build_member(side, "build-result.json"))),
                              f"{side} build result")
        for side in ("baseline", "candidate")
    })
    build_pair_command = load_json_bytes(archive.read(archive.find("build-pair-command.json"), 128 * 1024),
                                         "build pair command output")
    require(build_pair_command == build_pair_result, "build command output differs from saved pair result")
    prepare_command = load_json_bytes(archive.read(archive.find("prepare-command.json"), 128 * 1024),
                                      "paired prepare command output")
    require(prepare_command.get("sha256") == prereg_hash and
            prepare_command.get("preregistration", "").endswith("/evidence/graph-transport-e2e/paired-preregistration.json"),
            "paired prepare command does not bind the archived preregistration")

    require(not output_path.exists(), f"refusing to overwrite audit output {output_path}")
    result = {
        "schema_version": 1,
        "status": "complete",
        "audited_at": datetime.now(timezone.utc).isoformat().replace("+00:00", "Z"),
        "audit_source_sha256": sha_file(Path(__file__)),
        "artifact_zip": str(zip_path.resolve()),
        "artifact_zip_sha256": zip_sha256(zip_path),
        "artifact_zip_bytes": zip_path.stat().st_size,
        "run_id": RUN_ID,
        "run_attempt": attempt_number,
        "workflow_commit": WORKFLOW_COMMIT,
        "paired_runner_sha256": RUNNER_SHA256,
        "build_helper_sha256": HELPER_SHA256,
        "sample_oracle_auditor_sha256": SAMPLE_AUDITOR_SHA256,
        "preregistration_sha256": prereg_hash,
        "candidate_patch_sha256": PATCH_SHA256,
        "candidate_patch_archive_sha256": PATCH_ARCHIVE_SHA256,
        "harness_sha256": HARNESS_SHA256,
        "local_source_identity": local_source,
        "build_provenance": build_summary,
        "host": report["host"],
        "timer_boundary_source_review": "The pinned Rust harness captures Instant before GraphSession::start and immediately after run_to_completion; verify_outputs and event checks follow the timer.",
        "protocol": expected_protocol(),
        "workload_oracle": {
            "records_per_consumer": RECORD_COUNT,
            "text_bytes_per_record": TEXT_BYTES,
            "terminal_events_per_sample": RECORD_COUNT * 2 + 1,
            "input_fixture_bytes": len(fixture),
            "input_fixture_sha256": fixture_hash,
            "samples_per_invocation": INNER_SAMPLES,
            "raw_validation": "Each sample passed the separate frozen oracle validator: exact fixture bytes, full ordered input/output identities, equal fanout sequences, payload hashes, generation/revision/metadata checks, natural EOF/status accounting, 8193 terminal events, and zero dropped messages.",
        },
        "attempt_count": TOTAL_ATTEMPTS,
        "successful_attempt_count": TOTAL_ATTEMPTS,
        "failed_attempt_count": 0,
        "unattempted_attempt_count": 0,
        "warmup_pair_count": WARMUP_PAIRS,
        "measured_pair_count": MEASURED_PAIRS,
        "measured_candidate_wins": len(wins),
        "measured_candidate_losses": len(losses),
        "measured_ties": len(ties),
        "win_pair_indices_global": wins,
        "loss_pair_indices_global": losses,
        "tie_pair_indices_global": ties,
        "median_paired_candidate_over_baseline": statistics.median(ratios),
        "median_paired_candidate_minus_baseline_ns": statistics.median(deltas),
        "baseline_arm_median_of_48_invocation_medians_ns": statistics.median(arm_medians["baseline"]),
        "candidate_arm_median_of_48_invocation_medians_ns": statistics.median(arm_medians["candidate"]),
        "candidate_over_baseline_arm_median_ratio": statistics.median(arm_medians["candidate"]) / statistics.median(arm_medians["baseline"]),
        "pairs": metrics,
        "attempts": [{"attempt_id": row["attempt_id"], "side": row["side"], "phase": row["phase"],
                      "global_pair_index": row["global_pair_index"],
                      "durations_ns": raw_results[row["attempt_id"]]["durations_ns"],
                      "median_duration_ns": raw_results[row["attempt_id"]]["median_duration_ns"],
                      "raw_sha256": raw_results[row["attempt_id"]]["original_raw_sha256"],
                      "journal_sha256": row["sample_journal_sha256"],
                      "fixture_sha256": row["input_fixture_sha256"],
                      "elapsed_wall_ns": row["elapsed_wall_ns"]} for row in attempts],
        "zip_members": ledger,
        "limitations": [
            "Read-only audit; no Linux binary was executed locally and no Cargo build was run by the auditor.",
            "This is one fixed 4,096-record Linux x86_64 hosted run, not a whole-application or general runtime performance claim.",
            "It verifies the archived protocol and evidence; it does not establish broader workload representativeness.",
        ],
    }
    output_path.parent.mkdir(parents=True, exist_ok=True)
    output_path.write_text(json.dumps(result, sort_keys=True, indent=2, ensure_ascii=False, allow_nan=False) + "\n", encoding="utf-8")
    return result


def sample_auditor_module(path: Path) -> Any:
    require(sha_file(path) == SAMPLE_AUDITOR_SHA256, "reference sample-oracle auditor SHA differs")
    spec = importlib.util.spec_from_file_location("graph_transport_sample_audit", path)
    require(spec is not None and spec.loader is not None, "cannot load frozen sample-oracle auditor")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--artifact-zip", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--baseline-root", type=Path, default=DEFAULT_BASELINE)
    parser.add_argument("--candidate-root", type=Path, default=DEFAULT_CANDIDATE)
    parser.add_argument("--helper-source", type=Path, default=DEFAULT_HELPER)
    parser.add_argument("--patch-archive", type=Path, default=DEFAULT_PATCH)
    parser.add_argument("--runner-source", type=Path, default=PACKAGE / "graph_transport_e2e_pair.py")
    parser.add_argument("--harness-source", type=Path, default=DEFAULT_HARNESS)
    parser.add_argument("--sample-auditor", type=Path, default=DEFAULT_SAMPLE_AUDITOR)
    args = parser.parse_args()
    result = audit(args.artifact_zip.expanduser().resolve(strict=True), args.output.expanduser().absolute(),
                   baseline_root=args.baseline_root, candidate_root=args.candidate_root,
                   helper_path=args.helper_source, patch_path=args.patch_archive,
                   runner_path=args.runner_source, harness_path=args.harness_source,
                   sample_auditor_path=args.sample_auditor)
    print(json.dumps({"status": result["status"], "attempts": result["attempt_count"],
                      "measured_pairs": result["measured_pair_count"],
                      "wins": result["measured_candidate_wins"],
                      "losses": result["measured_candidate_losses"],
                      "median_paired_ratio": result["median_paired_candidate_over_baseline"],
                      "artifact_sha256": result["artifact_zip_sha256"],
                      "output": str(args.output.absolute())}, sort_keys=True))
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (AuditError, RuntimeError, OSError, KeyError, TypeError, ValueError,
            subprocess.SubprocessError, zipfile.BadZipFile, lzma.LZMAError) as error:
        print(f"hosted graph E2E ZIP audit failed: {error}", file=sys.stderr)
        raise SystemExit(1)
