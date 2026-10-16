#!/usr/bin/env python3
"""Independently audit the retained equal-command-plan control report."""
import argparse
import base64
import hashlib
import itertools
import json
import lzma
import math
from pathlib import Path, PurePosixPath
import statistics

EXPECTED_RAW_SHA256 = "3d29d62a2f0a8ed4b9b8263d4bb0d3bb4fc57b3c98f0dda9d0b826a2b3851b0f"
EXPECTED_PUBLIC_TASK_SHA256 = "e02fc57cb4a40c5ee7f4deb4dc82c80f8a2f649bc614b3adb88295f3ccff1e84"
PUBLIC_TASK_SPEC_RELATIVE = Path("benchmarks/ai_productivity/public-development.json")
ARMS = ("execute_full", "bash", "execute_minimal_inert", "nushell")


def sha256(data):
    return hashlib.sha256(data).hexdigest()


def summarize(values):
    ordered = sorted(values)
    return {
        "sample_count": len(ordered),
        "median_ns": statistics.median(ordered),
        "p95_nearest_rank_ns": ordered[math.ceil(0.95 * len(ordered)) - 1],
        "minimum_ns": ordered[0],
        "maximum_ns": ordered[-1],
    }


def read_report(path):
    blob = path.read_bytes()
    if path.suffix == ".xz":
        blob = lzma.decompress(blob)
    digest = sha256(blob)
    if digest != EXPECTED_RAW_SHA256:
        raise ValueError("raw report digest differs from the recorded campaign receipt")
    return blob, json.loads(blob)


def inferred_repo_root():
    """Find the repository when this file is installed under benchmarks/…/validation."""
    path = Path(__file__).resolve()
    for candidate in path.parents:
        if ((candidate / "benchmarks/matched_language.py").is_file()
                and (candidate / PUBLIC_TASK_SPEC_RELATIVE).is_file()):
            return candidate
    return None


def load_public_scenarios(repo_root, public_task_path):
    import sys

    benchmark_dir = repo_root / "benchmarks"
    sys.path.insert(0, str(benchmark_dir))
    try:
        import matched_language
        definition, scenarios = matched_language.load_scenarios(public_task_path)
    finally:
        if sys.path and sys.path[0] == str(benchmark_dir):
            sys.path.pop(0)
    return definition, scenarios


def check_public_fixture_binding(report, repo_root):
    public_task_path = repo_root / PUBLIC_TASK_SPEC_RELATIVE
    public_task_bytes = public_task_path.read_bytes()
    public_task_sha = sha256(public_task_bytes)
    recorded = report["artifacts"]["full"]["files"]["public_task_specification"]
    if public_task_sha != EXPECTED_PUBLIC_TASK_SHA256 or recorded["sha256"] != EXPECTED_PUBLIC_TASK_SHA256:
        raise ValueError("checked-in public task specification does not match the SHA-256 pinned in the raw campaign artifact")

    definition, scenarios = load_public_scenarios(repo_root, public_task_path)
    scenario_by_key = {(scenario.task_id, scenario.case_id): scenario for scenario in scenarios}
    assert len(scenario_by_key) == 6
    for workload in report["workloads"]:
        fixture = workload["fixture"]
        case_id = fixture["name"].split("/", 1)[1]
        scenario = scenario_by_key[(workload["base_task_id"], case_id)]
        expected_files = {path: sha256(payload) for path, payload in scenario.files.items()}
        actual_files = {path: value["sha256"] for path, value in fixture["input_files"].items()}
        if fixture["input_path"] != scenario.input_value:
            raise ValueError("report fixture input value differs from checked-in public task specification: " + scenario.name)
        if fixture["output_path"] != scenario.output_path:
            raise ValueError("report fixture output path differs from checked-in public task specification: " + scenario.name)
        if actual_files != expected_files:
            raise ValueError("report fixture input-file hashes differ from checked-in public task specification: " + scenario.name)
        expected_output = scenario.expected_stdout
        if (fixture["expected_output_bytes"] != len(expected_output)
                or fixture["expected_output_base64"] != base64.b64encode(expected_output).decode("ascii")
                or fixture["expected_output_sha256"] != sha256(expected_output)):
            raise ValueError("report expected output differs from checked-in public task specification: " + scenario.name)
    return {
        "repo_relative_path": PUBLIC_TASK_SPEC_RELATIVE.as_posix(),
        "sha256": public_task_sha,
        "pinned_sha256_in_raw_artifact": recorded["sha256"],
        "fixture_count_bound_to_public_specification": len(scenario_by_key),
    }


