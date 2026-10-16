#!/usr/bin/env python3
"""Independent audit of the paired finite-graph CPU diagnostic artifacts."""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import statistics
import subprocess
from typing import Any, Mapping


CAMPAIGN = Path(__file__).resolve().parents[1]
EVIDENCE = CAMPAIGN / "evidence/collector-cpu-diagnostic"
DEFAULT_REPORT = EVIDENCE / "paired-run-attempt-1/paired-report.json"
AUDIT_OUTPUT = EVIDENCE / "paired-run-attempt-1-independent-audit.json"
BASELINE_COMMIT = "81bf9ba766a775cccf059007b19f5def9b9bfb03"
BASELINE_TRANSPORT_SHA256 = "049b2b13a6c82ba0c98f20b57a44e76c420d631815f1b673ba70d7902f24610c"
CANDIDATE_TRANSPORT_SHA256 = "bfc03a7028b2871a4823ed0a130e3b735ca8dc535cac9c12cd39d876174dad9c"
HARNESS_SHA256 = "f3d484860ca6252fd0f4fdca53d9493719d199fcf66a660905e600949dcbc134"
METHOD = (
    "wall-clock GraphSession start through run_to_completion; real finite cat NativeStream, typed JSON decoding, "
    "two bounded lossless fanout edges, two custom pass-through receivers and terminal collection; getrusage "
    "RUSAGE_SELF and RUSAGE_CHILDREN snapshots are taken before the wall timer and immediately after it; "
    "all exact payload/order/hash validation follows the timed region"
)
PATCH_SHA256 = "7d411b9b0fc3a8a37fbd589381720016916b6edabec43be06414e198cd39bca4"
RECORDS = 4096
TEXT_BYTES = 256
GENERATION = 17
REVISION = 29
WARMUP_PAIRS = 3
MEASURED_PAIRS = 12
INNER_SAMPLES = 1
TRANSPORT_RELATIVE = "src/graph/transport.rs"
HARNESS_RELATIVE = "tests/collector_cpu_diag_bench.rs"
BUILD_CONFIGS = {
    "Cargo.toml",
    "Cargo.lock",
    "build.rs",
    "rust-toolchain",
    "rust-toolchain.toml",
    "rustfmt.toml",
}
COUNTERS = (
    "user_cpu_us",
    "system_cpu_us",
    "voluntary_context_switches",
    "involuntary_context_switches",
)
FIXED_ENV = {
    "CARGO_HOME": "/Users/earlcameron/Documents/Codex/2026-10-12/you/work/rust/cargo",
    "RUSTUP_HOME": "/Users/earlcameron/Documents/Codex/2026-10-12/you/work/rust/rustup",
    "RUSTUP_TOOLCHAIN": "stable-aarch64-apple-darwin",
    "CARGO_TARGET_DIR": str(CAMPAIGN / "work/perf-target-candidate"),
    "CARGO_BUILD_JOBS": "4",
}
ENV_ALLOWLIST = {
    "PATH",
    "HOME",
    "TMPDIR",
    "LANG",
    "LC_ALL",
    "TZ",
    "SDKROOT",
    "DEVELOPER_DIR",
    "MACOSX_DEPLOYMENT_TARGET",
    *FIXED_ENV,
}


class AuditError(RuntimeError):
    pass


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
            raise AuditError(f"duplicate JSON key: {key}")
        result[key] = value
    return result


def _reject_constant(value: str) -> None:
    raise AuditError(f"non-finite JSON constant: {value}")


def load_json(path: Path, label: str, limit: int = 128 * 1024 * 1024) -> Any:
    try:
        data = path.read_bytes()
    except OSError as error:
        raise AuditError(f"cannot read {label} at {path}: {error}") from error
    if len(data) > limit:
        raise AuditError(f"{label} exceeds audit size limit ({len(data)} > {limit})")
    try:
        return json.loads(
            data.decode("utf-8"),
            object_pairs_hook=_unique_object,
            parse_constant=_reject_constant,
        )
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise AuditError(f"{label} is not strict UTF-8 JSON: {error}") from error


def require(condition: bool, message: str) -> None:
    if not condition:
        raise AuditError(message)


def exact_int(value: Any, label: str, minimum: int = 0) -> int:
    require(type(value) is int and value >= minimum, f"{label} is not an integer >= {minimum}")
    return value


def expected_fixture() -> bytes:
    return b"".join(
        canonical_json({"id": record_id, "text": "x" * TEXT_BYTES}) + b"\n"
        for record_id in range(1, RECORDS + 1)
    )


def expected_configuration() -> dict[str, Any]:
    typed_payload_bytes = sum(
        len(canonical_json({"Record": {"id": {"Integer": item}, "text": {"String": "x" * TEXT_BYTES}}}))
        for item in range(1, RECORDS + 1)
    )
    return {
        "inner_warmups": 0,
        "raw_samples": 1,
        "record_count": RECORDS,
        "text_bytes_per_record": TEXT_BYTES,
        "edge_capacity": 8,
        "edge_policy": "lossless",
        "terminal_capacity": 16,
        "generation": GENERATION,
        "revision": REVISION,
        "max_collected_events": RECORDS * 2 + 1,
        "max_collected_bytes": typed_payload_bytes * 2 + len(canonical_json({"Integer": 0})),
        "max_message_bytes": 4096,
        "expected_launched_processes": 1,
        "resource_counters": {
            "source": "getrusage",
            "self_scope": "process-wide counters including threads, as exposed by the host OS",
            "children_scope": "children already reaped before the post-run snapshot",
            "fields": list(COUNTERS),
            "snapshots_outside_wall_timer": True,
        },
    }


def expected_manifest_sha256() -> str:
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


def expected_template_sha256() -> str:
    template = (
        "graph graph_transport_e2e\n"
        "stream from bench.jsonl_source with path __FIXTURE__ as producer\n"
        "show each line\n"
        "end graph\n"
    )
    return sha256_bytes(template.encode("utf-8"))


def expected_schedule() -> list[dict[str, Any]]:
    schedule = []
    for index in range(WARMUP_PAIRS + MEASURED_PAIRS):
        phase = "warmup" if index < WARMUP_PAIRS else "measurement"
        phase_index = index + 1 if phase == "warmup" else index - WARMUP_PAIRS + 1
        order = ["baseline", "candidate"] if index % 2 == 0 else ["candidate", "baseline"]
        schedule.append(
            {
                "global_pair_index": index,
                "phase": phase,
                "phase_pair_index": phase_index,
                "order": order,
                "attempt_ids": [
                    f"{phase}-{phase_index:02d}-{position}-{side}"
                    for position, side in enumerate(order, start=1)
                ],
            }
        )
    return schedule


