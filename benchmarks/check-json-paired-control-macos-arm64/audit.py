#!/usr/bin/env python3
"""Offline verifier and independent summary generator for this retained study."""

from __future__ import annotations

import argparse
import base64
import hashlib
import json
import lzma
import math
import statistics
import sys
from pathlib import Path


ROOT = Path(__file__).resolve().parent
RUNS = (
    ("initial", "initial.json.xz", "63e2e10227541f97eada1d4d79c391a0ac92c1afb16e08d94900a8d91e466f4b"),
    ("repeat-2", "repeat-2.json.xz", "022c790c3c0c05f43a6f7479d3280c6a14cad3092e2ad3b60bc6689e53e68310"),
    ("repeat-3", "repeat-3.json.xz", "ca04d8b04da8207790b3f7ce775de316f03a1b2cf4f2319da95bbf2317f4dd7b"),
)
CELLS = (
    "001_steps-full75-json",
    "001_steps-full75-ordinary",
    "001_steps-inert-cat-json",
    "001_steps-inert-cat-ordinary",
    "100_steps-full75-json",
    "100_steps-full75-ordinary",
    "100_steps-inert-cat-json",
    "100_steps-inert-cat-ordinary",
)
SIDES = ("baseline", "candidate")
EMPTY_SHA256 = hashlib.sha256(b"").hexdigest()


class AuditError(Exception):
    pass


def require(condition: bool, message: str) -> None:
    if not condition:
        raise AuditError(message)


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def canonical_json(value: object) -> bytes:
    return (json.dumps(value, indent=2, sort_keys=True, ensure_ascii=False) + "\n").encode()


def read_json(path: Path) -> dict:
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except Exception as exc:
        raise AuditError(f"cannot read JSON {path.relative_to(ROOT)}: {exc}") from exc


def verify_checksums() -> None:
    manifest = ROOT / "SHA256SUMS"
    if not manifest.exists():
        return
    declared: dict[str, str] = {}
    for line in manifest.read_text(encoding="ascii").splitlines():
        if not line.strip():
            continue
        digest, name = line.split("  ", 1)
        require(len(digest) == 64 and name not in declared, f"bad/duplicate checksum row: {line}")
        declared[name] = digest
    actual_names = {
        path.relative_to(ROOT).as_posix()
        for path in ROOT.rglob("*")
        if path.is_file() and path != manifest
    }
    require(set(declared) == actual_names, "SHA256SUMS file set does not match the bundle")
    for name, expected in declared.items():
        actual = sha256((ROOT / name).read_bytes())
        require(actual == expected, f"checksum mismatch: {name}")


def percentile95(values: list[int]) -> int:
    ordered = sorted(values)
    return ordered[math.ceil(0.95 * len(ordered)) - 1]


def stats(values: list[int]) -> dict:
    return {
        "sample_count": len(values),
        "minimum_ns": min(values),
        "median_ns": statistics.median(values),
        "p95_nearest_rank_ns": percentile95(values),
        "maximum_ns": max(values),
    }


def close_to_recorded(actual: dict, recorded: dict, label: str) -> None:
    require(actual == recorded, f"independent statistic mismatch for {label}: {actual} != {recorded}")


def expected_json_stdout(raw: dict) -> bytes:
    command = raw["command"]
    require("--json" in command, "expected JSON command lacks --json")
    path_index = command.index("check") + 1
    requested_path = command[path_index]
    expected = {
        "diagnostics": [],
        "ok": True,
        "path": requested_path,
        "path_bytes": list(requested_path.encode("utf-8")),
        "protocol": "execute.check/1",
        "validation": "static",
    }
    return json.dumps(expected, indent=2).encode("utf-8") + b"\n"


