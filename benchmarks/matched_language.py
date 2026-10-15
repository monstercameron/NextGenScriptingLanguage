#!/usr/bin/env python3
"""Matched current-source Execute/Bash/Nushell workflow benchmark.

The measured tasks are checked-in public development references, not model
trials. This measures whole-workflow execution only; it does not compare
programmer effort, generated tokens, maintainability, or isolated interpreter
throughput. No builds, providers, network requests, or shell startup files are
used by the harness.
"""
from __future__ import annotations

import argparse
import base64
from dataclasses import dataclass
from datetime import datetime, timezone
import itertools
import json
import os
from pathlib import Path
import platform
import shutil
import statistics
import subprocess
import sys
import tempfile
from typing import Any, Callable, Dict, List, Mapping, Optional, Sequence, Tuple
from urllib.parse import urlparse

import source_pair


ROOT = Path(__file__).resolve().parents[1]
PUBLIC_TASKS = ROOT / "benchmarks/ai_productivity/public-development.json"
REFERENCE_ROOT = ROOT / "benchmarks/ai_productivity/public_development/reference_solutions"
SIDES = ("execute", "bash", "nushell")
ORDERS = tuple(itertools.permutations(SIDES))
TASK_IDS = ("filesystem_inventory", "layered_config_merge", "typed_event_normalizer")
ALLOWED_TOOL_PATH = ("/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", "/bin")
MAX_SAMPLES = 100
MAX_WARMUPS = 20
MAX_TIMEOUT_SECONDS = 120


class BenchmarkError(RuntimeError):
    pass


@dataclass(frozen=True)
class Scenario:
    task_id: str
    case_id: str
    environment_input: str
    program_input: str
    input_value: str
    files: Mapping[str, bytes]
    output_path: str
    expected_stdout: bytes

    @property
    def name(self) -> str:
        return self.task_id + "/" + self.case_id


def _safe_relative_path(value: str, label: str) -> str:
    path = Path(value)
    if path.is_absolute() or not path.parts or any(part in (".", "..") for part in path.parts):
        raise BenchmarkError(label + " must be a safe relative path: " + value)
    return path.as_posix()


def _compact_json(value: Any, *, sort_keys: bool = False) -> bytes:
    return (json.dumps(value, ensure_ascii=False, separators=(",", ":"), sort_keys=sort_keys) + "\n").encode("utf-8")


def _expected_bytes(task: Mapping[str, Any], case: Mapping[str, Any]) -> bytes:
    kind = task["oracle"]["kind"]
    expected = case["expected"]
    if kind == "utf8_exact":
        if not isinstance(expected, str):
            raise BenchmarkError("utf8_exact oracle must contain a string")
        return expected.encode("utf-8")
    if kind == "json_value":
        return _compact_json(expected, sort_keys=True)
    if kind == "json_lines":
        if not isinstance(expected, list):
            raise BenchmarkError("json_lines oracle must contain a list")
        fields = ("event_id", "occurred_at", "kind", "source", "actor")
        records = [{field: record.get(field) for field in fields} for record in expected]
        return b"".join(_compact_json(record) for record in records)
    raise BenchmarkError("unsupported public oracle kind: " + str(kind))


def load_scenarios(definition_path: Path = PUBLIC_TASKS) -> Tuple[Dict[str, Any], List[Scenario]]:
    definition = json.loads(definition_path.read_text(encoding="utf-8"))
    if definition.get("schema") != "execute.ai-productivity.public-development/1" or definition.get("fixture_only") is not True:
        raise BenchmarkError("expected the checked-in public-development fixture specification")
    tasks_by_id = {task["task_id"]: task for task in definition["tasks"]}
    scenarios: List[Scenario] = []
    for task_id in TASK_IDS:
        task = tasks_by_id.get(task_id)
        if task is None:
            raise BenchmarkError("missing public reference task: " + task_id)
        if len(task.get("inputs", [])) != 1:
            raise BenchmarkError(task_id + " must have exactly one matched input")
        input_spec = task["inputs"][0]
        input_key = input_spec["program_input"]
        for case in task["cases"]:
            input_value = case.get("inputs", {}).get(input_key, input_spec["default"])
            files: Dict[str, bytes] = {}
            for relative_name, content in case["files"].items():
                relative_name = _safe_relative_path(relative_name, "fixture file")
                if not isinstance(content, str):
                    raise BenchmarkError(task_id + "/" + case["case_id"] + ": fixture contents must be text")
                files[relative_name] = content.encode("utf-8")
            scenarios.append(
                Scenario(
                    task_id=task_id,
                    case_id=case["case_id"],
                    environment_input=input_spec["environment"],
                    program_input=input_key,
                    input_value=str(input_value),
                    files=files,
                    output_path=_safe_relative_path(task["output_path"], "output file"),
                    expected_stdout=_expected_bytes(task, case),
                )
            )
    return definition, scenarios