def source_fingerprint(root: Path) -> dict[str, Any]:
    paths = subprocess.run(
        ["git", "-C", str(root), "ls-files", "-z"],
        check=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    ).stdout.split(b"\0")
    selected = sorted(
        item.decode("utf-8")
        for item in paths
        if item
        and (
            item.decode("utf-8").startswith(("src/", "vendor/", ".cargo/"))
            or item.decode("utf-8") in BUILD_CONFIGS
        )
    )
    files: dict[str, str] = {}
    digest = hashlib.sha256()
    for relative in selected:
        path = root / relative
        require(path.is_file(), f"tracked build input missing: {path}")
        file_hash = sha256_file(path)
        files[relative] = file_hash
        digest.update(relative.encode("utf-8"))
        digest.update(b"\0")
        digest.update(file_hash.encode("ascii"))
        digest.update(b"\n")
    return {"sha256": digest.hexdigest(), "files": files}


def git_text(root: Path, *args: str) -> str:
    return subprocess.run(
        ["git", "-C", str(root), *args],
        check=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    ).stdout.strip()


def audit_current_source(identity: Mapping[str, Any], side: str) -> None:
    root = Path(identity["root"]).resolve(strict=True)
    require(git_text(root, "rev-parse", "--show-toplevel") == str(root), f"{side} root is not the worktree top")
    require(git_text(root, "rev-parse", "HEAD") == BASELINE_COMMIT, f"{side} commit differs")
    expected_changed = [] if side == "baseline" else [TRANSPORT_RELATIVE]
    changed = git_text(root, "diff", "--name-only").splitlines()
    staged = git_text(root, "diff", "--cached", "--name-only").splitlines()
    untracked = [
        item.decode("utf-8")
        for item in subprocess.run(
            ["git", "-C", str(root), "ls-files", "--others", "--exclude-standard", "-z"],
            check=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        ).stdout.split(b"\0")
        if item
    ]
    require(sorted(changed) == expected_changed and staged == [], f"{side} tracked worktree state changed")
    require(untracked == [HARNESS_RELATIVE], f"{side} untracked worktree state changed")
    require(source_fingerprint(root) == identity["source_fingerprint"], f"{side} source fingerprint changed")
    expected_transport = BASELINE_TRANSPORT_SHA256 if side == "baseline" else CANDIDATE_TRANSPORT_SHA256
    require(sha256_file(root / TRANSPORT_RELATIVE) == expected_transport, f"{side} transport pin changed")
    require(sha256_file(root / HARNESS_RELATIVE) == HARNESS_SHA256, f"{side} harness copy changed")


def verify_config_file(item: Mapping[str, Any]) -> None:
    path = Path(item["path"])
    present = path.exists() or path.is_symlink()
    require(present is item["present"], f"Cargo config presence changed at {path}")
    if not present:
        require(item["resolved_path"] is None and item["sha256"] is None and item["symlink_target"] is None,
                f"absent Cargo config metadata is inconsistent at {path}")
        return
    require(path.is_file(), f"Cargo config is no longer a regular file: {path}")
    require(str(path.resolve(strict=True)) == item["resolved_path"], f"Cargo config resolved path changed: {path}")
    require(sha256_file(path) == item["sha256"], f"Cargo config content changed: {path}")
    target = os.readlink(path) if path.is_symlink() else None
    require(target == item["symlink_target"], f"Cargo config symlink target changed: {path}")


def verify_configuration_fingerprint(fingerprint: Mapping[str, Any], env: Mapping[str, str], identities: Mapping[str, Any]) -> None:
    require(fingerprint["schema"] == "execute.graph.cargo_configuration_fingerprint/1", "config fingerprint schema changed")
    body = {key: value for key, value in fingerprint.items() if key != "sha256"}
    require(sha256_bytes(canonical_json(body)) == fingerprint["sha256"], "config fingerprint digest is invalid")
    cargo_home = Path(env["CARGO_HOME"]).absolute()
    require(fingerprint["cargo_home"]["path"] == str(cargo_home), "CARGO_HOME fingerprint path differs from environment")
    expected_names = ("config", "config.toml")
    cargo_files = fingerprint["cargo_home"]["config_files"]
    require([item["path"] for item in cargo_files] == [str(cargo_home / name) for name in expected_names],
            "CARGO_HOME fingerprint does not list both config spellings")
    for item in cargo_files:
        verify_config_file(item)
    home = Path(env["HOME"]).expanduser().resolve(strict=True)
    roots = fingerprint["source_roots"]
    require(set(roots) == {"baseline", "candidate"}, "config fingerprint lacks both source roots")
    for side in ("baseline", "candidate"):
        identity = identities[side]
        root = Path(identity["root"]).resolve(strict=True)
        entry = roots[side]
        require(entry["root"] == str(root), f"{side} config fingerprint is bound to another root")
        dirs: list[Path] = []
        current = root
        home_is_ancestor = False
        while True:
            dirs.append(current)
            if current == home:
                home_is_ancestor = True
                break
            if current == current.parent:
                break
            current = current.parent
        require(entry["home_is_ancestor"] is home_is_ancestor, f"{side} HOME ancestry flag differs")
        expected_paths = [str(directory / ".cargo" / name) for directory in dirs for name in expected_names]
        config_files = entry["config_files"]
        require([item["path"] for item in config_files] == expected_paths,
                f"{side} config fingerprint does not cover every source-root ancestor")
        for item in config_files:
            verify_config_file(item)


