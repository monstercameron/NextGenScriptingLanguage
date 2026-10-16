#!/usr/bin/env python3
"""Verify the portable text JSON decoder evidence bundle using Python stdlib only."""
from __future__ import annotations
import csv, hashlib, json, math, statistics, sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent
MANIFEST = ROOT / "SHA256SUMS.json"
CASE = "text_json_decode_10000"

def check(ok, message):
    if not ok:
        raise SystemExit("FAIL: " + message)

def sha(path):
    h = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            h.update(block)
    return h.hexdigest()

def load(path):
    return json.loads(path.read_text(encoding="utf-8"))

def main():
    expected = load(MANIFEST)
    listed = expected["files"]
    actual_paths = sorted(p.relative_to(ROOT).as_posix() for p in ROOT.rglob("*") if p.is_file() and p != MANIFEST)
    check(actual_paths == sorted(listed), "package file membership differs from SHA256SUMS.json")
    for rel, digest in listed.items():
        check(sha(ROOT / rel) == digest, "sha256 mismatch: " + rel)

    src = load(ROOT / "source/source-fingerprints.json")
    arms = src["arms"]
    base, cand = arms["baseline"], arms["candidate"]
    bf, cf = base["tree"]["files"], cand["tree"]["files"]
    check(base["tree"]["file_count"] == cand["tree"]["file_count"] == 5917, "expected 5,917 source paths per arm")
    check(set(bf) == set(cf), "source-map path sets differ")
    changed = sorted(p for p in bf if bf[p] != cf[p])
    check(changed == ["src/contracts.rs"], "product source map must differ only at src/contracts.rs")
    check(sha(ROOT / "source/source-fingerprints.json") == "3e414d06b9f1fa78f0dc70df25b958d439329636450fd67141d36bd7a140c57a", "source map pin mismatch")
    check(sha(ROOT / "source/text_json_decoder_product.patch") == "95f4d413f86f0905421500f182e372e55a3c30163dc3364810ca71386ca03f36", "product patch pin mismatch")
    check(sha(ROOT / "runner/text_json_decoder_pair.py") == "6dd826353dff66d318ae57b9cddaf7e08eeb529210f82f6f421af47992e893eb", "paired runner pin mismatch")
    check(sha(ROOT / "runner/build_text_json_decoder_pair.py") == "2c25beda9297988a24a86af1dc411c588c29e9c2f0a3c55f8996d22db9c44c06", "build driver pin mismatch")
    check(sha(ROOT / "reviews/independent-raw-audit-attempt-1/audit.py") == "6361da1c1cd7b25e4ffe7f1de13bd65c57505e06fb5f5dce05c548e873bb1271", "independent auditor pin mismatch")
    check(sha(ROOT / "reviews/independent-raw-audit-attempt-1/audit.json") == "0431d2259efec63a9a55cc4903512e91842c3f7fbc4a2c3c0f878ab327ad66f2", "independent audit report pin mismatch")
    audit = load(ROOT / "reviews/independent-raw-audit-attempt-1/audit.json")
    check(audit["status"] == "pass", "independent raw audit did not pass")

    base_contracts = ROOT / "source/baseline/src/contracts.rs"
    cand_contracts = ROOT / "source/candidate/src/contracts.rs"
    harness_base = ROOT / "source/baseline/tests/output_schema_text_json_decode.rs"
    harness_cand = ROOT / "source/candidate/tests/output_schema_text_json_decode.rs"
    check(sha(base_contracts) == bf["src/contracts.rs"] and sha(cand_contracts) == cf["src/contracts.rs"], "contracts snapshots differ from source maps")
    check(harness_base.read_bytes() == harness_cand.read_bytes(), "harness differs across arms")
    check(sha(harness_base) == "2293bc1e04377fdf23b430655ec51bf41643723f9133c3cf181136edbc6c146d", "harness pin mismatch")

    report = load(ROOT / "measurement/paired-run-attempt-1/paired-report.json")
    report_path = ROOT / "measurement/paired-run-attempt-1/paired-report.json"
    prereg_path = ROOT / "measurement/preregistration-attempt-1.json"
    check(audit["report_sha256"] == sha(report_path), "independent audit report pin mismatch")
    check(audit["preregistration_sha256"] == sha(prereg_path), "independent audit preregistration pin mismatch")
    check(audit["runner_sha256"] == sha(ROOT / "runner/text_json_decoder_pair.py"), "independent audit runner pin mismatch")
    check(audit["source_map_sha256"] == sha(ROOT / "source/source-fingerprints.json"), "independent audit source-map pin mismatch")
    attempts = report["attempts"]
    check(report["status"] == "complete" and report["attempt_count"] == len(attempts) == 102, "paired report must contain exactly 102 attempts")
    check(report["successful_attempt_count"] == 102 and report["failed_attempt_count"] == 0, "paired report has failed invocations")
    check(len(report["warmup_pairs"]) == 3 and len(report["measured_pairs"]) == 48, "fixed schedule count mismatch")
    workload = report["workload_contract"]
    for side in ("baseline", "candidate"):
        recorded = report["inputs"][side]["source_fingerprint"]
        check(recorded["file_count"] == arms[side]["tree"]["file_count"] and recorded["sha256"] == arms[side]["tree"]["sha256"], f"{side} report source map summary mismatch")
        check(recorded["files"] == arms[side]["tree"]["files"], f"{side} report source map differs from packaged source map")
    check(workload["case"] == CASE and workload["api"] == "flow::contracts::OutputSchema::decode", "workload scope mismatch")
    check(workload["decoded_string_bytes"] == 4096 and workload["iterations_per_sample"] == 10000, "workload size mismatch")
    check(workload["fixture"] == "ASCII byte x repeated 4096 times, JSON encoded once before timing", "fixture descriptor mismatch")

    by_key = {}
    run_root = ROOT / "measurement/paired-run-attempt-1/attempts"
    for a in attempts:
        key = (a["phase"], a["global_pair_index"], a["side"])
        check(key not in by_key, "duplicate invocation slot")
        by_key[key] = a
        d = run_root / a["attempt_id"]
        check(d.is_dir(), "missing raw attempt directory: " + a["attempt_id"])
        raw_bytes = (d / "raw.json").read_bytes()
        check(sha(d / "raw.json") == a["raw_report_sha256"] and len(raw_bytes) == a["raw_report_bytes"], "raw report digest/length mismatch: " + a["attempt_id"])
        check(sha(d / "stdout.log") == a["stdout_sha256"] and len((d / "stdout.log").read_bytes()) == a["stdout_bytes"], "stdout digest/length mismatch: " + a["attempt_id"])
        check(sha(d / "stderr.log") == a["stderr_sha256"] and len((d / "stderr.log").read_bytes()) == a["stderr_bytes"], "stderr digest/length mismatch: " + a["attempt_id"])
        raw = json.loads(raw_bytes)
        result = raw["results"][CASE]
        samples = result["raw_sample_ns"]
        check(len(samples) == 3 and samples == a["raw_sample_ns"], "sample vector mismatch: " + a["attempt_id"])
        med = statistics.median(samples)
        check(med == result["p50_ns"] == a["median_duration_ns"], "sample median mismatch: " + a["attempt_id"])
        check(raw["workload"] == workload == a["workload"], "workload identity mismatch: " + a["attempt_id"])
        check(raw["measured_samples"] == 3 and raw["warmup_runs"] == 0, "inner sample protocol mismatch")
        oracle = result["semantic_oracles"]
        check(len(oracle) == 3 and oracle == a["semantic_oracles"], "oracle record mismatch: " + a["attempt_id"])
        for o in oracle:
            check(o["decoded_values"] == o["exact_value_matches"] == 10000 and o["errors"] == 0, "semantic oracle failure")
            check(o["decoded_string_bytes"] == 4096 and o["decoded_string_sha256"] == workload["decoded_string_sha256"], "decoded value mismatch")
        check(a["successful"] and a["returncode"] == 0 and not a["timed_out"] and a["execution_error"] is None and a["retry"] == 0, "unsuccessful/retried invocation")
        check(a["binary_sha256"] == a["binary_sha256_after"], "binary changed during invocation")
        started, finished = load(d / "started.json"), load(d / "result.json")
        check(started["attempt_id"] == finished["attempt_id"] == a["attempt_id"], "start/result attempt id mismatch")
        check(started["binary_sha256_before"] == finished["binary_sha256_after"] == a["binary_sha256"], "start/result binary identity mismatch")
        check(finished["successful"] is True and finished["returncode"] == 0, "result receipt is not success")

    check(len(by_key) == 102, "unexpected invocation slot count")
    measured = report["measured_pairs"]
    rows = []
    for i, p in enumerate(measured):
        check(p["phase"] == "measurement" and p["phase_pair_index"] == i + 1 and p["global_pair_index"] == i + 3, "measurement index mismatch")
        expected_order = ["candidate", "baseline"] if p["global_pair_index"] % 2 else ["baseline", "candidate"]
        check(p["order"] == expected_order, "nonalternating AB/BA order")
        b = by_key[("measurement", p["global_pair_index"], "baseline")]
        c = by_key[("measurement", p["global_pair_index"], "candidate")]
        check(p["baseline_median_ns"] == b["median_duration_ns"], "baseline pair table mismatch")
        check(p["candidate_median_ns"] == c["median_duration_ns"], "candidate pair table mismatch")
        check(p["candidate_minus_baseline_ns"] == c["median_duration_ns"] - b["median_duration_ns"], "paired delta mismatch")
        check(p["valid_for_timing"] and p["workload_compatible"] and p["baseline_successful"] and p["candidate_successful"], "timing validity flag false")
        rows.append([i + 1, p["global_pair_index"], "".join(s[0].upper() for s in p["order"]), b["median_duration_ns"], c["median_duration_ns"], p["candidate_minus_baseline_ns"], format(c["median_duration_ns"] / b["median_duration_ns"], ".12f")])

    deltas = [p["candidate_minus_baseline_ns"] for p in measured]
    wins = sum(x < 0 for x in deltas); losses = sum(x > 0 for x in deltas); ties = sum(x == 0 for x in deltas)
    base_medians = [p["baseline_median_ns"] for p in measured]
    cand_medians = [p["candidate_median_ns"] for p in measured]
    ratio = statistics.median([c / b for b, c in zip(base_medians, cand_medians)])
    ratio_of_medians = statistics.median(cand_medians) / statistics.median(base_medians)
    check((wins, losses, ties) == (42, 6, 0), "win/loss/tie totals mismatch")
    check(statistics.median(deltas) == report["paired_summary"]["median_candidate_minus_baseline_ns"] == -864729.0, "paired median mismatch")
    check(deltas == report["paired_summary"]["raw_deltas_ns"], "raw delta vector mismatch")
    check(wins == report["paired_summary"]["wins"] and losses == report["paired_summary"]["losses"] and ties == report["paired_summary"]["ties"], "report tally mismatch")
    order_counts = {"BC": sum("".join(s[0].upper() for s in p["order"]) == "BC" for p in measured), "CB": sum("".join(s[0].upper() for s in p["order"]) == "CB" for p in measured)}
    check(order_counts == {"BC": 24, "CB": 24}, "AB/BA balance mismatch")

    # Check all source-level pinned sources / build receipts and saved benchmark binaries.
    for side in ("baseline", "candidate"):
        receipt = load(ROOT / f"build/{side}/build-receipt.json")
        inp = report["inputs"][side]
        check(sha(ROOT / f"build/{side}/build-receipt.json") == inp["receipt_sha256"], f"{side} build-receipt hash mismatch")
        check(receipt["status"] == "success" and receipt["build_return_code"] == 0 and receipt["clean_return_code"] == 0, f"{side} build failed")
        check(receipt["artifact"]["cargo_json"]["fresh"] is False, f"{side} artifact unexpectedly fresh")
        check(receipt["artifact"]["cargo_json"]["profile"]["opt_level"] == "3", f"{side} is not release opt-level 3")
        binpath = ROOT / f"build/{side}/binaries/output_schema_text_json_decode"
        check(sha(binpath) == receipt["artifact"]["saved_binary_sha256"], f"{side} immutable binary hash mismatch")
        check((binpath.stat().st_mode & 0o777) == 0o555, f"{side} binary is not immutable mode 0555")
        check(receipt["source_fingerprint_sha256"] == arms[side]["tree"]["sha256"], f"{side} build source fingerprint mismatch")
        check(receipt["product_patch_sha256"] == inp["product_patch_sha256"], f"{side} product patch receipt mismatch")
        lines = [json.loads(line) for line in (ROOT / f"build/{side}/build.jsonl").read_text(encoding="utf-8").splitlines() if line.strip()]
        exact_artifacts = [row for row in lines if row.get("reason") == "compiler-artifact" and row.get("target", {}).get("name") == "output_schema_text_json_decode"]
        check(len(exact_artifacts) == 1 and exact_artifacts[0] == receipt["artifact"]["cargo_json"], f"{side} Cargo JSON artifact row does not match receipt")

    fail1 = load(ROOT / "validation/correctness-smokes-attempt-1/failure-receipt.json")
    check(fail1["status"] == "failed_validation_before_candidate_smoke_or_any_paired_trial" and fail1["unattempted"] == ["candidate"] and fail1["scheduled_trial_started"] is False, "attempt-1 failure disclosure missing")
    smoke2 = load(ROOT / "validation/correctness-smokes-attempt-2/receipt.json")
    check(smoke2["status"] == "passed" and len(smoke2["cases"]) == 2 and all(c["status"] == "passed" and c["raw_samples"] == 3 for c in smoke2["cases"]), "correctness smoke attempt 2 did not pass on both arms")
    for side in ("baseline", "candidate"):
        smoke_raw = load(ROOT / f"validation/correctness-smokes-attempt-2/{side}/raw.json")
        smoke_result = smoke_raw["results"][CASE]
        check(len(smoke_result["raw_sample_ns"]) == 3 and len(smoke_result["semantic_oracles"]) == 3, f"{side} smoke raw report does not contain three samples")
        check(all(o["exact_value_matches"] == 10000 and o["errors"] == 0 for o in smoke_result["semantic_oracles"]), f"{side} smoke oracle mismatch")
    libtest = load(ROOT / "validation/root-candidate-library-test-receipt.json")
    check((libtest["library_tests_passed"], libtest["ignored"], libtest["failed"]) == (313, 20, 0), "candidate library test receipt mismatch")
    check(sha(ROOT / "validation/root-candidate-library-test-binary") == libtest["saved_binary_sha256"], "candidate library test binary hash mismatch")
    check(libtest["source_bindings_sha256"] == sha(ROOT / "source/source-fingerprints.json") and libtest["contracts_source_sha256"] == cf["src/contracts.rs"], "candidate library-test source binding mismatch")
    integration = load(ROOT / "validation/main-integration-validation-receipt.json")
    check(integration["product_commit"] == "690c5aa11d05976caa43447be2c7a75bafd34b67" and integration["parent"] == "3be9fe7eac71c371d3ec5c5a5fadb3161a83ea6c" and integration["committed_patch_byte_identical_to_measured"], "integration commit receipt mismatch")
    check(integration["patch_sha256"] == "95f4d413f86f0905421500f182e372e55a3c30163dc3364810ca71386ca03f36", "integration product patch is not the measured patch")
    check(sha(ROOT / "validation/main-commit-hook.log") == integration["hook_log_sha256"], "main commit-hook log hash mismatch")

    # Cross-check the retained loss table with the complete 48-row data, no filtering.
    losses = [rows[i] for i, d in enumerate(deltas) if d > 0]
    with (ROOT / "results/paired-results.csv").open(newline="", encoding="utf-8") as f:
        table = list(csv.DictReader(f))
    check(len(table) == 48, "published pair table must include every measured pair")
    for row, source in zip(table, rows):
        check([row[k] for k in ("phase_pair_index", "global_pair_index", "order", "baseline_median_ns", "candidate_median_ns", "candidate_minus_baseline_ns", "candidate_over_baseline_ratio")] == [str(x) for x in source], "published pair table differs from raw report")
    with (ROOT / "results/losses.csv").open(newline="", encoding="utf-8") as f:
        loss_table = list(csv.DictReader(f))
    check(len(loss_table) == 6, "loss table must contain all six measured losses")
    for row, source in zip(loss_table, losses):
        check([row[k] for k in ("phase_pair_index", "global_pair_index", "order", "baseline_median_ns", "candidate_median_ns", "candidate_minus_baseline_ns", "candidate_over_baseline_ratio")] == [str(x) for x in source], "loss table differs from full paired table")
    markdown = ["# All measured pairs", "", "All 48 preregistered measured pairs are listed; no pair was filtered. Times are per invocation medians across the harness’s three in-process samples. Candidate minus baseline is the signed paired delta in ns.", "", "| Pair | Global index | Order | Baseline ns | Candidate ns | Candidate − baseline ns | Candidate / baseline |", "|---:|---:|:---:|---:|---:|---:|---:|"]
    markdown.extend(f"| {x[0]} | {x[1]} | {x[2]} | {x[3]} | {x[4]} | {x[5]} | {x[6]} |" for x in rows)
    markdown.extend(["", "## All six losses", "", "| Pair | Global index | Order | Baseline ns | Candidate ns | Candidate − baseline ns | Candidate / baseline |", "|---:|---:|:---:|---:|---:|---:|---:|"])
    markdown.extend(f"| {x[0]} | {x[1]} | {x[2]} | {x[3]} | {x[4]} | {x[5]} | {x[6]} |" for x in losses)
    check((ROOT / "results/tables.md").read_text(encoding="utf-8") == "\n".join(markdown) + "\n", "Markdown pair/loss tables differ from full raw-derived rows")
    summary = load(ROOT / "results/summary.json")
    check(summary["wins"] == 42 and summary["losses"] == 6 and summary["ties"] == 0, "summary counts mismatch")
    check(summary["median_candidate_minus_baseline_ns"] == -864729.0, "summary median mismatch")
    check(math.isclose(summary["median_paired_candidate_over_baseline_ratio"], ratio, rel_tol=0, abs_tol=1e-15), "summary paired ratio mismatch")
    check(math.isclose(summary["ratio_of_median_invocation_times"], ratio_of_medians, rel_tol=0, abs_tol=1e-15), "summary ratio-of-medians mismatch")

    print("PASS: manifest exact-membership hashes; 5,917-path map diff; build receipts/artifacts/binaries; 313 library tests, smokes, integration commit; all 102 raw attempts and 306 oracles; full 48-pair AB/BA table; 42 wins, 6 losses, 0 ties; summaries and loss table.")
    print(f"median paired delta ns: {statistics.median(deltas)}; median paired candidate/baseline ratio: {ratio:.12f}; ratio of overall invocation medians: {ratio_of_medians:.12f}")

if __name__ == "__main__":
    main()