def decode_invocation(record: dict, cell: str) -> tuple[bytes, bytes]:
    require(record["state"] == "completed" and record["output_available"], f"{cell}: incomplete invocation")
    require(record["status"] == 0, f"{cell}: nonzero status for {record['phase']} {record['side']}")
    require(record["elapsed_ns"] is not None and record["elapsed_ns"] > 0, f"{cell}: missing elapsed time")
    stdout = base64.b64decode(record["stdout_base64"], validate=True)
    stderr = base64.b64decode(record["stderr_base64"], validate=True)
    require(len(stdout) == record["stdout_bytes"] and sha256(stdout) == record["stdout_sha256"], f"{cell}: stdout capture metadata mismatch")
    require(len(stderr) == record["stderr_bytes"] and sha256(stderr) == record["stderr_sha256"], f"{cell}: stderr capture metadata mismatch")
    require(stderr == b"" and record["stderr_sha256"] == EMPTY_SHA256, f"{cell}: stderr was not empty")
    command = record["command"]
    require(command[1:3] == ["check", "programs/check.program"], f"{cell}: unexpected command {command}")
    require(record["timeout_seconds"] == 30.0, f"{cell}: invocation timeout differs from registration")
    if "--json" in command:
        require(stdout == expected_json_stdout(record), f"{cell}: JSON output does not match the independent execute.check/1 oracle")
    else:
        require(stdout == b"", f"{cell}: ordinary-check stdout must be empty")
    return stdout, stderr


def verify_observation_matches_raw(observation: dict, record: dict, cell: str, label: str) -> None:
    stdout, stderr = decode_invocation(record, cell)
    require(observation["side"] == record["side"], f"{cell}: {label} side mismatch")
    require(observation["status"] == record["status"], f"{cell}: {label} status mismatch")
    require(observation["elapsed_ns"] == record["elapsed_ns"], f"{cell}: {label} timing mismatch")
    for stream, content in (("stdout", stdout), ("stderr", stderr)):
        require(observation[f"{stream}_bytes"] == len(content), f"{cell}: {label} {stream} length mismatch")
        require(observation[f"{stream}_sha256"] == sha256(content), f"{cell}: {label} {stream} hash mismatch")


