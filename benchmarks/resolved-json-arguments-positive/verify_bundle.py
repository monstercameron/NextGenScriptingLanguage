#!/usr/bin/env python3
"""Verify both platform result sets in the literal JSON arguments bundle."""
from __future__ import annotations

import hashlib
import json
import lzma
import math
import statistics
import sys
import tarfile
from pathlib import Path, PurePosixPath

ROOT = Path(__file__).resolve().parent
HOSTED = ROOT / "hosted-linux"
HOSTED_ATTEMPTS = HOSTED / "evidence/run/paired-runs/run-38862234332-1/attempts"


def sha(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def read_json(path: Path):
    return json.loads(path.read_text(encoding="utf-8"))


def safe_join(root: Path, raw: str) -> Path:
    rel = PurePosixPath(raw)
    if rel.is_absolute() or not rel.parts or ".." in rel.parts or "\\" in raw:
        raise ValueError(f"unsafe relative path: {raw!r}")
    return root.joinpath(*rel.parts)


def verify_checksum_manifest(root: Path, manifest_name: str, sidecar_name: str | None = None):
    manifest = root / manifest_name
    expected = {}
    for line in manifest.read_text(encoding="utf-8").splitlines():
        try:
            digest, rel = line.split("  ", 1)
        except ValueError as error:
            raise ValueError(f"invalid checksum row in {manifest_name}: {line!r}") from error
        if rel in expected:
            raise ValueError(f"duplicate {manifest_name} checksum path: {rel}")
        path = safe_join(root, rel)
        if not path.is_file() or sha(path.read_bytes()) != digest:
            raise ValueError(f"{manifest_name} checksum mismatch: {rel}")
        expected[rel] = digest

    excluded = {manifest_name}
    if sidecar_name:
        sidecar = root / sidecar_name
        sidecar_expected = f"{sha(manifest.read_bytes())}  {manifest_name}"
        if not sidecar.is_file() or sidecar.read_text(encoding="utf-8").strip() != sidecar_expected:
            raise ValueError(f"{manifest_name} detached checksum mismatch")
        excluded.add(sidecar_name)

    actual = {
        path.relative_to(root).as_posix()
        for path in root.rglob("*")
        if path.is_file() and path.relative_to(root).as_posix() not in excluded
    }
    if set(expected) != actual:
        raise ValueError(
            f"{manifest_name} file set differs: "
            f"missing={sorted(actual - set(expected))} extra={sorted(set(expected) - actual)}"
        )
    return expected


def require_close(actual, expected, label: str, *, rel_tol=1e-12, abs_tol=1e-9):
    if not math.isclose(float(actual), float(expected), rel_tol=rel_tol, abs_tol=abs_tol):
        raise ValueError(f"{label} mismatch: actual={actual!r} expected={expected!r}")


def verify_local_darwin_result():
    patchmap = read_json(ROOT / "source/patch-uncompressed-sha256.json")
    for item in patchmap["patches"]:
        compressed = safe_join(ROOT, item["compressed_path"]).read_bytes()
        if sha(compressed) != item["compressed_sha256"]:
            raise ValueError(f"compressed Darwin patch checksum mismatch: {item['compressed_path']}")
        plain = lzma.decompress(compressed, format=lzma.FORMAT_XZ)
        if sha(plain) != item["uncompressed_sha256"] or len(plain) != item["uncompressed_size_bytes"]:
            raise ValueError(f"Darwin patch uncompressed checksum mismatch: {item['uncompressed_name']}")

    source_manifest = read_json(ROOT / "source/source-manifest.json")
    for item in source_manifest["files"]:
        path = safe_join(ROOT, item["path"])
        if not path.is_file() or sha(path.read_bytes()) != item["sha256"] or path.stat().st_size != item["size_bytes"]:
            raise ValueError(f"Darwin source checksum mismatch: {item['path']}")

    raw_manifest = read_json(ROOT / "raw-artifacts.manifest.json")
    raw_archive = ROOT / raw_manifest["archive"]
    if sha(raw_archive.read_bytes()) != raw_manifest["archive_sha256"]:
        raise ValueError("Darwin raw archive compressed checksum mismatch")
    with tarfile.open(raw_archive, mode="r:xz") as archive:
        members = archive.getmembers()
        names = [member.name for member in members]
        if names != sorted(names) or len(names) != len(set(names)):
            raise ValueError("Darwin raw archive paths are duplicated or unsorted")
        if set(names) != {item["path"] for item in raw_manifest["members"]}:
            raise ValueError("Darwin raw archive member names differ from manifest")
        for item in raw_manifest["members"]:
            member = archive.getmember(item["path"])
            if not member.isfile():
                raise ValueError(f"non-regular Darwin raw archive member: {item['path']}")
            data = archive.extractfile(member).read()
            if len(data) != item["size_bytes"] or sha(data) != item["sha256"]:
                raise ValueError(f"Darwin raw member checksum mismatch: {item['path']}")

    audit_path = ROOT / "independent-audit.json"
    audit = read_json(audit_path)
    audit_hash = sha(audit_path.read_bytes())
    sidecar = (ROOT / "independent-audit.sha256").read_text(encoding="utf-8").strip()
    if sidecar != f"{audit_hash}  independent-audit.json" or not audit["validation"]["passed"]:
        raise ValueError("Darwin independent audit checksum or validation status mismatch")

    summary = read_json(ROOT / "result-summary.json")
    if len(summary["all_measured_pairs"]) != 48:
        raise ValueError("Darwin summary does not retain all 48 measured pairs")
    if len(summary["all_candidate_slower_pairs"]) != summary["measured_outcomes"]["candidate_slower_pairs"]:
        raise ValueError("Darwin summary loss count differs from its loss ledger")
    if summary["all_candidate_slower_pairs"] != audit["result"]["all_candidate_slower_pairs"]:
        raise ValueError("Darwin summary loss ledger differs from its independent audit")
    result = summary["measured_outcomes"]
    if (result["candidate_faster_pairs"], result["candidate_slower_pairs"], result["ties"]) != (47, 1, 0):
        raise ValueError("Darwin published win/loss/tie counts changed")
    require_close(result["median_of_per_pair_candidate_over_baseline_ratios"], 0.8239204161427683, "Darwin median paired ratio")
    require_close(result["median_paired_candidate_minus_baseline_ns"], -180395.5, "Darwin median paired delta")
    if not summary["all_102_attempts_successful"] or summary["protocol"]["measured_pairs"] != 48:
        raise ValueError("Darwin attempt or protocol summary is incomplete")
    return {
        "audit_sha256": audit_hash,
        "raw_archive_sha256": raw_manifest["archive_sha256"],
        "raw_members": len(raw_manifest["members"]),
        "attempt_directories": raw_manifest["attempt_directory_count"],
        "source_files": len(source_manifest["files"]),
        "loss_rows": len(summary["all_candidate_slower_pairs"]),
    }


def verify_hosted_linux_result():
    hosted_checksums = verify_checksum_manifest(HOSTED, "SHA256SUMS", "SHA256SUMS.sha256")
    audit_path = HOSTED / "evidence/independent-audit.json"
    audit = read_json(audit_path)
    report_path = HOSTED / "evidence/run/paired-runs/run-38862234332-1/paired-report.json"
    report = read_json(report_path)
    prereg_path = HOSTED / "evidence/run/hosted-preregistration.json"
    prereg = read_json(prereg_path)
    run_status_path = HOSTED / "evidence/run-status.json"
    run_status = read_json(run_status_path)

    checks = audit["checks"]
    if checks.get("all_passed") is not True or checks.get("failures") != []:
        raise ValueError("hosted independent audit is not fully passing")
    if audit["run"].get("id") != "38862234332" or audit["run"].get("attempt") != 1:
        raise ValueError("hosted independent audit identifies the wrong workflow run")
    if audit["run"].get("head_sha") != "278541e7d9a88bc55058ca133b3031675dfb84d0":
        raise ValueError("hosted audit dispatch HEAD changed")
    if run_status.get("headSha") != audit["run"]["head_sha"] or run_status.get("conclusion") != "success":
        raise ValueError("hosted run status does not match the independent audit")
    if audit["input_integrity"]["run_status_sha256"] != sha(run_status_path.read_bytes()):
        raise ValueError("hosted run-status digest differs from the independent audit")
    if audit["input_integrity"]["paired_report_sha256"] != sha(report_path.read_bytes()):
        raise ValueError("hosted paired-report digest differs from the independent audit")
    if sha(prereg_path.read_bytes()) != audit["source_provenance"]["hosted_preregistration_sha256"]:
        raise ValueError("hosted preregistration digest differs from the independent audit")
    if sha((HOSTED / "evidence/run/hosted-request.txt").read_bytes()) != audit["source_provenance"]["hosted_request_sha256"]:
        raise ValueError("hosted request digest differs from the independent audit")

    protocol = audit["protocol"]
    if protocol != {
        "actual_order": {
            "first_measured_pair": ["candidate", "baseline"],
            "first_warmup_pair": ["baseline", "candidate"],
            "last_measured_pair": ["baseline", "candidate"],
        },
        "alternating_pair_order": "AB/BA from global pair 0; measured pair 1 begins BA after 3 warmups",
        "each_test_runs_in_its_own_saved_test_binary_process": True,
        "in_invocation_warmups": 0,
        "measured_pairs": 48,
        "observed_attempts": 102,
        "observed_measured_invocations": 96,
        "observed_warmup_invocations": 6,
        "retry_policy": "none",
        "samples_per_invocation": 3,
        "timeout_or_retries_observed": False,
        "timeout_seconds": 120,
        "warmup_pairs": 3,
    }:
        raise ValueError("hosted independent audit protocol is incomplete or changed")
    if report.get("status") != "complete" or report.get("attempt_count") != 102:
        raise ValueError("hosted paired report did not complete all 102 invocations")
    if report.get("successful_attempt_count") != 102 or report.get("failed_attempt_count") != 0:
        raise ValueError("hosted paired report has failed or missing invocations")
    if report.get("protocol", {}).get("no_retries") is not True:
        raise ValueError("hosted paired report does not attest the no-retry protocol")

    # Verify dispatch files against both the checked-in snapshot and the independent audit.
    source_manifest = read_json(HOSTED / "source/source-manifest.json")
    if source_manifest.get("dispatch_commit") != audit["source_provenance"]["workflow_commit"]:
        raise ValueError("hosted source manifest dispatch commit differs from the audit")
    if source_manifest.get("measured_base_commit") != audit["source_provenance"]["base_commit"]:
        raise ValueError("hosted source manifest measured base differs from the audit")
    expected_head = audit["source_provenance"]["expected_source_hashes_at_head"]
    dispatch_rows = {row["path"]: row for row in source_manifest["dispatch_files"]}
    if set(dispatch_rows) != set(expected_head):
        raise ValueError("hosted source manifest does not cover the complete audited dispatch input set")
    for path, expected in expected_head.items():
        snapshot = safe_join(HOSTED / "source/checked-in-at-dispatch", path)
        row = dispatch_rows[path]
        if not snapshot.is_file() or sha(snapshot.read_bytes()) != row["sha256"] or snapshot.stat().st_size != row["size_bytes"]:
            raise ValueError(f"hosted dispatch source hash mismatch: {path}")
        if row["sha256"] != expected["sha256_at_head"] or expected["matches_expected"] is not True:
            raise ValueError(f"hosted dispatch source does not match audit: {path}")

    patch_rows = {row["path"]: row for row in source_manifest["measured_product_patches"]}
    if set(patch_rows) != {"patches/baseline-b824.patch", "patches/candidate-b824.patch"}:
        raise ValueError("hosted product patch manifest is incomplete")
    audit_patches = audit["source_provenance"]["patches"]
    for side in ("baseline", "candidate"):
        patch_name = f"{side}-b824.patch.xz"
        patch_record = audit_patches[patch_name]
        manifest_patch_rel = f"patches/{side}-b824.patch"
        plain_rel = f"source/{manifest_patch_rel}"
        plain = safe_join(HOSTED, plain_rel).read_bytes()
        compressed = safe_join(HOSTED / "source/checked-in-at-dispatch", f"benchmarks/runtime_json_arguments/{patch_name}").read_bytes()
        if sha(compressed) != patch_record["compressed_sha256_at_head"]:
            raise ValueError(f"hosted compressed dispatch patch hash mismatch: {patch_name}")
        if sha(plain) != patch_record["uncompressed_sha256"] or len(plain) != patch_record["uncompressed_bytes"]:
            raise ValueError(f"hosted applied patch hash mismatch: {plain_rel}")
        if lzma.decompress(compressed, format=lzma.FORMAT_XZ) != plain:
            raise ValueError(f"hosted dispatch patch does not decompress to its applied patch: {patch_name}")
        row = patch_rows[manifest_patch_rel]
        if row["sha256"] != sha(plain) or row["size_bytes"] != len(plain):
            raise ValueError(f"hosted patch source manifest mismatch: {plain_rel}")

    # Pin the driver/harness/workload that define the fixed workload and timer.
    source_hash = {path: row["sha256"] for path, row in dispatch_rows.items()}
    if prereg["driver"]["sha256"] != source_hash["benchmarks/runtime_json_arguments/resolved_json_arguments_pair.py"]:
        raise ValueError("hosted driver hash differs from the preregistration")
    for side in ("baseline", "candidate"):
        if prereg["inputs"][side]["harness_source_sha256"] != source_hash["benchmarks/runtime_json_arguments/resolved_json_arguments_bench.rs"]:
            raise ValueError(f"hosted {side} harness hash differs from the preregistration")
    expected = prereg["expected"]
    if expected["child_launches"] != 0 or expected["completed_call_count"] != 1000:
        raise ValueError("hosted expected correctness oracle changed")
    if expected["output_sha256"] != "4e07408562bedb8b60ce05c1decfe3ad16b72230967de01f640b7e4729b49fce":
        raise ValueError("hosted expected output identity changed")

    # Check retained binary identity receipts without distributing executable bytes.
    excluded = read_json(HOSTED / "excluded-executables.json")
    excluded_rows = {row["side"]: row for row in excluded["executables"]}
    build_audit = audit["build_provenance"]
    if set(excluded_rows) != {"baseline", "candidate"}:
        raise ValueError("hosted excluded-executable identities are incomplete")
    if build_audit["distinct_saved_binaries"] is not True or build_audit["no_build_retries"] is not True:
        raise ValueError("hosted build provenance does not show distinct single-build artifacts")
    for side in ("baseline", "candidate"):
        arm = build_audit["arms"][side]
        excluded_row = excluded_rows[side]
        if excluded_row["sha256"] != arm["binary_sha256"] or excluded_row["size_bytes"] <= 0:
            raise ValueError(f"hosted excluded {side} binary identity is not bound to its receipt")
        receipt = HOSTED / f"evidence/run/build/{side}/build-receipt.json"
        build_result = HOSTED / f"evidence/run/build/{side}/build-result.json"
        if sha(receipt.read_bytes()) != arm["receipt_sha256"] or sha(build_result.read_bytes()) != arm["build_result_sha256"]:
            raise ValueError(f"hosted {side} build receipt identity differs from the audit")
        if arm["fresh_target"] is not True or arm["source_stable"] is not True:
            raise ValueError(f"hosted {side} build is not fresh/source-stable")
    if build_audit["failed_sides"] != [] or build_audit["independently_verified_binary_bytes"] is not True:
        raise ValueError("hosted build audit has failures or lacks binary-byte identity verification")

    attempts = report["attempts"]
    audit_attempts = {item["attempt_id"]: item for item in audit["recomputation"]["attempts"]}
    if len(attempts) != 102 or len(audit_attempts) != 102:
        raise ValueError("hosted audit does not retain all attempt rows")
    attempt_by_id = {}
    attempt_ids = set()
    expected_output_sha = expected["output_sha256"]
    expected_input_sha = expected["input_sha256"]
    binaries = {side: build_audit["arms"][side]["binary_sha256"] for side in ("baseline", "candidate")}

    for attempt in attempts:
        attempt_id = attempt["attempt_id"]
        if attempt_id in attempt_ids:
            raise ValueError(f"duplicate hosted attempt id: {attempt_id}")
        attempt_ids.add(attempt_id)
        side = attempt["side"]
        phase = attempt["phase"]
        if side not in binaries or phase not in {"warmup", "measurement"}:
            raise ValueError(f"invalid hosted attempt identity: {attempt_id}")
        attempt_dir = HOSTED_ATTEMPTS / attempt_id
        expected_sidecars = {"raw.json", "started.json", "finished.json", "stdout.bin", "stderr.bin"}
        if not attempt_dir.is_dir() or {p.name for p in attempt_dir.iterdir() if p.is_file()} != expected_sidecars:
            raise ValueError(f"hosted attempt files are incomplete or unexpected: {attempt_id}")

        raw_bytes = (attempt_dir / "raw.json").read_bytes()
        stdout_bytes = (attempt_dir / "stdout.bin").read_bytes()
        stderr_bytes = (attempt_dir / "stderr.bin").read_bytes()
        raw = json.loads(raw_bytes)
        if sha(raw_bytes) != attempt["raw_report_sha256"] or sha(stdout_bytes) != attempt["stdout_sha256"] or sha(stderr_bytes) != attempt["stderr_sha256"]:
            raise ValueError(f"hosted attempt raw/stdout/stderr digest mismatch: {attempt_id}")
        if attempt["successful"] is not True or attempt["returncode"] != 0 or attempt["timed_out"] is not False:
            raise ValueError(f"hosted attempt did not complete normally: {attempt_id}")
        if attempt["execution_error"] is not None or attempt["validation_error"] is not None or attempt["raw_report_present"] is not True:
            raise ValueError(f"hosted attempt reports an execution/validation error: {attempt_id}")
        if attempt["binary_sha256"] != binaries[side] or attempt["binary_sha256_before"] != binaries[side] or attempt["binary_sha256_after"] != binaries[side]:
            raise ValueError(f"hosted attempt binary identity changed: {attempt_id}")
        expected_fingerprint = audit["source_provenance"]["arm_fingerprints"][side]["sha256"]
        if attempt["source_fingerprint_sha256"] != expected_fingerprint:
            raise ValueError(f"hosted attempt source fingerprint differs from its audit: {attempt_id}")

        if raw.get("all_runs_valid") is not True or raw.get("samples_configured") != 3 or raw.get("warmups") != []:
            raise ValueError(f"hosted raw correctness/sample protocol failed: {attempt_id}")
        workload = raw["workload"]
        if workload.get("input_sha256") != expected_input_sha or workload.get("expected_output_sha256") != expected_output_sha:
            raise ValueError(f"hosted raw workload identity differs from preregistration: {attempt_id}")
        raw_samples = raw.get("samples", [])
        durations = []
        for sample in raw_samples:
            if (
                sample.get("valid") is not True
                or sample.get("output_matches") is not True
                or sample.get("call_count_matches") is not True
                or sample.get("no_child_launches") is not True
                or sample.get("output_sha256") != expected_output_sha
                or sample.get("completed_call_count") != expected["completed_call_count"]
                or sample.get("child_launches") != expected["child_launches"]
            ):
                raise ValueError(f"hosted raw correctness oracle failed: {attempt_id}")
            durations.append(sample["elapsed_ns"])
        if len(durations) != 3 or durations != attempt["sample_duration_ns"]:
            raise ValueError(f"hosted raw sample durations differ from the paired report: {attempt_id}")
        median_duration = statistics.median(durations)
        if median_duration != attempt["median_duration_ns"]:
            raise ValueError(f"hosted attempt median differs from raw samples: {attempt_id}")

        audited = audit_attempts.get(attempt_id)
        if audited is None:
            raise ValueError(f"hosted independent audit omits attempt: {attempt_id}")
        if (
            audited["raw_sha256"] != sha(raw_bytes)
            or audited["stdout_sha256"] != sha(stdout_bytes)
            or audited["stderr_sha256"] != sha(stderr_bytes)
            or audited["sample_duration_ns"] != durations
            or audited["median_duration_ns"] != median_duration
            or audited["binary_sha256"] != binaries[side]
            or audited["binary_unchanged"] is not True
            or audited["successful"] is not True
        ):
            raise ValueError(f"hosted independent attempt recomputation differs: {attempt_id}")
        attempt_by_id[attempt_id] = attempt

    if {p.name for p in HOSTED_ATTEMPTS.iterdir() if p.is_dir()} != attempt_ids:
        raise ValueError("hosted attempt directory set differs from the report; no attempt may be skipped")

    # Require every scheduled pair and every position; no filtering or fallback path is permitted.
    for global_index in range(51):
        rows = sorted((row for row in attempts if row["global_pair_index"] == global_index), key=lambda row: row["position_in_pair"])
        expected_order = ["baseline", "candidate"] if global_index % 2 == 0 else ["candidate", "baseline"]
        expected_phase = "warmup" if global_index < 3 else "measurement"
        expected_phase_index = global_index + 1 if global_index < 3 else global_index - 2
        if len(rows) != 2 or [row["side"] for row in rows] != expected_order:
            raise ValueError(f"hosted pair order/invocation count mismatch at global pair {global_index}")
        if any(row["phase"] != expected_phase or row["phase_pair_index"] != expected_phase_index for row in rows):
            raise ValueError(f"hosted pair phase/index mismatch at global pair {global_index}")
        if [row["position_in_pair"] for row in rows] != [1, 2]:
            raise ValueError(f"hosted pair position mismatch at global pair {global_index}")

    if len(report.get("warmup_pairs", [])) != 3 or len(report.get("measured_pairs", [])) != 48:
        raise ValueError("hosted paired report lost a warmup or measured pair")
    audit_pairs = audit["recomputation"]["measured_pairs"]
    if len(audit_pairs) != 48 or len(report["measured_pairs"]) != 48:
        raise ValueError("hosted independent audit does not preserve all 48 measured pairs")

    deltas = []
    ratios = []
    baseline_medians = []
    candidate_medians = []
    wins = losses = ties = 0
    for index, (reported_pair, audited_pair) in enumerate(zip(report["measured_pairs"], audit_pairs), start=1):
        global_index = index + 2
        pair_rows = [row for row in attempts if row["phase"] == "measurement" and row["phase_pair_index"] == index]
        baseline_rows = [row for row in pair_rows if row["side"] == "baseline"]
        candidate_rows = [row for row in pair_rows if row["side"] == "candidate"]
        if len(baseline_rows) != 1 or len(candidate_rows) != 1:
            raise ValueError(f"hosted measured pair {index} is missing an arm")
        baseline_ns = baseline_rows[0]["median_duration_ns"]
        candidate_ns = candidate_rows[0]["median_duration_ns"]
        delta = candidate_ns - baseline_ns
        ratio = candidate_ns / baseline_ns
        winner = "candidate" if delta < 0 else "baseline" if delta > 0 else "tie"
        if reported_pair.get("global_pair_index") != global_index or reported_pair.get("pair_index") != index:
            raise ValueError(f"hosted report pair numbering changed at pair {index}")
        if reported_pair.get("valid_for_timing") is not True:
            raise ValueError(f"hosted report excludes measured pair {index}")
        if reported_pair.get("baseline_median_ns") != baseline_ns or reported_pair.get("candidate_median_ns") != candidate_ns:
            raise ValueError(f"hosted paired report timings differ from raw attempts at pair {index}")
        if reported_pair.get("candidate_minus_baseline_ns") != delta:
            raise ValueError(f"hosted paired report delta differs from raw attempts at pair {index}")
        if (
            audited_pair.get("global_pair_index") != global_index
            or audited_pair.get("pair_index") != index
            or audited_pair.get("baseline_median_ns") != baseline_ns
            or audited_pair.get("candidate_median_ns") != candidate_ns
            or audited_pair.get("candidate_minus_baseline_ns") != delta
            or audited_pair.get("order") != reported_pair.get("order")
            or audited_pair.get("winner") != winner
        ):
            raise ValueError(f"hosted independent pair recomputation differs at pair {index}")
        require_close(audited_pair.get("candidate_over_baseline_ratio"), ratio, f"hosted pair {index} ratio")
        deltas.append(delta)
        ratios.append(ratio)
        baseline_medians.append(baseline_ns)
        candidate_medians.append(candidate_ns)
        if winner == "candidate":
            wins += 1
        elif winner == "baseline":
            losses += 1
        else:
            ties += 1

    median_delta = statistics.median(deltas)
    median_ratio = statistics.median(ratios)
    baseline_arm_median = statistics.median(baseline_medians)
    candidate_arm_median = statistics.median(candidate_medians)
    ratio_of_arm_medians = candidate_arm_median / baseline_arm_median
    metrics = audit["recomputation"]["metrics"]
    summary = report["paired_summary"]
    if (wins, losses, ties) != (48, 0, 0):
        raise ValueError(f"hosted win/loss/tie count mismatch: {(wins, losses, ties)}")
    if (summary.get("candidate_improvements"), summary.get("candidate_losses"), summary.get("ties"), summary.get("valid_pair_count")) != (48, 0, 0, 48):
        raise ValueError("hosted report summary counts are incorrect")
    if summary.get("paired_candidate_minus_baseline_ns") != deltas:
        raise ValueError("hosted report summary does not retain the complete delta vector")
    require_close(summary.get("median_paired_candidate_minus_baseline_ns"), median_delta, "hosted report median delta")
    expected_metrics = {
        "baseline_arm_median_of_invocation_medians_ns": baseline_arm_median,
        "candidate_arm_median_of_invocation_medians_ns": candidate_arm_median,
        "baseline_sample_count": 48,
        "candidate_sample_count": 48,
        "baseline_wins": 0,
        "candidate_wins": 48,
        "measured_pairs": 48,
        "measurement_invocations": 96,
        "median_paired_delta_ns": median_delta,
        "median_pairwise_candidate_over_baseline_ratio": median_ratio,
        "ratio_of_arm_medians": ratio_of_arm_medians,
        "successful_invocations": 102,
        "ties": 0,
        "warmup_invocations": 6,
    }
    for key, value in expected_metrics.items():
        if isinstance(value, float):
            require_close(metrics.get(key), value, f"hosted audit metric {key}")
        elif metrics.get(key) != value:
            raise ValueError(f"hosted audit metric {key} mismatch: {metrics.get(key)!r} != {value!r}")
    if audit["recomputation"]["attempt_count"] != 102 or audit["recomputation"]["stored_report_pairings_and_summary_match_recomputation"] is not True:
        raise ValueError("hosted audit attempt count or report cross-check is incorrect")
    integrity = audit["input_integrity"]
    for key in (
        "all_attempts_returncode_zero_and_not_timed_out",
        "all_binary_hashes_before_after_match",
        "all_raw_correctness_checks_passed",
        "all_raw_report_stdout_stderr_hashes_recomputed",
    ):
        if integrity.get(key) is not True:
            raise ValueError(f"hosted audit integrity check failed: {key}")

    # This file is the final bundle-local summary; do not allow a draft to pass.
    hosted_readme = (HOSTED / "README.md").read_text(encoding="utf-8").lower()
    if "draft" in hosted_readme or "pending" in hosted_readme:
        raise ValueError("hosted README still has draft/pending wording")
    if any(path.is_file() and path.name.endswith("test-binary") for path in HOSTED.rglob("*")):
        raise ValueError("hosted source-only subtree unexpectedly contains executable bytes")

    return {
        "audit_sha256": sha(audit_path.read_bytes()),
        "hosted_sha256sum_entries": len(hosted_checksums),
        "attempt_count": len(attempts),
        "measured_pairs": 48,
        "wins": wins,
        "losses": losses,
        "ties": ties,
        "median_paired_delta_ns": median_delta,
        "median_pairwise_ratio": median_ratio,
        "ratio_of_arm_medians": ratio_of_arm_medians,
        "excluded_binaries": len(excluded_rows),
        "dispatch_source_files": len(dispatch_rows),
        "applied_product_patches": len(patch_rows),
    }


def verify():
    bundle_manifest = verify_checksum_manifest(ROOT, "bundle.SHA256SUMS")
    local = verify_local_darwin_result()
    hosted = verify_hosted_linux_result()
    unexpected_binaries = [path.relative_to(ROOT).as_posix() for path in ROOT.rglob("*") if path.is_file() and path.name.endswith("test-binary")]
    if unexpected_binaries:
        raise ValueError(f"source-only bundle unexpectedly contains executable blobs: {unexpected_binaries}")
    return {
        "status": "verified",
        "bundle_manifest_entries": len(bundle_manifest),
        "darwin": local,
        "hosted_linux": hosted,
    }


if __name__ == "__main__":
    try:
        print(json.dumps(verify(), sort_keys=True))
    except Exception as error:
        print(f"bundle verification failed: {error}", file=sys.stderr)
        sys.exit(1)