def source_files(task_id: str) -> Dict[str, Path]:
    return {
        "execute": REFERENCE_ROOT / "execute_program" / (task_id + ".program"),
        "bash": REFERENCE_ROOT / "bash" / (task_id + ".sh"),
        "nushell": REFERENCE_ROOT / "nushell" / (task_id + ".nu"),
    }


def balanced_order(round_index: int) -> Tuple[str, ...]:
    if round_index < 0:
        raise ValueError("round index must be nonnegative")
    return ORDERS[round_index % len(ORDERS)]


def safe_relative_workspace_snapshot(root: Path) -> Dict[str, Any]:
    files: Dict[str, str] = {}
    directories: List[str] = []
    for path in sorted(root.rglob("*")):
        relative = path.relative_to(root).as_posix()
        if path.is_symlink():
            raise BenchmarkError("workspace unexpectedly contains a symlink: " + relative)
        if path.is_dir():
            directories.append(relative)
        elif path.is_file():
            files[relative] = source_pair.sha256_file(path)
        else:
            raise BenchmarkError("workspace contains a non-regular entry: " + relative)
    return {"directories": directories, "files_sha256": files}


def fixture_workspace(root: Path, scenario: Scenario) -> Path:
    workspace = root / scenario.task_id / scenario.case_id
    workspace.mkdir(parents=True)
    (workspace / "reports").mkdir()
    for relative, payload in scenario.files.items():
        path = workspace / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(payload)
    return workspace


def artifact_snapshot(
    binaries: Mapping[str, Optional[Path]],
    helpers: Mapping[str, Optional[Path]],
    tools: Mapping[str, Path],
    manifest_directory: Path,
    script_paths: Mapping[str, Path],
    harness_path: Path,
    execute_source_root: Path,
    execute_revision: str,
    nushell_archive: Optional[Path] = None,
    definition_path: Path = PUBLIC_TASKS,
) -> Dict[str, Any]:
    files: Dict[str, Dict[str, Any]] = {}

    def add(label: str, path: Path) -> None:
        resolved = path.resolve(strict=True)
        if not resolved.is_file() or resolved.is_symlink():
            raise BenchmarkError(label + " is not a regular file: " + str(resolved))
        files[label] = {
            "path": str(resolved),
            "bytes": resolved.stat().st_size,
            "sha256": source_pair.sha256_file(resolved),
        }

    for side, binary in binaries.items():
        if binary is not None:
            add(side + "_binary", binary)
    for side, helper in helpers.items():
        if helper is not None:
            add(side + "_helper", helper)
    for name, path in tools.items():
        add("tool:" + name, path)
    for side, path in script_paths.items():
        add("reference:" + side, path)
    if nushell_archive is not None:
        add("nushell_release_archive", nushell_archive)
    add("public_task_specification", definition_path)
    add("benchmark_harness", harness_path)

    catalog_files: Dict[str, str] = {}
    for path in sorted(manifest_directory.rglob("*")):
        relative = path.relative_to(manifest_directory).as_posix()
        if path.is_symlink():
            raise BenchmarkError("manifest catalog contains a symlink: " + relative)
        if path.is_file():
            catalog_files[relative] = source_pair.sha256_file(path)
    if not catalog_files:
        raise BenchmarkError("manifest catalog contains no regular files")
    return {
        "files": files,
        "execute_source": source_pair.candidate_source_snapshot(execute_source_root, execute_revision),
        "manifest_catalog": {
            "path": str(manifest_directory.resolve(strict=True)),
            "file_count": len(catalog_files),
            "files_sha256": catalog_files,
            "combined_sha256": source_pair.combined_hash(catalog_files),
        },
    }


def _safe_environment(
    side: str,
    workspace: Path,
    home: Path,
    temp_directory: Path,
    path_value: str,
    scenario: Scenario,
) -> Dict[str, str]:
    environment = {
        "PATH": path_value,
        "HOME": str(home),
        "TMPDIR": str(temp_directory),
        "LC_ALL": "C",
        "LANG": "C",
        "TZ": "UTC",
        "NO_COLOR": "1",
        "CLICOLOR": "0",
        "TERM": "dumb",
        "XDG_CONFIG_HOME": str(home / ".config"),
        "XDG_CACHE_HOME": str(home / ".cache"),
        scenario.environment_input: scenario.input_value,
    }
    if side == "nushell":
        environment["NU_CONFIG_DIR"] = str(home / "nushell-config")
    return environment


def _reset_output(workspace: Path, output_path: str) -> None:
    output = workspace / output_path
    if output.is_symlink():
        raise BenchmarkError("output target unexpectedly became a symlink: " + output_path)
    output.unlink(missing_ok=True)


def _observation_signature(observation: Mapping[str, Any]) -> Tuple[Any, ...]:
    return (
        observation["status"],
        observation["stdout_sha256"],
        observation["stdout_bytes"],
        observation["stderr_sha256"],
        observation["stderr_bytes"],
        json.dumps(observation["effects"], sort_keys=True, separators=(",", ":")),
    )