def verify_cell(cell: str, workload: dict, report: dict) -> dict:
    require(workload["program_steps"] in (1, 100), f"{cell}: unexpected program length")
    require(len(workload["raw_invocations"]) == 108, f"{cell}: expected 108 raw invocation records")
    require(len(workload["raw_paired_samples"]) == 96, f"{cell}: expected 96 measured rows")
    require(len(workload["warmup_observations"]) == 10, f"{cell}: expected 10 warmup rows")
    require(set(workload["preflight"]) == set(SIDES), f"{cell}: missing preflight side")

    invocation_lookup = {}
    phase_counts = {"preflight": {side: 0 for side in SIDES}, "warmup": {side: 0 for side in SIDES}, "sample": {side: 0 for side in SIDES}}
    for record in workload["raw_invocations"]:
        phase, side = record["phase"], record["side"]
        require(phase in phase_counts and side in SIDES, f"{cell}: unknown phase/side")
        key = (phase, record.get("round") if phase == "warmup" else record.get("pair"), side)
        require(key not in invocation_lookup, f"{cell}: duplicate invocation {key}")
        invocation_lookup[key] = record
        phase_counts[phase][side] += 1
        decode_invocation(record, cell)
        manifest_index = record["command"].index("--manifest-dir") + 1
        expected_catalog_suffix = "/catalogs/full75" if workload["catalog"] == "full75" else "/catalogs/inert-one-unused-cat"
        require(record["command"][manifest_index].endswith(expected_catalog_suffix), f"{cell}: manifest catalog mismatch")

    require(phase_counts == {
        "preflight": {"baseline": 1, "candidate": 1},
        "warmup": {"baseline": 5, "candidate": 5},
        "sample": {"baseline": 48, "candidate": 48},
    }, f"{cell}: phase counts do not match the registered design")
    for side in SIDES:
        require(workload["preflight"][side]["status"] == 0, f"{cell}: preflight failed")
        require(workload["preflight"][side]["stderr_bytes"] == 0, f"{cell}: preflight stderr not empty")

    expected_program_hash = report["fingerprints"]["before"]["fixture_data"]["programs"][str(workload["program_steps"])]["sha256"]
    expected_effects = {"programs/": "directory", "programs/check.program": expected_program_hash}
    require("forbidden" not in workload["raw_invocations"][0]["command"], f"{cell}: forbidden output path unexpectedly appears as command token")
    preflight_bytes = {}
    for side in SIDES:
        record = invocation_lookup[("preflight", None, side)]
        stdout, stderr = decode_invocation(record, cell)
        preflight = workload["preflight"][side]
        require(preflight["effects"] == expected_effects, f"{cell}: preflight effects show mutation/output")
        require(preflight["stdout_bytes"] == len(stdout) and preflight["stdout_sha256"] == sha256(stdout), f"{cell}: preflight stdout metadata mismatch")
        require(preflight["stderr_bytes"] == len(stderr) and preflight["stderr_sha256"] == sha256(stderr), f"{cell}: preflight stderr metadata mismatch")
        preflight_bytes[side] = (stdout, stderr)
    require(preflight_bytes["baseline"] == preflight_bytes["candidate"], f"{cell}: preflight output bytes differ between sides")

    expected_order = [
        {"pair": pair, "order": ["baseline", "candidate"] if pair % 2 else ["candidate", "baseline"]}
        for pair in range(1, 49)
    ]
    require(workload["paired_order"] == expected_order, f"{cell}: paired order does not match alternating design")
    expected_warmup_order = [
        {"round": round_number, "order": ["baseline", "candidate"] if round_number % 2 else ["candidate", "baseline"]}
        for round_number in range(1, 6)
    ]
    require(workload["warmup_order"] == expected_warmup_order, f"{cell}: warmup order does not match alternating design")

    samples_by_pair = {}
    for observation in workload["raw_paired_samples"]:
        pair, side = observation["pair"], observation["side"]
        samples_by_pair.setdefault(pair, {})[side] = observation
        raw = invocation_lookup[("sample", pair, side)]
        verify_observation_matches_raw(observation, raw, cell, "sample")
        require(observation["effects"] == expected_effects, f"{cell}: sample effects show mutation/output")
    deltas = []
    wins = 0
    for pair in range(1, 49):
        pair_rows = samples_by_pair.get(pair, {})
        require(set(pair_rows) == set(SIDES), f"{cell}: incomplete measured pair {pair}")
        base = pair_rows["baseline"]
        cand = pair_rows["candidate"]
        require(base["status"] == cand["status"] == 0, f"{cell}: paired statuses differ")
        base_bytes = decode_invocation(invocation_lookup[("sample", pair, "baseline")], cell)
        cand_bytes = decode_invocation(invocation_lookup[("sample", pair, "candidate")], cell)
        require(base_bytes == cand_bytes, f"{cell}: paired stdout/stderr bytes differ")
        require(base["effects"] == cand["effects"] == expected_effects, f"{cell}: paired filesystem effects differ")
        require(base["order_position"] != cand["order_position"], f"{cell}: pair lacks order positions")
        require([row["side"] for row in sorted((base, cand), key=lambda row: row["order_position"])] == expected_order[pair - 1]["order"], f"{cell}: sample ordering metadata mismatch")
        deltas.append(cand["elapsed_ns"] - base["elapsed_ns"])
        wins += cand["elapsed_ns"] < base["elapsed_ns"]

    warmups_by_round = {}
    for observation in workload["warmup_observations"]:
        round_number, side = observation["round"], observation["side"]
        warmups_by_round.setdefault(round_number, {})[side] = observation
        raw = invocation_lookup[("warmup", round_number, side)]
        verify_observation_matches_raw(observation, raw, cell, "warmup")
        require(observation["effects"] == expected_effects, f"{cell}: warmup effects show mutation/output")
    for round_number in range(1, 6):
        rows = warmups_by_round.get(round_number, {})
        require(set(rows) == set(SIDES), f"{cell}: incomplete warmup round")
        base, cand = rows["baseline"], rows["candidate"]
        base_bytes = decode_invocation(invocation_lookup[("warmup", round_number, "baseline")], cell)
        cand_bytes = decode_invocation(invocation_lookup[("warmup", round_number, "candidate")], cell)
        require(base["status"] == cand["status"] == 0 and base_bytes == cand_bytes, f"{cell}: warmup parity failed")
        require(base["effects"] == cand["effects"] == expected_effects, f"{cell}: warmup effects differ")

    stats_by_side = {
        side: stats([row["elapsed_ns"] for row in workload["raw_paired_samples"] if row["side"] == side])
        for side in SIDES
    }
    for side in SIDES:
        close_to_recorded(stats_by_side[side], workload["summary"][side], f"{cell}/{side}")
    close_to_recorded(stats(deltas), workload["paired_delta_summary"], f"{cell}/paired delta")
    return {
        "cell": cell,
        "catalog": workload["catalog"],
        "mode": workload["mode"],
        "program_steps": workload["program_steps"],
        "baseline": stats_by_side["baseline"],
        "candidate": stats_by_side["candidate"],
        "median_change_percent": (stats_by_side["candidate"]["median_ns"] / stats_by_side["baseline"]["median_ns"] - 1.0) * 100.0,
        "p95_change_percent": (stats_by_side["candidate"]["p95_nearest_rank_ns"] / stats_by_side["baseline"]["p95_nearest_rank_ns"] - 1.0) * 100.0,
        "candidate_faster_pairs": wins,
        "pairs": len(deltas),
    }