def expected_directories(fixture):
    directories = set()

    def include(path, include_self=False):
        posix = PurePosixPath(path)
        parents = list(posix.parents)
        if include_self:
            parents.append(posix)
        for parent in parents:
            value = str(parent)
            if value not in (".", ""):
                directories.add(value)

    include(fixture["input_path"], include_self=fixture["input_path"] not in fixture["input_files"])
    for path in fixture["input_files"]:
        include(path)
    include(fixture["output_path"])
    return sorted(directories)


def check_observation(row, fixture, expected_files, expected_directories):
    output = base64.b64decode(row["stdout_base64"], validate=True)
    stderr = base64.b64decode(row["stderr_base64"], validate=True)
    expected = base64.b64decode(fixture["expected_output_base64"], validate=True)
    assert row["status"] == 0
    assert output == expected
    assert row["stdout_bytes"] == len(expected) == fixture["expected_output_bytes"]
    assert row["stdout_sha256"] == fixture["expected_output_sha256"] == sha256(expected)
    assert stderr == b"" and row["stderr_bytes"] == 0
    assert row["stderr_sha256"] == sha256(b"")
    assert row["effects"] == {
        "created_output_path": fixture["output_path"],
        "created_output_sha256": fixture["expected_output_sha256"],
        "workspace_after": {
            "directories": expected_directories,
            "files_sha256": expected_files,
        },
    }