def verify_build_binding(prereg: Mapping[str, Any]) -> dict[str, Any]:
    inputs = prereg["inputs"]
    env_receipt_path = Path(inputs["build_provenance"]["environment"]["path"])
    env_receipt = load_json(env_receipt_path, "build environment receipt")
    require(sha256_file(env_receipt_path) == inputs["build_provenance"]["environment"]["sha256"],
            "build environment receipt hash differs from preregistration")
    policy = prereg["protocol"]["build_environment_policy"]
    require(env_receipt["build_environment_policy"] == policy, "build environment policy differs")
    for label, pinned in (("paired-run driver", prereg["driver"]),
                          ("protocol source", inputs["protocol_source"])):
        path = Path(pinned["path"])
        require(sha256_file(path) == pinned["sha256"], f"{label} source hash differs")
    protocol_source = load_json(Path(inputs["protocol_source"]["path"]), "protocol source")
    require(protocol_source["schema"] == "execute.graph.transport_cpu_diagnostic_preregistration/1",
            "standalone protocol schema differs")
    require(protocol_source["baseline"]["commit"] == BASELINE_COMMIT
            and protocol_source["baseline"]["transport_sha256"] == BASELINE_TRANSPORT_SHA256,
            "standalone baseline identity differs")
    require(protocol_source["candidate"]["commit"] == BASELINE_COMMIT
            and protocol_source["candidate"]["transport_sha256"] == CANDIDATE_TRANSPORT_SHA256
            and protocol_source["candidate"]["product_patch_sha256"] == PATCH_SHA256,
            "standalone candidate identity differs")
    require(protocol_source["shared_harness"]["sha256"] == HARNESS_SHA256
            and protocol_source["pairing"]["schedule"] == expected_schedule(),
            "standalone harness or pair schedule differs")
    require(protocol_source["pairing"]["attempt_count"] == 30
            and protocol_source["pairing"]["measured_pairs"] == MEASURED_PAIRS
            and protocol_source["pairing"]["retry_policy"] == "none; retain every attempt and all losses",
            "standalone pairing policy differs")
    require(protocol_source["resource_counters"]["api"] == "getrusage"
            and protocol_source["resource_counters"]["wall_timer_unchanged"] is True
            and protocol_source["resource_counters"]["fields"] == list(COUNTERS),
            "standalone resource-counter contract differs")
    require(protocol_source["workload"]["records_per_consumer"] == RECORDS
            and protocol_source["workload"]["terminal_events_per_sample"] == RECORDS * 2 + 1
            and protocol_source["workload"]["natural_eof"] is True
            and protocol_source["workload"]["expected_dropped_messages"] == 0,
            "standalone workload contract differs")
    for side in ("baseline", "candidate"):
        identity = inputs[side]
        audit_current_source(identity, side)
        receipt_path = Path(identity["receipt_path"])
        require(sha256_file(receipt_path) == identity["receipt_sha256"], f"{side} build receipt hash differs")
        receipt = load_json(receipt_path, f"{side} build receipt")
        require(receipt["sha256"] == identity["binary_sha256"], f"{side} binary receipt differs")
        binary = Path(identity["saved_binary"])
        require(sha256_file(binary) == identity["binary_sha256"], f"{side} saved binary hash differs")
        require((binary.stat().st_mode & 0o777) == 0o555, f"{side} saved binary mode differs")
        require(receipt["build_metadata"] == identity["build_metadata"], f"{side} build metadata differs")
        artifact = receipt["artifact"]
        require(artifact["target"]["name"] == "collector_cpu_diag_bench", f"{side} artifact target differs")
        require(artifact["target"]["kind"] == ["test"] and artifact["target"]["test"] is True,
                f"{side} artifact is not the intended test target")
        require(artifact["profile"]["opt_level"] == "3" and artifact["profile"]["debug_assertions"] is False,
                f"{side} artifact profile differs")
        require(artifact["fresh"] is False, f"{side} artifact was not freshly built")
    selected = inputs["baseline"]["build_metadata"]["selected_build_environment"]
    require(inputs["candidate"]["build_metadata"]["selected_build_environment"] == selected,
            "arm build environments differ")
    require(env_receipt["selected_build_environment"] == selected,
            "build environment receipt differs from recorded artifact environment")
    require(set(selected) <= ENV_ALLOWLIST and all(selected.get(key) == value for key, value in FIXED_ENV.items()),
            "selected build environment violates the controlled policy")
    require(inputs["build_provenance"]["driver"]["sha256"] == inputs["baseline"]["build_metadata"]["build_driver_sha256"],
            "build driver pin differs from receipt metadata")
    for side in ("baseline", "candidate"):
        require(inputs[side]["build_metadata"]["cargo_configuration_fingerprint"] == env_receipt["cargo_configuration_fingerprint"],
                f"{side} Cargo config snapshot differs from environment receipt")
    verify_configuration_fingerprint(env_receipt["cargo_configuration_fingerprint"], selected, inputs)
    # The preregistration's build-provenance object pins the environment, driver, all logs and both build receipts.
    def verify_pins(value: Any) -> None:
        if isinstance(value, dict):
            if set(value) == {"path", "sha256"}:
                path = Path(value["path"])
                require(sha256_file(path) == value["sha256"], f"build provenance file pin changed: {path}")
            else:
                for item in value.values():
                    verify_pins(item)
        elif isinstance(value, list):
            for item in value:
                verify_pins(item)
    verify_pins(inputs["build_provenance"])
    return {"environment_receipt_sha256": sha256_file(env_receipt_path), "selected_environment": selected}


def _validate_identity_event(value: Any, expected_port: dict[str, str], expected_input: Any = None) -> int:
    require(isinstance(value, dict), "event identity is not an object")
    require(set(value) == {"port", "input", "generation", "revision", "sequence", "sensitivity", "remote"},
            "event identity field set changed")
    require(value["port"] == expected_port and value["input"] == expected_input, "event port identity changed")
    require(value["generation"] == GENERATION and value["revision"] == REVISION, "event generation/revision changed")
    require(value["sensitivity"] == ["unknown"] and value["remote"] is None, "event metadata changed")
    return exact_int(value["sequence"], "event sequence", 1)