def verify_fixture_bundle(report: dict) -> None:
    data = report["fingerprints"]["before"]["fixture_data"]
    for catalog, entry in data["catalogs"].items():
        directory = ROOT / "control" / "fixtures" / "catalogs" / catalog
        files = sorted(path for path in directory.iterdir() if path.is_file())
        require(len(files) == entry["file_count"], f"fixture catalog count mismatch: {catalog}")
        for path in files:
            require(sha256(path.read_bytes()) == entry["files_sha256"].get(path.name), f"fixture hash mismatch: {catalog}/{path.name}")
    for steps, entry in data["programs"].items():
        path = ROOT / "control" / "fixtures" / "cases" / f"steps-{int(steps):03d}" / "programs" / "check.program"
        content = path.read_bytes()
        require(path.stat().st_size == entry["bytes"] and sha256(content) == entry["sha256"], f"program fixture mismatch: {steps} steps")
        lines = content.decode("utf-8").splitlines()
        expected_lines = ["accept path as text"] + [
            'run "execute-check-json-reuse-nonexistent-command" with arguments [path]'
        ] * int(steps) + ['then save to "forbidden"']
        require(lines == expected_lines, f"program fixture does not match its declared static-no-effects workload: {steps} steps")


def verify_artifact_pins(report: dict, source_pins: dict) -> None:
    before = report["fingerprints"]["before"]
    require(report["fingerprints"]["unchanged"] is True and before == report["fingerprints"]["after"], "source/artifact fingerprints changed during run")
    require(source_pins["fingerprints"] == before, "published source/artifact pins differ from raw report")
    require(sha256((ROOT / "control" / "driver.py").read_bytes()) == before["control_driver"]["sha256"], "measured driver hash mismatch")
    require(sha256((ROOT / "control" / "test_control.py").read_bytes()) == before["control_tests"]["sha256"], "measured test hash mismatch")
    require(sha256((ROOT / "control" / "harness" / "source_pair.py").read_bytes()) == before["source_pair_harness"]["sha256"], "source_pair harness hash mismatch")
    require(sha256(lzma.decompress((ROOT / "evidence" / "check-json-reuse-final.patch.xz").read_bytes())) == before["candidate_reviewed_patch"]["sha256"], "reviewed patch hash mismatch")
    require(before["binaries"]["baseline"]["sha256"] == "c1b39031fa4f7924093179c1aeb57fe92d01ad85e1ecb834b0d72968c309ed69", "baseline binary pin mismatch")
    require(before["binaries"]["candidate"]["sha256"] == "5a2d2d0eea72e7154f4cca26aaa7fa12e8329423917c2e22d45f346fe8b6a331", "candidate binary pin mismatch")