def audit(report_path, package_root, repo_root):
    blob, report = read_report(report_path)
    stored_blob = report_path.read_bytes()
    report_ref = report_path.resolve().relative_to(package_root.resolve()).as_posix()
    assert report["schema"] == "execute.equal-command-plan-control/1"
    assert report["status"] == "passed"
    assert report["artifacts"] == report["final_artifacts"]
    assert report["summary"] == {
        "passed_workloads": 6,
        "unavailable_workloads": 0,
        "unavailable_language_cells": 0,
    }
    assert report["requested"]["samples"] == 48
    assert report["requested"]["warmups"] == 5
    permutations = [list(p) for p in itertools.permutations(ARMS)]
    assert report["requested"]["counterbalance"]["permutations"] == permutations
    public_task_binding = check_public_fixture_binding(report, repo_root)

    workloads = report["workloads"]
    assert len(workloads) == 6
    names = [row["name"] for row in workloads]
    assert len(set(names)) == 6
    schedule = report["round_schedule"]
    assert len(schedule) == 53
    assert [row["round"] for row in schedule] == list(range(1, 54))

    schedule_rows = {}
    seen_indices = []
    measured_orders = []
    per_workload_orders = {name: [] for name in names}
    for round_row in schedule:
        number = round_row["round"]
        warmup = number <= 5
        assert round_row["warmup"] is warmup
        order = round_row["arm_order"]
        assert len(order) == 4 and set(order) == set(ARMS)
        assert round_row["cells"] and len(round_row["cells"]) == 6
        if not warmup:
            measured_orders.append(tuple(order))
        index = (number - 1) * 24
        for cell in round_row["cells"]:
            name = cell["workload"]
            assert name in per_workload_orders
            assert cell["arm_order"] == order
            assert len(cell["invocation_indices"]) == 4
            assert cell["invocation_indices"] == list(range(index, index + 4))
            per_workload_orders[name].append(tuple(order))
            for arm, invocation in zip(order, cell["invocation_indices"]):
                schedule_rows[(name, number, arm)] = invocation
                seen_indices.append(invocation)
            index += 4
        assert len({cell["workload"] for cell in round_row["cells"]}) == 6
    assert len(seen_indices) == 1272
    assert sorted(seen_indices) == list(range(1272))
    assert len(measured_orders) == 48
    assert all(measured_orders.count(tuple(order)) == 2 for order in permutations)
    assert all(len(rows) == 53 for rows in per_workload_orders.values())
    assert all(all(rows[5:].count(tuple(order)) == 2 for order in permutations)
               for rows in per_workload_orders.values())

    result_rows = []
    total_raw = total_warmups = total_preflights = 0
    for workload in workloads:
        name = workload["name"]
        fixture = workload["fixture"]
        assert fixture["name"] == name
        assert workload["status"] == "passed"
        expected = {path: value["sha256"] for path, value in fixture["input_files"].items()}
        expected[fixture["output_path"]] = fixture["expected_output_sha256"]
        directories = expected_directories(fixture)
        assert set(expected) == set(fixture["input_files"]) | {fixture["output_path"]}

        assert set(workload["preflight"]) == set(ARMS)
        total_preflights += len(workload["preflight"])
        for row in workload["preflight"].values():
            check_observation(row, fixture, expected, directories)

        warmups = workload["warmup_observations"]
        raw = workload["raw_samples"]
        assert len(warmups) == 20
        assert len(raw) == 192
        total_warmups += len(warmups)
        total_raw += len(raw)
        for row in warmups + raw:
            check_observation(row, fixture, expected, directories)
            arm = row["arm"]
            number = row["round"]
            assert arm in ARMS
            assert row["order"] == next(item["arm_order"] for item in schedule if item["round"] == number)
            assert row["warmup"] is (number <= 5)
            assert row["campaign_invocation_index"] == schedule_rows[(name, number, arm)]
            assert row["elapsed_ns"] >= 0

        by_arm = {arm: [row for row in raw if row["arm"] == arm] for arm in ARMS}
        warm_by_arm = {arm: [row for row in warmups if row["arm"] == arm] for arm in ARMS}
        assert all(len(by_arm[arm]) == 48 for arm in ARMS)
        assert all(len(warm_by_arm[arm]) == 5 for arm in ARMS)
        arm_stats = {}
        for arm in ARMS:
            computed = summarize([row["elapsed_ns"] for row in by_arm[arm]])
            assert workload["summary"][arm] == computed
            arm_stats[arm] = computed

        pairs = {}
        for arm in ("bash", "nushell"):
            pairs[arm] = {}
            for mode, execute_arm in (("full", "execute_full"), ("minimal_inert", "execute_minimal_inert")):
                language_by_round = {row["round"]: row["elapsed_ns"] for row in by_arm[arm]}
                execute_by_round = {row["round"]: row["elapsed_ns"] for row in by_arm[execute_arm]}
                deltas = [language_by_round[n] - execute_by_round[n] for n in sorted(execute_by_round)]
                computed = summarize(deltas)
                assert workload["paired_language_comparisons"][arm][mode]["summary"] == computed
                pairs[arm][mode] = computed

        full_by_round = {row["round"]: row["elapsed_ns"] for row in by_arm["execute_full"]}
        inert_by_round = {row["round"]: row["elapsed_ns"] for row in by_arm["execute_minimal_inert"]}
        catalog_deltas = [inert_by_round[n] - full_by_round[n] for n in sorted(full_by_round)]
        catalog_stats = summarize(catalog_deltas)
        assert workload["paired_execute_catalog_delta"]["summary"] == catalog_stats

        # The retained run's values describe this workload and machine only.
        assert arm_stats["execute_full"]["median_ns"] > arm_stats["bash"]["median_ns"]
        assert arm_stats["execute_full"]["median_ns"] < arm_stats["nushell"]["median_ns"]
        result_rows.append({
            "fixture": name,
            "medians_ns": {arm: arm_stats[arm]["median_ns"] for arm in ARMS},
            "p95_nearest_rank_ns": {arm: arm_stats[arm]["p95_nearest_rank_ns"] for arm in ARMS},
            "paired_execute_minimal_minus_full_ns": catalog_stats["median_ns"],
            "paired_execute_minimal_minus_full_p95_ns": catalog_stats["p95_nearest_rank_ns"],
            "paired_catalog_sample_count": catalog_stats["sample_count"],
        })

    assert (total_raw, total_warmups, total_preflights) == (1152, 120, 24)
    driver = package_root / "equal_command_plan.py"
    actual_driver_sha = sha256(driver.read_bytes())
    expected_driver_sha = report["artifacts"]["full"]["files"]["benchmark_harness"]["sha256"]
    assert actual_driver_sha == expected_driver_sha

    # Verify every controlled source and the one-entry diagnostic catalog against
    # the file hashes recorded before and after the measured campaign.
    control = package_root
    for workload in workloads:
        task = workload["base_task_id"]
        suffixes = {"execute": ".program", "bash": ".sh", "nushell": ".nu"}
        for side, suffix in suffixes.items():
            relative = "sources/{}/{}{}".format(side, task, suffix)
            actual = sha256((control / relative).read_bytes())
            key = "reference:full:{}:{}".format(task, side)
            expected_source = report["artifacts"]["full"]["files"][key]["sha256"]
            assert actual == expected_source
    inert_hash = sha256((control / "catalogs/minimal-unreferenced/cat.json").read_bytes())
    assert inert_hash == report["artifacts"]["minimal_inert"]["manifest_catalog"]["files_sha256"]["cat.json"]

    full_files = report["final_artifacts"]["full"]["files"]
    binary_keys = ("execute_binary", "execute_helper", "bash_binary", "nushell_binary")
    binaries = {key: full_files[key] for key in binary_keys}
    catalog_fingerprints = {
        mode: {key: value for key, value in data["manifest_catalog"].items() if key != "path"}
        for mode, data in report["final_artifacts"].items()
    }
    source_hashes = report["final_artifacts"]["full"]["execute_source"]["files_sha256"]
    source_fingerprint = sha256(json.dumps(source_hashes, sort_keys=True, separators=(",", ":")).encode())
    return {
        "schema": "execute.equal-command-plan-control-summary/1",
        "report": {
            "publication_path": report_ref,
            "raw_xz_sha256": sha256(stored_blob),
            "raw_xz_bytes": len(stored_blob),
            "uncompressed_raw_sha256": sha256(blob),
            "uncompressed_raw_bytes": len(blob),
        },
        "run": {
            "status": report["status"],
            "execute_revision": report["execute_revision"],
            "platform": report["platform"],
            "language_versions": report["language_versions"],
            "requested_measured_samples_per_arm_per_workload": report["requested"]["samples"],
            "warmup_rounds": report["requested"]["warmups"],
            "measured_workloads": len(workloads),
            "measured_invocations": total_raw,
            "warmup_invocations": total_warmups,
            "oracle_preflights": total_preflights,
            "balanced_four_arm_orders": 24,
            "measurements_per_order_per_workload": 2,
            "wall_time_only": True,
            "artifacts_identical_before_after": True,
        },
        "public_task_specification": public_task_binding,
        "binary_fingerprints": binaries,
        "catalog_fingerprints": catalog_fingerprints,
        "execute_source_closure": {
            "file_count": len(source_hashes),
            "sorted_path_sha256_map_sha256": source_fingerprint,
        },
        "package_fingerprints": {
            "driver_sha256": actual_driver_sha,
            "unit_tests_sha256": sha256((control / "tests/test_equal_command_plan.py").read_bytes()),
            "minimal_catalog_cat_json_sha256": inert_hash,
            "controlled_source_sha256": {
                str(path.relative_to(control)): sha256(path.read_bytes())
                for path in sorted((control / "sources").rglob("*")) if path.is_file()
            },
        },
        "workloads": result_rows,
        "audit": {
            "counterbalance_schedule_valid": True,
            "raw_and_warmup_counts_valid": True,
            "status_stdout_stderr_and_workspace_effects_match_independent_fixture_oracle": True,
            "arm_summaries_and_paired_deltas_recomputed_from_raw_samples": True,
            "observed_full_catalog_median_order_each_fixture": "Bash < Execute < Nushell",
            "claims": [
                "current-source, whole-workflow wall-time evidence for these six fixtures and this macOS arm64 environment",
                "the inert-catalog diagnostic uses a one-entry unreferenced catalog and does not represent the default runtime catalog",
                "no cross-revision speedup, universal Bash advantage, CPU-time, or RSS claim",
            ],
        },
    }


