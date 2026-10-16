#!/usr/bin/env python3
"""Read-only, independent raw-evidence audit for the line-framing memchr trial.

This verifier hashes source/build/evidence files and recomputes trial metrics. It
does not invoke Cargo, Rust tests, benchmark binaries, or product processes.
"""
from __future__ import annotations

import hashlib
import json
import os
import statistics
import subprocess
from collections import Counter
from pathlib import Path
from typing import Any


CAMPAIGN = Path(__file__).resolve().parents[2]
EVIDENCE = CAMPAIGN / "evidence/line-framing-memchr"
RUN = EVIDENCE / "paired-run-attempt-1"
PREREG_PATH = EVIDENCE / "source/preregistration-attempt-3.json"
RESULTS_PATH = RUN / "results.json"
REPORT_PATH = EVIDENCE / "independent-raw-audit.json"
RECEIPT_PATH = EVIDENCE / "independent-raw-audit-receipt.json"
BASE_COMMIT = "a884f6561fd43f45e43b25a3cc2233be17ee3944"
PRODUCT_PATCH = "eeb7c5512fd78c0264d6d96a85b6959c8f1302ac481ab1e98047e3fb52145c94"
HARNESS_SHA256 = "7171a2cb59c6749d656ed8eb5202109c17810e22d5e41c4256daeeec926256ad"
BUILD_DRIVER_SHA256 = "91cc1f09f8e58206fe6358dacf922fb6222a7d252b02406e9d0272e451825d79"
RUNNER_SHA256 = "64258df9b4a3e66d19e903e8ff2065a1c42a6a61a1d56e319f246515671fa7b0"
PREREG_SHA256 = "9e0d3b55cb4c7b065d1b4e30f511d4d9fcc0cba4850eb4f97b51ab301360b92e"
STAGES = (1, 4, 8)
RECORDS = 131_072
FIXTURE_BYTES = 5_000_698
FIXTURE_SHA256 = "2ee93eabea3e8bdd50e7461df8242e78cf9eb9d57f84849154b4411433b698ce"
DIFFERENTIAL_TEST = "runtime::streaming::framing_evidence::borrowed_record_framing_matches_legacy_failures_and_bytes"
STAGE_ORDERS = ((1, 4, 8), (4, 8, 1), (8, 1, 4))
SAMPLE_FIELDS = {
    "round", "stages", "elapsed_ns", "bytes", "output_sha256", "output_matches",
    "result_records", "callback_records", "launched_processes", "successful_statuses",
    "result_counts_match", "launch_count_matches", "callback_counts_match", "sample_valid", "error",
}
ORACLE_FIELDS = (
    "round", "stages", "bytes", "output_sha256", "result_records", "callback_records",
    "launched_processes", "successful_statuses", "result_counts_match", "launch_count_matches",
    "callback_counts_match", "output_matches", "sample_valid", "error",
)
SOURCE_ROOTS = ("src/", "vendor/", ".cargo/")
SOURCE_CONFIG_FILES = {
    "Cargo.toml", "Cargo.lock", "build.rs", "rust-toolchain", "rust-toolchain.toml", "rustfmt.toml",
}


class AuditFailure(RuntimeError):
    pass


def require(ok: bool, label: str, detail: Any = None) -> None:
    if not ok:
        raise AuditFailure(f"{label}: {detail!r}")


def sha_bytes(value: bytes) -> str:
    return hashlib.sha256(value).hexdigest()


def sha_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for block in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def read_json(path: Path) -> Any:
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except Exception as error:
        raise AuditFailure(f"cannot decode JSON {path}: {error}") from error


def git(root: Path, *args: str) -> bytes:
    result = subprocess.run(
        ["git", "-C", str(root), *args], stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=20, check=False,
    )
    if result.returncode != 0:
        raise AuditFailure(f"git {args!r} failed in {root}: {result.stderr.decode(errors='replace')}")
    return result.stdout


def source_fingerprint(root: Path) -> dict[str, Any]:
    tracked = git(root, "ls-files", "-z").split(b"\0")
    paths = sorted(
        item.decode("utf-8") for item in tracked if item
        if item.decode("utf-8").startswith(SOURCE_ROOTS) or item.decode("utf-8") in SOURCE_CONFIG_FILES
    )
    files: dict[str, str] = {}
    combined = hashlib.sha256()
    for relative in paths:
        source = root / relative
        require(source.is_file(), "tracked source input exists", str(source))
        digest = sha_file(source)
        files[relative] = digest
        combined.update(relative.encode("utf-8") + b"\0" + digest.encode("ascii") + b"\n")
    harness = root / "tests/native_stream_status_bench.rs"
    require(harness.is_file() and sha_file(harness) == HARNESS_SHA256, "pinned benchmark harness exists", str(harness))
    files["tests/native_stream_status_bench.rs"] = sha_file(harness)
    combined.update(b"tests/native_stream_status_bench.rs\0")
    combined.update(files["tests/native_stream_status_bench.rs"].encode("ascii") + b"\n")
    return {"sha256": combined.hexdigest(), "files": files}


def expected_fixture() -> tuple[int, str]:
    digest = hashlib.sha256()
    byte_count = 0
    for index in range(RECORDS):
        record = f'{{"n":{index},"value":"record-{index:08}"}}\n'.encode("ascii")
        digest.update(record)
        byte_count += len(record)
    return byte_count, digest.hexdigest()


def report_source_revision(side: str, source: dict[str, Any]) -> str:
    patch = source["product_patch_sha256"] or "clean"
    return f"{side}:{source['commit']}:{source['fingerprint_sha256']}:{patch}"