def verify_registration(reports: dict[str, dict]) -> None:
    registration = ROOT / "registration"
    amendment = read_json(registration / "premeasurement-amendment.json")
    fixed = read_json(registration / "fixed-replications.json")
    receipt = read_json(registration / "fixed-replications-receipt.json")
    review = read_json(registration / "adversarial-review.json")
    prereg_path = registration / "original-preregistration.json"
    require(sha256(prereg_path.read_bytes()) == amendment["preregistration"]["sha256_unchanged"], "original preregistration hash mismatch")
    require(amendment["current_files"]["driver"]["sha256"] == sha256((ROOT / "control/driver.py").read_bytes()), "amendment driver pin mismatch")
    require(amendment["current_files"]["test"]["sha256"] == sha256((ROOT / "control/test_control.py").read_bytes()), "amendment test pin mismatch")
    require(fixed["original_raw_sha256"] == sha256_bytes_for_run(reports["initial"]), "fixed-replication registration does not bind initial raw run")
    require(fixed["same_driver_sha256"] == amendment["current_files"]["driver"]["sha256"], "fixed runs use another driver")
    require(fixed["same_eight_cells_samples_warmups_oracles"] is True and fixed["additional_runs"] == 2, "fixed-replication design differs from registration")
    for archive_key, relative_path in (
        ("driver_archive", "control/history/driver-before-amendment.py"),
        ("test_archive", "control/history/tests-before-amendment.py"),
    ):
        expected = amendment["prior_files"][archive_key]["sha256"]
        require(sha256((ROOT / relative_path).read_bytes()) == expected, f"archived pre-amendment file mismatch: {archive_key}")
    receipt_by_label = {entry["label"]: entry for entry in receipt["runs"]}
    require(set(receipt_by_label) == {"repeat-2", "repeat-3"}, "fixed replication receipt labels mismatch")
    for label in ("repeat-2", "repeat-3"):
        expected = sha256_bytes_for_run(reports[label])
        require(receipt_by_label[label]["raw_sha256"] == expected and receipt_by_label[label]["exit_code"] == 0, f"receipt mismatch for {label}")
    review_reports = {Path(entry["path"]).name: entry["sha256"] for entry in review["raw_reports"]}
    for label, report in reports.items():
        expected_name = {
            "initial": "check-json-reuse-control-macos-arm64.raw.json",
            "repeat-2": "check-json-reuse-control-macos-arm64-repeat-2.raw.json",
            "repeat-3": "check-json-reuse-control-macos-arm64-repeat-3.raw.json",
        }[label]
        require(review_reports.get(expected_name) == sha256_bytes_for_run(report), f"adversarial review raw pin mismatch for {label}")


def sha256_bytes_for_run(report: dict) -> str:
    label = report["_bundle_run_label"]
    filename = dict((name, compressed) for name, compressed, _ in RUNS)[label]
    return sha256(lzma.decompress((ROOT / "raw" / filename).read_bytes()))


