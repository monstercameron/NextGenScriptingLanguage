#!/usr/bin/env python3
"""Independently audit the structured CSV typed-carry paired trial.

Reads frozen source/build provenance and all 102 saved invocations. It never
builds or executes the Rust product. A fresh output path is required.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import statistics
import subprocess
import sys
from typing import Any


CAMPAIGN = Path(__file__).resolve().parents[1]
EVIDENCE = CAMPAIGN / "evidence/typed-json-carry-structured"
BUILD = EVIDENCE / "build-attempt-1"
RUN = EVIDENCE / "paired-run-attempt-1"
REPORT = RUN / "paired-report.json"
PREREG = EVIDENCE / "preregistration-attempt-1.json"
SOURCE_BINDINGS = EVIDENCE / "source-fingerprints.json"
BUILD_DRIVER = CAMPAIGN / "work/build_typed_json_carry_structured_pair.py"
RUNNER = CAMPAIGN / "work/typed-json-carry-structured-paired-runner/typed_json_carry_structured_pair.py"
BASE_COMMIT = "e5fa5162b9bdca608f7f3c9d0d273db40a93c17b"
PRODUCT_PATCH_SHA256 = "99650ce4d9c2a2d4c8418e52bc3dbbda2f99a2a745e54c48332f9dc6a1958bb9"
HARNESS_PATCH_SHA256 = "732a1926e71033ce5d73ed070e99a58c6a60db2a2b9ea316f5f65fd84a44b2d0"
HARNESS_SOURCE_SHA256 = "bac676768a6bf6e748f3cd0590c200ccaa58e1afe712ab99679f35ff29d5ddf3"
SOURCE_BINDINGS_SHA256 = "dc9fc282efbcf2cb791eb2e1fba5a5e4b72d2c638bd97ef0eee5e9c300a04dd6"
BUILD_DRIVER_SHA256 = "72b67e06ca7d658bf01b5166f2d6312cdbd6c21e62ae982f4b97b23ccc05ebaa"
RUNNER_SHA256 = "8fb1f0b624179d9fd62c0aa2f538b08c75aa44a826466592022e4d922bb3136c"
TARGET_NAME = "typed_json_carry_structured"
TEST_NAME = "structured_csv_parse_1000"
HARNESS_RELATIVE = "tests/typed_json_carry_structured.rs"
EXPECTED_CHANGED_PATHS = {
    "src/contracts.rs", "src/manifest.rs", "src/runtime.rs",
    "src/runtime/program_backend.rs", "src/runtime/windows_process.rs",
    "tests/typed_contracts.rs",
}
EXPECTED_METHOD = (
    "Public runtime::run API; 1,000 uncached in-process csv.parse calls over one fixed 64-row, four-column CSV input. "
    "The language requires distinct result names, so calls bind table0000 through table0999 and all 1,000 tables remain "
    "live; the final table is encoded as compact JSON. Includes repeated in-process parsing, typed output validation/carry, "
    "binding retention, and runtime result construction; excludes registry/program/fixture setup, hashing, report writing, "
    "and RunResult destruction. No child processes."
)
EMPTY_SHA256 = hashlib.sha256(b"").hexdigest()


class AuditError(RuntimeError):
    pass


def sha_bytes(payload: bytes) -> str:
    return hashlib.sha256(payload).hexdigest()


def sha_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def _unique_object(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            raise AuditError(f"duplicate JSON key: {key}")
        result[key] = value
    return result


def _reject_constant(value: str) -> None:
    raise AuditError(f"non-finite JSON constant is forbidden: {value}")


def load_json(path: Path, label: str) -> Any:
    try:
        return json.loads(path.read_text(encoding="utf-8"), object_pairs_hook=_unique_object,
                          parse_constant=_reject_constant)
    except (OSError, UnicodeDecodeError, json.JSONDecodeError, AuditError) as error:
        raise AuditError(f"cannot load strict JSON {label}: {error}") from error


class Auditor:
    def __init__(self) -> None:
        self.checks: list[str] = []

    def require(self, condition: bool, label: str) -> None:
        if not condition:
            raise AuditError(label)
        self.checks.append(label)


def git(root: Path, *args: str) -> bytes:
    completed = subprocess.run(["git", *args], cwd=root, stdin=subprocess.DEVNULL,
                               stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=False)
    if completed.returncode != 0:
        raise AuditError(f"git {' '.join(args)} failed in {root}: {completed.stderr.decode(errors='replace')}")
    return completed.stdout


def source_fingerprint(root: Path) -> dict[str, Any]:
    tracked = git(root, "ls-files", "-z").split(b"\0")
    untracked = git(root, "ls-files", "--others", "--exclude-standard", "-z").split(b"\0")
    paths = sorted({os.fsdecode(item) for item in tracked + untracked if item})
    files: dict[str, str] = {}
    for relative in paths:
        path = root / relative
        payload = b"symlink\0" + os.fsencode(os.readlink(path)) if path.is_symlink() else path.read_bytes()
        files[relative] = sha_bytes(payload)
    canonical = json.dumps(files, sort_keys=True, separators=(",", ":")).encode()
    return {"sha256": sha_bytes(canonical), "file_count": len(files), "files": files}


def diff_sha(root: Path, *, staged: bool = False) -> str:
    args = ["diff", "--binary"] + (["--cached"] if staged else [])
    return sha_bytes(git(root, *args))


def status_paths(root: Path) -> list[str]:
    rows = git(root, "status", "--porcelain=v1", "--untracked-files=all").decode().splitlines()
    return sorted(line[3:] for line in rows)


def expected_workload() -> dict[str, Any]:
    headers = ["identifier", "label", "category", "note"]
    rows = [
        [f"record-{index:03}", f"label-{index:03}", f"group-{index % 8:02}",
         f"note-{index:03}-fixed-structured-output"]
        for index in range(64)
    ]
    csv_text = ",".join(headers) + "\n" + "".join(",".join(row) + "\n" for row in rows)
    program = "accept csv as text\n" + "".join(
        f"run csv.parse with text csv and has_headers true as table{index:04}\n" for index in range(1000)
    ) + "then encode table0999 as json\n"
    output = json.dumps({"headers": headers, "rows": rows}, ensure_ascii=False,
                        separators=(",", ":"), allow_nan=False).encode()
    return {
        "calls": 1000, "applet": "csv.parse",
        "binding_pattern": "table0000 through table0999 (distinct bindings required by the DSL)",
        "retained_parsed_tables": 1000, "rows": 64, "columns": 4,
        "input_bytes": len(csv_text.encode()), "input_sha256": sha_bytes(csv_text.encode()),
        "source_bytes": len(program.encode()), "source_sha256": sha_bytes(program.encode()),
        "expected_headers": headers, "expected_rows": rows,
        "expected_output_hex": output.hex(), "expected_output_bytes": len(output),
        "expected_output_sha256": sha_bytes(output),
        "expected_per_call_stdout_bytes_including_lf": len(output) + 1,
    }


def p95_nearest_rank(values: list[int]) -> int:
    ordered = sorted(values)
    return ordered[math.ceil(0.95 * len(ordered)) - 1]


def audit() -> dict[str, Any]:
    a = Auditor()
    binding = load_json(SOURCE_BINDINGS, "full source bindings")
    prereg = load_json(PREREG, "preregistration")
    report = load_json(REPORT, "paired report")
    build_pair = load_json(BUILD / "build-pair-receipt.json", "paired build receipt")
    expected_work = expected_workload()

    a.require(sha_file(BUILD_DRIVER) == BUILD_DRIVER_SHA256, "current build driver matches the frozen final hash")
    a.require(sha_file(RUNNER) == RUNNER_SHA256, "current paired runner matches the frozen final hash")
    a.require(sha_file(SOURCE_BINDINGS) == SOURCE_BINDINGS_SHA256, "full source bindings match their fixed SHA-256")
    a.require(binding.get("base_commit") == BASE_COMMIT, "source binding base commit is exact")
    a.require(binding.get("product_patch_sha256") == PRODUCT_PATCH_SHA256, "source binding product patch is exact")
    a.require(binding.get("harness_patch_sha256") == HARNESS_PATCH_SHA256, "source binding harness patch is exact")
    a.require(binding.get("harness_source_sha256") == HARNESS_SOURCE_SHA256, "source binding harness bytes are exact")
    a.require(prereg.get("schema") == "typed-json-carry-structured-paired-preregistration/1", "preregistration schema is fixed")
    a.require(sha_file(PREREG) == "b2425cfd8a798da4a5269f60b97e8d66faa1fb243caaa1a7e2edbfd24bcecf11", "preregistration matches its report-pinned SHA-256")
    a.require(prereg.get("protocol", {}).get("case") == "structured_csv_parse_1000", "preregistered case is fixed")
    a.require(prereg.get("protocol", {}).get("warmup_pairs") == 3 and prereg.get("protocol", {}).get("measured_pairs") == 48, "preregistered outer schedule is 3 warmups plus 48 measurements")
    a.require(prereg.get("protocol", {}).get("invocation_samples") == 3 and prereg.get("protocol", {}).get("invocation_warmups") == 0, "preregistered inner sample counts are fixed")
    a.require(prereg.get("protocol", {}).get("retry_policy", "").startswith("none;"), "preregistered retry policy forbids retries")
    a.require(prereg.get("protocol", {}).get("interpretation_limit", "").startswith("mechanistic public-runtime"), "preregistration limits the claim to the mechanistic public-runtime case")
    a.require(prereg.get("runner") == {"path": str(RUNNER.resolve()), "sha256": RUNNER_SHA256}, "preregistration binds the final runner")
    a.require(prereg.get("workload") == expected_work, "preregistration workload matches independently reconstructed CSV/program/output")
    a.require(report.get("schema") == "typed-json-carry-structured-paired-report/1" and report.get("status") == "complete", "paired report is complete with the fixed schema")
    a.require(report.get("preregistration_path") == str(PREREG.resolve()), "paired report points to the fixed preregistration")
    a.require(report.get("preregistration_sha256") == sha_file(PREREG), "paired report preregistration pin is exact")
    a.require(report.get("runner") == prereg.get("runner"), "paired report runner pin matches preregistration")
    a.require(report.get("protocol") == prereg.get("protocol"), "paired report protocol matches preregistration")
    a.require(report.get("workload_contract") == prereg.get("workload"), "paired report workload contract matches preregistration")
    a.require(report.get("inputs") == prereg.get("inputs"), "paired report arm identities match preregistration")
    a.require(report.get("python") == prereg.get("python"), "paired report Python identity/flags match preregistration")
    a.require(report.get("runtime_environment") == prereg.get("runtime_environment"), "paired report environment policy matches preregistration")

    a.require(build_pair.get("schema") == "typed_json_carry_structured_build_pair_receipt/1" and build_pair.get("status") == "success", "paired build receipt is successful")
    a.require(build_pair.get("tests_executed") is False and build_pair.get("cargo_configuration_stable") is True, "build receipt is build-only with stable Cargo configuration")
    a.require(build_pair.get("build_driver_sha256") == BUILD_DRIVER_SHA256, "paired build receipt binds the final driver")
    a.require(build_pair.get("product_patch_sha256") == PRODUCT_PATCH_SHA256 and build_pair.get("harness_patch_sha256") == HARNESS_PATCH_SHA256, "paired build receipt binds product and harness patches")
    a.require(build_pair.get("source_bindings_sha256") == SOURCE_BINDINGS_SHA256, "paired build receipt binds the complete source map")
    a.require(build_pair.get("arms") == {side: str((BUILD / side / "build-receipt.json")) for side in ("baseline", "candidate")}, "build pair receipt names its exact arm receipts")
    a.require(prereg.get("provenance", {}).get("build_driver_sha256") == BUILD_DRIVER_SHA256, "preregistration binds the final build driver")
    a.require(prereg.get("provenance", {}).get("source_bindings_sha256") == SOURCE_BINDINGS_SHA256, "preregistration binds the complete source map")

    arm_audit: dict[str, Any] = {}
    expected_status = {
        "baseline": [HARNESS_RELATIVE],
        "candidate": sorted([HARNESS_RELATIVE, *EXPECTED_CHANGED_PATHS]),
    }
    for side in ("baseline", "candidate"):
        arm = prereg["inputs"][side]
        root = Path(arm["root"]).resolve(strict=True)
        frozen = binding["arms"][side]
        receipt_path = BUILD / side / "build-receipt.json"
        receipt = load_json(receipt_path, f"{side} build receipt")
        saved_binary = Path(arm["test_binary"]["binary"]).resolve(strict=True)
        current_tree = source_fingerprint(root)
        a.require(arm["side"] == side and arm["root"] == str(root), f"{side} preregistered root/side is exact")
        a.require(arm["head"] == BASE_COMMIT and git(root, "rev-parse", "HEAD").decode().strip() == BASE_COMMIT, f"{side} full source root is on the pinned base commit")
        a.require(frozen["root"] == str(root) and frozen["head"] == BASE_COMMIT, f"{side} source fingerprint root/commit matches preregistration")
        a.require(current_tree == frozen["tree"], f"{side} live 2,197-file tree matches the frozen complete map")
        a.require(current_tree["file_count"] == 2197 and len(current_tree["files"]) == 2197, f"{side} source tree has all 2,197 files")
        a.require(status_paths(root) == expected_status[side], f"{side} checkout contains only the reviewed harness/product paths")
        expected_diff = EMPTY_SHA256 if side == "baseline" else PRODUCT_PATCH_SHA256
        a.require(diff_sha(root) == expected_diff, f"{side} tracked source diff matches the frozen arm")
        a.require(diff_sha(root, staged=True) == EMPTY_SHA256, f"{side} source has no staged changes")
        a.require(arm["source_fingerprint"] == frozen["tree"], f"{side} preregistration binds its complete source map")
        a.require(arm["tracked_diff_sha256"] == expected_diff and arm["staged_diff_sha256"] == EMPTY_SHA256, f"{side} preregistration binds the exact diff state")
        a.require(receipt.get("schema") == "typed_json_carry_structured_build_receipt/1" and receipt.get("status") == "success", f"{side} build receipt is successful")
        a.require(receipt.get("side") == side and receipt.get("head") == BASE_COMMIT and receipt.get("worktree") == str(root), f"{side} build receipt binds its source arm")
        a.require(receipt.get("tests_executed") is False and receipt.get("source_stable") is True, f"{side} receipt records build-only stable-source build")
        a.require(receipt.get("source_fingerprint_sha256") == frozen["tree"]["sha256"] and receipt.get("source_fingerprint_file_count") == 2197, f"{side} receipt source tree hash/count match")
        a.require(receipt.get("source_fingerprint_after") == {"sha256": frozen["tree"]["sha256"], "file_count": 2197}, f"{side} post-build source tree is stable")
        a.require(receipt.get("tracked_diff_sha256") == expected_diff and receipt.get("staged_diff_sha256") == EMPTY_SHA256, f"{side} build receipt diff pins match")
        a.require(receipt.get("build_driver_sha256") == BUILD_DRIVER_SHA256 and receipt.get("source_bindings_sha256") == SOURCE_BINDINGS_SHA256, f"{side} build receipt driver/source-map pins match")
        a.require(sha_file(receipt_path) == arm["receipt_sha256"], f"{side} preregistered build receipt SHA-256 matches")
        a.require(receipt.get("build_return_code") == 0 and receipt.get("clean_return_code") == 0, f"{side} clean/build commands returned zero")
        cargo_row = receipt["artifact"]["cargo_json"]
        build_log = BUILD / side / "build.jsonl"
        a.require(sha_file(build_log) == receipt.get("build_stdout_sha256"), f"{side} Cargo JSONL log matches its recorded hash")
        log_rows = []
        for line_number, line in enumerate(build_log.read_bytes().splitlines(), start=1):
            if not line.strip():
                continue
            row = json.loads(line.decode("utf-8"), object_pairs_hook=_unique_object, parse_constant=_reject_constant)
            a.require(isinstance(row, dict), f"{side} Cargo JSONL line {line_number} is an object")
            target = row.get("target")
            if row.get("reason") == "compiler-artifact" and isinstance(target, dict) and target.get("name") == TARGET_NAME:
                log_rows.append(row)
        a.require(len(log_rows) == 1 and log_rows[0] == cargo_row, f"{side} Cargo log has exactly one exact receipt artifact row")
        target = cargo_row["target"]
        profile = cargo_row["profile"]
        a.require(cargo_row.get("reason") == "compiler-artifact" and cargo_row.get("fresh") is False, f"{side} Cargo artifact is the recorded release compile")
        a.require(target.get("name") == TARGET_NAME and target.get("src_path") == str(root / HARNESS_RELATIVE), f"{side} Cargo artifact target/source are exact")
        a.require(target.get("kind") == ["test"] and target.get("test") is True, f"{side} Cargo artifact is the benchmark test target")
        a.require(profile.get("test") is True and profile.get("opt_level") == "3" and profile.get("debug_assertions") is False, f"{side} Cargo artifact is optimized release profile")
        a.require(cargo_row.get("manifest_path") == str(root / "Cargo.toml"), f"{side} Cargo artifact comes from the bound manifest")
        a.require(saved_binary == Path(receipt["artifact"]["saved_binary"]).resolve(strict=True), f"{side} saved test binary path matches receipt and preregistration")
        saved_sha = sha_file(saved_binary)
        a.require(saved_sha == receipt["artifact"]["saved_binary_sha256"] == arm["test_binary"]["binary_sha256"], f"{side} immutable saved test binary hash matches all receipts")
        a.require(os.access(saved_binary, os.X_OK) and (saved_binary.stat().st_mode & 0o777) == 0o555, f"{side} saved test binary is read-only executable")
        a.require(arm["test_binary"]["artifact"] == cargo_row, f"{side} preregistration carries exact Cargo artifact row")
        arm_audit[side] = {
            "root": str(root), "head": BASE_COMMIT,
            "source_file_count": current_tree["file_count"], "source_fingerprint_sha256": current_tree["sha256"],
            "changed_tracked_paths_from_base": sorted(EXPECTED_CHANGED_PATHS) if side == "candidate" else [],
            "tracked_diff_sha256": expected_diff, "build_receipt": str(receipt_path.resolve()),
            "build_receipt_sha256": sha_file(receipt_path), "cargo_jsonl_sha256": sha_file(build_log),
            "cargo_artifact": cargo_row, "saved_test_binary_sha256": saved_sha,
        }

    base_files = binding["arms"]["baseline"]["tree"]["files"]
    candidate_files = binding["arms"]["candidate"]["tree"]["files"]
    changed_files = sorted(path for path in base_files if base_files[path] != candidate_files[path])
    a.require(set(base_files) == set(candidate_files), "baseline and candidate source maps contain identical paths")
    a.require(changed_files == sorted(EXPECTED_CHANGED_PATHS), "candidate differs from baseline only in the six typed-carry product files")
    a.require(arm_audit["baseline"]["saved_test_binary_sha256"] != arm_audit["candidate"]["saved_test_binary_sha256"], "saved baseline/candidate binaries are distinct")

    a.require(report.get("attempt_count") == 102 and len(report.get("attempts", [])) == 102, "report retains all 102 invocations")
    a.require(report.get("successful_attempt_count") == 102 and report.get("failed_attempt_count") == 0, "all 102 recorded invocations succeeded")
    a.require(len(report.get("warmup_pairs", [])) == 3 and len(report.get("measured_pairs", [])) == 48, "report retains exactly three warmup and 48 measured pairs")
    a.require(report.get("post_run_identity", {}).get("provenance_stable") is True, "runner post-run provenance check passed")
    a.require(report.get("post_run_identity", {}).get("runner_stable") is True and report.get("post_run_identity", {}).get("preregistration_stable") is True, "runner and preregistration remained stable during trial")
    a.require(report.get("post_run_identity", {}).get("source_bindings_sha256") == SOURCE_BINDINGS_SHA256, "post-run report pins the complete source map")

    expected = expected_work
    oracle = {
        "output_sha256": expected["expected_output_sha256"],
        "output_bytes": expected["expected_output_bytes"],
        "launched_processes": 0,
        "cache_hits": 0,
        "retained_execution_records": 256,
        "retained_all_success_status_0": True,
        "retained_stdout_bytes_per_call": expected["expected_per_call_stdout_bytes_including_lf"],
        "dropped_execution_records": 744,
        "dropped_stages": 744,
    }
    measured_attempts: dict[str, dict[str, Any]] = {}
    invocation_audit: list[dict[str, Any]] = []
    catalog_hashes: set[str] = set()
    raw_samples: dict[str, list[int]] = {"baseline": [], "candidate": []}
    invocation_medians: dict[str, list[int]] = {"baseline": [], "candidate": []}
    expected_environment_base = {"LC_ALL": "C", "TZ": "UTC"}
    for index, attempt in enumerate(report["attempts"]):
        pair_index = index // 2
        position = index % 2 + 1
        global_pair = pair_index
        phase = "warmup" if global_pair < 3 else "measurement"
        phase_index = global_pair + 1 if phase == "warmup" else global_pair - 2
        order = ("baseline", "candidate") if global_pair % 2 == 0 else ("candidate", "baseline")
        side = order[position - 1]
        attempt_id = f"{phase}-{phase_index:02d}-{position}-{side}"
        a.require(attempt.get("attempt_id") == attempt_id and attempt.get("side") == side, f"invocation {index} follows the fixed AB/BA schedule")
        a.require(attempt.get("phase") == phase and attempt.get("phase_pair_index") == phase_index and attempt.get("global_pair_index") == global_pair and attempt.get("position_in_pair") == position, f"invocation {attempt_id} schedule coordinates are exact")
        identity = prereg["inputs"][side]
        a.require(attempt.get("successful") is True and attempt.get("returncode") == 0 and attempt.get("timed_out") is False and attempt.get("execution_error") is None, f"invocation {attempt_id} completed successfully")
        a.require(attempt.get("retry") == 0, f"invocation {attempt_id} was not retried")
        a.require(attempt.get("binary_sha256") == identity["test_binary"]["binary_sha256"] == attempt.get("binary_sha256_after"), f"invocation {attempt_id} binds an unchanged arm binary")
        a.require(attempt.get("build_receipt_sha256") == identity["receipt_sha256"] and attempt.get("source_fingerprint_sha256") == identity["source_fingerprint"]["sha256"], f"invocation {attempt_id} binds its arm receipt/source fingerprint")
        expected_command = [identity["test_binary"]["binary"], "--exact", TEST_NAME, "--ignored", "--nocapture", "--test-threads=1"]
        a.require(attempt.get("command") == expected_command and attempt.get("cwd") == identity["root"], f"invocation {attempt_id} command and working directory are exact")
        attempt_dir = RUN / "attempts" / attempt_id
        a.require(Path(attempt["attempt_dir"]).resolve(strict=True) == attempt_dir.resolve(strict=True), f"invocation {attempt_id} stays in its prescribed evidence directory")
        raw_path = attempt_dir / "raw.json"
        a.require(Path(attempt["raw_report_path"]).resolve(strict=True) == raw_path.resolve(strict=True), f"invocation {attempt_id} raw path is exact")
        environment = {**expected_environment_base, "OPT_BENCH_REPORT": str(raw_path.resolve())}
        env_hash = sha_bytes(json.dumps(environment, sort_keys=True, separators=(",", ":"), ensure_ascii=False, allow_nan=False).encode())
        a.require(attempt.get("execution_environment") == environment and attempt.get("execution_environment_sha256") == env_hash, f"invocation {attempt_id} has exact isolated child environment and hash")
        started = load_json(attempt_dir / "started.json", f"{attempt_id} started record")
        a.require(started.get("attempt_id") == attempt_id and started.get("side") == side and started.get("global_pair_index") == global_pair and started.get("position_in_pair") == position, f"invocation {attempt_id} started record schedule matches")
        a.require(started.get("binary_sha256_before") == attempt.get("binary_sha256") and started.get("raw_report_path") == str(raw_path.resolve()), f"invocation {attempt_id} started record binds binary and raw path")
        a.require(started.get("execution_environment") == environment and started.get("execution_environment_sha256") == env_hash, f"invocation {attempt_id} started record binds exact environment")
        a.require(started.get("command") == expected_command and started.get("cwd") == identity["root"] and started.get("retry") == 0, f"invocation {attempt_id} started command/cwd/retry match")
        result_file = load_json(attempt_dir / "result.json", f"{attempt_id} result record")
        a.require(result_file == attempt, f"invocation {attempt_id} outer report equals its immutable result record")
        for stream in ("stdout", "stderr"):
            path = attempt_dir / f"{stream}.log"
            a.require(attempt.get(f"{stream}_path") == str(path.resolve(strict=True)), f"invocation {attempt_id} {stream} path is exact")
            payload = path.read_bytes()
            a.require(sha_bytes(payload) == attempt.get(f"{stream}_sha256") and len(payload) == attempt.get(f"{stream}_bytes"), f"invocation {attempt_id} {stream} bytes/hash match")
        raw_payload = raw_path.read_bytes()
        a.require(sha_bytes(raw_payload) == attempt.get("raw_report_sha256") and len(raw_payload) == attempt.get("raw_report_bytes"), f"invocation {attempt_id} raw report bytes/hash match")
        raw = json.loads(raw_payload.decode("utf-8"), object_pairs_hook=_unique_object, parse_constant=_reject_constant)
        a.require(set(raw) == {"schema", "method", "warmup_runs", "measured_samples", "workload", "results"}, f"invocation {attempt_id} raw JSON schema has exact fields")
        a.require(raw.get("schema") == "typed_json_carry_public_runtime_structured_bench/v1" and raw.get("method") == EXPECTED_METHOD, f"invocation {attempt_id} reports the fixed runtime case")
        a.require(raw.get("warmup_runs") == 0 and raw.get("measured_samples") == 3, f"invocation {attempt_id} has three inner samples and no inner warmups")
        workload = raw.get("workload")
        expected_raw_workload = {**expected, "catalog_sha256": workload.get("catalog_sha256") if isinstance(workload, dict) else None}
        a.require(isinstance(workload, dict) and set(workload) == set(expected_raw_workload) and workload == expected_raw_workload, f"invocation {attempt_id} matches independently reconstructed 64x4 CSV, program, and output")
        catalog_hash = workload.get("catalog_sha256")
        a.require(isinstance(catalog_hash, str) and len(catalog_hash) == 64 and all(char in "0123456789abcdef" for char in catalog_hash), f"invocation {attempt_id} catalog hash is well-formed")
        catalog_hashes.add(catalog_hash)
        case = raw.get("results", {}).get(TEST_NAME) if isinstance(raw.get("results"), dict) else None
        a.require(isinstance(case, dict) and set(case) == {"p50_ns", "p95_ns", "raw_sample_ns", "semantic_oracles"}, f"invocation {attempt_id} contains only the fixed case result")
        samples = case.get("raw_sample_ns")
        a.require(isinstance(samples, list) and len(samples) == 3 and all(type(value) is int and 0 < value < 2**64 for value in samples), f"invocation {attempt_id} retains three positive raw durations")
        middle = sorted(samples)[1]
        a.require(case.get("p50_ns") == middle and case.get("p95_ns") == max(samples), f"invocation {attempt_id} reported percentiles match its three raw samples")
        oracles = case.get("semantic_oracles")
        a.require(isinstance(oracles, list) and len(oracles) == 3 and all(item == oracle for item in oracles), f"invocation {attempt_id} has three exact independent semantic oracles")
        a.require(attempt.get("raw_sample_ns") == samples and attempt.get("median_duration_ns") == middle, f"invocation {attempt_id} outer record matches raw durations")
        a.require(attempt.get("workload") == workload and attempt.get("semantic_oracles") == oracles, f"invocation {attempt_id} outer record matches raw workload/oracles")
        raw_samples[side].extend(samples)
        invocation_medians[side].append(middle)
        measured_attempts[attempt_id] = attempt
        invocation_audit.append({
            "attempt_id": attempt_id, "phase": phase, "pair_index": phase_index,
            "order": list(order), "position": position, "arm": side,
            "raw_report_sha256": attempt["raw_report_sha256"],
            "raw_sample_ns": samples, "invocation_median_ns": middle,
            "output_sha256": expected["expected_output_sha256"],
            "catalog_sha256": catalog_hash,
        })

    a.require(len(catalog_hashes) == 1, "all invocations used the identical eagerly loaded catalog")

    pair_audit: list[dict[str, Any]] = []
    order_counts: dict[str, dict[str, int]] = {"warmup": {"AB": 0, "BA": 0}, "measurement": {"AB": 0, "BA": 0}}
    all_pairs = report["warmup_pairs"] + report["measured_pairs"]
    for global_pair, pair in enumerate(all_pairs):
        phase = "warmup" if global_pair < 3 else "measurement"
        phase_index = global_pair + 1 if phase == "warmup" else global_pair - 2
        order = ("baseline", "candidate") if global_pair % 2 == 0 else ("candidate", "baseline")
        label = "AB" if order == ("baseline", "candidate") else "BA"
        order_counts[phase][label] += 1
        a.require(pair.get("phase") == phase and pair.get("phase_pair_index") == phase_index and pair.get("global_pair_index") == global_pair, f"pair {global_pair} coordinates follow fixed schedule")
        a.require(pair.get("order") == list(order), f"pair {global_pair} AB/BA order matches fixed schedule")
        a.require(pair.get("valid_for_timing") is True and pair.get("workload_compatible") is True, f"pair {global_pair} is valid with compatible workload")
        baseline_id = f"{phase}-{phase_index:02d}-{order.index('baseline') + 1}-baseline"
        candidate_id = f"{phase}-{phase_index:02d}-{order.index('candidate') + 1}-candidate"
        baseline = measured_attempts[baseline_id]
        candidate = measured_attempts[candidate_id]
        delta = candidate["median_duration_ns"] - baseline["median_duration_ns"]
        ratio = candidate["median_duration_ns"] / baseline["median_duration_ns"]
        a.require(pair.get("baseline_median_ns") == baseline["median_duration_ns"] and pair.get("candidate_median_ns") == candidate["median_duration_ns"], f"pair {global_pair} arm medians match invocation records")
        a.require(pair.get("candidate_minus_baseline_ns") == delta, f"pair {global_pair} delta is independently recomputed")
        a.require(pair.get("baseline_successful") is True and pair.get("candidate_successful") is True, f"pair {global_pair} arms both completed successfully")
        if phase == "measurement":
            pair_audit.append({
                "measurement_pair": phase_index, "global_pair_index": global_pair,
                "order": label, "baseline_median_ns": baseline["median_duration_ns"],
                "candidate_median_ns": candidate["median_duration_ns"],
                "candidate_minus_baseline_ns": delta, "candidate_over_baseline_ratio": ratio,
            })

    a.require(order_counts == {"warmup": {"AB": 2, "BA": 1}, "measurement": {"AB": 24, "BA": 24}}, "warmup/measurement AB-BA counts match the preregistered schedule")
    deltas = [row["candidate_minus_baseline_ns"] for row in pair_audit]
    ratios = [row["candidate_over_baseline_ratio"] for row in pair_audit]
    candidate_wins = sum(row["candidate_median_ns"] < row["baseline_median_ns"] for row in pair_audit)
    candidate_losses = sum(row["candidate_median_ns"] > row["baseline_median_ns"] for row in pair_audit)
    ties = len(pair_audit) - candidate_wins - candidate_losses
    summary = report["paired_summary"]
    a.require(summary == {
        "valid_pairs": 48, "wins": candidate_wins, "losses": candidate_losses, "ties": ties,
        "median_candidate_minus_baseline_ns": int(statistics.median(deltas)), "raw_deltas_ns": deltas,
    }, "runner paired summary exactly matches independently recomputed measurements")

    post = report["post_run_identity"]
    a.require(post.get("provenance_stable") is True and post.get("preregistration_stable") is True and post.get("runner_stable") is True, "runner post-run identities remain stable")
    stats = {
        "measurement_pairs": len(pair_audit),
        "attempt_count": len(invocation_audit),
        "successful_attempt_count": len(invocation_audit),
        "order_counts": order_counts,
        "candidate_wins": candidate_wins,
        "candidate_losses": candidate_losses,
        "ties": ties,
        "median_candidate_minus_baseline_ns": int(statistics.median(deltas)),
        "median_candidate_minus_baseline_ns_exact": str(statistics.median(deltas)),
        "median_paired_candidate_over_baseline_ratio": statistics.median(ratios),
        "median_paired_candidate_improvement_percent": (1.0 - statistics.median(ratios)) * 100.0,
        "paired_ratio_min": min(ratios),
        "paired_ratio_max": max(ratios),
        "baseline_invocation_median_ns": int(statistics.median(invocation_medians["baseline"][3:])),
        "candidate_invocation_median_ns": int(statistics.median(invocation_medians["candidate"][3:])),
        "baseline_invocation_median_ns_exact": str(statistics.median(invocation_medians["baseline"][3:])),
        "candidate_invocation_median_ns_exact": str(statistics.median(invocation_medians["candidate"][3:])),
        "baseline_invocation_median_p95_ns_nearest_rank": p95_nearest_rank(invocation_medians["baseline"][3:]),
        "candidate_invocation_median_p95_ns_nearest_rank": p95_nearest_rank(invocation_medians["candidate"][3:]),
        "baseline_raw_sample_median_ns": int(statistics.median(raw_samples["baseline"][9:])),
        "candidate_raw_sample_median_ns": int(statistics.median(raw_samples["candidate"][9:])),
        "baseline_raw_sample_p95_ns_nearest_rank": p95_nearest_rank(raw_samples["baseline"][9:]),
        "candidate_raw_sample_p95_ns_nearest_rank": p95_nearest_rank(raw_samples["candidate"][9:]),
        "catalog_sha256": next(iter(catalog_hashes)),
        "expected_output_bytes": expected["expected_output_bytes"],
        "expected_output_sha256": expected["expected_output_sha256"],
        "interpretation": "This is one narrow in-process 1,000-call structured csv.parse workload. All 1,000 distinct result bindings stay live as required by the DSL. It is not CLI startup, default catalog loading, or a general runtime-performance claim.",
    }
    return {
        "schema": "execute.typed_json_carry_structured_independent_audit/1",
        "result": "pass",
        "audit_script": str(Path(__file__).resolve()),
        "audit_script_sha256": sha_file(Path(__file__).resolve()),
        "evidence": {
            "report_path": str(REPORT.resolve()), "report_sha256": sha_file(REPORT),
            "preregistration_path": str(PREREG.resolve()), "preregistration_sha256": sha_file(PREREG),
            "build_pair_receipt_path": str((BUILD / "build-pair-receipt.json").resolve()),
            "build_pair_receipt_sha256": sha_file(BUILD / "build-pair-receipt.json"),
            "source_bindings_path": str(SOURCE_BINDINGS.resolve()), "source_bindings_sha256": sha_file(SOURCE_BINDINGS),
            "runner_path": str(RUNNER.resolve()), "runner_sha256": sha_file(RUNNER),
            "build_driver_path": str(BUILD_DRIVER.resolve()), "build_driver_sha256": sha_file(BUILD_DRIVER),
        },
        "source_provenance": {
            "base_commit": BASE_COMMIT,
            "product_patch_sha256": PRODUCT_PATCH_SHA256,
            "harness_patch_sha256": HARNESS_PATCH_SHA256,
            "harness_source_sha256": HARNESS_SOURCE_SHA256,
            "source_bindings_sha256": SOURCE_BINDINGS_SHA256,
            "arms": arm_audit,
            "changed_candidate_paths": changed_files,
            "saved_binaries_are_distinct": True,
        },
        "checks_passed": len(a.checks),
        "checks": a.checks,
        "invocations": invocation_audit,
        "measurement_pairs": pair_audit,
        "order_counts": order_counts,
        "summary": stats,
    }


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, default=EVIDENCE / "independent-audit-attempt-1/audit.json")
    args = parser.parse_args(argv)
    destination = args.output.expanduser().absolute()
    if destination.exists() or destination.is_symlink():
        print(f"refusing to overwrite audit report: {destination}", file=sys.stderr)
        return 2
    try:
        report = audit()
        destination.parent.mkdir(parents=True, exist_ok=True)
        payload = (json.dumps(report, indent=2, sort_keys=True, ensure_ascii=False, allow_nan=False) + "\n").encode()
        with destination.open("xb") as stream:
            stream.write(payload)
            stream.flush()
            os.fsync(stream.fileno())
    except (AuditError, OSError, KeyError, TypeError, ValueError, subprocess.SubprocessError) as error:
        print(f"structured independent audit failed: {type(error).__name__}: {error}", file=sys.stderr)
        return 1
    print(json.dumps({"audit_report": str(destination), "sha256": sha_bytes(payload), "checks_passed": report["checks_passed"], "result": report["result"]}, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
