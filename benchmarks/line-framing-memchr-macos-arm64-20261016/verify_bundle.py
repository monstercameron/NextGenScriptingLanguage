#!/usr/bin/env python3
"""Verify checksums, frozen source snapshots, raw outputs, and paired summary."""
from __future__ import annotations
import csv, hashlib, json, statistics, sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent

def sha(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as f:
        for block in iter(lambda: f.read(1024 * 1024), b""):
            h.update(block)
    return h.hexdigest()

def read_json(path: Path):
    return json.loads(path.read_text(encoding="utf-8"))

def require(ok: bool, message: str):
    if not ok:
        raise ValueError(message)

def verify_checksums():
    checksum_file = ROOT / "CHECKSUMS.sha256"
    expected = {}
    for line in checksum_file.read_text(encoding="utf-8").splitlines():
        if not line:
            continue
        digest, relative = line.split("  ", 1)
        require(relative not in expected, f"duplicate checksum entry: {relative}")
        p = (ROOT / relative).resolve(strict=True)
        require(p.is_relative_to(ROOT.resolve()), f"checksum path escapes bundle: {relative}")
        require(p.is_file() and not p.is_symlink(), f"not a regular file: {relative}")
        require(sha(p) == digest, f"checksum mismatch: {relative}")
        expected[relative] = digest
    actual = {
        p.relative_to(ROOT).as_posix()
        for p in ROOT.rglob("*")
        if p.is_file() and p != checksum_file
    }
    require(actual == set(expected), f"checksum inventory mismatch: missing={sorted(actual-set(expected))}, extra={sorted(set(expected)-actual)}")
    return expected

def verify_sources(prereg):
    source = ROOT / "source/snapshots"
    patch_hash = sha(ROOT / "source/product.patch")
    require(patch_hash == prereg["inputs"]["candidate_product_patch"]["sha256"], "product patch hash differs")
    harness_hash = sha(ROOT / "source/native_stream_status_bench.rs")
    require(harness_hash == prereg["inputs"]["harness_sha256"], "harness hash differs")
    result = {}
    for side in ("baseline", "candidate"):
        pin = prereg["inputs"][side]["source"]
        expected = dict(pin["fingerprint"]["files"])
        require(expected.get("tests/native_stream_status_bench.rs") == pin["harness_sha256"], f"{side} harness fingerprint differs")
        tree = source / side
        actual = {p.relative_to(tree).as_posix() for p in tree.rglob("*") if p.is_file()}
        require(actual == set(expected), f"{side} source snapshot file set differs")
        for relative, digest in expected.items():
            require(sha(tree / relative) == digest, f"{side} source hash differs: {relative}")
        digest = hashlib.sha256()
        harness_path = "tests/native_stream_status_bench.rs"
        ordered = [(k, expected[k]) for k in sorted(expected) if k != harness_path] + [(harness_path, expected[harness_path])]
        for relative, file_hash in ordered:
            digest.update(relative.encode("utf-8")); digest.update(b"\0")
            digest.update(file_hash.encode("ascii")); digest.update(b"\n")
        require(digest.hexdigest() == pin["fingerprint_sha256"], f"{side} source fingerprint differs")
        result[side] = len(expected)
    base = prereg["inputs"]["baseline"]["source"]["fingerprint"]["files"]
    cand = prereg["inputs"]["candidate"]["source"]["fingerprint"]["files"]
    changed = sorted(key for key in set(base) | set(cand) if base.get(key) != cand.get(key))
    require(changed == ["Cargo.lock", "Cargo.toml", "src/runtime/streaming.rs"], f"unexpected source delta: {changed}")
    return result

def verify_build(prereg):
    build = ROOT / "evidence/build/attempt-1"
    pair_path = build / "build-pair-receipt.json"
    pair = read_json(pair_path)
    require(pair == prereg["inputs"]["build_pair_receipt_content"], "build-pair receipt differs from preregistration")
    require(sha(pair_path) == prereg["inputs"]["build_pair_receipt"]["sha256"], "build-pair receipt hash differs")
    for side in ("baseline", "candidate"):
        row = prereg["inputs"][side]
        receipt_path = build / f"{side}-build-receipt.json"
        receipt = read_json(receipt_path)
        require(receipt == row["build_receipt_content"], f"{side} build receipt content differs")
        require(sha(receipt_path) == row["receipt_sha256"], f"{side} build receipt hash differs")
        targets = {"native_stream_status_bench": row}
        if side == "candidate":
            targets["line_framing_lib_test"] = row["lib_test"]
        for target, identity in targets.items():
            binary = build / Path(identity["binary"]).name
            require(sha(binary) == identity["binary_sha256"], f"{side}/{target} saved binary differs")
            artifact = next(a for a in receipt["artifacts"] if a["target"] == target)
            require(artifact["fresh"] is False and artifact["opt_level"] == "3" and artifact["debug_assertions"] is False, f"{side}/{target} artifact is not fresh optimized release")
    return True

def verify_trial(prereg, audit):
    evidence = ROOT / "evidence"
    results = read_json(evidence / "paired-run/attempt-1/results.json")
    require(results["status"] == "complete" and results["integrity_abort"] is None, "trial is incomplete")
    require(results["pair_count_planned"] == results["pair_count_recorded"] == 51, "pair count differs")
    require(results["attempt_count_planned"] == results["attempt_count_recorded"] == 102, "attempt count differs")
    require(not results["losses"], "runner recorded invocation loss")
    pairs = results["pairs"]
    require([p["pair_index"] for p in pairs] == list(range(51)), "pair indexes are incomplete")
    expected_orders = [("warmup", ["baseline", "candidate"]), ("warmup", ["candidate", "baseline"]), ("warmup", ["baseline", "candidate"])]
    expected_orders.extend(("measured", ["candidate", "baseline"] if i % 2 == 0 else ["baseline", "candidate"]) for i in range(48))
    reports = 0
    measured = []
    for pair, (phase, order) in zip(pairs, expected_orders):
        require(pair["phase"] == phase and pair["arm_order"] == order, f"pair {pair['pair_index']} schedule differs")
        stage_rows = {}
        for side in ("baseline", "candidate"):
            arm = pair["arm_reports"][side]
            attempt_id = arm["attempt_id"]
            report_path = evidence / "paired-run/attempt-1/attempts" / attempt_id / "report.json"
            report = read_json(report_path)
            reports += 1
            source = prereg["inputs"][side]["source"]
            expected_revision = f"{side}:{source['commit']}:{source['fingerprint_sha256']}:{source['product_patch_sha256'] or 'clean'}"
            binary_hash = prereg["inputs"][side]["binary_sha256"]
            require(report["schema"] == "execute.native-stream-status-polling.raw/1", f"{attempt_id}: report schema")
            require(report["source_revision"] == expected_revision, f"{attempt_id}: source revision")
            require(report["test_binary_sha256"] == binary_hash, f"{attempt_id}: saved binary hash")
            require(report["stage_binary_sha256"] == prereg["inputs"]["stage_binary"]["sha256"], f"{attempt_id}: stage binary hash")
            require(report["warmup_samples"] == [] and report["samples_per_case"] == 3, f"{attempt_id}: sample plan")
            require(report["raw_samples"] == arm["raw_samples"], f"{attempt_id}: report differs from results JSON")
            require(len(report["raw_samples"]) == 9, f"{attempt_id}: sample count")
            expected_order = [[1, 4, 8], [4, 8, 1], [8, 1, 4]]
            require(report["interleaving_stage_order_by_round"] == expected_order, f"{attempt_id}: stage rotation")
            for sample in report["raw_samples"]:
                stages = sample["stages"]
                require(stages in (1, 4, 8), f"{attempt_id}: unexpected stage count")
                require(sample["sample_valid"] and sample["error"] is None, f"{attempt_id}: invalid sample")
                require(sample["bytes"] == 5_000_698 and sample["output_sha256"] == "2ee93eabea3e8bdd50e7461df8242e78cf9eb9d57f84849154b4411433b698ce", f"{attempt_id}: output bytes/digest")
                require(sample["result_records"] == sample["callback_records"] == 131_072, f"{attempt_id}: record count")
                require(sample["launched_processes"] == stages and sample["successful_statuses"], f"{attempt_id}: process/status oracle")
                require(sample["result_counts_match"] and sample["launch_count_matches"] and sample["callback_counts_match"] and sample["output_matches"], f"{attempt_id}: boolean output oracle")
                require(type(sample["elapsed_ns"]) is int and sample["elapsed_ns"] > 0, f"{attempt_id}: elapsed time")
            stage_rows[side] = {
                stage: statistics.median(s["elapsed_ns"] for s in report["raw_samples"] if s["stages"] == stage)
                for stage in (1, 4, 8)
            }
        if phase == "measured":
            for stage in (1, 4, 8):
                measured.append({
                    "pair_index": pair["pair_index"], "stage": stage,
                    "order": "AB" if order == ["baseline", "candidate"] else "BA",
                    "baseline_median_ns": stage_rows["baseline"][stage],
                    "candidate_median_ns": stage_rows["candidate"][stage],
                    "delta_ns": stage_rows["candidate"][stage] - stage_rows["baseline"][stage],
                    "ratio": stage_rows["candidate"][stage] / stage_rows["baseline"][stage],
                })
    require(reports == 102, "raw report count differs")
    require(len(measured) == 144 and sum(r["delta_ns"] > 0 for r in measured) == 29, "paired slower outcome count differs")
    for stage in (1, 4, 8):
        selected = [r for r in measured if r["stage"] == stage]
        require(len(selected) == 48, f"stage {stage}: pair count")
        audit_stage = audit["independently_recomputed_metrics"]["per_stage"][str(stage)]
        require(sum(r["delta_ns"] < 0 for r in selected) == audit_stage["candidate_faster_pairs"], f"stage {stage}: faster pair count")
        require(sum(r["delta_ns"] > 0 for r in selected) == audit_stage["candidate_slower_pairs"], f"stage {stage}: slower pair count")
        require(statistics.median([r["delta_ns"] for r in selected]) == audit_stage["median_paired_candidate_minus_baseline_ns"], f"stage {stage}: median delta differs")
        require(statistics.median([r["ratio"] for r in selected]) == audit_stage["median_paired_candidate_over_baseline_ratio"], f"stage {stage}: median ratio differs")
    slower = sorted((r for r in measured if r["delta_ns"] > 0), key=lambda r: (r["stage"], r["pair_index"]))
    with (evidence / "paired-slower-stage-outcomes.csv").open(newline="", encoding="utf-8") as stream:
        csv_rows = list(csv.DictReader(stream))
    require(len(csv_rows) == len(slower) == 29, "slower outcome CSV count differs")
    for row, actual in zip(csv_rows, slower):
        require(int(row["pair_index"]) == actual["pair_index"] and int(row["stage_count"]) == actual["stage"], "slower outcome row identity differs")
        require(int(row["candidate_minus_baseline_ns"]) == actual["delta_ns"], "slower outcome delta differs")
        require(int(row["baseline_pair_median_ns"]) == actual["baseline_median_ns"], "slower outcome baseline differs")
        require(int(row["candidate_pair_median_ns"]) == actual["candidate_median_ns"], "slower outcome candidate differs")
    return {"raw_reports": reports, "raw_samples": reports * 9, "measured_pair_stage_rows": len(measured), "candidate_slower_rows": len(slower)}

def markdown_table_rows(text, header):
    lines = text.splitlines()
    matches = [i for i, line in enumerate(lines) if line.strip() == header]
    require(len(matches) == 1, f"README table header missing or duplicated: {header}")
    rows = []
    for line in lines[matches[0] + 1:]:
        if not line.strip().startswith("|"):
            break
        cells = [cell.strip() for cell in line.strip().strip("|").split("|")]
        if cells and all(set(cell) <= {"-", ":"} for cell in cells):
            continue
        rows.append(cells)
    return rows

def verify_published_tables(audit):
    metrics = audit["independently_recomputed_metrics"]["per_stage"]
    expected_summary = []
    summary_fields = [
        "stage_count", "pair_order", "pairs", "baseline_median_of_144_raw_samples_ns",
        "candidate_median_of_144_raw_samples_ns", "baseline_median_of_pair_medians_ns",
        "candidate_median_of_pair_medians_ns", "median_paired_candidate_minus_baseline_ns",
        "median_paired_candidate_over_baseline_ratio", "ratio_of_arm_medians",
        "candidate_faster_slower_ties",
    ]
    for stage in ("1", "4", "8"):
        overall = metrics[stage]
        expected_summary.append([
            stage, "all", str(overall["pair_count"]),
            str(overall["baseline_median_of_144_raw_samples_ns"]),
            str(overall["candidate_median_of_144_raw_samples_ns"]),
            str(overall["baseline_median_of_pair_medians_ns"]),
            str(overall["candidate_median_of_pair_medians_ns"]),
            str(overall["median_paired_candidate_minus_baseline_ns"]),
            repr(overall["median_paired_candidate_over_baseline_ratio"]),
            repr(overall["ratio_of_arm_medians"]),
            f"{overall['candidate_faster_pairs']} / {overall['candidate_slower_pairs']} / {overall['ties']}",
        ])
        for order in ("AB", "BA"):
            row = overall["pair_order_slices"][order]
            expected_summary.append([
                stage, order, str(row["pair_count"]), "—", "—",
                str(row["baseline_median_of_pair_medians_ns"]),
                str(row["candidate_median_of_pair_medians_ns"]),
                str(row["median_paired_candidate_minus_baseline_ns"]),
                repr(row["median_paired_candidate_over_baseline_ratio"]),
                repr(row["ratio_of_arm_medians"]),
                f"{row['candidate_faster_pairs']} / {row['candidate_slower_pairs']} / {row['ties']}",
            ])
    summary_path = ROOT / "evidence/paired-stage-summary.csv"
    with summary_path.open(newline="", encoding="utf-8") as stream:
        reader = csv.DictReader(stream)
        require(reader.fieldnames == summary_fields, "summary CSV columns differ")
        summary_rows = [[row[field] for field in summary_fields] for row in reader]
    require(summary_rows == expected_summary and len(summary_rows) == 9, "summary CSV differs from independent raw-derived metrics")
    readme = (ROOT / "README.md").read_text(encoding="utf-8")
    summary_header = "| Stages | Order | Pairs | Baseline raw median (ns) | Candidate raw median (ns) | Baseline paired-median center (ns) | Candidate paired-median center (ns) | Median paired Δ (ns) | Median paired ratio | Ratio of arm medians | Faster / slower / ties |"
    require(markdown_table_rows(readme, summary_header) == expected_summary, "README summary table differs from independent raw-derived metrics")

    expected_slow = []
    for stage in ("1", "4", "8"):
        for observation in metrics[stage]["all_48_pair_observations"]:
            if observation["candidate_minus_baseline_ns"] > 0:
                expected_slow.append([
                    str(observation["pair_index"]), stage,
                    "AB" if observation["order"] == ["baseline", "candidate"] else "BA",
                    str(observation["baseline_median_ns"]),
                    str(observation["candidate_median_ns"]),
                    str(observation["candidate_minus_baseline_ns"]),
                    repr(observation["candidate_over_baseline_ratio"]),
                ])
    expected_slow.sort(key=lambda row: (int(row[1]), int(row[0])))
    slow_fields = ["pair_index", "stage_count", "order", "baseline_pair_median_ns", "candidate_pair_median_ns", "candidate_minus_baseline_ns", "candidate_over_baseline_ratio"]
    with (ROOT / "evidence/paired-slower-stage-outcomes.csv").open(newline="", encoding="utf-8") as stream:
        reader = csv.DictReader(stream)
        require(reader.fieldnames == slow_fields, "slower-pair CSV columns differ")
        slow_rows = [[row[field] for field in slow_fields] for row in reader]
    require(slow_rows == expected_slow and len(slow_rows) == 29, "slower-pair CSV differs from independent raw-derived metrics")
    slow_header = "| Pair index | Stages | Order | Baseline median (ns) | Candidate median (ns) | Δ (ns) | Candidate / baseline |"
    require(markdown_table_rows(readme, slow_header) == expected_slow, "README slower-pair table differs from independent raw-derived metrics")
    return {"summary_rows": len(summary_rows), "slower_rows": len(slow_rows)}

def verify_integration_followup(manifest):
    followup = manifest.get("integration_followup")
    require(isinstance(followup, dict), "integration follow-up metadata missing")
    root = ROOT / "evidence/integration-followup"
    receipt = read_json(root / "integration-followup-receipt.json")
    source_receipt = read_json(root / "combined-integration-source-receipt.json")
    validation = read_json(root / "combined-integration-validation-receipt.json")
    require(receipt["commit"] == followup["commit"] == validation["product_commit"] == "da81d266d9f423ad979b8bc45d8139f2a0c272e2", "integration commit identity differs")
    require(receipt["measured_native_patch_sha256"] == manifest["measurement"]["product_patch_sha256"] == source_receipt["native_patch_sha256"] == validation["measured_native_patch_sha256"], "integrated native patch differs from measured patch")
    require(receipt["native_patch_exactly_included"] is True and source_receipt["native_product_exact_measured_patch"] is True, "source receipt does not bind exact native patch")
    patch = root / "combined-integration-product.patch"
    require(sha(patch) == followup["combined_patch_sha256"] == receipt["combined_patch_sha256"] == source_receipt["combined_patch_sha256"] == validation["combined_integration_patch_sha256"], "combined integration patch hash differs")
    require(sha(root / "combined-integration-source-receipt.json") == receipt["source_receipt_sha256"], "source receipt hash differs")
    validation_path = root / "combined-integration-validation-receipt.json"
    require(sha(validation_path) == receipt["validation_receipt_sha256"] == followup["validation_receipt_sha256"], "validation receipt hash differs")
    require(validation["library_passed"] == 313 and validation["library_ignored"] == 20 and validation["integration_passed"] == 68 and validation["total_passed"] == 381 and validation["failed"] == 0, "integration test totals differ")
    require(validation["format"] == "passed" and validation["native_all_target_clippy"] == "passed -D warnings" and "passed -D warnings" in validation["staged_snapshot_native_and_wasm_hook"], "integration validation receipt differs")
    tests = (root / "combined-tests.log").read_text(encoding="utf-8")
    require("313 passed; 0 failed; 20 ignored" in tests and all(f"{count} passed; 0 failed" in tests for count in (1, 10, 6, 4, 15, 11, 12, 9)), "integration test log summary is missing")
    clippy = (root / "combined-clippy.log").read_text(encoding="utf-8")
    require(clippy.count("Finished `dev` profile") == 1, "Clippy follow-up log is incomplete")
    commit_log = (root / "main-commit-hook.log").read_text(encoding="utf-8")
    require("cargo fmt --check" in commit_log and "--target wasm32-unknown-unknown -- -D warnings" in commit_log, "commit hook checks missing")
    for name, digest in receipt["artifacts"].items():
        require(sha(root / name) == digest, f"integration evidence hash differs: {name}")
    return True

def main():
    checksums = verify_checksums()
    manifest = read_json(ROOT / "bundle-manifest.json")
    prereg_path = ROOT / "evidence/preregistration.json"
    prereg = read_json(prereg_path)
    audit_path = ROOT / "evidence/independent-raw-audit.json"
    audit = read_json(audit_path)
    audit_receipt = read_json(ROOT / "evidence/independent-raw-audit-receipt.json")
    require(sha(prereg_path) == manifest["measurement"]["preregistration_sha256"], "preregistration hash differs")
    require(sha(audit_path) == manifest["measurement"]["independent_audit_sha256"], "audit hash differs")
    require(audit["result"] == "pass" and audit["checks_failed"] == [], "independent audit failed")
    require(audit["checks_passed"] == audit["check_count"] == 85, "audit check count differs")
    require(audit_receipt["report_sha256"] == sha(audit_path), "audit receipt does not bind audit report")
    require(sha(ROOT / "source/product.patch") == manifest["measurement"]["product_patch_sha256"], "product patch pin differs")
    require(sha(ROOT / "source/native_stream_status_bench.rs") == manifest["measurement"]["harness_sha256"], "harness pin differs")
    source_counts = verify_sources(prereg)
    verify_build(prereg)
    trial = verify_trial(prereg, audit)
    verify_integration_followup(manifest)
    published_tables = verify_published_tables(audit)
    require(trial["raw_samples"] == 918 and trial["candidate_slower_rows"] == 29, "trial sample/outcome count")
    print(json.dumps({"status":"verified", "checksum_files":len(checksums), "sources":source_counts, "audit_checks":85, "integration_followup":"verified", **published_tables, **trial}, sort_keys=True))

if __name__ == "__main__":
    try:
        main()
    except Exception as exc:
        print(f"verification failed: {type(exc).__name__}: {exc}", file=sys.stderr)
        raise SystemExit(1)