def validate_verification(value: Any, fixture_hash: str) -> dict[str, Any]:
    require(isinstance(value, dict) and set(value) == {"terminal_event_count", "source_status_event_count", "source_status_identity", "consumers"},
            "verification schema changed")
    require(exact_int(value["terminal_event_count"], "terminal events") == RECORDS * 2 + 1,
            "terminal output count is not 8193")
    require(exact_int(value["source_status_event_count"], "source status count") == 1,
            "producer status is missing or duplicated")
    status_identity = value["source_status_identity"]
    status_sequence = _validate_identity_event(status_identity, {"node": "producer", "port": "status"})
    consumers = value["consumers"]
    require(isinstance(consumers, list) and len(consumers) == 2, "consumer receipt count changed")
    emissions: set[int] = set()
    normalized_consumers: list[dict[str, Any]] = []
    common_inputs: list[int] | None = None
    for index, consumer_id in enumerate(("consumer-0", "consumer-1")):
        consumer = consumers[index]
        expected_keys = {
            "consumer", "input_event_count", "input_sequence_sha256", "input_source_port", "input_target_port",
            "input_sensitivity", "input_remote_metadata", "input_event_identities", "output_event_count",
            "output_payload_sha256", "output_event_identities",
        }
        require(isinstance(consumer, dict) and set(consumer) == expected_keys, f"{consumer_id} receipt schema changed")
        require(consumer["consumer"] == consumer_id, "consumer order or identity changed")
        require(exact_int(consumer["input_event_count"], "input count") == RECORDS, f"{consumer_id} input count changed")
        require(exact_int(consumer["output_event_count"], "output count") == RECORDS, f"{consumer_id} output count changed")
        require(consumer["input_source_port"] == {"node": "producer", "port": "stdout"}, "source port changed")
        require(consumer["input_target_port"] == {"node": consumer_id, "port": "events"}, "target port changed")
        require(consumer["input_sensitivity"] == ["unknown"] and consumer["input_remote_metadata"] is None,
                "input sensitivity or remote metadata changed")
        inputs = consumer["input_event_identities"]
        require(isinstance(inputs, list) and len(inputs) == RECORDS, f"{consumer_id} input identity count changed")
        sequences: list[int] = []
        prior = 0
        normalized_inputs = []
        for event in inputs:
            require(isinstance(event, dict) and set(event) == {
                "sequence", "generation", "revision", "source_port_matches", "input_port_matches",
                "sensitivity_unknown_only", "remote_absent",
            }, f"{consumer_id} input identity schema changed")
            sequence = exact_int(event["sequence"], "input sequence", 1)
            require(sequence > prior, f"{consumer_id} input order changed")
            prior = sequence
            require(event["generation"] == GENERATION and event["revision"] == REVISION, "input generation/revision changed")
            require(event["source_port_matches"] is True and event["input_port_matches"] is True,
                    "input port identity changed")
            require(event["sensitivity_unknown_only"] is True and event["remote_absent"] is True,
                    "input metadata changed")
            sequences.append(sequence)
            emissions.add(sequence)
            normalized_inputs.append({key: item for key, item in event.items() if key != "sequence"})
        seq_hash = sha256_bytes(b"".join(number.to_bytes(8, "little") for number in sequences))
        require(consumer["input_sequence_sha256"] == seq_hash, f"{consumer_id} input sequence digest changed")
        if common_inputs is None:
            common_inputs = sequences
        else:
            require(sequences == common_inputs, "fanout consumers received different source sequence identities")

        outputs = consumer["output_event_identities"]
        require(isinstance(outputs, list) and len(outputs) == RECORDS, f"{consumer_id} output identity count changed")
        prior = 0
        normalized_outputs = []
        for output_index, event in enumerate(outputs):
            sequence = _validate_identity_event(event, {"node": consumer_id, "port": "stdout"})
            require(sequence > prior and sequence > sequences[output_index], f"{consumer_id} output ordering changed")
            require(sequence not in emissions, "duplicate graph event sequence")
            emissions.add(sequence)
            prior = sequence
            normalized_outputs.append({key: item for key, item in event.items() if key != "sequence"})
        require(consumer["output_payload_sha256"] == fixture_hash, f"{consumer_id} output payload hash differs from fixture")
        normalized_consumers.append({
            "consumer": consumer_id,
            "input_event_count": RECORDS,
            "input_source_port": consumer["input_source_port"],
            "input_target_port": consumer["input_target_port"],
            "input_sensitivity": consumer["input_sensitivity"],
            "input_remote_metadata": consumer["input_remote_metadata"],
            "input_event_identities_without_sequences": normalized_inputs,
            "output_event_count": RECORDS,
            "output_payload_sha256": consumer["output_payload_sha256"],
            "output_event_identities_without_sequences": normalized_outputs,
        })
    require(common_inputs is not None and status_sequence > common_inputs[-1], "producer status does not follow source stream")
    require(status_sequence not in emissions, "producer status sequence duplicates another event")
    emissions.add(status_sequence)
    require(len(emissions) == RECORDS + RECORDS * 2 + 1, "event sequence set has duplicates")
    return {
        "terminal_event_count": RECORDS * 2 + 1,
        "source_status_event_count": 1,
        "source_status_identity_without_sequence": {
            key: item for key, item in status_identity.items() if key != "sequence"
        },
        "consumers": normalized_consumers,
    }