def _run_one(
    scenario: Scenario,
    side: str,
    command: Sequence[str],
    workspace: Path,
    environment: Mapping[str, str],
    timeout_seconds: float,
    process_runner: Callable[[Sequence[str], Path, Mapping[str, str], float], source_pair.Completed],
    context: str,
) -> Dict[str, Any]:
    _reset_output(workspace, scenario.output_path)
    before = safe_relative_workspace_snapshot(workspace)
    try:
        result = process_runner(command, workspace, environment, timeout_seconds)
    except Exception as error:
        raise BenchmarkError(context + " subprocess failed: " + str(error)) from error
    if result.status != 0:
        raise BenchmarkError(context + " exited with status " + str(result.status) + source_pair._stderr_excerpt(result.stderr))
    if result.stdout != scenario.expected_stdout:
        raise BenchmarkError(context + " stdout differs from the independent byte oracle" + source_pair._stderr_excerpt(result.stderr))
    if result.stderr != b"":
        raise BenchmarkError(context + " stderr is not empty" + source_pair._stderr_excerpt(result.stderr))

    after = safe_relative_workspace_snapshot(workspace)
    output = workspace / scenario.output_path
    if not output.is_file() or output.is_symlink():
        raise BenchmarkError(context + " did not create its declared output file")
    if output.read_bytes() != scenario.expected_stdout:
        raise BenchmarkError(context + " file output differs from the independent byte oracle")
    expected_files = dict(before["files_sha256"])
    expected_files[scenario.output_path] = source_pair.sha256_bytes(scenario.expected_stdout)
    if after["files_sha256"] != expected_files or after["directories"] != before["directories"]:
        raise BenchmarkError(context + " changed unexpected fixture or workspace effects")
    effects = {
        "created_output_path": scenario.output_path,
        "created_output_sha256": source_pair.sha256_bytes(scenario.expected_stdout),
        "workspace_after": after,
    }
    return {
        "status": result.status,
        "stdout_bytes": len(result.stdout),
        "stdout_sha256": source_pair.sha256_bytes(result.stdout),
        "stdout_base64": base64.b64encode(result.stdout).decode("ascii"),
        "stderr_bytes": len(result.stderr),
        "stderr_sha256": source_pair.sha256_bytes(result.stderr),
        "stderr_base64": base64.b64encode(result.stderr).decode("ascii"),
        "effects": effects,
        "elapsed_ns": result.elapsed_ns,
    }


def preflight_scenario(
    scenario: Scenario,
    commands: Mapping[str, Sequence[str]],
    workspace: Path,
    environments: Mapping[str, Mapping[str, str]],
    timeout_seconds: float,
    process_runner: Callable[[Sequence[str], Path, Mapping[str, str], float], source_pair.Completed] = source_pair.run_process,
) -> Dict[str, Dict[str, Any]]:
    if not commands or any(side not in SIDES for side in commands):
        raise ValueError("preflight requires one or more known language sides")
    preflight: Dict[str, Dict[str, Any]] = {}
    for side in SIDES:
        if side not in commands:
            continue
        observation = _run_one(
            scenario,
            side,
            commands[side],
            workspace,
            environments[side],
            timeout_seconds,
            process_runner,
            scenario.name + " side=" + side + " phase=preflight",
        )
        preflight[side] = {key: value for key, value in observation.items() if key != "elapsed_ns"}
    signatures = {_observation_signature(observation) for observation in preflight.values()}
    if len(signatures) != 1:
        raise BenchmarkError(scenario.name + " preflight status/output/stderr/effects differ across language arms")
    return preflight


