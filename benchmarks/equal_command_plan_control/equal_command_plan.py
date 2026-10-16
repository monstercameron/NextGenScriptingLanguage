#!/usr/bin/env python3
"""Run a paired, equal-command-plan control over the public workflow fixtures.

This campaign-local driver delegates fixture oracles, subprocess checks,
environment setup, summaries, and artifact hashing to matched_language.py.
Actual runs require an explicit idle-window flag; importing or --help is safe.
"""
from __future__ import annotations

import argparse
from dataclasses import replace
from datetime import datetime, timezone
import itertools
import json
from pathlib import Path
import platform
import shutil
import sys
import tempfile
from typing import Any, Dict, List, Mapping, Optional, Sequence, Tuple
from urllib.parse import urlparse

TASKS = ("filesystem_inventory", "layered_config_merge", "typed_event_normalizer")
MODES = ("full", "minimal_inert")
SIDES = ("execute", "bash", "nushell")
ARMS = ("execute_full", "bash", "execute_minimal_inert", "nushell")
ARM_PERMUTATIONS = tuple(itertools.permutations(ARMS))
TOOLS = {"filesystem_inventory": ("find", "basename", "sort", "tee"),
         "layered_config_merge": ("jq", "tee"),
         "typed_event_normalizer": ("jq", "tee")}


def shared(repo: Path) -> Tuple[Any, Any]:
    sys.path.insert(0, str(repo / "benchmarks"))
    import matched_language as mh  # type: ignore[import-not-found]
    import source_pair  # type: ignore[import-not-found]
    return mh, source_pair


def source_paths(root: Path, task: str) -> Dict[str, Path]:
    return {"execute": root / "sources/execute" / (task + ".program"),
            "bash": root / "sources/bash" / (task + ".sh"),
            "nushell": root / "sources/nushell" / (task + ".nu")}


def direct_commands(text: str) -> Tuple[str, ...]:
    import re
    return tuple(re.findall(r'^\s*(?:run|then)\s+"([^"]+)"', text, re.MULTILINE))


def check_sources(root: Path, repo: Path, sp: Any) -> Dict[str, Dict[str, Path]]:
    output_paths = {"filesystem_inventory": "reports/inventory.txt",
                    "layered_config_merge": "reports/effective.json",
                    "typed_event_normalizer": "reports/normalized.jsonl"}
    result = {}
    for task in TASKS:
        paths = source_paths(root, task)
        if any(not p.is_file() or p.is_symlink() for p in paths.values()):
            raise ValueError("missing controlled source for " + task)
        program = paths["execute"].read_text(encoding="utf-8")
        expected = ("find", "sort", "tee") if task == TASKS[0] else ("jq", "tee")
        if direct_commands(program) != expected or "save" in program or '"cat"' in program:
            raise ValueError(task + " does not use the declared direct command plan")
        if "settings_path]" not in program and task == TASKS[1]:
            raise ValueError("config merge must pass its input file directly to jq")
        if "events_path]" not in program and task == TASKS[2]:
            raise ValueError("event normalization must pass its input file directly to jq")
        output_path = output_paths[task]
        if '"' + output_path + '"' not in program:
            raise ValueError(task + " Execute tee output path differs from the controlled reference")
        for side, suffix in (("bash", ".sh"), ("nushell", ".nu")):
            if "tee " + output_path not in paths[side].read_text(encoding="utf-8"):
                raise ValueError(task + " " + side + " tee output path differs from the controlled reference")
            reference = repo / "benchmarks/ai_productivity/public_development/reference_solutions" / side / (task + suffix)
            if sp.sha256_file(paths[side]) != sp.sha256_file(reference):
                raise ValueError(task + " " + side + " source differs from the preserved public reference")
        result[task] = paths
    return result


def control_cases(scenarios: Sequence[Any]) -> List[Any]:
    return [replace(s, task_id="equal_command_plan_control_" + s.task_id) for s in scenarios]


def commands(binary: Path, shells: Mapping[str, Optional[Path]], paths: Mapping[str, Path],
             catalogs: Mapping[str, Path], scenario: Any, timeout_ms: int) -> Dict[str, List[str]]:
    base = [str(binary), str(paths["execute"]), "--input", scenario.program_input + "=" + scenario.input_value,
            "--command-timeout-ms", str(timeout_ms)]
    result = {"execute_full": [*base, "--manifest-dir", str(catalogs["full"])],
              "execute_minimal_inert": [*base, "--manifest-dir", str(catalogs["minimal_inert"])],
              "bash": [str(shells["bash"]), "--noprofile", "--norc", str(paths["bash"])]}
    if shells["nushell"] is not None:
        result["nushell"] = [str(shells["nushell"]), "--no-config-file", "--no-history", str(paths["nushell"])]
    return result