def verify_sample_report(raw_path: Path, attempt_dir: Path) -> dict[str, Any]:
    raw_bytes = raw_path.read_bytes()
    report = load_json(raw_path, "raw harness report")
    require(isinstance(report, dict) and set(report) == {
        "format", "method", "configuration", "identity", "warmups", "raw_samples_in_execution_order"
    }, "harness report schema changed")
    expected_config = expected_configuration()
    require(report["format"] == "execute.graph.transport_cpu_diagnostic.v1", "harness report format changed")
    require(report["method"] == METHOD, "harness measurement method differs from source oracle")
    require(report["configuration"] == expected_config, "harness configuration differs from source oracle")
    require(report["warmups"] == [], "unexpected inner warmups")
    fixture_bytes = expected_fixture()
    fixture_hash = sha256_bytes(fixture_bytes)
    fixture_path = raw_path.with_name("raw.input.jsonl").resolve()
    journal_path = raw_path.with_name("raw.journal.jsonl").resolve()
    require(report["identity"] == {
        "harness_sha256": HARNESS_SHA256,
        "source_manifest_sha256": expected_manifest_sha256(),
        "graph_template_sha256": expected_template_sha256(),
        "input_fixture_path": str(fixture_path),
        "input_fixture_bytes": len(fixture_bytes),
        "input_fixture_sha256": fixture_hash,
        "expected_payload_sha256_per_consumer": fixture_hash,
        "sample_journal_path": str(journal_path),
    }, "harness identity/manifest/configuration/fixture binding differs")
    require(fixture_path.read_bytes() == fixture_bytes, "input fixture differs from deterministic 4096-record stream")
    samples = report["raw_samples_in_execution_order"]
    require(isinstance(samples, list) and len(samples) == INNER_SAMPLES, "raw sample count changed")
    sample = samples[0]
    require(isinstance(sample, dict) and set(sample) == {"sample_index", "duration_ns", "evidence"},
            "sample schema changed")
    require(exact_int(sample["sample_index"], "sample index") == 0, "sample index changed")
    duration = exact_int(sample["duration_ns"], "GraphSession duration", 1)
    evidence = sample["evidence"]
    require(isinstance(evidence, dict) and set(evidence) == {
        "duration_ns", "resource_usage_delta", "dropped_messages", "launched_processes", "verification"
    }, "sample evidence schema changed")
    require(evidence["duration_ns"] == duration, "sample duration copies disagree")
    require(exact_int(evidence["dropped_messages"], "dropped events") == 0, "graph dropped events")
    require(exact_int(evidence["launched_processes"], "launched processes") == 1, "process count differs from one cat")
    usage = evidence["resource_usage_delta"]
    require(isinstance(usage, dict) and set(usage) == {"process", "reaped_children"}, "rusage scopes changed")
    for scope, counters in usage.items():
        require(isinstance(counters, dict) and set(counters) == set(COUNTERS), f"{scope} rusage fields changed")
        for name, number in counters.items():
            exact_int(number, f"{scope}.{name}")
    normalized = validate_verification(evidence["verification"], fixture_hash)

    journal_bytes = journal_path.read_bytes()
    require(journal_bytes.endswith(b"\n"), "journal lacks final newline")
    rows = [json.loads(line, object_pairs_hook=_unique_object, parse_constant=_reject_constant)
            for line in journal_bytes.splitlines()]
    require(len(rows) == 2, "journal must contain preparation and one sample row")
    require(rows[0] == {
        "status": "prepared", "planned_warmups": 0, "planned_samples": 1,
        "record_count": RECORDS, "input_fixture_sha256": fixture_hash,
    }, "journal preparation record changed")
    expected_journal_row = {
        "phase": "sample",
        "index": 0,
        "status": "ok",
        "duration_ns": duration,
        "resource_usage_delta": usage,
        "dropped_messages": 0,
        "launched_processes": 1,
        "input_fixture_sha256": fixture_hash,
        "verification_sha256": sha256_bytes(canonical_json(evidence["verification"])),
    }
    require(rows[1] == expected_journal_row, "journal sample row differs from raw report")
    return {
        "duration_ns": duration,
        "usage": usage,
        "normalized_verification": normalized,
        "normalized_verification_sha256": sha256_bytes(canonical_json(normalized)),
        "raw_sha256": sha256_bytes(raw_bytes),
        "fixture_sha256": sha256_bytes(fixture_path.read_bytes()),
        "journal_sha256": sha256_bytes(journal_bytes),
        "fixture_bytes": len(fixture_bytes),
        "journal_rows": len(rows),
    }


def ratio_summary(pairs: list[dict[str, Any]], metric_key: str) -> dict[str, Any]:
    baseline = [pair["baseline"] for pair in pairs]
    candidate = [pair["candidate"] for pair in pairs]
    deltas = [right - left for left, right in zip(baseline, candidate)]
    ratios = [right / left if left else None for left, right in zip(baseline, candidate)]
    valid_ratios = [ratio for ratio in ratios if ratio is not None]
    lower = [pair["global_pair_index"] for pair, delta in zip(pairs, deltas) if delta < 0]
    higher = [pair["global_pair_index"] for pair, delta in zip(pairs, deltas) if delta > 0]
    ties = [pair["global_pair_index"] for pair, delta in zip(pairs, deltas) if delta == 0]
    result: dict[str, Any] = {
        "baseline_arm_median": statistics.median(baseline),
        "candidate_arm_median": statistics.median(candidate),
        "median_paired_delta_candidate_minus_baseline": statistics.median(deltas),
        "median_paired_ratio_candidate_over_baseline": statistics.median(valid_ratios) if valid_ratios else None,
        "candidate_lower_count": len(lower),
        "candidate_higher_count": len(higher),
        "ties": len(ties),
        "candidate_lower_pair_indices": lower,
        "candidate_higher_pair_indices": higher,
        "tie_pair_indices": ties,
        "per_pair": [
            {
                "global_pair_index": pair["global_pair_index"],
                "order": pair["order"],
                "baseline": left,
                "candidate": right,
                "candidate_minus_baseline": right - left,
                "candidate_over_baseline": (right / left) if left else None,
            }
            for pair, left, right in zip(pairs, baseline, candidate)
        ],
    }
    by_order = {}
    for order in ("AB", "BA"):
        order_pairs = [pair for pair in pairs if pair["order_code"] == order]
        order_baseline = [pair["baseline"] for pair in order_pairs]
        order_candidate = [pair["candidate"] for pair in order_pairs]
        order_deltas = [right - left for left, right in zip(order_baseline, order_candidate)]
        order_ratios = [right / left for left, right in zip(order_baseline, order_candidate) if left]
        by_order[order] = {
            "pair_count": len(order_pairs),
            "baseline_arm_median": statistics.median(order_baseline) if order_baseline else None,
            "candidate_arm_median": statistics.median(order_candidate) if order_candidate else None,
            "median_paired_delta_candidate_minus_baseline": statistics.median(order_deltas) if order_deltas else None,
            "median_paired_ratio_candidate_over_baseline": statistics.median(order_ratios) if order_ratios else None,
            "candidate_lower_count": sum(delta < 0 for delta in order_deltas),
            "candidate_higher_count": sum(delta > 0 for delta in order_deltas),
            "ties": sum(delta == 0 for delta in order_deltas),
        }
    result["order_effects"] = by_order
    return result