def _summary(values: Sequence[int]) -> Dict[str, Any]:
    if not values:
        raise ValueError("cannot summarize an empty sample")
    ordered = sorted(values)
    p95 = ordered[(95 * len(ordered) + 99) // 100 - 1]
    return {
        "sample_count": len(values),
        "median_ns": statistics.median(values),
        "p95_nearest_rank_ns": p95,
        "minimum_ns": min(values),
        "maximum_ns": max(values),
    }


def interleaved_observations(
    scenario: Scenario,
    commands: Mapping[str, Sequence[str]],
    workspace: Path,
    environments: Mapping[str, Mapping[str, str]],
    timeout_seconds: float,
    rounds: int,
    *,
    warmup: bool,
    round_offset: int = 0,
    preflight: Optional[Mapping[str, Mapping[str, Any]]] = None,
    rows_out: Optional[List[Dict[str, Any]]] = None,
    process_runner: Callable[[Sequence[str], Path, Mapping[str, str], float], source_pair.Completed] = source_pair.run_process,
) -> List[Dict[str, Any]]:
    rows: List[Dict[str, Any]] = rows_out if rows_out is not None else []
    for round_index in range(rounds):
        round_number = round_offset + round_index + 1
        order = balanced_order(round_offset + round_index)
        for position, side in enumerate(side for side in order if side in commands):
            observation = _run_one(
                scenario,
                side,
                commands[side],
                workspace,
                environments[side],
                timeout_seconds,
                process_runner,
                scenario.name + " side=" + side + " phase=" + ("warmup" if warmup else "sample")
                + " round=" + str(round_number),
            )
            if preflight is not None and _observation_signature(observation) != _observation_signature(preflight[side]):
                raise BenchmarkError(
                    scenario.name + " side=" + side + " phase=" + ("warmup" if warmup else "sample")
                    + " round=" + str(round_number) + " differs from preflight"
                )
            rows.append(
                {
                    "round": round_number,
                    "order": list(side for side in order if side in commands),
                    "order_position": position + 1,
                    "side": side,
                    "warmup": warmup,
                    **observation,
                }
            )
    return rows


def compare_to_execute(sample_rows: Sequence[Mapping[str, Any]]) -> Dict[str, List[Dict[str, Any]]]:
    by_round: Dict[int, Dict[str, Mapping[str, Any]]] = {}
    for row in sample_rows:
        if row["warmup"]:
            continue
        by_round.setdefault(int(row["round"]), {})[str(row["side"])] = row
    comparisons: Dict[str, List[Dict[str, Any]]] = {"bash": [], "nushell": []}
    for round_number, group in sorted(by_round.items()):
        if "execute" not in group:
            continue
        execute_ns = int(group["execute"]["elapsed_ns"])
        for side in comparisons:
            if side not in group:
                continue
            side_ns = int(group[side]["elapsed_ns"])
            comparisons[side].append(
                {
                    "round": round_number,
                    "execute_ns": execute_ns,
                    "language_ns": side_ns,
                    "language_minus_execute_ns": side_ns - execute_ns,
                    "language_over_execute": side_ns / execute_ns if execute_ns else None,
                }
            )
    return comparisons


def _unmatched_cells() -> List[Dict[str, str]]:
    return [
        {
            "workload": "applet_match_1000_catalog_top_100",
            "language": "bash,nushell",
            "status": "not_matched",
            "reason": "This Execute discovery path has no checked-in shell implementation; adding one would measure an independently implemented ranking algorithm.",
        },
        {
            "workload": "legacy_typed_graph_two_inprocess_nodes",
            "language": "bash,nushell",
            "status": "not_matched",
            "reason": "This workload calls Execute-only in-process applets and graph wiring, with no matched checked-in Bash/Nushell source.",
        },
        {
            "workload": "external_failure_contracts",
            "language": "execute",
            "status": "not_timed",
            "reason": "Malformed-input and nonzero-native-command status/diagnostic behavior is outside the valid public fixtures and is not inferred from success cases.",
        },
        {
            "workload": "external_failure_contracts",
            "language": "bash",
            "status": "not_timed",
            "reason": "The checked-in Bash references use set -euo pipefail; failure status and diagnostics differ from Execute and are intentionally not counted as matched runtime observations.",
        },
        {
            "workload": "external_failure_contracts",
            "language": "nushell",
            "status": "not_timed",
            "reason": "Nushell stops on nonzero external commands and treats any failed pipeline stage as failure by default; Nu diagnostics/status are distinct and are not counted as matched runtime observations.",
        },
    ]


def _nearest_version(binary: Path, arguments: Sequence[str]) -> str:
    result = subprocess.run(
        [str(binary), *arguments], stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=5, check=False,
    )
    text = (result.stdout + result.stderr).decode("utf-8", errors="replace").strip()
    return text.splitlines()[0] if result.returncode == 0 and text else "unavailable (version command failed)"


def _tool_paths(path_value: str) -> Dict[str, Path]:
    required = ("find", "basename", "sort", "tee", "cat", "jq")
    result: Dict[str, Path] = {}
    for name in required:
        found = shutil.which(name, path=path_value)
        if found is not None:
            result[name] = Path(found).resolve(strict=True)
    return result


def _build_path() -> str:
    path_value = os.pathsep.join(ALLOWED_TOOL_PATH)
    return path_value


def _required_tools(task_id: str) -> Tuple[str, ...]:
    if task_id == "filesystem_inventory":
        return ("find", "basename", "sort", "tee")
    if task_id in ("layered_config_merge", "typed_event_normalizer"):
        return ("cat", "jq", "tee")
    raise BenchmarkError("no external-tool inventory for task " + task_id)


def _commands(
    scenario: Scenario,
    script_paths: Mapping[str, Path],
    binaries: Mapping[str, Optional[Path]],
    manifest_directory: Path,
    command_timeout_ms: int,
) -> Dict[str, List[str]]:
    assert binaries["execute"] is not None and binaries["bash"] is not None
    execute = [
        str(binaries["execute"]), str(script_paths["execute"]),
        "--manifest-dir", str(manifest_directory),
        "--input", scenario.program_input + "=" + scenario.input_value,
        "--command-timeout-ms", str(command_timeout_ms),
    ]
    commands = {
        "execute": execute,
        "bash": [str(binaries["bash"]), "--noprofile", "--norc", str(script_paths["bash"])],
    }
    if binaries["nushell"] is not None:
        commands["nushell"] = [
            str(binaries["nushell"]), "--no-config-file", "--no-history", str(script_paths["nushell"])
        ]
    return commands


def _verify_nushell_provenance(
    binary: Optional[Path], archive: Optional[Path], release_url: Optional[str], expected_archive_sha256: Optional[str]
) -> Optional[Path]:
    if binary is None:
        return None
    if archive is None or not release_url or not expected_archive_sha256:
        raise ValueError("a Nushell binary requires its official release URL, archive, and published archive SHA-256")
    parsed = urlparse(release_url)
    if parsed.scheme != "https" or parsed.netloc != "github.com" or not parsed.path.startswith("/nushell/nushell/releases/download/"):
        raise ValueError("Nushell release URL must point to an HTTPS asset under github.com/nushell/nushell/releases/download")
    if len(expected_archive_sha256) != 64 or any(character not in "0123456789abcdef" for character in expected_archive_sha256.lower()):
        raise ValueError("Nushell archive SHA-256 must be 64 hexadecimal characters")
    resolved = archive.expanduser().resolve(strict=True)
    actual_hash = source_pair.sha256_file(resolved)
    if actual_hash.lower() != expected_archive_sha256.lower():
        raise BenchmarkError("Nushell release archive does not match the supplied published SHA-256")
    return resolved


def _workspace_state(scenario: Scenario, workspace: Path) -> Dict[str, Any]:
    return {
        "name": scenario.name,
        "input_path": scenario.input_value,
        "input_files": {
            name: {"bytes": len(payload), "sha256": source_pair.sha256_bytes(payload)}
            for name, payload in sorted(scenario.files.items())
        },
        "output_path": scenario.output_path,
        "expected_output_bytes": len(scenario.expected_stdout),
        "expected_output_sha256": source_pair.sha256_bytes(scenario.expected_stdout),
        "expected_output_base64": base64.b64encode(scenario.expected_stdout).decode("ascii"),
        "workspace": str(workspace.resolve()),
    }


def _save_report(path: Path, report: Mapping[str, Any], *, create: bool = False) -> None:
    serialized = json.dumps(report, indent=2, ensure_ascii=False) + "\n"
    if create:
        with path.open("x", encoding="utf-8") as destination:
            destination.write(serialized)
    else:
        path.write_text(serialized, encoding="utf-8")


def run_campaign(
    execute_binary: Path,
    execute_source_root: Path,
    manifest_directory: Path,
    report_path: Path,
    *,
    execute_revision: str,
    bash_binary: Path = Path("/bin/bash"),
    nushell_binary: Optional[Path] = None,
    nushell_archive: Optional[Path] = None,
    nushell_archive_sha256: Optional[str] = None,
    nushell_release_url: Optional[str] = None,
    samples: int = 30,
    warmups: int = 5,
    timeout_seconds: float = 30.0,
    command_timeout_ms: int = 25000,
    process_runner: Callable[[Sequence[str], Path, Mapping[str, str], float], source_pair.Completed] = source_pair.run_process,
) -> Dict[str, Any]:
    if not execute_revision.strip():
        raise ValueError("--execute-revision is required")
    if not 1 <= samples <= MAX_SAMPLES:
        raise ValueError("--samples must be between 1 and " + str(MAX_SAMPLES))
    if not 0 <= warmups <= MAX_WARMUPS:
        raise ValueError("--warmups must be between 0 and " + str(MAX_WARMUPS))
    if not 0 < timeout_seconds <= MAX_TIMEOUT_SECONDS:
        raise ValueError("--timeout-seconds must be in (0, " + str(MAX_TIMEOUT_SECONDS) + "]")
    if not 0 < command_timeout_ms <= int(timeout_seconds * 1000):
        raise ValueError("--command-timeout-ms must not exceed the outer timeout")

    report_path = source_pair.validate_report_path(report_path)
    binaries: Dict[str, Optional[Path]] = {
        "execute": source_pair.verify_executable(execute_binary, "current-source Execute binary"),
        "bash": source_pair.verify_executable(bash_binary, "Bash binary"),
        "nushell": source_pair.verify_executable(nushell_binary, "Nushell binary") if nushell_binary else None,
    }
    helpers: Dict[str, Optional[Path]] = {
        "execute": source_pair.helper_path(binaries["execute"]),
        "bash": None,
        "nushell": None,
    }
    manifest_directory = manifest_directory.expanduser().resolve(strict=True)
    if not manifest_directory.is_dir():
        raise ValueError("manifest catalog must be a directory: " + str(manifest_directory))
    execute_source_root = execute_source_root.expanduser().resolve(strict=True)
    if not execute_source_root.is_dir():
        raise ValueError("Execute source root must be a directory: " + str(execute_source_root))
    nushell_archive = _verify_nushell_provenance(
        binaries["nushell"], nushell_archive, nushell_release_url, nushell_archive_sha256
    )

    definition, scenarios = load_scenarios()
    script_paths_by_task: Dict[str, Dict[str, Path]] = {}
    reference_directories: Dict[str, Path] = {}
    for task_id in TASK_IDS:
        script_paths_by_task[task_id] = {}
        for side, path in source_files(task_id).items():
            if not path.is_file():
                raise BenchmarkError("checked-in reference source is missing: " + str(path))
            resolved = path.resolve(strict=True)
            existing = reference_directories.setdefault(side, resolved.parent)
            if existing != resolved.parent:
                raise BenchmarkError("unexpected reference directory layout")
            script_paths_by_task[task_id][side] = resolved

    path_value = _build_path()
    tools = _tool_paths(path_value)
    # Catalogued tools are resolved through the exact child PATH and hash-locked.
    source_snapshot = artifact_snapshot(
        binaries, helpers, tools, manifest_directory,
        {side + ":" + task_id: path.resolve(strict=True)
         for task_id in TASK_IDS for side, path in source_files(task_id).items()},
        Path(__file__).resolve(strict=True),
        execute_source_root,
        execute_revision,
        nushell_archive,
    )

    side_versions: Dict[str, str] = {
        "execute": execute_revision,
        "bash": _nearest_version(binaries["bash"], ("--version",)),
        "nushell": _nearest_version(binaries["nushell"], ("--version",)) if binaries["nushell"] else "unavailable",
    }
    if binaries["nushell"] is not None:
        release_version = urlparse(nushell_release_url or "").path.split("/")[-2]
        if side_versions["nushell"] != release_version:
            raise BenchmarkError(
                "Nushell binary version " + side_versions["nushell"]
                + " does not match the pinned release URL version " + release_version
            )
    child_paths = [str(path) for path in (binaries["execute"], binaries["bash"], binaries["nushell"]) if path]
    environment_description = {
        "controlled_tool_path": path_value,
        "locale": "LC_ALL=C, LANG=C",
        "timezone": "UTC",
        "shell_startup": "Bash --noprofile --norc with hook variables absent; Nu --no-config-file --no-history and private HOME/XDG paths.",
        "per_invocation": "A fresh process with the same cwd and fixture payload; reports/ output is removed before each invocation.",
        "inherited_environment": "None; child environment is constructed by the harness.",
        "binary_directories": child_paths,
    }

    workspaces: Dict[str, Path] = {}
    environments: Dict[str, Dict[str, Dict[str, str]]] = {}
    commands: Dict[str, Dict[str, List[str]]] = {}
    initial_workspaces: Dict[str, Dict[str, Any]] = {}
    preflights: Dict[str, Dict[str, Dict[str, Any]]] = {}
    rows_by_scenario: Dict[str, Dict[str, Any]] = {}
    unavailable_scenarios: Dict[str, List[str]] = {}

    with tempfile.TemporaryDirectory(prefix="execute-matched-languages-") as temporary:
        scratch = Path(temporary)
        home = scratch / "private-home"
        temp_directory = scratch / "tmp"
        home.mkdir()
        temp_directory.mkdir()
        (home / ".config").mkdir()
        (home / ".cache").mkdir()
        for scenario in scenarios:
            workspace = fixture_workspace(scratch / "workspaces", scenario)
            workspaces[scenario.name] = workspace
            commands[scenario.name] = _commands(
                scenario, script_paths_by_task[scenario.task_id], binaries,
                manifest_directory, command_timeout_ms,
            )
            missing_tools = [name for name in _required_tools(scenario.task_id) if name not in tools]
            if missing_tools:
                unavailable_scenarios[scenario.name] = missing_tools
                commands[scenario.name] = {}
            environments[scenario.name] = {
                side: _safe_environment(side, workspace, home, temp_directory, path_value, scenario)
                for side in commands[scenario.name]
            }
            initial_workspaces[scenario.name] = safe_relative_workspace_snapshot(workspace)

        # Every available implementation must pass its independent oracle and
        # agree on exact status, byte streams, and allowed filesystem effects
        # before any report or timing record is created.
        for scenario in scenarios:
            if not commands[scenario.name]:
                preflights[scenario.name] = {}
                continue
            preflights[scenario.name] = preflight_scenario(
                scenario, commands[scenario.name], workspaces[scenario.name],
                environments[scenario.name], timeout_seconds, process_runner,
            )

        report: Dict[str, Any] = {
            "schema": "execute.matched-language-benchmark/1",
            "status": "running",
            "recorded_at_utc": datetime.now(timezone.utc).isoformat(),
            "platform": {"system": platform.platform(), "machine": platform.machine(), "python": sys.version.split()[0]},
            "language_versions": side_versions,
            "execute_revision": execute_revision,
            "nushell_release_url": nushell_release_url,
            "nushell_archive_sha256_expected": nushell_archive_sha256,
            "artifacts": source_snapshot,
            "execute_source_root": str(execute_source_root),
            "environment": environment_description,
            "child_environment_template": {
                "PATH": path_value, "LC_ALL": "C", "LANG": "C", "TZ": "UTC",
                "HOME": str(home), "TMPDIR": str(temp_directory),
                "XDG_CONFIG_HOME": str(home / ".config"), "XDG_CACHE_HOME": str(home / ".cache"),
                "NO_COLOR": "1", "CLICOLOR": "0", "TERM": "dumb",
            },
            "method": {
                "source_provenance": "The three arms are checked-in public development reference solutions used to test the fixture evaluator; they are not model-generated study trials or a held-out real-task sample.",
                "claim_scope": "Descriptive local whole-workflow runtime only. No programmer-economy, code-size, token, maintainability, broad Bash-replacement, universal speed, or isolated-interpreter claim.",
                "timed_boundary": "Fresh interpreter process start through stdout/stderr capture and process exit; fixture setup, preflight oracle/effect checks, artifact hashing, and summaries are outside the timed interval.",
                "correctness": "Independent fixture-derived byte oracle for stdout and output file, exact zero status and empty stderr, exact allowed workspace delta, then exact cross-language parity before timing and after every sample.",
                "sampling": "Each warmup/sample round runs every available side once in a balanced six-permutation interleaving; raw per-invocation streams, effects, status, and elapsed nanoseconds are retained.",
                "implementation_scope": "The workflows are behaviorally matched, not necessarily identical command plans. The Execute references use internal save and, for jq workflows, an explicit cat stage; shell references use tee or jq's file argument. These are end-to-end workflow observations, not interpreter-only comparisons.",
                "catalog_scope": "The immutable supplied applet manifest catalog is passed to Execute and hashed before/after. The Bash/Nushell reference scripts call the same hashed external tool binaries through the controlled PATH and do not consult the Execute manifest catalog.",
                "resources": "Only outer wall time is measured. CPU and peak RSS are not measured or inferred.",
                "conditions": "One local macOS/Linux host, warm working filesystem, no cache flushing, no CPU pinning, no concurrent benchmark claim, and no cross-platform parity claim.",
            },
            "requested": {"samples": samples, "warmups": warmups, "timeout_seconds": timeout_seconds,
                          "command_timeout_ms": command_timeout_ms, "interleave_permutations": [list(order) for order in ORDERS]},
            "available_languages": [side for side in SIDES if binaries[side] is not None],
            "unavailable_languages": ({"nushell": "--nushell-binary was not provided"} if binaries["nushell"] is None else {}),
            "unmatched_cells": _unmatched_cells(),
            "workloads": [],
        }
        if binaries["nushell"] is None:
            report["unmatched_cells"].append({
                "workload": "all public workflows",
                "language": "nushell",
                "status": "unavailable",
                "reason": "No explicit --nushell-binary was supplied; no Nu measurements are imputed.",
            })

        for scenario in scenarios:
            sources = source_files(scenario.task_id)
            missing_tools = unavailable_scenarios.get(scenario.name, [])
            language_cells = {}
            for side in SIDES:
                if side not in commands[scenario.name]:
                    if binaries[side] is None:
                        reason = "language executable not provided"
                    else:
                        reason = "required public-reference tools unavailable: " + ", ".join(missing_tools)
                    language_cells[side] = {"status": "unavailable", "reason": reason}
                else:
                    language_cells[side] = {"status": "pending", "reason": None}
            rows_by_scenario[scenario.name] = {
                "name": scenario.name,
                "task_id": scenario.task_id,
                "case_id": scenario.case_id,
                "fixture": _workspace_state(scenario, workspaces[scenario.name]),
                "sources": {
                    side: {
                        "path": str(path.resolve(strict=True)),
                        "bytes": path.stat().st_size,
                        "sha256": source_pair.sha256_file(path),
                        "source_utf8": path.read_text(encoding="utf-8"),
                    }
                    for side, path in sources.items()
                },
                "status": "unavailable" if not commands[scenario.name] else "running",
                "unavailable_reason": (
                    "required public-reference executable(s) unavailable on controlled PATH: "
                    + ", ".join(unavailable_scenarios[scenario.name])
                    if scenario.name in unavailable_scenarios else None
                ),
                "language_cells": language_cells,
                "available_languages": list(commands[scenario.name]),
                "preflight": preflights[scenario.name],
                "child_environment": environments[scenario.name],
                "warmup_observations": [],
                "raw_samples": [],
                "summary": {},
                "paired_to_execute": {},
            }
            report["workloads"].append(rows_by_scenario[scenario.name])

        _save_report(report_path, report, create=True)
        try:
            for scenario in scenarios:
                workload_row = rows_by_scenario[scenario.name]
                if not commands[scenario.name]:
                    continue
                interleaved_observations(
                    scenario, commands[scenario.name], workspaces[scenario.name],
                    environments[scenario.name], timeout_seconds, warmups,
                    warmup=True, preflight=preflights[scenario.name], process_runner=process_runner,
                    rows_out=workload_row["warmup_observations"],
                )
                interleaved_observations(
                    scenario, commands[scenario.name], workspaces[scenario.name],
                    environments[scenario.name], timeout_seconds, samples,
                    warmup=False, round_offset=warmups,
                    preflight=preflights[scenario.name], process_runner=process_runner,
                    rows_out=workload_row["raw_samples"],
                )
                sample_rows = workload_row["raw_samples"]
                paired_comparisons = compare_to_execute(sample_rows)
                workload_row["paired_comparisons_to_execute"] = paired_comparisons
                workload_row["summary"] = {
                    side: _summary([int(row["elapsed_ns"]) for row in sample_rows if row["side"] == side])
                    for side in commands[scenario.name]
                }
                workload_row["paired_to_execute"] = {
                    side: _summary([int(row["language_minus_execute_ns"]) for row in comparisons])
                    for side, comparisons in paired_comparisons.items() if comparisons
                }
                workload_row["status"] = "passed"
                for side in commands[scenario.name]:
                    workload_row["language_cells"][side] = {"status": "passed", "reason": None}
                _save_report(report_path, report)

            final_artifacts = artifact_snapshot(
                binaries, helpers, tools, manifest_directory,
                {side + ":" + task_id: path.resolve(strict=True)
                 for task_id in TASK_IDS for side, path in source_files(task_id).items()},
                Path(__file__).resolve(strict=True),
                execute_source_root,
                execute_revision,
                nushell_archive,
            )
            source_pair.assert_artifacts_unchanged(source_snapshot, final_artifacts)
            for scenario in scenarios:
                final_workspace = safe_relative_workspace_snapshot(workspaces[scenario.name])
                expected_files = dict(initial_workspaces[scenario.name]["files_sha256"])
                if commands[scenario.name]:
                    expected_files[scenario.output_path] = source_pair.sha256_bytes(scenario.expected_stdout)
                if (final_workspace["files_sha256"] != expected_files
                        or final_workspace["directories"] != initial_workspaces[scenario.name]["directories"]):
                    raise BenchmarkError(scenario.name + " final workspace effects differ from the allowed output delta")
            passed_workloads = sum(row["status"] == "passed" for row in report["workloads"])
            unavailable_workloads = sum(row["status"] == "unavailable" for row in report["workloads"])
            report["status"] = "passed" if unavailable_workloads == 0 else "completed_with_unavailable"
            if passed_workloads == 0:
                report["status"] = "no_matched_workloads_available"
            report["summary"] = {
                "passed_workloads": passed_workloads,
                "unavailable_workloads": unavailable_workloads,
                "measured_language_cells": sum(len(row["available_languages"]) for row in report["workloads"] if row["status"] == "passed"),
                "unavailable_language_cells": sum(
                    cell["status"] == "unavailable"
                    for row in report["workloads"] for cell in row["language_cells"].values()
                ),
            }
            report["final_artifacts"] = final_artifacts
            _save_report(report_path, report)
        except BaseException as error:
            report["status"] = "failed"
            report["failure"] = type(error).__name__ + ": " + str(error)
            _save_report(report_path, report)
            raise
        return report


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--execute-binary", type=Path, required=True)
    parser.add_argument("--execute-source-root", type=Path, required=True)
    parser.add_argument("--manifest-dir", type=Path, required=True)
    parser.add_argument("--execute-revision", required=True, help="explicit label for current-source binary")
    parser.add_argument("--bash-binary", type=Path, default=Path("/bin/bash"))
    parser.add_argument("--nushell-binary", type=Path, help="pinned standalone Nu binary; if omitted, Nu cells are retained as unavailable")
    parser.add_argument("--nushell-archive", type=Path, help="exact official release archive from which the Nu binary was extracted")
    parser.add_argument("--nushell-archive-sha256", help="published SHA-256 for --nushell-archive")
    parser.add_argument("--nushell-release-url", help="official release URL used to obtain the exact pinned Nu binary")
    parser.add_argument("--report", type=Path, required=True)
    parser.add_argument("--samples", type=int, default=30)
    parser.add_argument("--warmups", type=int, default=5)
    parser.add_argument("--timeout-seconds", type=float, default=30.0)
    parser.add_argument("--command-timeout-ms", type=int, default=25000)
    args = parser.parse_args()
    try:
        report = run_campaign(
            args.execute_binary, args.execute_source_root, args.manifest_dir, args.report,
            execute_revision=args.execute_revision,
            bash_binary=args.bash_binary,
            nushell_binary=args.nushell_binary,
            nushell_archive=args.nushell_archive,
            nushell_archive_sha256=args.nushell_archive_sha256,
            nushell_release_url=args.nushell_release_url,
            samples=args.samples,
            warmups=args.warmups,
            timeout_seconds=args.timeout_seconds,
            command_timeout_ms=args.command_timeout_ms,
        )
    except Exception as error:
        parser.error(str(error))
    print(json.dumps({"status": report["status"], "workloads": len(report["workloads"]),
                      "samples_per_language_per_workload": args.samples,
                      "available_languages": report["available_languages"],
                      "summary": report.get("summary", {})}, indent=2))


if __name__ == "__main__":
    main()