def arm_order(round_index: int) -> Tuple[str, ...]:
    """Cycle through every possible four-arm order exactly once per 24 rounds."""
    return ARM_PERMUTATIONS[round_index % len(ARM_PERMUTATIONS)]


def nushell_version_from_release_url(url: str) -> str:
    parsed = urlparse(url)
    parts = parsed.path.rstrip("/").split("/")
    if (parsed.scheme != "https" or parsed.netloc != "github.com" or len(parts) < 7
            or parts[-3] != "download" or not parts[-2] or not parts[-1]):
        raise ValueError("expected official Nushell release asset URL /releases/download/<version>/<asset>")
    return parts[-2]


def run(args: argparse.Namespace) -> Dict[str, Any]:
    if not args.confirm_idle_window:
        raise ValueError("actual runs require --confirm-idle-window after builds and tests have stopped")
    if not 1 <= args.samples <= 100 or args.samples % len(ARM_PERMUTATIONS) != 0 or not 0 <= args.warmups <= 20:
        raise ValueError("samples must be a multiple of 24 up to 96 and warmups must be 0..20")
    if not 0 < args.timeout_seconds <= 120 or not 0 < args.command_timeout_ms <= int(args.timeout_seconds * 1000):
        raise ValueError("timeouts must be positive and command timeout cannot exceed the outer timeout")
    repo = args.repo_root.expanduser().resolve(strict=True)
    mh, sp = shared(repo)
    report_path = sp.validate_report_path(args.report)
    root = Path(__file__).resolve().parent
    sources = check_sources(root, repo, sp)
    full = args.manifest_dir.expanduser().resolve(strict=True)
    inert = args.inert_catalog.expanduser().resolve(strict=True)
    if full == inert:
        raise ValueError("full and minimal inert catalogs must be different directories")
    for catalog in (full, inert):
        if not catalog.is_dir():
            raise ValueError("catalog path is not a directory: " + str(catalog))
    cat_copy, cat_original = inert / "cat.json", repo / "manifests/cat.json"
    inert_files = list(inert.iterdir())
    if (len(inert_files) != 1 or inert_files[0] != cat_copy or not cat_copy.is_file()
            or cat_copy.is_symlink() or sp.sha256_file(cat_copy) != sp.sha256_file(cat_original)):
        raise ValueError("minimal inert catalog must contain the repository's unchanged cat.json")
    if any("cat" in direct_commands(p["execute"].read_text(encoding="utf-8")) for p in sources.values()):
        raise ValueError("the minimal catalog's cat entry is referenced by a controlled command")

    execute = sp.verify_executable(args.execute_binary, "Execute binary")
    shells = {"bash": sp.verify_executable(args.bash_binary, "Bash binary"),
              "nushell": sp.verify_executable(args.nushell_binary, "Nushell binary") if args.nushell_binary else None}
    archive = mh._verify_nushell_provenance(shells["nushell"], args.nushell_archive,
                                             args.nushell_release_url, args.nushell_archive_sha256)
    path = mh._build_path()
    tools = {name: Path(found).resolve(strict=True) for name in sorted(set(sum(TOOLS.values(), ())))
             if (found := shutil.which(name, path=path)) is not None}
    binaries = {"execute": execute, "bash": shells["bash"], "nushell": shells["nushell"]}
    helpers = {"execute": sp.helper_path(execute), "bash": None, "nushell": None}
    base_scenarios = mh.load_scenarios()[1]
    cases = control_cases(base_scenarios)
    execute_source_root = args.execute_source_root.expanduser().resolve(strict=True)
    catalogs = {"full": full, "minimal_inert": inert}
    pathmap = {task: paths for task, paths in sources.items()}
    script_hash_paths = {mode + ":" + task + ":" + side: path
                         for task, paths in pathmap.items() for side, path in paths.items() for mode in MODES}

    def artifacts() -> Dict[str, Any]:
        result = {}
        for mode, catalog in catalogs.items():
            snap = mh.artifact_snapshot(binaries, helpers, tools, catalog, script_hash_paths,
                                        Path(__file__).resolve(), execute_source_root,
                                        args.execute_revision, archive)
            snap["shared_harnesses"] = {
                name: sp.file_snapshot(repo / "benchmarks" / filename, name)
                for name, filename in (("matched_language", "matched_language.py"), ("source_pair", "source_pair.py"))
            }
            result[mode] = snap
        return result

    before = artifacts()
    prepared = []
    tool_missing = {}
    with tempfile.TemporaryDirectory(prefix="equal-command-plan-") as temp:
        scratch = Path(temp)
        home, tmp = scratch / "home", scratch / "tmp"
        home.mkdir(); tmp.mkdir(); (home / ".config").mkdir(); (home / ".cache").mkdir()
        for scenario in cases:
            ws = mh.fixture_workspace(scratch / "workspaces", scenario)
            task = next(t for t in TASKS if scenario.task_id.endswith(t))
            task_missing = sorted(set(TOOLS[task]) - set(tools))
            tool_missing[scenario.name] = task_missing
            cmd = commands(execute, shells, sources[task], catalogs, scenario, args.command_timeout_ms)
            env = {arm: mh._safe_environment("execute" if arm.startswith("execute_") else arm,
                                             ws, home, tmp, path, scenario) for arm in cmd}
            item = {"task": task, "scenario": scenario, "workspace": ws,
                    "initial": mh.safe_relative_workspace_snapshot(ws), "commands": cmd, "env": env,
                    "row": {"name": scenario.name, "task_id": scenario.task_id, "base_task_id": task,
                            "catalog_modes": {mode: str(catalog) for mode, catalog in catalogs.items()},
                            "fixture": mh._workspace_state(scenario, ws),
                    "sources": {side: {"path": str(p.resolve()), "bytes": p.stat().st_size,
                                       "sha256": sp.sha256_file(p)}
                                for side, p in sources[task].items()},
                            "arms": {arm: ({"status": "pending"} if arm in cmd else
                                           {"status": "unavailable", "reason": "language executable not supplied"})
                                     for arm in ARMS},
                            "preflight": {}, "warmup_observations": [],
                            "raw_samples": [], "summary": {}}}
            prepared.append(item)

        # Require every available arm to pass the independent byte/effect oracle
        # before creating a report or recording sample times.
        for item in prepared:
            scenario = item["scenario"]
            task_missing = tool_missing[scenario.name]
            if task_missing:
                item["row"].update(status="unavailable", unavailable_reason="missing tools: " + ", ".join(task_missing))
                item["row"]["arms"] = {
                    arm: {"status": "unavailable", "reason": "required tools unavailable: " + ", ".join(task_missing)}
                    for arm in ARMS
                }
                item["commands"] = {}
                continue
            obs = {arm: mh._run_one(scenario, "execute" if arm.startswith("execute_") else arm,
                                    item["commands"][arm], item["workspace"], item["env"][arm],
                                    args.timeout_seconds, sp.run_process,
                                    scenario.name + " arm=" + arm + " phase=preflight")
                   for arm in ARMS if arm in item["commands"]}
            if len({mh._observation_signature(value) for value in obs.values()}) != 1:
                raise ValueError(scenario.name + " language preflight outputs/status/effects differ")
            item["preflight"] = {side: {k: v for k, v in value.items() if k != "elapsed_ns"}
                                  for side, value in obs.items()}
            item["row"]["preflight"] = item["preflight"]
            item["row"]["available_arms"] = list(obs)
            item["row"]["arms"] = {arm: ({"status": "pending"} if arm in obs else
                                          {"status": "unavailable", "reason": "language executable not supplied"})
                                    for arm in ARMS}
            item["row"]["status"] = "running"

        available = [i for i in prepared if i["commands"]]
        if not available:
            raise ValueError("no workload has all required external tools")
        versions = {"execute": args.execute_revision, "bash": mh._nearest_version(shells["bash"], ("--version",)),
                    "nushell": mh._nearest_version(shells["nushell"], ("--version",)) if shells["nushell"] else "unavailable"}
        if shells["nushell"] and args.nushell_release_url:
            expected_version = nushell_version_from_release_url(args.nushell_release_url)
            if versions["nushell"] != expected_version:
                raise ValueError("Nushell binary does not match the pinned release URL")
        report = {
            "schema": "execute.equal-command-plan-control/1", "status": "running",
            "recorded_at_utc": datetime.now(timezone.utc).isoformat(),
            "platform": {"system": platform.platform(), "machine": platform.machine(), "python": sys.version.split()[0]},
            "language_versions": versions, "execute_revision": args.execute_revision, "artifacts": before,
            "minimal_inert_catalog_entry": {"path": str(cat_copy.resolve()), "copied_from": str(cat_original.resolve()),
                                            "sha256": sp.sha256_file(cat_copy), "canonical_name": "cat", "invoked": False},
            "environment": {"controlled_tool_path": path, "locale": "C", "timezone": "UTC",
                            "outer_wall_time_only": True, "cpu_or_rss_inferred": False},
            "method": {"scope": "paired public whole-workflow startup/pipeline control; no isolated interpreter or universal speed claim",
                       "equal_plan": "find -> sort -> tee; jq with input file argument -> tee, with the same output paths across all arms",
                       "catalog_scope": "Full catalog versus a one-entry valid but unreferenced catalog; not a physically empty catalog and not default-runtime behavior",
                       "correctness": "matched_language fixture byte oracle, exact status/stdout/stderr and allowed workspace effects before samples and on every invocation",
                       "sampling": "Four-arm balanced interleaving: Execute with each catalog, Bash, and Nushell. Workload order alternates each round; warmups and samples retained raw. Shell arms run once because they do not read the Execute catalog."},
            "requested": {"samples": args.samples, "warmups": args.warmups, "timeout_seconds": args.timeout_seconds,
                          "command_timeout_ms": args.command_timeout_ms,
                          "counterbalance": {"permutations": [list(x) for x in ARM_PERMUTATIONS],
                                              "cycle_rounds": len(ARM_PERMUTATIONS),
                                              "measured_samples_per_arm_per_workload": args.samples}},
            "unmatched_cells": ([] if shells["nushell"] else [{"language": "nushell", "status": "unavailable",
                                                                  "reason": "No --nushell-binary supplied"}]),
            "workloads": [i["row"] for i in prepared], "round_schedule": []}
        mh._save_report(report_path, report, create=True)
        try:
            total = args.warmups + args.samples
            by_key = {(i["task"], i["scenario"].case_id): i for i in prepared}
            invocation_index = 0
            for r in range(total):
                warmup = r < args.warmups
                order = arm_order(r)
                scenarios = list(base_scenarios)
                if r % 2:
                    scenarios.reverse()
                schedule = {"round": r + 1, "warmup": warmup, "arm_order": list(order), "cells": []}
                for scenario in scenarios:
                    task = scenario.task_id
                    item = by_key[(task, scenario.case_id)]
                    if not item["commands"]:
                        continue
                    actual_order = [arm for arm in order if arm in item["commands"]]
                    cell = {"workload": item["scenario"].name, "arm_order": actual_order,
                            "invocation_indices": []}
                    for arm in actual_order:
                        side = "execute" if arm.startswith("execute_") else arm
                        value = mh._run_one(item["scenario"], side, item["commands"][arm], item["workspace"], item["env"][arm],
                                            args.timeout_seconds, sp.run_process,
                                            item["scenario"].name + " arm=" + arm + " phase=" + ("warmup" if warmup else "sample"))
                        if mh._observation_signature(value) != mh._observation_signature(item["preflight"][arm]):
                            raise ValueError(item["scenario"].name + " diverged from preflight on " + arm)
                        catalog_mode = "full" if arm == "execute_full" else "minimal_inert" if arm == "execute_minimal_inert" else None
                        sample_row = {"round": r + 1, "order": actual_order, "arm": arm, "side": side,
                                      "catalog_mode": catalog_mode, "warmup": warmup,
                                      "campaign_invocation_index": invocation_index, **value}
                        (item["row"]["warmup_observations"] if warmup else item["row"]["raw_samples"]).append(sample_row)
                        cell["invocation_indices"].append(invocation_index)
                        invocation_index += 1
                    schedule["cells"].append(cell)
                report["round_schedule"].append(schedule)
                mh._save_report(report_path, report)

            for item in prepared:
                row = item["row"]
                if not item["commands"]:
                    continue
                samples = row["raw_samples"]
                row["summary"] = {arm: mh._summary([int(x["elapsed_ns"]) for x in samples if x["arm"] == arm])
                                   for arm in item["commands"]}
                by_round = {x["round"]: {} for x in samples}
                for sample in samples:
                    by_round[sample["round"]][sample["arm"]] = sample
                row["paired_language_comparisons"] = {}
                for side in ("bash", "nushell"):
                    if side not in item["commands"]:
                        continue
                    row["paired_language_comparisons"][side] = {}
                    for mode in MODES:
                        pairs = [{"round": number,
                                  "execute_ns": int(values["execute_" + mode]["elapsed_ns"]),
                                  "language_ns": int(values[side]["elapsed_ns"]),
                                  "language_minus_execute_ns": int(values[side]["elapsed_ns"]) - int(values["execute_" + mode]["elapsed_ns"])}
                                 for number, values in sorted(by_round.items())
                                 if "execute_" + mode in values and side in values]
                        row["paired_language_comparisons"][side][mode] = {
                            "paired_samples": pairs,
                            "summary": mh._summary([x["language_minus_execute_ns"] for x in pairs]),
                        }
                catalog_deltas = [
                    {"round": number,
                     "minimal_minus_full_ns": int(values["execute_minimal_inert"]["elapsed_ns"])
                     - int(values["execute_full"]["elapsed_ns"])}
                    for number, values in sorted(by_round.items())
                    if "execute_full" in values and "execute_minimal_inert" in values
                ]
                row["paired_execute_catalog_delta"] = {
                    "paired_samples": catalog_deltas,
                    "summary": mh._summary([x["minimal_minus_full_ns"] for x in catalog_deltas]),
                }
                row["status"] = "passed"
                row["arms"] = {
                    arm: ({"status": "passed"} if arm in item["commands"] else
                          {"status": "unavailable", "reason": "language executable not supplied"})
                    for arm in ARMS
                }
                final = mh.safe_relative_workspace_snapshot(item["workspace"])
                expected_files = dict(item["initial"]["files_sha256"])
                expected_files[item["scenario"].output_path] = sp.sha256_bytes(item["scenario"].expected_stdout)
                if final["files_sha256"] != expected_files or final["directories"] != item["initial"]["directories"]:
                    raise ValueError(item["scenario"].name + " final workspace effects differ from the fixture oracle")
            after = artifacts()
            for mode in MODES:
                sp.assert_artifacts_unchanged(before[mode], after[mode])
            report["final_artifacts"] = after
            all_available = shells["nushell"] is not None and all(i["row"]["status"] == "passed" for i in prepared)
            report["status"] = "passed" if all_available else "completed_with_unavailable"
            report["summary"] = {"passed_workloads": sum(x["row"]["status"] == "passed" for x in prepared),
                                  "unavailable_workloads": sum(x["row"]["status"] == "unavailable" for x in prepared),
                                  "unavailable_language_cells": sum(
                                      cell["status"] == "unavailable"
                                      for row in report["workloads"] for cell in row["arms"].values()
                                  )}
            mh._save_report(report_path, report)
        except BaseException as error:
            report["status"] = "failed"
            report["failure"] = type(error).__name__ + ": " + str(error)
            mh._save_report(report_path, report)
            raise
    return report


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo-root", type=Path, required=True)
    parser.add_argument("--execute-source-root", type=Path, required=True)
    parser.add_argument("--execute-binary", type=Path, required=True)
    parser.add_argument("--manifest-dir", type=Path, required=True)
    parser.add_argument("--inert-catalog", type=Path, default=Path(__file__).resolve().parent / "catalogs/minimal-unreferenced")
    parser.add_argument("--execute-revision", required=True)
    parser.add_argument("--bash-binary", type=Path, default=Path("/bin/bash"))
    parser.add_argument("--nushell-binary", type=Path)
    parser.add_argument("--nushell-archive", type=Path)
    parser.add_argument("--nushell-archive-sha256")
    parser.add_argument("--nushell-release-url")
    parser.add_argument("--report", type=Path, required=True)
    parser.add_argument("--samples", type=int, default=48)
    parser.add_argument("--warmups", type=int, default=5)
    parser.add_argument("--timeout-seconds", type=float, default=30)
    parser.add_argument("--command-timeout-ms", type=int, default=25000)
    parser.add_argument("--confirm-idle-window", action="store_true")
    args = parser.parse_args()
    if not args.confirm_idle_window:
        parser.error("actual runs require --confirm-idle-window after builds and tests have stopped")
    try:
        result = run(args)
    except Exception as error:
        parser.error(str(error))
    print(json.dumps({"status": result["status"], "workloads": len(result["workloads"]),
                      "summary": result.get("summary", {})}, indent=2))


if __name__ == "__main__":
    main()