def build_summary() -> dict:
    reports: dict[str, dict] = {}
    report_bytes: dict[str, bytes] = {}
    for label, filename, expected_sha in RUNS:
        compressed = (ROOT / "raw" / filename).read_bytes()
        raw_bytes = lzma.decompress(compressed)
        require(sha256(raw_bytes) == expected_sha, f"decompressed raw report hash mismatch for {label}")
        report = json.loads(raw_bytes)
        report["_bundle_run_label"] = label
        require(report["status"] == "completed", f"{label}: report is not complete")
        require(set(report["workloads"]) == set(CELLS), f"{label}: workload cell set differs")
        require(set(report["completed_cells"]) == set(CELLS), f"{label}: completed cell set differs")
        reports[label] = report
        report_bytes[label] = raw_bytes

    first = reports["initial"]
    verify_fixture_bundle(first)
    source_pins = read_json(ROOT / "registration" / "source-artifact-pins.json")
    verify_artifact_pins(first, source_pins)
    for label, report in reports.items():
        require(report["fingerprints"]["unchanged"] is True, f"{label}: artifact fingerprints report mutation")
        require(report["fingerprints"]["before"] == first["fingerprints"]["before"], f"{label}: source/artifacts differ from initial run")
        require(report["fingerprints"]["after"] == first["fingerprints"]["after"], f"{label}: after fingerprints differ")
        require(report["environment"] == first["environment"], f"{label}: environment differs")
        require(report["provenance"] == first["provenance"], f"{label}: provenance differs")
        require(report["method"] == first["method"], f"{label}: collection method differs")
    verify_registration(reports)

    rows = []
    for label, _, _ in RUNS:
        report = reports[label]
        for cell in CELLS:
            rows.append({"run": label, **verify_cell(cell, report["workloads"][cell], report)})

    target = [row for row in rows if row["catalog"] == "full75" and row["mode"] == "check --json"]
    control_groups = {}
    for catalog, mode, key in (
        ("full75", "ordinary check", "full75_ordinary"),
        ("inert-cat", "check --json", "inert_catalog_json"),
        ("inert-cat", "ordinary check", "inert_catalog_ordinary"),
    ):
        selected = [row["median_change_percent"] for row in rows if row["catalog"] == catalog and row["mode"] == mode]
        control_groups[key] = {"observations": len(selected), "minimum_median_change_percent": min(selected), "maximum_median_change_percent": max(selected)}

    measured_processes = len(reports) * len(CELLS) * 48 * 2
    warmups = len(reports) * len(CELLS) * 5 * 2
    preflights = len(reports) * len(CELLS) * 2
    require((measured_processes, warmups, preflights) == (2304, 240, 48), "study invocation totals mismatch")
    wins = sum(row["candidate_faster_pairs"] for row in target)
    require(wins == 271, f"target paired win count mismatch: {wins}")
    lower_p95 = sum(row["candidate"]["p95_nearest_rank_ns"] < row["baseline"]["p95_nearest_rank_ns"] for row in target)
    require(lower_p95 == 5, f"target lower-p95 count mismatch: {lower_p95}")
    initial_one = next(row for row in target if row["run"] == "initial" and row["program_steps"] == 1)
    require(initial_one["baseline"]["p95_nearest_rank_ns"] == 24_715_792 and initial_one["candidate"]["p95_nearest_rank_ns"] == 45_004_125 and initial_one["candidate"]["maximum_ns"] == 261_007_042, "initial p95/max regression changed")

    review = read_json(ROOT / "registration" / "adversarial-review.json")
    review_totals = review["recomputation"]["total_across_three_runs"]
    require(review_totals == {"measured_processes": measured_processes, "excluded_warmup_processes": warmups, "preflight_processes": preflights, "total_process_invocations": measured_processes + warmups + preflights}, "adversarial review invocation totals disagree with raw evidence")
    reviewed_rows = review["full75_json_medians_and_p95_ms"]
    require(len(reviewed_rows) == len(target), "adversarial review does not contain six target cells")
    for computed, reviewed in zip(target, reviewed_rows):
        require(reviewed["run"] == {"initial": 1, "repeat-2": 2, "repeat-3": 3}[computed["run"]] and reviewed["steps"] == computed["program_steps"], "adversarial review target cell order differs")
        require(round(computed["baseline"]["median_ns"] / 1_000_000, 3) == reviewed["baseline_median"], "adversarial review baseline median differs")
        require(round(computed["candidate"]["median_ns"] / 1_000_000, 3) == reviewed["candidate_median"], "adversarial review candidate median differs")
        require(round(computed["median_change_percent"], 1) == reviewed["median_change_percent"], "adversarial review median change differs")
        require(round(computed["baseline"]["p95_nearest_rank_ns"] / 1_000_000, 3) == reviewed["baseline_p95"], "adversarial review baseline p95 differs")
        require(round(computed["candidate"]["p95_nearest_rank_ns"] / 1_000_000, 3) == reviewed["candidate_p95"], "adversarial review candidate p95 differs")
        require(round(computed["p95_change_percent"], 1) == reviewed["p95_change_percent"], "adversarial review p95 change differs")
        require(computed["candidate_faster_pairs"] == reviewed["faster_candidate_pairs"], "adversarial review paired win count differs")

    return {
        "schema": "execute.check-json-paired-control-summary/1",
        "result_scope": "Narrow static `check --json` startup/validation path on macOS ARM64 with two captured program lengths and a 75-manifest catalog; not a general runtime-performance result.",
        "runs": [
            {"label": label, "raw_sha256": sha256(report_bytes[label]), "raw_file": f"raw/{filename}", "compression": "XZ; decompression is byte-identical to measured raw JSON"}
            for label, filename, _ in RUNS
        ],
        "totals": {"runs": len(reports), "cells_per_run": len(CELLS), "measured_processes": measured_processes, "excluded_warmup_processes": warmups, "preflight_processes": preflights, "all_process_invocations": measured_processes + warmups + preflights, "timeouts_or_nonzero_statuses": 0, "correctness_failures": 0},
        "pins": {
            "baseline_source_revision": first["provenance"]["baseline_source_revision"],
            "baseline_binary_sha256": first["fingerprints"]["before"]["binaries"]["baseline"]["sha256"],
            "candidate_source_label": first["provenance"]["candidate_source_label"],
            "candidate_binary_sha256": first["fingerprints"]["before"]["binaries"]["candidate"]["sha256"],
            "reviewed_patch_sha256": first["fingerprints"]["before"]["candidate_reviewed_patch"]["sha256"],
            "driver_sha256": first["fingerprints"]["before"]["control_driver"]["sha256"],
            "tests_sha256": first["fingerprints"]["before"]["control_tests"]["sha256"],
            "source_pair_harness_sha256": first["fingerprints"]["before"]["source_pair_harness"]["sha256"],
        },
        "full75_json": {
            "observations": len(target),
            "median_change_percent_range": [min(row["median_change_percent"] for row in target), max(row["median_change_percent"] for row in target)],
            "candidate_faster_pairs": wins,
            "paired_comparisons": len(target) * 48,
            "lower_candidate_p95_observations": lower_p95,
            "initial_one_step_p95_ms": {"baseline": initial_one["baseline"]["p95_nearest_rank_ns"] / 1_000_000, "candidate": initial_one["candidate"]["p95_nearest_rank_ns"] / 1_000_000, "candidate_max": initial_one["candidate"]["maximum_ns"] / 1_000_000},
        },
        "cells": rows,
        "controls": control_groups,
        "limitations": [
            "The two fixed replications were registered after the initial one-step p95 regression appeared; all outcomes are retained, but the three-run block is not a wholly predeclared independent multi-run study.",
            "Evidence is from one macOS ARM64 host and has no formal confidence intervals, other-platform performance results, or CPU/RSS measurements.",
            "The ordinary-check and inert-catalog controls are mixed; no claim is made that all paths improve or that general runtime execution is faster.",
        ],
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--write-summary", action="store_true", help="write recomputed summary.json instead of compare it")
    args = parser.parse_args()
    try:
        verify_checksums()
        summary = build_summary()
        summary_path = ROOT / "summary.json"
        rendered = canonical_json(summary)
        if args.write_summary:
            summary_path.write_bytes(rendered)
        else:
            require(summary_path.exists() and summary_path.read_bytes() == rendered, "summary.json differs from independent raw recomputation")
        print(f"PASS: {summary['totals']['measured_processes']} measured + {summary['totals']['excluded_warmup_processes']} warmup + {summary['totals']['preflight_processes']} preflight invocations; all status/output/effect/statistic oracles pass")
        print(f"PASS: {summary['full75_json']['candidate_faster_pairs']}/{summary['full75_json']['paired_comparisons']} full75 JSON pairs favor candidate medians; initial one-step p95 regression retained")
        return 0
    except (AuditError, KeyError, TypeError, ValueError, OSError) as exc:
        print(f"FAIL: {exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