def stats(rows: list[dict[str, Any]]) -> dict[str, Any]:
    require(bool(rows), "metric group is nonempty")
    baseline = [row["baseline_median_ns"] for row in rows]
    candidate = [row["candidate_median_ns"] for row in rows]
    deltas = [candidate_ns - baseline_ns for baseline_ns, candidate_ns in zip(baseline, candidate)]
    ratios = [candidate_ns / baseline_ns for baseline_ns, candidate_ns in zip(baseline, candidate)]
    base_median = statistics.median(baseline)
    candidate_median = statistics.median(candidate)
    return {
        "pair_count": len(rows),
        "baseline_median_of_pair_medians_ns": base_median,
        "candidate_median_of_pair_medians_ns": candidate_median,
        "median_paired_candidate_minus_baseline_ns": statistics.median(deltas),
        "median_paired_candidate_over_baseline_ratio": statistics.median(ratios),
        "ratio_of_arm_medians": candidate_median / base_median,
        "candidate_faster_pairs": sum(delta < 0 for delta in deltas),
        "candidate_slower_pairs": sum(delta > 0 for delta in deltas),
        "ties": sum(delta == 0 for delta in deltas),
        "all_pair_deltas_ns": deltas,
        "all_pair_ratios": ratios,
    }


def main() -> int:
    prereg = read_json(PREREG_PATH)
    results = read_json(RESULTS_PATH)
    prereg_hash = sha_file(PREREG_PATH)
    checks: list[dict[str, Any]] = []

    def check(name: str, ok: bool, detail: Any = None) -> None:
        checks.append({"name": name, "passed": bool(ok), "detail": detail})
        require(bool(ok), name, detail)

    check("pre-registration SHA matches frozen run pin", prereg_hash == PREREG_SHA256, prereg_hash)
    check("runner/build driver/product patch pins match request", (
        prereg["driver"]["sha256"] == RUNNER_SHA256
        and prereg["inputs"]["build_driver"]["sha256"] == BUILD_DRIVER_SHA256
        and prereg["inputs"]["candidate_product_patch"]["sha256"] == PRODUCT_PATCH
    ))
    check("results bind exact preregistration and inputs", (
        results["preregistration"] == str(PREREG_PATH.resolve())
        and results["preregistration_sha256"] == prereg_hash
        and results["inputs"] == prereg["inputs"]
    ))
    check("paired runner, build driver, plan, patch files match their pins", (
        sha_file(Path(prereg["driver"]["path"])) == RUNNER_SHA256
        and sha_file(Path(prereg["inputs"]["build_driver"]["path"])) == BUILD_DRIVER_SHA256
        and sha_file(Path(prereg["inputs"]["run_plan"]["path"])) == prereg["inputs"]["run_plan"]["sha256"]
        and sha_file(Path(prereg["inputs"]["candidate_product_patch"]["path"])) == PRODUCT_PATCH
    ))
    check("run is complete, clean, and exactly 3 warmups plus 48 measured pairs", (
        results["schema"] == "execute.line-framing-pair-results/1"
        and results["status"] == "complete" and results["integrity_abort"] is None
        and results["pair_count_planned"] == results["pair_count_recorded"] == 51
        and results["attempt_count_planned"] == results["attempt_count_recorded"] == 102
        and len(results["pairs"]) == 51 and results["losses"] == []
    ))
    protocol = prereg["protocol"]
    check("fixed run protocol and runner summary agree", (
        results["protocol"] == protocol and protocol["warmup_pairs"] == 3
        and protocol["measured_pairs"] == 48 and protocol["retries"] == 0
        and protocol["stage_cases"] == list(STAGES)
        and protocol["inner_harness_samples"] == 3 and protocol["inner_harness_warmups"] == 0
        and protocol["stage_order_by_inner_round"] == [list(order) for order in STAGE_ORDERS]
    ))
    check("one deterministic workload is frozen", (
        prereg["workload"]["api"] == "runtime::run_stream"
        and prereg["workload"]["fixture_bytes"] == FIXTURE_BYTES
        and prereg["workload"]["fixture_sha256"] == FIXTURE_SHA256
        and prereg["workload"]["records_per_case_sample"] == RECORDS
    ))

    inputs = prereg["inputs"]
    plan_path = Path(inputs["run_plan"]["path"])
    plan = read_json(plan_path)
    check("run plan pins patch/harness/runner and one preflight before timing", (
        plan["candidate_product_patch_sha256"] == PRODUCT_PATCH
        and plan["benchmark_harness_sha256"] == HARNESS_SHA256
        and plan["paired_runner_sha256"] == RUNNER_SHA256
        and plan["paired_driver_plan"]["candidate_preflight"].startswith("run only the saved candidate --lib test binary")
        and plan["paired_driver_plan"]["retries"] == 0
    ))
    harness_path = Path(inputs["baseline"]["root"]) / "tests/native_stream_status_bench.rs"
    candidate_harness = Path(inputs["candidate"]["root"]) / "tests/native_stream_status_bench.rs"
    check("benchmark harness bytes are identical and pinned in both source roots", (
        sha_file(harness_path) == sha_file(candidate_harness) == HARNESS_SHA256
        and inputs["harness_sha256"] == HARNESS_SHA256
    ))

    source_summaries: dict[str, Any] = {}
    saved_binaries: dict[str, str] = {}
    input_artifacts: dict[str, str] = {}

    def record(path: Path, expected: str | None = None) -> str:
        path = path.resolve(strict=True)
        digest = sha_file(path)
        if expected is not None:
            require(digest == expected, "artifact hash matches pin", {"path": str(path), "expected": expected, "actual": digest})
        input_artifacts[str(path)] = digest
        return digest

    record(PREREG_PATH, PREREG_SHA256)
    record(RESULTS_PATH)
    record(Path(prereg["driver"]["path"]), RUNNER_SHA256)
    record(Path(inputs["build_driver"]["path"]), BUILD_DRIVER_SHA256)
    record(plan_path, inputs["run_plan"]["sha256"])
    patch_path = Path(inputs["candidate_product_patch"]["path"])
    record(patch_path, PRODUCT_PATCH)

    # Verify the source maps from actual files and the exact intended worktree delta.
    expected_patch_files = ["Cargo.lock", "Cargo.toml", "src/runtime/streaming.rs"]
    for side in ("baseline", "candidate"):
        arm = inputs[side]
        root = Path(arm["root"]).resolve(strict=True)
        check(f"{side} worktree starts from the pinned base", git(root, "rev-parse", "HEAD").decode().strip() == BASE_COMMIT)
        actual = source_fingerprint(root)
        check(f"{side} 125-file source fingerprint and every file hash match prereg", actual == arm["source"]["fingerprint"] and actual["sha256"] == arm["source"]["fingerprint_sha256"], {"sha256": actual["sha256"], "file_count": len(actual["files"])})
        staged = git(root, "diff", "--cached", "--name-only").decode().splitlines()
        changed = git(root, "diff", "--name-only", "HEAD").decode().splitlines()
        untracked = [item.decode("utf-8") for item in git(root, "ls-files", "--others", "--exclude-standard", "-z").split(b"\0") if item]
        actual_patch = b"" if side == "baseline" else git(root, "diff", "--binary", "HEAD", "--", *expected_patch_files)
        allowed_changes = [] if side == "baseline" else expected_patch_files
        check(f"{side} tree has no extra staged/tracked/untracked changes", (
            not staged and changed == allowed_changes and untracked == ["tests/native_stream_status_bench.rs"]
        ), {"staged": staged, "tracked": changed, "untracked": untracked})
        if side == "baseline":
            check("baseline has no product patch", arm["source"]["product_patch_sha256"] is None and not actual_patch)
        else:
            check("candidate tracked diff and frozen patch bytes match", sha_bytes(actual_patch) == PRODUCT_PATCH and actual_patch == patch_path.read_bytes())
        check(f"{side} source identity and harness pins agree", (
            arm["source"]["root"] == str(root) and arm["source"]["commit"] == BASE_COMMIT
            and arm["source"]["harness_sha256"] == HARNESS_SHA256
            and arm["source"]["product_patch_sha256"] == (PRODUCT_PATCH if side == "candidate" else None)
        ))
        source_summaries[side] = {
            "root": str(root), "commit": BASE_COMMIT,
            "fingerprint_sha256": actual["sha256"], "file_count": len(actual["files"]),
            "harness_sha256": sha_file(root / "tests/native_stream_status_bench.rs"),
            "product_patch_sha256": PRODUCT_PATCH if side == "candidate" else None,
        }

    # Verify build receipts, exact target/artifact metadata, copied binaries, and captured logs.
    build_pair_path = Path(inputs["build_pair_receipt"]["path"])
    build_pair = read_json(build_pair_path)
    check("build-pair receipt content and hash match the pre-registration", (
        sha_file(build_pair_path) == inputs["build_pair_receipt"]["sha256"]
        and build_pair == inputs["build_pair_receipt_content"]
        and build_pair["status"] == "complete_build_only"
        and build_pair["benchmark_timing"] == "not run" and build_pair["test_execution"] == "not run"
        and build_pair["base_commit"] == BASE_COMMIT
        and build_pair["candidate_product_patch_sha256"] == PRODUCT_PATCH
        and build_pair["build_driver_sha256"] == BUILD_DRIVER_SHA256
    ))
    build_dir = Path(inputs["build_directory"])
    for side in ("baseline", "candidate"):
        arm = inputs[side]
        receipt_path = Path(arm["receipt_path"])
        receipt = read_json(receipt_path)
        check(f"{side} build receipt matches file and pair receipt", (
            sha_file(receipt_path) == arm["receipt_sha256"]
            and receipt == arm["build_receipt_content"]
            and receipt["status"] == "complete_build_only"
            and receipt["benchmark_timing"] == "not run" and receipt["test_execution"] == "not run"
            and receipt["commit"] == BASE_COMMIT
            and receipt["build_driver_sha256"] == BUILD_DRIVER_SHA256
            and receipt["harness_sha256"] == HARNESS_SHA256
        ))
        expected_targets = ["native_stream_status_bench"] if side == "baseline" else ["native_stream_status_bench", "line_framing_lib_test"]
        check(f"{side} build receipt target set is exact", [a["target"] for a in receipt["artifacts"]] == expected_targets)
        build_command_by_target = {
            command["target"]: command for command in receipt["commands"]
            if command.get("phase") in ("build_test_target", "build_library_test_target")
            and command.get("target")
        }
        for artifact_entry in receipt["artifacts"]:
            target = artifact_entry["target"]
            artifact = artifact_entry["artifact"]
            build_command = build_command_by_target.get(target)
            require(build_command is not None, f"{side}/{target}: build command receipt is missing")
            saved = Path(artifact_entry["saved_binary"])
            digest = sha_file(saved)
            require(saved.is_file() and os.access(saved, os.X_OK) and digest == artifact_entry["saved_binary_sha256"],
                    f"{side}/{target}: immutable saved binary hash/executable check failed")
            require(artifact_entry["fresh"] is False and artifact["fresh"] is False
                    and artifact["profile"]["test"] is True
                    and artifact["profile"]["opt_level"] == "3"
                    and artifact["profile"]["debug_assertions"] is False
                    and artifact["target"]["test"] is True,
                    f"{side}/{target}: release test artifact metadata differs")
            require(artifact["executable"] in artifact["filenames"]
                    and Path(artifact["executable"]).name.startswith(artifact["target"]["name"] + "-"),
                    f"{side}/{target}: compiler artifact filename metadata mismatch")
            jsonl_path = Path(build_command["stdout_jsonl"])
            stderr_path = Path(build_command["stderr"])
            require(artifact_entry["cargo_stdout_jsonl_sha256"] == sha_file(jsonl_path)
                    and artifact_entry["cargo_stderr_sha256"] == sha_file(stderr_path),
                    f"{side}/{target}: Cargo JSONL/stderr build log hash mismatch")
            jsonl = jsonl_path.read_text(encoding="utf-8").splitlines()
            artifact_rows = [json.loads(line) for line in jsonl if line.strip()]
            matching = [row for row in artifact_rows if row.get("reason") == "compiler-artifact"
                        and row.get("target", {}).get("name") == artifact["target"]["name"]]
            require(len(matching) == 1 and matching[0] == artifact,
                    f"{side}/{target}: Cargo JSONL does not contain the exact pinned artifact row")
            saved_binaries[f"{side}/{target}"] = digest
            for name, path_value in (("saved_binary", saved), ("cargo_stdout_jsonl", jsonl_path),
                                     ("cargo_stderr", stderr_path)):
                input_artifacts[str(path_value.resolve(strict=True))] = sha_file(path_value)
        require(receipt["source_sha256_before"] == receipt["source_sha256_after"], f"{side}: source changed across build")
        require(receipt["cargo_configuration_checks"]["before_build"]["sha256"] == arm["cargo_configuration_before_build"]["sha256"]
                and receipt["cargo_configuration_checks"]["after_build"]["sha256"] == arm["cargo_configuration_before_build"]["sha256"],
                f"{side}: Cargo configuration changed during build")
        require(receipt["effective_build_environment_sha256"] == arm["effective_build_environment_sha256"],
                f"{side}: effective build environment hash differs")
        input_artifacts[str(receipt_path.resolve(strict=True))] = sha_file(receipt_path)
        input_artifacts[str(Path(arm["binary"]).resolve(strict=True))] = sha_file(Path(arm["binary"]))
        require(Path(arm["binary"]) == Path(arm["build_receipt_content"]["artifacts"][0]["saved_binary"])
                and sha_file(Path(arm["binary"])) == arm["binary_sha256"],
                f"{side}: paired binary not the saved receipt artifact")

    check("pre-clean effective environment is recorded and pinned", (
        inputs["build_tool_metadata"]["build_environment"]["effective_environment_sha256"]
        == build_pair["effective_build_environment_sha256"]
        == inputs["baseline"]["effective_build_environment_sha256"]
        == inputs["candidate"]["effective_build_environment_sha256"]
        and inputs["baseline"]["effective_build_environment"] == inputs["candidate"]["effective_build_environment"]
    ))
    preclean_path = Path(inputs["preclean_metadata"]["path"])
    preclean = read_json(preclean_path)
    check("pre-clean metadata file hash/content is bound", (
        sha_file(preclean_path) == inputs["preclean_metadata"]["sha256"]
        and preclean == inputs["build_tool_metadata"]
        and preclean["preclean_capture"] is True
        and preclean["build_driver_sha256"] == BUILD_DRIVER_SHA256
    ))
    input_artifacts[str(preclean_path.resolve(strict=True))] = sha_file(preclean_path)
    input_artifacts[str(build_pair_path.resolve(strict=True))] = sha_file(build_pair_path)

    stage_binary = Path(inputs["stage_binary"]["path"])
    check("external stage executable is pinned", stage_binary == Path("/bin/cat") and sha_file(stage_binary) == inputs["stage_binary"]["sha256"])
    fixture_bytes, fixture_hash = expected_fixture()
    check("independently generated JSONL fixture matches exact size and hash", fixture_bytes == FIXTURE_BYTES and fixture_hash == FIXTURE_SHA256)
    input_artifacts[str(stage_binary.resolve(strict=True))] = sha_file(stage_binary)

    # Verify the saved candidate differential-test preflight and its journal records.
    preflight = results["candidate_framing_preflight"]
    preflight_path = RUN / "preflight/result.json"
    preflight_file = read_json(preflight_path)
    lib = inputs["candidate"]["lib_test"]
    preflight_stdout = Path(preflight["stdout_path"])
    preflight_stderr = Path(preflight["stderr_path"])
    check("candidate differential-test preflight passed before timing", (
        preflight["passed"] is True and preflight["one_expected_test_ran"] is True
        and preflight["returncode"] == 0 and preflight["timed_out"] is False
        and preflight["spawn_error"] is None and preflight["termination_error"] is None
        and preflight["test"] == DIFFERENTIAL_TEST and preflight_file == preflight
        and preflight["binary"] == lib["binary"] and preflight["binary_sha256"] == lib["binary_sha256"]
        and sha_file(Path(lib["binary"])) == lib["binary_sha256"]
        and sha_file(preflight_stdout) == preflight["stdout_sha256"]
        and sha_file(preflight_stderr) == preflight["stderr_sha256"]
    ))
    preflight_text = preflight_stdout.read_text(encoding="utf-8")
    check("preflight stdout reports exactly the named regression test passing", (
        f"test {DIFFERENTIAL_TEST} ... ok" in preflight_text
        and "test result: ok. 1 passed; 0 failed; 0 ignored" in preflight_text
        and preflight["stderr_bytes"] == 0
    ))
    for path in (preflight_path, preflight_stdout, preflight_stderr):
        input_artifacts[str(path.resolve(strict=True))] = sha_file(path)
    input_artifacts[str(Path(lib["binary"]).resolve(strict=True))] = sha_file(Path(lib["binary"]))

    # Verify host and runtime execution environment at the run boundary.
    journal_path = Path(results["journal"])
    journal_rows = [json.loads(line) for line in journal_path.read_text(encoding="utf-8").splitlines() if line.strip()]
    event_counts = dict(Counter(row.get("event") for row in journal_rows))
    require(journal_rows[0]["event"] == "run_started" and journal_rows[-1]["event"] == "run_finished"
            and journal_rows[-1]["status"] == "complete" and journal_rows[-1]["integrity_abort"] is None,
            "journal does not bracket the complete run")
    start_host = journal_rows[0]["host"]
    stable_host_fields = start_host["identity_policy"]["stable_fields"]
    host_node_start = start_host["uname"][1]
    check("run-start host snapshot matches frozen preregistration", (
        start_host == inputs["host"]
        and start_host["identity_policy"] == inputs["host"]["identity_policy"]
        and start_host["platform"] == inputs["host"]["platform"]
        and start_host["uname"][0] == "Darwin" and start_host["uname"][4] == "arm64"
        and set(stable_host_fields) >= {"platform", "uname.system", "uname.release", "uname.version", "uname.machine", "uname.processor"}
        and start_host["identity_policy"]["recorded_only_fields"].get("uname.node")
    ), {"platform": start_host["platform"], "uname_node_recorded_only": host_node_start,
        "journal_captures_host_at_run_finish": "host" in journal_rows[-1]})
    require(event_counts == {
        "run_started": 1, "candidate_differential_preflight_started": 1,
        "candidate_differential_preflight_finished": 1, "started": 102,
        "finished": 102, "pair_finished": 51, "run_finished": 1,
    }, f"journal event counts differ: {event_counts}")
    input_artifacts[str(journal_path.resolve(strict=True))] = sha_file(journal_path)

    attempt_dirs = RUN / "attempts"
    attempt_folders = sorted(path for path in attempt_dirs.iterdir() if path.is_dir())
    require(len(attempt_folders) == 102, f"expected exactly 102 attempt directories, found {len(attempt_folders)}")
    require(len(results["pairs"]) == 51, "expected exactly 51 pair rows")
    expected_attempt_ids: set[str] = set()
    pair_order_counts: Counter[str] = Counter()
    source_summaries_json: dict[str, Any] = source_summaries
    all_raw_samples: list[dict[str, Any]] = []
    stage_rows: dict[int, list[dict[str, Any]]] = {stage: [] for stage in STAGES}
    pair_details: list[dict[str, Any]] = []

    # Pair-wise audit of 102 reports, 918 samples, all stdout/stderr and sidecars.
    for pair_index, pair in enumerate(results["pairs"]):
        phase = "warmup" if pair_index < 3 else "measured"
        order = ["baseline", "candidate"] if pair_index % 2 == 0 else ["candidate", "baseline"]
        check(f"pair {pair_index} phase/order and validity exact", (
            pair["pair_index"] == pair_index and pair["phase"] == phase and pair["arm_order"] == order
            and pair["attempts_valid"] == {"baseline": True, "candidate": True}
            and pair["losses_by_side"] == {"baseline": [], "candidate": []}
        ))
        pair_order_counts["AB" if order[0] == "baseline" else "BA"] += 1
        reports: dict[str, dict[str, Any]] = {}
        for position, side in enumerate(order):
            attempt_id = f"{phase}-{pair_index:03d}-{side}"
            expected_attempt_ids.add(attempt_id)
            attempt_dir = attempt_dirs / attempt_id
            start_path = attempt_dir / "started.json"
            finish_path = attempt_dir / "finished.json"
            report_path = attempt_dir / "report.json"
            stdout_path = attempt_dir / "stdout.bin"
            stderr_path = attempt_dir / "stderr.bin"
            start = read_json(start_path)
            finish = read_json(finish_path)
            raw_bytes = report_path.read_bytes()
            report = json.loads(raw_bytes.decode("utf-8"))
            arm = inputs[side]
            arm_summary = pair["arm_reports"][side]
            source = arm["source"]
            revision = report_source_revision(side, source)
            require(set(start) >= {"attempt_id", "pair_index", "phase", "side", "arm_position", "binary_sha256_before", "environment", "raw_report_path", "source_revision"}, f"{attempt_id}: incomplete start sidecar")
            require(start["attempt_id"] == attempt_id and start["pair_index"] == pair_index
                    and start["phase"] == phase and start["side"] == side and start["arm_position"] == position
                    and start["binary"] == arm["binary"]
                    and start["binary_sha256_before"] == arm["binary_sha256"]
                    and start["binary_sha256_expected"] == arm["binary_sha256"]
                    and start["raw_report_path"] == str(report_path.resolve())
                    and start["source_revision"] == revision and start["source_identity"] == source,
                    f"{attempt_id}: start identity/build/source differs")
            environment = start["environment"]
            require(environment == {
                "LC_ALL": "C", "OPT_BENCH_REPORT": str(report_path.resolve()), "OPT_BENCH_SAMPLES": "3",
                "OPT_BENCH_SOURCE_REV": revision, "OPT_BENCH_WARMUPS": "0", "PATH": "/bin:/usr/bin",
                "TMPDIR": environment["TMPDIR"], "TZ": "UTC",
            }, f"{attempt_id}: runtime environment differs")
            require("TMPDIR" in environment and environment["TMPDIR"], f"{attempt_id}: TMPDIR missing")
            require(start["command"] == [arm["binary"], "--exact", "public_native_stream_status_polling",
                    "--ignored", "--nocapture", "--test-threads=1"], f"{attempt_id}: command differs")
            require(finish["attempt_id"] == attempt_id and finish["pair_index"] == pair_index
                    and finish["phase"] == phase and finish["side"] == side
                    and finish["arm_position"] == position and finish["returncode"] == 0
                    and finish["timed_out"] is False and finish["spawn_error"] is None
                    and finish["signal_error"] is None and finish["validation_error"] is None
                    and finish["validated"] is True and finish["losses"] == []
                    and finish["binary_sha256_before"] == finish["binary_sha256_after"] == arm["binary_sha256"],
                    f"{attempt_id}: finish status or binary identity differs")
            require(finish["raw_report_document"] == report, f"{attempt_id}: embedded raw report differs from raw file")
            report_hash = sha_bytes(raw_bytes)
            require(finish["report_path"] == str(report_path.resolve())
                    and finish["report_sha256"] == report_hash == arm_summary["report_sha256"]
                    and finish["report_byte_length"] == len(raw_bytes) == arm_summary["report_byte_length"]
                    and finish["report_present"] is True,
                    f"{attempt_id}: raw report path/hash/size differs")
            require(sha_file(stdout_path) == finish["stdout_sha256"] == arm_summary["stdout_sha256"]
                    and sha_file(stderr_path) == finish["stderr_sha256"] == arm_summary["stderr_sha256"]
                    and finish["stdout_path"] == str(stdout_path.resolve())
                    and finish["stderr_path"] == str(stderr_path.resolve()),
                    f"{attempt_id}: stdout/stderr hash or path differs")
            require(finish["input_identity_error"] is None and finish["input_identity_before_error"] is None
                    and finish["source_revision"] == revision and finish["source_identity"] == source,
                    f"{attempt_id}: finish source identity/input check differs")
            require(report["schema"] == "execute.native-stream-status-polling.raw/1"
                    and report["source_revision"] == revision
                    and report["test_binary"] == arm["binary"]
                    and report["test_binary_sha256"] == arm["binary_sha256"]
                    and report["stage_binary"] == inputs["stage_binary"]["path"]
                    and report["stage_binary_sha256"] == inputs["stage_binary"]["sha256"],
                    f"{attempt_id}: report source/binary identity differs")
            workload = report["workload"]
            require(workload["api"] == "runtime::run_stream" and workload["records"] == RECORDS
                    and workload["input_bytes"] == fixture_bytes == FIXTURE_BYTES
                    and workload["input_sha256"] == fixture_hash == FIXTURE_SHA256
                    and report["samples_per_case"] == 3 and report["warmups_per_case"] == 0
                    and report["warmup_samples"] == []
                    and report["interleaving_stage_order_by_round"] == [list(row) for row in STAGE_ORDERS],
                    f"{attempt_id}: report workload/timer/sample setup differs")
            expected_sequence = [(round_index, stage) for round_index, stage_order in enumerate(STAGE_ORDERS)
                                 for stage in stage_order]
            samples = report["raw_samples"]
            require(len(samples) == 9 and [(sample["round"], sample["stages"]) for sample in samples] == expected_sequence,
                    f"{attempt_id}: expected nine samples in the three rotated stage orders")
            validated = finish["validated_report"]
            medians_by_stage = {str(stage): int(statistics.median(
                sample["elapsed_ns"] for sample in samples if sample["stages"] == stage
            )) for stage in STAGES}
            samples_by_stage = {str(stage): [sample for sample in samples if sample["stages"] == stage]
                                for stage in STAGES}
            require(validated == {
                "interleaving_stage_order_by_round": [list(row) for row in STAGE_ORDERS],
                "median_elapsed_ns_by_stage": medians_by_stage,
                "raw_samples": samples,
                "samples_by_stage": samples_by_stage,
                "source_revision": revision,
                "workload": workload,
            }, f"{attempt_id}: independent validated-report projection differs")
            for sample in samples:
                require(set(sample) == SAMPLE_FIELDS, f"{attempt_id}: sample field schema differs")
                stage = sample["stages"]
                require(type(sample["elapsed_ns"]) is int and sample["elapsed_ns"] > 0
                        and sample["bytes"] == FIXTURE_BYTES
                        and sample["output_sha256"] == FIXTURE_SHA256
                        and sample["result_records"] == RECORDS
                        and sample["callback_records"] == RECORDS
                        and sample["launched_processes"] == stage
                        and sample["successful_statuses"] is True
                        and sample["result_counts_match"] is True
                        and sample["launch_count_matches"] is True
                        and sample["callback_counts_match"] is True
                        and sample["output_matches"] is True
                        and sample["sample_valid"] is True and sample["error"] is None,
                        f"{attempt_id}: sample effect/status oracle failed", sample)
                all_raw_samples.append({
                    "attempt_id": attempt_id, "phase": phase, "side": side,
                    "pair_index": pair_index, "round": sample["round"], "stages": stage,
                    "elapsed_ns": sample["elapsed_ns"], "raw_sample": sample,
                })
            require(arm_summary["raw_samples"] == samples
                    and arm_summary["source_revision"] == revision
                    and arm_summary["report_path"] == str(report_path.resolve()),
                    f"{attempt_id}: pair report arm summary does not match raw report")
            reports[side] = report
            for path in (start_path, finish_path, report_path, stdout_path, stderr_path):
                input_artifacts[str(path.resolve(strict=True))] = sha_file(path)

        require(pair["attempt_ids"] == {
            "baseline": f"{phase}-{pair_index:03d}-baseline",
            "candidate": f"{phase}-{pair_index:03d}-candidate",
        }, f"pair {pair_index}: attempt ID map differs")
        if phase == "measured":
            detail: dict[str, Any] = {
                "pair_index": pair_index, "order": order, "phase": phase,
                "attempt_ids": pair["attempt_ids"], "stages": {},
            }
            for stage in STAGES:
                base_samples = [sample for sample in reports["baseline"]["raw_samples"] if sample["stages"] == stage]
                candidate_samples = [sample for sample in reports["candidate"]["raw_samples"] if sample["stages"] == stage]
                base_values = [sample["elapsed_ns"] for sample in base_samples]
                candidate_values = [sample["elapsed_ns"] for sample in candidate_samples]
                base_median = statistics.median(base_values)
                candidate_median = statistics.median(candidate_values)
                delta = candidate_median - base_median
                ratio = candidate_median / base_median
                case = pair["per_case"][str(stage)]
                require(case["baseline_raw_elapsed_ns"] == base_values
                        and case["candidate_raw_elapsed_ns"] == candidate_values
                        and case["baseline_raw_samples"] == base_samples
                        and case["candidate_raw_samples"] == candidate_samples
                        and case["baseline_median_ns"] == base_median
                        and case["candidate_median_ns"] == candidate_median
                        and case["candidate_minus_baseline_ns"] == delta,
                        f"pair {pair_index}, stage {stage}: published raw list/medians/delta differ")
                oracle_equal = all(
                    {key: base[key] for key in ORACLE_FIELDS} == {key: candidate[key] for key in ORACLE_FIELDS}
                    for base, candidate in zip(base_samples, candidate_samples)
                )
                require(oracle_equal and case["cross_arm_sample_oracles_equal"] is True
                        and case["effect_valid"] is True,
                        f"pair {pair_index}, stage {stage}: cross-arm semantic oracles differ")
                row = {
                    "pair_index": pair_index, "order": order,
                    "baseline_raw_elapsed_ns": base_values,
                    "candidate_raw_elapsed_ns": candidate_values,
                    "baseline_median_ns": base_median,
                    "candidate_median_ns": candidate_median,
                    "candidate_minus_baseline_ns": delta,
                    "candidate_over_baseline_ratio": ratio,
                }
                stage_rows[stage].append(row)
                detail["stages"][str(stage)] = row
            pair_details.append(detail)

    expected_ids = {
        f"{'warmup' if index < 3 else 'measured'}-{index:03d}-{side}"
        for index in range(51) for side in ("baseline", "candidate")
    }
    actual_ids = {path.name for path in attempt_folders}
    require(expected_attempt_ids == expected_ids == actual_ids, "attempt folder and journal ID set exact")
    require(len(all_raw_samples) == 918, "all 102 reports retain exactly 918 raw timed samples", len(all_raw_samples))
    check("trial retains all 918 samples with no errors, timeouts, retries, or runner losses", (
        len(all_raw_samples) == 918 and results["losses"] == []
        and all(row["raw_sample"]["sample_valid"] for row in all_raw_samples)
        and all(pair["attempts_valid"] == {"baseline": True, "candidate": True} for pair in results["pairs"])
    ))

    # Cross-check every pair lifecycle event against the retained journal.
    started_ids = [row["attempt_id"] for row in journal_rows if row.get("event") == "started"]
    finished_ids = [row["attempt_id"] for row in journal_rows if row.get("event") == "finished"]
    pair_finished = [row["pair"] for row in journal_rows if row.get("event") == "pair_finished"]
    check("journal contains every attempt and pair exactly once", (
        len(started_ids) == len(set(started_ids)) == 102 and set(started_ids) == expected_ids
        and len(finished_ids) == len(set(finished_ids)) == 102 and set(finished_ids) == expected_ids
        and pair_finished == results["pairs"]
        and event_counts.get("candidate_differential_preflight_started") == 1
        and event_counts.get("candidate_differential_preflight_finished") == 1
    ))
    check("outer order is 3 AB/BA/AB warmups then balanced 24 AB and 24 BA measurements", (
        [pair["arm_order"] for pair in results["pairs"][:3]] == [
            ["baseline", "candidate"], ["candidate", "baseline"], ["baseline", "candidate"]
        ]
        and pair_order_counts["AB"] == 26 and pair_order_counts["BA"] == 25
        and sum(pair["arm_order"] == ["baseline", "candidate"] for pair in results["pairs"][3:]) == 24
        and sum(pair["arm_order"] == ["candidate", "baseline"] for pair in results["pairs"][3:]) == 24
        and results["pairs"][3]["arm_order"] == ["candidate", "baseline"]
    ), dict(pair_order_counts))

    metrics: dict[str, Any] = {}
    all_slow_pairs: list[dict[str, Any]] = []
    for stage in STAGES:
        rows = stage_rows[stage]
        require(len(rows) == 48, f"stage {stage} has 48 measured paired observations")
        metric = stats(rows)
        base_raw = [sample["elapsed_ns"] for sample in all_raw_samples
                    if sample["phase"] == "measured" and sample["side"] == "baseline" and sample["stages"] == stage]
        candidate_raw = [sample["elapsed_ns"] for sample in all_raw_samples
                         if sample["phase"] == "measured" and sample["side"] == "candidate" and sample["stages"] == stage]
        metric["baseline_median_of_144_raw_samples_ns"] = statistics.median(base_raw)
        metric["candidate_median_of_144_raw_samples_ns"] = statistics.median(candidate_raw)
        metric["ratio_of_raw_sample_medians"] = statistics.median(candidate_raw) / statistics.median(base_raw)
        metric["pair_order_slices"] = {}
        for order, label in ((["baseline", "candidate"], "AB"), (["candidate", "baseline"], "BA")):
            subset = [row for row in rows if row["order"] == order]
            order_stats = stats(subset)
            order_stats["pair_indexes"] = [row["pair_index"] for row in subset]
            metric["pair_order_slices"][label] = order_stats
        metric["minimum_delta_pair_index"] = min(rows, key=lambda row: row["candidate_minus_baseline_ns"])["pair_index"]
        metric["maximum_delta_pair_index"] = max(rows, key=lambda row: row["candidate_minus_baseline_ns"])["pair_index"]
        metric["all_48_pair_observations"] = rows
        slower = [row for row in rows if row["candidate_minus_baseline_ns"] > 0]
        metric["candidate_loss_pair_indexes"] = [row["pair_index"] for row in slower]
        all_slow_pairs.extend([{"stage": stage, **row} for row in slower])
        metrics[str(stage)] = metric

    require(metrics["1"]["candidate_faster_pairs"] == 38 and metrics["1"]["candidate_slower_pairs"] == 10,
            "stage 1 quick win/loss count recomputes")
    require(metrics["4"]["candidate_faster_pairs"] == 40 and metrics["4"]["candidate_slower_pairs"] == 8,
            "stage 4 quick win/loss count recomputes")
    require(metrics["8"]["candidate_faster_pairs"] == 37 and metrics["8"]["candidate_slower_pairs"] == 11,
            "stage 8 quick win/loss count recomputes")
    require(len(all_slow_pairs) == 29, "all 29 candidate-loss pair observations are retained", len(all_slow_pairs))

    # Hash and retain every input artifact used by this audit, including attempt sidecars above.
    for path, expected in (
        (Path(inputs["build_pair_receipt"]["path"]), inputs["build_pair_receipt"]["sha256"]),
        (Path(inputs["preclean_metadata"]["path"]), inputs["preclean_metadata"]["sha256"]),
    ):
        record(path, expected)
    for side in ("baseline", "candidate"):
        arm = inputs[side]
        record(Path(arm["receipt_path"]), arm["receipt_sha256"])
        record(Path(arm["binary"]), arm["binary_sha256"])
        if arm["lib_test"]:
            record(Path(arm["lib_test"]["binary"]), arm["lib_test"]["binary_sha256"])

    audit = {
        "schema": "execute.line-framing-memchr-independent-raw-audit/1",
        "result": "pass",
        "scope": "source/build/evidence-only; no Cargo, Rust test, benchmark binary, or product process was invoked by this auditor",
        "pins": {
            "campaign_root": str(CAMPAIGN), "preregistration_path": str(PREREG_PATH.resolve()),
            "preregistration_sha256": prereg_hash, "results_path": str(RESULTS_PATH.resolve()),
            "results_sha256": sha_file(RESULTS_PATH), "runner_path": prereg["driver"]["path"],
            "runner_sha256": RUNNER_SHA256, "build_driver_path": inputs["build_driver"]["path"],
            "build_driver_sha256": BUILD_DRIVER_SHA256, "product_patch_path": str(patch_path.resolve()),
            "product_patch_sha256": PRODUCT_PATCH, "run_plan_path": str(plan_path.resolve()),
            "run_plan_sha256": inputs["run_plan"]["sha256"], "base_commit": BASE_COMMIT,
        },
        "source_arms": source_summaries_json,
        "build_artifacts": saved_binaries,
        "preflight": {
            "binary": lib["binary"], "binary_sha256": lib["binary_sha256"],
            "test": DIFFERENTIAL_TEST, "passed": True, "one_expected_test_ran": True,
            "result_sha256": sha_file(preflight_path), "stdout_sha256": sha_file(preflight_stdout),
            "stderr_sha256": sha_file(preflight_stderr),
        },
        "run_integrity": {
            "status": results["status"], "integrity_abort": results["integrity_abort"],
            "planned_recorded_pairs": [results["pair_count_planned"], results["pair_count_recorded"]],
            "planned_recorded_invocations": [results["attempt_count_planned"], results["attempt_count_recorded"]],
            "attempt_directories": len(attempt_folders), "raw_reports": len(attempt_folders),
            "raw_samples_all_phases": len(all_raw_samples), "measured_samples_per_stage_per_arm": 144,
            "losses": results["losses"], "candidate_loss_count_across_three_stages": len(all_slow_pairs),
            "all_candidate_losses": all_slow_pairs, "journal_event_counts": event_counts,
            "measured_pair_order_counts": {
                "AB": sum(row["arm_order"] == ["baseline", "candidate"] for row in results["pairs"][3:]),
                "BA": sum(row["arm_order"] == ["candidate", "baseline"] for row in results["pairs"][3:]),
            },
            "host_stable_identity": {
                "platform": start_host["platform"], "uname_system": start_host["uname"][0],
                "uname_release": start_host["uname"][2], "uname_machine": start_host["uname"][4],
                "uname_node_start_recorded_only": host_node_start,
                "host_snapshot_scope": "run_started only; the journal run_finished event has no host snapshot",
            },
        },
        "independently_recomputed_metrics": {
            "per_stage": metrics,
            "metric_definition": (
                "For each measured invocation and stage, take the median of its three raw elapsed_ns samples. "
                "Each of the 48 stage-specific paired deltas is candidate median minus baseline median; paired "
                "ratio is candidate median divided by baseline median. AB/BA summaries include 24 pairs each. "
                "All raw attempts, samples, paired values, and outlier-sized deltas are retained without filtering."
            ),
            "all_48_measured_pairs": pair_details,
            "all_918_raw_samples": all_raw_samples,
        },
        "interpretation": (
            "The 48-pair local run_stream microbenchmark shows faster candidate medians in 38/48, 40/48, and "
            "37/48 pairs for 1, 4, and 8 stages respectively. These are narrow fixed-workload results, not a "
            "whole-application or general performance claim; every slower pair and all raw samples are retained."
        ),
        "input_artifact_sha256": dict(sorted(input_artifacts.items())),
        "checks": checks,
        "check_count": len(checks),
        "checks_passed": sum(check["passed"] for check in checks),
        "checks_failed": [check for check in checks if not check["passed"]],
    }
    REPORT_PATH.write_text(json.dumps(audit, allow_nan=False, sort_keys=True, indent=2) + "\n", encoding="utf-8")
    receipt = {
        "schema": "execute.line-framing-memchr-independent-raw-audit-receipt/1",
        "auditor_path": str(Path(__file__).resolve()), "auditor_sha256": sha_file(Path(__file__).resolve()),
        "report_path": str(REPORT_PATH.resolve()), "report_sha256": sha_file(REPORT_PATH),
        "preregistration_path": str(PREREG_PATH.resolve()), "preregistration_sha256": prereg_hash,
        "results_path": str(RESULTS_PATH.resolve()), "results_sha256": sha_file(RESULTS_PATH),
        "runner_sha256": RUNNER_SHA256, "build_driver_sha256": BUILD_DRIVER_SHA256,
        "audited_attempts": 102, "audited_raw_samples": 918,
        "reproducible_command": ["python3", str(Path(__file__).resolve())],
        "no_cargo_tests_or_product_executions": True,
    }
    RECEIPT_PATH.write_text(json.dumps(receipt, sort_keys=True, indent=2) + "\n", encoding="utf-8")
    print(json.dumps({
        "result": "pass", "report_path": str(REPORT_PATH.resolve()),
        "report_sha256": sha_file(REPORT_PATH), "receipt_path": str(RECEIPT_PATH.resolve()),
        "receipt_sha256": sha_file(RECEIPT_PATH), "checks": len(checks),
        "raw_samples": len(all_raw_samples), "metrics": {
            stage: {key: value for key, value in metrics[stage].items()
                    if key not in ("all_48_pair_observations", "all_pair_deltas_ns", "all_pair_ratios", "pair_order_slices")}
            for stage in ("1", "4", "8")
        },
    }, sort_keys=True, indent=2))
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (AuditFailure, OSError, KeyError, TypeError, ValueError) as error:
        raise SystemExit(f"independent line-framing audit failed: {error}")