def main():
    parser = argparse.ArgumentParser()
    package_root = Path(__file__).resolve().parents[1]
    parser.add_argument("--report", type=Path,
                        default=package_root / "raw/equal-command-plan-macos-arm64-58b42f6.raw.json.xz")
    parser.add_argument("--package-root", type=Path, default=package_root)
    parser.add_argument("--repo-root", type=Path, default=inferred_repo_root(),
                        help="repository root; inferred from benchmarks/equal_command_plan_control/validation when installed in the repository")
    parser.add_argument("--summary", type=Path, default=package_root / "summary.json")
    args = parser.parse_args()
    if args.repo_root is None:
        parser.error("cannot infer repository root from this publication copy; pass --repo-root")
    summary = audit(args.report.resolve(strict=True), args.package_root.resolve(strict=True),
                    args.repo_root.resolve(strict=True))
    args.summary.write_text(json.dumps(summary, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    print(json.dumps({
        "status": "passed",
        "raw_sha256": summary["report"]["uncompressed_raw_sha256"],
        "raw_bytes": summary["report"]["uncompressed_raw_bytes"],
        "workloads": len(summary["workloads"]),
        "samples": summary["run"]["measured_invocations"],
        "warmups": summary["run"]["warmup_invocations"],
        "preflights": summary["run"]["oracle_preflights"],
    }, sort_keys=True))


if __name__ == "__main__":
    main()