def audit(report_path: Path) -> dict[str, Any]:
    report_path = report_path.expanduser().resolve(strict=True)
    report = load_json(report_path, "paired run report")
    require(report.get("format") == "execute.graph.transport_cpu_diagnostic.paired_run.v1", "paired report format changed")
    prereg_path = Path(report["preregistration_path"]).resolve(strict=True)
    require(sha256_file(prereg_path) == report["preregistration_sha256"], "preregistration hash differs from run report")
    prereg = load_json(prereg_path, "paired preregistration", 16 * 1024 * 1024)
    require(report["protocol"] == prereg["protocol"], "run protocol differs from preregistration")
    require(prereg["protocol"]["baseline_commit"] == BASELINE_COMMIT, "frozen commit differs")
    require(prereg["protocol"]["harness_sha256"] == HARNESS_SHA256, "protocol harness pin differs")
    require(prereg["protocol"]["workload"]["record_count"] == RECORDS, "protocol record count differs")
    require(prereg["protocol"]["schedule"] == expected_schedule(), "frozen pair schedule differs")
    build_binding = verify_build_binding(prereg)
    inputs = prereg["inputs"]
    environment = inputs["baseline"]["build_metadata"]["selected_build_environment"]
    environment_sha = sha256_bytes(json.dumps(environment, sort_keys=True, separators=(",", ":"), ensure_ascii=True).encode("ascii"))
    require(report["runner_base_environment_sha256"] == environment_sha, "run environment hash differs from build receipt")
    require(report["preregistration_sha256"] == sha256_file(prereg_path), "outer report preregistration hash changed")
    require(report["initial_identity"]["baseline"]["stable"] is True, "baseline initial identity unstable")
    require(report["initial_identity"]["candidate"]["stable"] is True, "candidate initial identity unstable")
    require(report["post_run_identity"]["baseline"]["stable"] is True, "baseline final identity unstable")
    require(report["post_run_identity"]["candidate"]["stable"] is True, "candidate final identity unstable")
    for slot in ("initial_identity", "post_run_identity"):
        for side in ("baseline", "candidate"):
            snapshot = report[slot][side]
            identity = inputs[side]
            require(snapshot["commit"] == identity["commit"], f"{slot}.{side} commit differs")
            require(snapshot["transport_sha256"] == identity["transport_sha256"], f"{slot}.{side} transport differs")
            require(snapshot["source_fingerprint"] == identity["source_fingerprint"], f"{slot}.{side} source fingerprint differs")
            require(snapshot["harness_sha256"] == HARNESS_SHA256, f"{slot}.{side} harness differs")
            require(snapshot["binary_sha256"] == identity["binary_sha256"], f"{slot}.{side} binary differs")
            require(snapshot["receipt_sha256"] == identity["receipt_sha256"], f"{slot}.{side} receipt differs")

    expected_fixture_hash = sha256_bytes(expected_fixture())
    host_observation_path = EVIDENCE / "host-observations-post-run.json"
    host_observation = load_json(host_observation_path, "post-run host observation")
    protocol_schedule = expected_schedule()
    require(report["status"] == "complete", "paired run is not complete")
    require(report["planned_pair_count"] == 15 and report["planned_attempt_count"] == 30, "planned count changed")
    require(report["attempt_count"] == 30 and report["successful_attempt_count"] == 30, "not all 30 attempts succeeded")
    require(report["failed_attempt_count"] == 0 and report["unattempted_attempt_count"] == 0, "run contains failed or unattempted slots")
    require(report["losses"] == [] and report["incomplete_pairs"] == [] and report["unattempted_slots"] == [],
            "run has losses, incomplete pairs, or unattempted slots")
    require(len(report["warmup_pairs"]) == WARMUP_PAIRS and len(report["measured_pairs"]) == MEASURED_PAIRS,
            "warmup/measured pair counts changed")
    all_pairs = report["warmup_pairs"] + report["measured_pairs"]
    require(len(all_pairs) == len(protocol_schedule), "pair report count differs from schedule")
    attempts_by_id: dict[str, dict[str, Any]] = {}
    raw_audit: dict[str, dict[str, Any]] = {}
    for attempt_index, attempt in enumerate(report["attempts"]):
        schedule_index, position = divmod(attempt_index, 2)
        schedule = protocol_schedule[schedule_index]
        side = schedule["order"][position]
        attempt_id = schedule["attempt_ids"][position]
        require(attempt["attempt_id"] == attempt_id and attempt["side"] == side, "attempt order/identity differs from schedule")
        require(attempt["global_pair_index"] == schedule_index and attempt["phase"] == schedule["phase"],
                "attempt phase/index differs from schedule")
        require(attempt["successful"] is True and attempt["returncode"] == 0, f"{attempt_id} did not succeed")
        require(attempt["timed_out"] is False and attempt["cleanup_confirmed"] is True, f"{attempt_id} lifecycle changed")
        require(attempt.get("execution_error") is None and attempt.get("validation_error") is None,
                f"{attempt_id} contains an execution/validation error")
        identity = inputs[side]
        attempt_dir = Path(attempt["attempt_dir"]).resolve(strict=True)
        require(attempt_dir == report_path.parent / "attempts" / attempt_id, f"{attempt_id} directory differs")
        started_path = attempt_dir / "started.json"
        finished_path = attempt_dir / "finished.json"
        started = load_json(started_path, f"{attempt_id} started record")
        finished = load_json(finished_path, f"{attempt_id} finished record")
        require(finished == attempt, f"{attempt_id} finished record differs from outer report")
        require(started["attempt_id"] == attempt_id and started["side"] == side, f"{attempt_id} start record identity differs")
        require(started["command"] == [identity["saved_binary"], "--exact", "bounded_native_stream_fanout_records_process_cpu", "--ignored", "--nocapture", "--test-threads=1"],
                f"{attempt_id} invoked a different binary or test")
        expected_overrides = {
            "EXECUTE_GRAPH_TRANSPORT_CPU_REPORT": str((attempt_dir / "raw.json").resolve()),
            "EXECUTE_GRAPH_TRANSPORT_CPU_WARMUPS": "0",
            "EXECUTE_GRAPH_TRANSPORT_CPU_SAMPLES": "1",
            "LC_ALL": "C",
            "TZ": "UTC",
        }
        require(started["environment"] == expected_overrides, f"{attempt_id} invocation overrides changed")
        require(started["runner_base_environment_sha256"] == environment_sha, f"{attempt_id} environment binding differs")
        for field in ("identity_before", "identity_after"):
            snapshot = attempt[field]
            require(snapshot["stable"] is True, f"{attempt_id} {field} is unstable")
            require(snapshot["commit"] == identity["commit"] and snapshot["transport_sha256"] == identity["transport_sha256"],
                    f"{attempt_id} {field} source differs")
            require(snapshot["source_fingerprint"] == identity["source_fingerprint"], f"{attempt_id} source fingerprint differs")
            require(snapshot["harness_sha256"] == HARNESS_SHA256, f"{attempt_id} harness pin differs")
            require(snapshot["binary_sha256"] == identity["binary_sha256"] and snapshot["receipt_sha256"] == identity["receipt_sha256"],
                    f"{attempt_id} binary/receipt pin differs")
        raw_path = attempt_dir / "raw.json"
        fixture_path = attempt_dir / "raw.input.jsonl"
        journal_path = attempt_dir / "raw.journal.jsonl"
        stdout_path = attempt_dir / "stdout.bin"
        stderr_path = attempt_dir / "stderr.bin"
        require(attempt["raw_report_present"] and attempt["raw_report_sha256"] == sha256_file(raw_path),
                f"{attempt_id} raw report pin differs")
        require(attempt["input_fixture_present"] and attempt["input_fixture_sha256"] == sha256_file(fixture_path),
                f"{attempt_id} fixture pin differs")
        require(attempt["sample_journal_present"] and attempt["sample_journal_sha256"] == sha256_file(journal_path),
                f"{attempt_id} journal pin differs")
        require(attempt["stdout_sha256"] == sha256_file(stdout_path) and attempt["stderr_sha256"] == sha256_file(stderr_path),
                f"{attempt_id} process output hashes differ")
        stdout = stdout_path.read_bytes().decode("utf-8", errors="replace")
        require("running 1 test" in stdout and "test bounded_native_stream_fanout_records_process_cpu ... ok" in stdout,
                f"{attempt_id} test output is not a single passing harness invocation")
        require(re.search(r"test result: ok\. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in [0-9.]+s", stdout),
                f"{attempt_id} test result line differs")
        summary = verify_sample_report(raw_path, attempt_dir)
        require(attempt["raw_duration_ns"] == [summary["duration_ns"]], f"{attempt_id} duration summary differs from raw")
        require(attempt["raw_resource_usage_delta"] == [summary["usage"]], f"{attempt_id} CPU summary differs from raw")
        require(attempt["median_duration_ns"] == summary["duration_ns"], f"{attempt_id} median differs")
        require(attempt["oracle"]["normalized_sample_oracle_sha256"] == [summary["normalized_verification_sha256"]],
                f"{attempt_id} normalized oracle digest differs")
        attempts_by_id[attempt_id] = attempt
        raw_audit[attempt_id] = summary

    per_pair = []
    for global_index, pair in enumerate(all_pairs):
        schedule = protocol_schedule[global_index]
        require(pair["global_pair_index"] == global_index and pair["order"] == schedule["order"], "pair order/index differs")
        require(pair["phase"] == schedule["phase"] and pair["phase_pair_index"] == schedule["phase_pair_index"],
                "pair phase differs")
        require(pair["valid_for_timing_comparison"] is True and pair["full_oracle_equal"] is True,
                "pair is not valid with full equal oracle")
        baseline_id = schedule["attempt_ids"][schedule["order"].index("baseline")]
        candidate_id = schedule["attempt_ids"][schedule["order"].index("candidate")]
        baseline_attempt = attempts_by_id[baseline_id]
        candidate_attempt = attempts_by_id[candidate_id]
        require(raw_audit[baseline_id]["normalized_verification"] == raw_audit[candidate_id]["normalized_verification"],
                f"pair {global_index} full normalized oracles differ")
        baseline_duration = raw_audit[baseline_id]["duration_ns"]
        candidate_duration = raw_audit[candidate_id]["duration_ns"]
        measured = global_index >= WARMUP_PAIRS
        pair_record: dict[str, Any] = {
            "global_pair_index": global_index,
            "phase": schedule["phase"],
            "order": schedule["order"],
            "order_code": "AB" if schedule["order"] == ["baseline", "candidate"] else "BA",
            "baseline_attempt_id": baseline_id,
            "candidate_attempt_id": candidate_id,
            "measured": measured,
            "wall": {
                "baseline_ns": baseline_duration,
                "candidate_ns": candidate_duration,
                "candidate_minus_baseline_ns": candidate_duration - baseline_duration,
                "candidate_over_baseline": candidate_duration / baseline_duration,
            },
            "cpu": {},
        }
        for scope in ("process", "reaped_children"):
            base_usage = raw_audit[baseline_id]["usage"][scope]
            candidate_usage = raw_audit[candidate_id]["usage"][scope]
            base_total = base_usage["user_cpu_us"] + base_usage["system_cpu_us"]
            candidate_total = candidate_usage["user_cpu_us"] + candidate_usage["system_cpu_us"]
            pair_record["cpu"][scope] = {
                "baseline_user_us": base_usage["user_cpu_us"],
                "candidate_user_us": candidate_usage["user_cpu_us"],
                "user_delta_us": candidate_usage["user_cpu_us"] - base_usage["user_cpu_us"],
                "baseline_system_us": base_usage["system_cpu_us"],
                "candidate_system_us": candidate_usage["system_cpu_us"],
                "system_delta_us": candidate_usage["system_cpu_us"] - base_usage["system_cpu_us"],
                "baseline_user_plus_system_us": base_total,
                "candidate_user_plus_system_us": candidate_total,
                "candidate_minus_baseline_user_plus_system_us": candidate_total - base_total,
                "candidate_over_baseline_user_plus_system": candidate_total / base_total if base_total else None,
                "baseline_voluntary_context_switches": base_usage["voluntary_context_switches"],
                "candidate_voluntary_context_switches": candidate_usage["voluntary_context_switches"],
                "voluntary_context_switches_delta": (
                    candidate_usage["voluntary_context_switches"] - base_usage["voluntary_context_switches"]
                ),
                "baseline_involuntary_context_switches": base_usage["involuntary_context_switches"],
                "candidate_involuntary_context_switches": candidate_usage["involuntary_context_switches"],
                "involuntary_context_switches_delta": (
                    candidate_usage["involuntary_context_switches"] - base_usage["involuntary_context_switches"]
                ),
            }
        per_pair.append(pair_record)
        if global_index in range(WARMUP_PAIRS, WARMUP_PAIRS + MEASURED_PAIRS):
            actual = report["measured_pairs"][global_index - WARMUP_PAIRS]
            require(actual["baseline_attempt_id"] == baseline_id and actual["candidate_attempt_id"] == candidate_id,
                    "reported pair attempt references differ")
            require(actual["cases"]["whole_graph_session"]["baseline_raw_duration_ns"] == [baseline_duration],
                    "reported baseline wall sample differs")
            require(actual["cases"]["whole_graph_session"]["candidate_raw_duration_ns"] == [candidate_duration],
                    "reported candidate wall sample differs")
            require(actual["cases"]["whole_graph_session"]["candidate_minus_baseline_ns"] == candidate_duration - baseline_duration,
                    "reported wall delta differs")
            for scope in ("process", "reaped_children"):
                for metric in COUNTERS:
                    base_value = raw_audit[baseline_id]["usage"][scope][metric]
                    candidate_value = raw_audit[candidate_id]["usage"][scope][metric]
                    reported = actual["cases"]["resource_usage"][scope][metric]
                    require(reported["baseline"] == base_value and reported["candidate"] == candidate_value,
                            f"reported {scope}.{metric} values differ")
                    require(reported["candidate_minus_baseline"] == candidate_value - base_value,
                            f"reported {scope}.{metric} delta differs")

    measured_pairs = [pair for pair in per_pair if pair["measured"]]
    def summary_metric(key: str, extractor: Any) -> dict[str, Any]:
        rows = [{"global_pair_index": pair["global_pair_index"], "order_code": pair["order_code"],
                 "order": pair["order"],
                 "baseline": extractor(pair, "baseline"), "candidate": extractor(pair, "candidate")} for pair in measured_pairs]
        return ratio_summary(rows, key)
    wall_summary = summary_metric("value", lambda pair, side: pair["wall"][f"{side}_ns"])
    cpu_summaries = {}
    cpu_component_summaries: dict[str, dict[str, Any]] = {}
    for scope in ("process", "reaped_children"):
        cpu_summaries[scope] = summary_metric(
            "value", lambda pair, side, scope=scope: pair["cpu"][scope][f"{side}_user_plus_system_us"]
        )
        cpu_component_summaries[scope] = {
            "user_cpu_us": summary_metric(
                "value", lambda pair, side, scope=scope: pair["cpu"][scope][f"{side}_user_us"]
            ),
            "system_cpu_us": summary_metric(
                "value", lambda pair, side, scope=scope: pair["cpu"][scope][f"{side}_system_us"]
            ),
        }

    reported_summary = report["paired_summary"]
    require(reported_summary["valid_measured_pair_count"] == MEASURED_PAIRS, "runner summary measured pair count differs")
    expected_deltas = [item["wall"]["candidate_minus_baseline_ns"] for item in measured_pairs]
    require(reported_summary["median_paired_delta_ns"] == statistics.median(expected_deltas), "runner wall summary median differs")
    expected_loses = [item["global_pair_index"] for item in measured_pairs if item["wall"]["candidate_minus_baseline_ns"] > 0]
    expected_wins = [item["global_pair_index"] for item in measured_pairs if item["wall"]["candidate_minus_baseline_ns"] < 0]
    require(reported_summary["candidate_loss_pair_indices"] == expected_loses, "runner did not preserve wall-time regressions")
    require(reported_summary["candidate_improvement_pair_indices"] == expected_wins, "runner wall improvements differ")
    for scope in ("process", "reaped_children"):
        for field in COUNTERS:
            expected = [
                item["cases"]["resource_usage"][scope][field]["candidate_minus_baseline"]
                for item in report["measured_pairs"]
            ]
            summary = reported_summary["resource_metrics"][scope][field]
            require(summary["valid_measured_pair_count"] == MEASURED_PAIRS, f"runner {scope}.{field} pair count differs")
            require(summary["median_paired_delta"] == statistics.median(expected), f"runner {scope}.{field} median differs")

    return {
        "schema": "execute.graph.collector_cpu_independent_audit/1",
        "status": "passed",
        "audit_source_sha256": sha256_file(Path(__file__).resolve()),
        "report_path": str(report_path),
        "report_sha256": sha256_file(report_path),
        "preregistration_path": str(prereg_path),
        "preregistration_sha256": sha256_file(prereg_path),
        "build_environment_receipt_sha256": build_binding["environment_receipt_sha256"],
        "selected_build_environment": build_binding["selected_environment"],
        "post_run_host_observation": {
            "path": str(host_observation_path),
            "sha256": sha256_file(host_observation_path),
            "utc": host_observation["utc"],
            "phase": host_observation["phase"],
            "observation": host_observation["observation"],
            "interpretation": (
                "This is a post-run process snapshot, not an in-timer profile. It indicates other desktop Codex and "
                "Chromium activity on the shared host; elapsed lifetime may overlap some late pairs, but exact overlap "
                "and attributable CPU cannot be inferred. No pairs were excluded or retried."
            ),
        },
        "configuration": {
            "record_count": RECORDS,
            "events_per_terminal_run": RECORDS * 2 + 1,
            "fixture_sha256": expected_fixture_hash,
            "fixture_bytes": len(expected_fixture()),
            "one_native_cat_process_per_attempt": True,
            "zero_drops": True,
            "natural_eof_assertion_in_source": True,
            "full_metadata_sequence_order_and_payload_hash_checks": True,
        },
        "attempts": {
            "planned": 30,
            "completed": len(report["attempts"]),
            "successful": report["successful_attempt_count"],
            "failed": report["failed_attempt_count"],
            "unattempted": report["unattempted_attempt_count"],
            "warmup_pairs": WARMUP_PAIRS,
            "measured_pairs": MEASURED_PAIRS,
            "raw_file_hashes": {
                attempt_id: {
                    "raw_sha256": entry["raw_sha256"],
                    "fixture_sha256": entry["fixture_sha256"],
                    "journal_sha256": entry["journal_sha256"],
                }
                for attempt_id, entry in raw_audit.items()
            },
        },
        "measured_pair_results": per_pair[WARMUP_PAIRS:],
        "wall_time": wall_summary,
        "cpu_user_plus_system": cpu_summaries,
        "cpu_user_and_system_components": cpu_component_summaries,
        "interpretation_limit": (
            "Independent audit of this fixed graph/workload on the measured host only; RUSAGE_SELF is process-wide "
            "including threads, RUSAGE_CHILDREN covers reaped children, and the 12 pairs do not establish sustained, "
            "portable, whole-application, energy, or capacity behavior. AB/BA grouping is descriptive, not causal."
        ),
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--report", type=Path, default=DEFAULT_REPORT)
    parser.add_argument("--output", type=Path, default=AUDIT_OUTPUT)
    args = parser.parse_args()
    result = audit(args.report)
    output = args.output.expanduser().absolute()
    with output.open("x", encoding="utf-8") as stream:
        json.dump(result, stream, sort_keys=True, indent=2, ensure_ascii=False, allow_nan=False)
        stream.write("\n")
        stream.flush()
        os.fsync(stream.fileno())
    print(json.dumps({"audit": str(output), "sha256": sha256_file(output), "status": result["status"]}, sort_keys=True))
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (AuditError, OSError, ValueError, KeyError, TypeError, subprocess.SubprocessError) as error:
        raise SystemExit(f"collector CPU audit failed: {type(error).__name__}: {error}")
