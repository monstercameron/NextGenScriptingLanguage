#!/usr/bin/env python3
"""Public development-task runner and external holdout package freezer.

This runner executes only the checked-in public development fixture set. It
never calls a model and marks every output as a fixture, which the held-out
result importer rejects. External holdout packages are committed and scheduled
here, but their prompts/oracles are not passed to this public execution path.
"""

from __future__ import annotations

import argparse
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import platform
import random
import resource
import selectors
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import uuid
from typing import Any

import harness


HERE = Path(__file__).resolve().parent
PREREG = HERE / "preregistration.json"
PUBLIC_TASKS = HERE / "public-development.json"
PACKAGE_SCHEMA = "execute.ai-productivity.external-package/1"
FREEZE_SCHEMA = "execute.ai-productivity.package-freeze/1"
PLAN_SCHEMA = "execute.ai-productivity.run-plan/1"
FIXTURE_RESULT_SCHEMA = "execute.ai-productivity.fixture-run/1"
MAX_PACKAGE_BYTES = 16 * 1024 * 1024
MAX_FILES = 512
MAX_FILE_BYTES = 2 * 1024 * 1024
MAX_TOTAL_FIXTURE_BYTES = 8 * 1024 * 1024
ALLOWED_OUTPUTS = {
    "filesystem_inventory": "reports/inventory.txt",
    "layered_config_merge": "reports/effective.json",
    "typed_event_normalizer": "reports/normalized.jsonl",
}
PUBLIC_REFERENCE_SHA256 = {
    "bash/filesystem_inventory": "1c4d946fedc94a604557c82f8fb1dc7f8bb39d51901a3f157609d931a5b62c8b",
    "bash/layered_config_merge": "a915d11aa0f885b2756d126610b915d829f3365aa64887f12bedc0f13792e9d4",
    "bash/typed_event_normalizer": "0a5ae947931c7bafe3942a95d06a404a10038f8a5392bb0ee62b0d1b07644c8b",
    "nushell/filesystem_inventory": "df2a1140a77b124c54bbe1c91446b497bf3bbeb71dd72823c7766e95ef526fe6",
    "nushell/layered_config_merge": "b1d75354deb05ca20b9b4c1466e29b4c915c47001cac90f16444d1ee55c547ec",
    "nushell/typed_event_normalizer": "c7ad071ca8547c5e9f08ff4191f68d6ea91295df2e6bc3a71aea881c51fa9bd8",
    "execute_program/filesystem_inventory": "45e404fc4099b3cb7b0095761854cf8c7ae78363598769f0df2ba343fce80017",
    "execute_program/layered_config_merge": "5d8a1ad8d936358e86b2a2711e1cdf13dd2b6f34d780d188034da38a06fc3291",
    "execute_program/typed_event_normalizer": "0ab9c6dbdaa8dbc1332287cefe5df75b5996544f9428f4996920795a58038592",
}
PUBLIC_REFERENCE_ROOT = HERE / "public_development" / "reference_solutions"
SANDBOX_COMMAND = "{command}"
SANDBOX_WORKSPACE = "{workspace}"


class EvaluationError(ValueError):
    """Invalid package, task, candidate, or execution result."""


def canonical(value: Any) -> bytes:
    return json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":")).encode("utf-8")


def digest_bytes(value: bytes) -> str:
    return hashlib.sha256(value).hexdigest()


def digest_file(path: Path) -> str:
    return digest_bytes(path.read_bytes())


def oracle_material(task: dict[str, Any]) -> dict[str, Any]:
    return {
        "oracle": task["oracle"],
        "criteria": task["criteria"],
        "optional_criteria": task.get("optional_criteria", []),
        "expected_cases": [
            {"case_id": case["case_id"], "expected": case["expected"]}
            for case in task["cases"]
        ],
    }


def _json_file(path: Path, label: str) -> tuple[Any, bytes]:
    try:
        raw = path.read_bytes()
    except OSError as error:
        raise EvaluationError(f"cannot read {label} {path}: {error}") from error
    if len(raw) > MAX_PACKAGE_BYTES:
        raise EvaluationError(f"{label} exceeds {MAX_PACKAGE_BYTES} bytes")
    try:
        return json.loads(raw), raw
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise EvaluationError(f"{label} is not valid UTF-8 JSON: {error}") from error


def _safe_relative(value: Any, label: str) -> PurePosixPath:
    if not isinstance(value, str) or not value or "\\" in value or "\x00" in value:
        raise EvaluationError(f"{label} must be a nonempty POSIX relative path")
    path = PurePosixPath(value)
    if path.is_absolute() or any(part in {"", ".", ".."} for part in path.parts):
        raise EvaluationError(f"{label} may not be absolute or traverse directories")
    return path


def _validate_task(task: Any, index: int, *, public: bool) -> dict[str, Any]:
    label = f"tasks[{index}]"
    required = {
        "task_id", "family_id", "split", "stratum", "prompt", "inputs",
        "output_path", "oracle", "criteria", "cases",
    }
    if not isinstance(task, dict) or task.keys() != required:
        raise EvaluationError(f"{label} must have exactly {sorted(required)}")
    if not isinstance(task["task_id"], str) or not harness.TASK_ID.fullmatch(task["task_id"]):
        raise EvaluationError(f"{label}.task_id is malformed")
    if not isinstance(task["family_id"], str) or not harness.TASK_ID.fullmatch(task["family_id"]):
        raise EvaluationError(f"{label}.family_id is malformed")
    if task["split"] != "development" and public:
        raise EvaluationError("public development tasks must use the development split")
    if not isinstance(task["stratum"], str) or task["stratum"] not in harness.VARIANTS_BY_STRATUM:
        raise EvaluationError(f"{label}.stratum is unsupported")
    if not isinstance(task["prompt"], str) or not task["prompt"].strip():
        raise EvaluationError(f"{label}.prompt must be nonempty text")
    output_path = _safe_relative(task["output_path"], f"{label}.output_path")
    if not isinstance(task["oracle"], dict) or not isinstance(task["oracle"].get("kind"), str) or task["oracle"].get("kind") not in {"utf8_exact", "json_value", "json_lines"}:
        raise EvaluationError(f"{label}.oracle.kind is unsupported")
    if not isinstance(task["criteria"], list) or not task["criteria"] or not all(
        isinstance(item, str) and harness.CRITERION.fullmatch(item) for item in task["criteria"]
    ):
        raise EvaluationError(f"{label}.criteria must be a nonempty list of criterion IDs")
    if len(set(task["criteria"])) != len(task["criteria"]):
        raise EvaluationError(f"{label}.criteria has duplicate entries")
    if not isinstance(task["inputs"], list):
        raise EvaluationError(f"{label}.inputs must be an array")
    seen_environment: set[str] = set()
    seen_program: set[str] = set()
    for input_index, item in enumerate(task["inputs"]):
        if not isinstance(item, dict) or item.keys() != {"environment", "program_input", "default"}:
            raise EvaluationError(f"{label}.inputs[{input_index}] has an invalid shape")
        if not all(isinstance(item[field], str) and item[field] for field in ("environment", "program_input", "default")):
            raise EvaluationError(f"{label}.inputs[{input_index}] fields must be nonempty strings")
        if not item["environment"].startswith("EVAL_") or item["environment"] in seen_environment:
            raise EvaluationError(f"{label}.inputs[{input_index}].environment must be unique and start with EVAL_")
        if not item["program_input"].isidentifier() or item["program_input"] in seen_program:
            raise EvaluationError(f"{label}.inputs[{input_index}].program_input must be a unique identifier")
        _safe_relative(item["default"], f"{label}.inputs[{input_index}].default")
        seen_environment.add(item["environment"])
        seen_program.add(item["program_input"])
    if not isinstance(task["cases"], list) or not task["cases"]:
        raise EvaluationError(f"{label}.cases must be nonempty")
    seen_cases: set[str] = set()
    for case_index, case in enumerate(task["cases"]):
        case_label = f"{label}.cases[{case_index}]"
        if not isinstance(case, dict) or not {"case_id", "files", "expected"} <= case.keys() or case.keys() - {"case_id", "files", "expected", "inputs"}:
            raise EvaluationError(f"{case_label} has missing or unexpected fields")
        if not isinstance(case["case_id"], str) or not harness.TASK_ID.fullmatch(case["case_id"]) or case["case_id"] in seen_cases:
            raise EvaluationError(f"{case_label}.case_id is malformed or duplicated")
        seen_cases.add(case["case_id"])
        if not isinstance(case["files"], dict) or not case["files"]:
            raise EvaluationError(f"{case_label}.files must be a nonempty path-to-text object")
        if len(case["files"]) > MAX_FILES:
            raise EvaluationError(f"{case_label}.files exceeds {MAX_FILES} files")
        total = 0
        for name, content in case["files"].items():
            _safe_relative(name, f"{case_label}.files path")
            if not isinstance(content, str):
                raise EvaluationError(f"{case_label}.files values must be UTF-8 text")
            size = len(content.encode("utf-8"))
            if size > MAX_FILE_BYTES:
                raise EvaluationError(f"{case_label} file {name!r} exceeds {MAX_FILE_BYTES} bytes")
            total += size
        if total > MAX_TOTAL_FIXTURE_BYTES:
            raise EvaluationError(f"{case_label} exceeds total fixture size limit")
        kind = task["oracle"]["kind"]
        expected = case["expected"]
        if kind == "utf8_exact" and not isinstance(expected, str):
            raise EvaluationError(f"{case_label}.expected must be text for utf8_exact")
        if kind == "json_lines" and not isinstance(expected, list):
            raise EvaluationError(f"{case_label}.expected must be an array for json_lines")
        if "inputs" in case:
            if not isinstance(case["inputs"], dict) or not set(case["inputs"]) <= seen_program:
                raise EvaluationError(f"{case_label}.inputs may only override declared program inputs")
            if not all(isinstance(value, str) for value in case["inputs"].values()):
                raise EvaluationError(f"{case_label}.inputs values must be text")
            for value in case["inputs"].values():
                _safe_relative(value, f"{case_label}.inputs value")
    expected_output = ALLOWED_OUTPUTS.get(task["task_id"])
    if public and expected_output != output_path.as_posix():
        raise EvaluationError(f"public task output must be the fixed oracle path {expected_output!r}")
    return task


def load_public_tasks(path: Path = PUBLIC_TASKS) -> dict[str, dict[str, Any]]:
    document, _ = _json_file(path, "public development task package")
    if not isinstance(document, dict) or document.get("schema") != "execute.ai-productivity.public-development/1" or document.get("fixture_only") is not True or not isinstance(document.get("tasks"), list):
        raise EvaluationError("public task package must be fixture-only and use its supported schema")
    tasks = {}
    for index, task in enumerate(document["tasks"]):
        validated = _validate_task(task, index, public=True)
        if task["task_id"] in tasks:
            raise EvaluationError(f"duplicate public task {task['task_id']!r}")
        tasks[task["task_id"]] = validated
    return tasks


def _validate_external_package(document: Any, package_bytes: bytes, prereg: dict[str, Any]) -> dict[str, dict[str, Any]]:
    if not isinstance(document, dict) or document.get("schema") != PACKAGE_SCHEMA:
        raise EvaluationError(f"external package must use {PACKAGE_SCHEMA}")
    expected_keys = {"schema", "preregistration_sha256", "randomization_seed", "frozen_utc", "tasks"}
    if document.keys() != expected_keys:
        raise EvaluationError(f"external package must have exactly {sorted(expected_keys)}")
    if document["preregistration_sha256"] != digest_file(PREREG):
        raise EvaluationError("external package is bound to a different preregistration")
    expected_seed = prereg["analysis"]["randomization_seed"]
    if type(document["randomization_seed"]) is not int or document["randomization_seed"] != expected_seed:
        raise EvaluationError(f"external package randomization_seed must equal preregistered seed {expected_seed}")
    try:
        frozen_at = datetime.fromisoformat(str(document["frozen_utc"]).replace("Z", "+00:00"))
    except ValueError as error:
        raise EvaluationError("external package frozen_utc is malformed") from error
    if frozen_at.tzinfo is None or frozen_at.utcoffset().total_seconds() != 0:
        raise EvaluationError("external package frozen_utc must include a UTC offset")
    if not isinstance(document["tasks"], list):
        raise EvaluationError("external package tasks must be an array")
    roster_tasks = []
    seen: set[str] = set()
    for index, task in enumerate(document["tasks"]):
        allowed = {
            "task_id", "family_id", "split", "stratum", "prompt", "oracle",
            "criteria", "optional_criteria", "inputs", "output_path", "cases",
        }
        if not isinstance(task, dict) or not allowed <= task.keys() or task.keys() - allowed:
            raise EvaluationError(f"external tasks[{index}] has missing or unexpected fields")
        if not isinstance(task["task_id"], str) or not harness.TASK_ID.fullmatch(task["task_id"]):
            raise EvaluationError(f"external tasks[{index}].task_id is malformed")
        if not isinstance(task["family_id"], str) or not harness.TASK_ID.fullmatch(task["family_id"]):
            raise EvaluationError(f"external tasks[{index}].family_id is malformed")
        if task["task_id"] in seen:
            raise EvaluationError(f"duplicate external task {task['task_id']!r}")
        seen.add(task["task_id"])
        if not isinstance(task["split"], str) or task["split"] not in {"development", "calibration", "held_out"}:
            raise EvaluationError(f"external tasks[{index}].split is invalid")
        if not isinstance(task["stratum"], str) or task["stratum"] not in harness.VARIANTS_BY_STRATUM:
            raise EvaluationError(f"external tasks[{index}].stratum is invalid")
        if not isinstance(task["prompt"], str) or not task["prompt"].strip():
            raise EvaluationError(f"external tasks[{index}].prompt must be nonempty")
        if not isinstance(task["oracle"], dict):
            raise EvaluationError(f"external tasks[{index}].oracle must be a structured declarative oracle")
        _validate_task(
            {
                "task_id": task["task_id"], "family_id": task["family_id"],
                "split": task["split"], "stratum": task["stratum"],
                "prompt": task["prompt"], "inputs": task["inputs"],
                "output_path": task["output_path"],
                "oracle": task["oracle"], "criteria": task["criteria"], "cases": task["cases"],
            },
            index,
            public=False,
        )
        criteria = task["criteria"]
        optional = task.get("optional_criteria", [])
        if not isinstance(optional, list) or not all(isinstance(item, str) and harness.CRITERION.fullmatch(item) for item in optional):
            raise EvaluationError(f"external tasks[{index}].optional_criteria is invalid")
        if len(set(optional)) != len(optional) or set(optional) & set(criteria):
            raise EvaluationError(f"external tasks[{index}] has duplicated/overlapping criteria")
        roster_tasks.append({
            "task_id": task["task_id"], "family_id": task["family_id"],
            "split": task["split"], "stratum": task["stratum"],
            "prompt_sha256": digest_bytes(task["prompt"].encode("utf-8")),
            "oracle_sha256": digest_bytes(canonical(oracle_material(task))),
            "criteria": criteria, "optional_criteria": optional,
        })
    roster = {
        "schema": "execute.ai-productivity.roster/1",
        "fixture_only": False,
        "frozen_utc": document["frozen_utc"],
        "task_package_sha256": digest_bytes(package_bytes),
        "tasks": roster_tasks,
    }
    harness.validate_roster(roster, production=True)
    return {task["task_id"]: task for task in document["tasks"]}


def freeze_external_package(package_path: Path, seed: int, out_path: Path) -> dict[str, Any]:
    prereg = json.loads(PREREG.read_text())
    if seed != prereg["analysis"]["randomization_seed"]:
        raise EvaluationError("freeze seed must be the value already recorded in preregistration.json")
    document, package_bytes = _json_file(package_path, "external package")
    _validate_external_package(document, package_bytes, prereg)
    if out_path.exists() or out_path.is_symlink():
        raise EvaluationError(f"refusing to overwrite freeze record: {out_path}")
    freeze = {
        "schema": FREEZE_SCHEMA,
        "package_sha256": digest_bytes(package_bytes),
        "preregistration_sha256": digest_file(PREREG),
        "randomization_seed": seed,
        "frozen_utc": datetime.now(timezone.utc).isoformat(),
        "task_count": len(document["tasks"]),
        "family_counts": {
            split: {
                stratum: len({task["family_id"] for task in document["tasks"] if task["split"] == split and task["stratum"] == stratum})
                for stratum in harness.VARIANTS_BY_STRATUM
            }
            for split in ("development", "calibration", "held_out")
        },
        "claim": "none",
    }
    out_path.parent.mkdir(parents=True, exist_ok=True)
    out_path.write_text(json.dumps(freeze, sort_keys=True, indent=2) + "\n", encoding="utf-8")
    return freeze


def verify_external_package(package_path: Path, freeze_path: Path) -> tuple[dict[str, Any], dict[str, dict[str, Any]]]:
    prereg = json.loads(PREREG.read_text())
    document, package_bytes = _json_file(package_path, "external package")
    tasks = _validate_external_package(document, package_bytes, prereg)
    freeze, _ = _json_file(freeze_path, "package freeze record")
    if not isinstance(freeze, dict) or freeze.get("schema") != FREEZE_SCHEMA:
        raise EvaluationError("freeze record has an unsupported schema")
    if freeze.get("package_sha256") != digest_bytes(package_bytes):
        raise EvaluationError("external package bytes differ from the preregistered freeze")
    if freeze.get("preregistration_sha256") != digest_file(PREREG):
        raise EvaluationError("freeze record is bound to a different preregistration")
    if freeze.get("randomization_seed") != prereg["analysis"]["randomization_seed"]:
        raise EvaluationError("freeze record seed differs from preregistration")
    try:
        timestamp = datetime.fromisoformat(str(freeze["frozen_utc"]).replace("Z", "+00:00"))
    except (KeyError, ValueError) as error:
        raise EvaluationError("freeze record frozen_utc is malformed") from error
    if timestamp.tzinfo is None or timestamp.utcoffset().total_seconds() != 0:
        raise EvaluationError("freeze record timestamp must be UTC")
    if freeze.get("task_count") != len(tasks):
        raise EvaluationError("freeze task count does not match package")
    return freeze, tasks


def export_external_roster(package_path: Path, freeze_path: Path, output_path: Path) -> dict[str, Any]:
    freeze, tasks = verify_external_package(package_path, freeze_path)
    document, package_bytes = _json_file(package_path, "external package")
    roster = {
        "schema": "execute.ai-productivity.roster/1",
        "fixture_only": False,
        "frozen_utc": document["frozen_utc"],
        "task_package_sha256": digest_bytes(package_bytes),
        "tasks": [
            {
                "task_id": task_id,
                "family_id": task["family_id"],
                "split": task["split"],
                "stratum": task["stratum"],
                "prompt_sha256": digest_bytes(task["prompt"].encode("utf-8")),
                "oracle_sha256": digest_bytes(canonical(oracle_material(task))),
                "criteria": task["criteria"],
                "optional_criteria": task.get("optional_criteria", []),
            }
            for task_id, task in sorted(tasks.items())
        ],
    }
    harness.validate_roster(roster, production=True)
    if output_path.exists() or output_path.is_symlink():
        raise EvaluationError(f"refusing to overwrite roster: {output_path}")
    output_path.parent.mkdir(parents=True, exist_ok=True)
    output_path.write_text(json.dumps(roster, sort_keys=True, indent=2) + "\n", encoding="utf-8")
    return roster


def build_run_plan(package_path: Path, freeze_path: Path, arms_path: Path, output_path: Path) -> dict[str, Any]:
    freeze, tasks = verify_external_package(package_path, freeze_path)
    records = []
    # Validate the sealed arm map using the same importer validation rules.
    arm_document, _ = _json_file(arms_path, "sealed arm map")
    if not isinstance(arm_document, dict) or arm_document.get("schema") != "execute.ai-productivity.arms/1" or not isinstance(arm_document.get("arms"), dict):
        raise EvaluationError("arm map must use execute.ai-productivity.arms/1")
    arms = arm_document["arms"]
    harness.read_arm_map(arms_path, [])
    seed = freeze["randomization_seed"]
    rng = random.Random(seed)
    order_index = 0
    for task_id, task in sorted(tasks.items()):
        required_variants = harness.VARIANTS_BY_STRATUM[task["stratum"]]
        blocks: dict[str, list[tuple[str, dict[str, Any]]]] = {}
        for arm_id, arm in arms.items():
            if arm["variant"] in required_variants:
                blocks.setdefault(arm["comparison_block"], []).append((arm_id, arm))
        if not blocks:
            raise EvaluationError(f"sealed arm map has no comparison block for {task['stratum']}")
        complete_blocks = 0
        for block_name, entries in sorted(blocks.items()):
            variants = [arm["variant"] for _, arm in entries]
            if set(variants) != required_variants or len(variants) != len(required_variants):
                raise EvaluationError(f"comparison block {block_name!r} does not have exactly one arm per required variant for {task['stratum']}")
            model_settings = {(arm["provider"], arm["model_snapshot"], canonical(arm["generation_parameters"])) for _, arm in entries}
            if len(model_settings) != 1:
                raise EvaluationError(f"comparison block {block_name!r} mixes model or decoding settings")
            complete_blocks += 1
            rng.shuffle(entries)
            for arm_id, arm in entries:
                order_index += 1
                run_id = str(uuid.uuid5(uuid.NAMESPACE_URL, f"{freeze['package_sha256']}:{task_id}:{block_name}:{arm_id}:1"))
                records.append({
                    "order_index": order_index,
                    "run_id": run_id,
                    "task_id": task_id,
                    "family_id": task["family_id"],
                    "split": task["split"],
                    "stratum": task["stratum"],
                    "comparison_block": block_name,
                    "blind_arm_id": arm_id,
                    "replicate": 1,
                    "variant_order_index": sum(1 for row in records if row["task_id"] == task_id and row["comparison_block"] == block_name) + 1,
                    "prompt_sha256": digest_bytes(task["prompt"].encode("utf-8")),
                    "oracle_sha256": digest_bytes(canonical(oracle_material(task))),
                    "execution_state": "pending_external_provider_runner",
                })
        if complete_blocks == 0:
            raise EvaluationError(f"no complete matched model block for task {task_id!r}")
    if not records:
        raise EvaluationError("run plan contains no task/arm cells")
    plan = {
        "schema": PLAN_SCHEMA,
        "status": "schedule_only_trials_pending",
        "claim": "none",
        "package_sha256": freeze["package_sha256"],
        "freeze_sha256": digest_file(freeze_path),
        "preregistration_sha256": freeze["preregistration_sha256"],
        "arms_sha256": digest_file(arms_path),
        "randomization_seed": seed,
        "task_arm_replicate_count": len(records),
        "created_utc": datetime.now(timezone.utc).isoformat(),
        "schedule": records,
        "provider_runs": "not_run_by_this_tool",
        "note": "This is a randomized schedule only. It does not call a provider or expose task/oracle contents in the output.",
    }
    if output_path.exists() or output_path.is_symlink():
        raise EvaluationError(f"refusing to overwrite run plan: {output_path}")
    output_path.parent.mkdir(parents=True, exist_ok=True)
    output_path.write_text(json.dumps(plan, sort_keys=True, indent=2) + "\n", encoding="utf-8")
    return plan


def _snapshot(root: Path) -> dict[str, tuple[str, str]]:
    snapshot: dict[str, tuple[str, str]] = {}
    for parent, dirs, files in os.walk(root, followlinks=False):
        parent_path = Path(parent)
        for name in sorted(dirs):
            path = parent_path / name
            if path.is_symlink():
                snapshot[path.relative_to(root).as_posix()] = ("symlink", os.readlink(path))
        dirs[:] = sorted(name for name in dirs if not (parent_path / name).is_symlink())
        for name in sorted(files):
            path = parent_path / name
            relative = path.relative_to(root).as_posix()
            if path.is_symlink():
                snapshot[relative] = ("symlink", os.readlink(path))
            elif path.is_file():
                snapshot[relative] = ("file", digest_file(path))
            else:
                snapshot[relative] = ("other", "")
    return snapshot


def _limit_resources(cpu_seconds: int, file_bytes: int, memory_bytes: int) -> None:
    resource.setrlimit(resource.RLIMIT_CORE, (0, 0))
    resource.setrlimit(resource.RLIMIT_CPU, (cpu_seconds, cpu_seconds + 1))
    resource.setrlimit(resource.RLIMIT_FSIZE, (file_bytes, file_bytes))
    resource.setrlimit(resource.RLIMIT_NOFILE, (64, 64))
    if platform.system() == "Linux" and hasattr(resource, "RLIMIT_AS"):
        resource.setrlimit(resource.RLIMIT_AS, (memory_bytes, memory_bytes))


def _kill_group(process: subprocess.Popen[bytes]) -> None:
    try:
        os.killpg(process.pid, signal.SIGKILL)
    except ProcessLookupError:
        pass


def _capture(command: list[str], cwd: Path, env: dict[str, str], *, timeout_s: float, output_limit: int, cpu_s: int, memory_bytes: int) -> dict[str, Any]:
    started = time.monotonic()
    options: dict[str, Any] = {"cwd": cwd, "env": env, "stdin": subprocess.DEVNULL, "stdout": subprocess.PIPE, "stderr": subprocess.PIPE, "start_new_session": True}
    if os.name == "posix":
        options["preexec_fn"] = lambda: _limit_resources(cpu_s, output_limit, memory_bytes)
    try:
        process = subprocess.Popen(command, **options)
    except OSError as error:
        return {"started": False, "error": str(error), "returncode": None, "stdout": b"", "stderr": b"", "elapsed_ms": int((time.monotonic() - started) * 1000), "timed_out": False, "output_limited": False}
    selector = selectors.DefaultSelector()
    assert process.stdout is not None and process.stderr is not None
    for stream, name in ((process.stdout, "stdout"), (process.stderr, "stderr")):
        os.set_blocking(stream.fileno(), False)
        selector.register(stream, selectors.EVENT_READ, name)
    output = {"stdout": bytearray(), "stderr": bytearray()}
    total = 0
    timed_out = False
    output_limited = False
    while selector.get_map() or process.poll() is None:
        remaining = timeout_s - (time.monotonic() - started)
        if remaining <= 0 and process.poll() is None:
            timed_out = True
            _kill_group(process)
        events = selector.select(max(0, min(0.05, remaining)) if remaining > 0 else 0.01)
        for key, _ in events:
            chunk = os.read(key.fileobj.fileno(), 64 * 1024)
            if not chunk:
                selector.unregister(key.fileobj)
                key.fileobj.close()
                continue
            total += len(chunk)
            capacity = max(0, output_limit - len(output[key.data]))
            output[key.data].extend(chunk[:capacity])
            if total > output_limit and not output_limited:
                output_limited = True
                _kill_group(process)
        if process.poll() is not None and not selector.get_map():
            break
    try:
        returncode = process.wait(timeout=1)
    except subprocess.TimeoutExpired:
        _kill_group(process)
        returncode = process.wait()
    # A wrapper may exit after leaving children in its process group.
    _kill_group(process)
    selector.close()
    return {
        "started": True,
        "returncode": returncode,
        "stdout": bytes(output["stdout"]),
        "stderr": bytes(output["stderr"]),
        "elapsed_ms": int((time.monotonic() - started) * 1000),
        "timed_out": timed_out,
        "output_limited": output_limited,
        "stdout_bytes_captured": len(output["stdout"]),
        "stderr_bytes_captured": len(output["stderr"]),
    }


def _canonical_output(kind: str, data: bytes) -> Any:
    text = data.decode("utf-8")
    if kind == "utf8_exact":
        return text
    if kind == "json_value":
        return json.loads(text)
    if text and not text.endswith("\n"):
        raise EvaluationError("JSON Lines output must end with a newline")
    values = []
    for line_number, line in enumerate(text.splitlines(), 1):
        if not line.strip():
            raise EvaluationError(f"JSON Lines output contains an empty line at {line_number}")
        values.append(json.loads(line))
    return values


def _candidate_file(directory: Path, task_id: str, arm: str) -> Path:
    suffix = {"bash": ".sh", "nushell": ".nu", "execute_program": ".program"}[arm]
    source_path = directory / f"{task_id}{suffix}"
    if source_path.is_symlink():
        raise EvaluationError(f"candidate must not be a symlink: {source_path}")
    candidate = source_path.resolve(strict=True)
    base = directory.resolve(strict=True)
    if base not in candidate.parents or not candidate.is_file() or candidate.is_symlink():
        raise EvaluationError(f"candidate must be a regular file inside {directory}")
    return candidate


def parse_sandbox_wrapper(value: str) -> list[str]:
    """Validate a direct-argv wrapper template; no shell command is parsed.

    The wrapper receives the host fixture workspace at `{workspace}` and the
    candidate argv spliced at `{command}`. The wrapper must preserve access to
    that workspace and apply the operator's OS/container isolation policy. The
    evaluator cannot verify the policy implemented by an arbitrary wrapper.
    """
    try:
        argv = json.loads(value)
    except json.JSONDecodeError as error:
        raise EvaluationError("sandbox wrapper must be a JSON argv array, not a shell command string") from error
    return validate_sandbox_wrapper_argv(argv)


def validate_sandbox_wrapper_argv(argv: Any) -> list[str]:
    """Validate and resolve an argv-list wrapper supplied by library callers."""
    if not isinstance(argv, list) or not argv or len(argv) > 256:
        raise EvaluationError("sandbox wrapper must be a nonempty JSON argv array of at most 256 items")
    if not all(isinstance(item, str) and item and "\x00" not in item and len(item) <= 4096 for item in argv):
        raise EvaluationError("sandbox wrapper argv items must be nonempty strings without NUL and at most 4096 characters")
    if sum(item.count(SANDBOX_COMMAND) for item in argv) != 1 or sum(item.count(SANDBOX_WORKSPACE) for item in argv) != 1:
        raise EvaluationError("sandbox wrapper must contain exactly one {workspace} and one {command} item")
    for item in argv:
        if "{command}" in item and item != SANDBOX_COMMAND:
            raise EvaluationError("{command} must occupy a complete argv item")
    executable = shutil.which(argv[0]) if not os.path.isabs(argv[0]) else argv[0]
    if not executable or not os.path.isfile(executable) or not os.access(executable, os.X_OK):
        raise EvaluationError("sandbox wrapper executable must resolve to an executable file")
    validated = list(argv)
    validated[0] = str(Path(executable).resolve())
    return validated


def _is_pinned_public_reference(arm: str, task_id: str, directory: Path, candidate: Path) -> bool:
    expected_dir = (PUBLIC_REFERENCE_ROOT / arm).resolve()
    expected_file = expected_dir / candidate.name
    expected_hash = PUBLIC_REFERENCE_SHA256.get(f"{arm}/{task_id}")
    return (
        expected_hash is not None
        and directory.resolve(strict=True) == expected_dir
        and candidate == expected_file
        and digest_file(candidate) == expected_hash
    )


def _wrapped_command(wrapper: list[str], workspace: Path, command: list[str]) -> list[str]:
    expanded: list[str] = []
    for item in wrapper:
        if item == SANDBOX_COMMAND:
            expanded.extend(command)
        else:
            expanded.append(item.replace(SANDBOX_WORKSPACE, str(workspace.resolve())))
    return expanded


def _run_candidate(
    task: dict[str, Any], case: dict[str, Any], arm: str, candidate: Path,
    *, execute_binary: Path | None, nushell_binary: str, timeout_s: float,
    output_limit: int, cpu_s: int, memory_bytes: int,
    sandbox_wrapper: list[str] | None,
) -> dict[str, Any]:
    with tempfile.TemporaryDirectory(prefix="execute-ai-dev-") as temporary:
        root = Path(temporary)
        (root / "reports").mkdir()
        (root / "tmp").mkdir()
        for relative, content in case["files"].items():
            path = root.joinpath(*_safe_relative(relative, "fixture path").parts)
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(content, encoding="utf-8")
        extension = {"bash": ".sh", "nushell": ".nu", "execute_program": ".program"}[arm]
        candidate_copy = root / f"candidate{extension}"
        shutil.copyfile(candidate, candidate_copy)
        inputs = {item["program_input"]: item["default"] for item in task["inputs"]}
        inputs.update(case.get("inputs", {}))
        env = {
            "PATH": "/usr/bin:/bin" if sandbox_wrapper else os.environ.get("PATH", "/usr/bin:/bin"),
            "HOME": str(root / "home"),
            "TMPDIR": str(root / "tmp"),
            "LC_ALL": "C",
            "LANG": "C",
            "TZ": "UTC",
        }
        (root / "home").mkdir()
        for item in task["inputs"]:
            env[item["environment"]] = inputs[item["program_input"]]
        if arm == "bash":
            command = ["/bin/bash", "--noprofile", "--norc", str(candidate_copy)]
        elif arm == "nushell":
            nu = shutil.which(nushell_binary)
            if not nu:
                return {"status": "unavailable", "reason": "Nushell executable not found", "case_id": case["case_id"]}
            command = [nu, "--no-config-file", str(candidate_copy)]
        else:
            if execute_binary is None:
                return {"status": "unavailable", "reason": "execute binary not supplied", "case_id": case["case_id"]}
            binary = execute_binary.resolve(strict=True)
            if not binary.is_file() or not os.access(binary, os.X_OK):
                raise EvaluationError(f"execute binary is not executable: {binary}")
            command = [str(binary), "run", str(candidate_copy)]
            for name, value in sorted(inputs.items()):
                command.extend(("--input", f"{name}={value}"))
            command.extend(("--command-timeout-ms", str(max(1, int(timeout_s * 1000))), "--max-output-bytes", str(output_limit)))
        initial_snapshot = _snapshot(root)
        launched_command = _wrapped_command(sandbox_wrapper, root, command) if sandbox_wrapper else command
        run = _capture(
            launched_command, root, env, timeout_s=timeout_s, output_limit=output_limit,
            cpu_s=cpu_s, memory_bytes=memory_bytes,
        )
        final_snapshot = _snapshot(root)
        expected_path = task["output_path"]
        expected_only_delta = {expected_path}
        changed = {
            name for name in initial_snapshot.keys() | final_snapshot.keys()
            if initial_snapshot.get(name) != final_snapshot.get(name)
        }
        forbidden = sorted(changed - expected_only_delta)
        output_file = root.joinpath(*_safe_relative(expected_path, "output path").parts)
        output_present = output_file.is_file() and not output_file.is_symlink()
        oracle_kind = task["oracle"]["kind"]
        try:
            expected_value = case["expected"]
            stdout_value = _canonical_output(oracle_kind, run["stdout"])
            output_value = _canonical_output(oracle_kind, output_file.read_bytes()) if output_present else None
            stdout_match = stdout_value == expected_value
            output_match = output_value == expected_value
        except (UnicodeDecodeError, json.JSONDecodeError, EvaluationError, OSError):
            stdout_match = False
            output_match = False
        exit_success = run.get("started") is True and run.get("returncode") == 0
        within_limits = not run.get("timed_out") and not run.get("output_limited") and run.get("returncode") not in (-signal.SIGXCPU, -signal.SIGXFSZ)
        criteria = {
            "expected_stdout_and_exit_status": bool(exit_success and stdout_match),
            "exact_expected_state_delta": bool(output_present and output_match and changed == expected_only_delta),
            "forbidden_side_effects_absent": not forbidden,
            "timeout_and_resource_limits_respected": bool(within_limits),
        }
        return {
            "case_id": case["case_id"],
            "status": "pass" if all(criteria.values()) else ("unavailable" if not run.get("started", False) and run.get("returncode") is None else "fail"),
            "returncode": run.get("returncode"),
            "elapsed_ms": run.get("elapsed_ms"),
            "stdout_sha256": digest_bytes(run.get("stdout", b"")),
            "stderr_sha256": digest_bytes(run.get("stderr", b"")),
            "stdout_bytes": run.get("stdout_bytes_captured", 0),
            "stderr_bytes": run.get("stderr_bytes_captured", 0),
            "stderr_excerpt": run.get("stderr", b"").decode("utf-8", errors="replace")[:2048],
            "timed_out": run.get("timed_out", False),
            "output_limited": run.get("output_limited", False),
            "exit_success": exit_success,
            "stdout_matches_oracle": stdout_match,
            "output_file_present": output_present,
            "output_file_matches_oracle": output_match,
            "workspace_delta": sorted(changed),
            "forbidden_changes": forbidden,
            "criteria_results": criteria,
            "runner_error": run.get("error"),
        }


def run_public_fixture(
    task_id: str, candidates: dict[str, Path], *, execute_binary: Path | None,
    nushell_binary: str = "nu", timeout_s: float = 5.0, output_limit: int = 1_048_576,
    cpu_s: int = 5, memory_bytes: int = 512 * 1024 * 1024,
    trusted_reference_fixtures: bool = False,
    sandbox_wrapper: list[str] | None = None,
    allow_unsandboxed: bool = False,
) -> dict[str, Any]:
    tasks = load_public_tasks()
    if task_id not in tasks:
        raise EvaluationError(f"unknown public development task {task_id!r}")
    if timeout_s <= 0 or output_limit < 1 or cpu_s < 1 or memory_bytes < 32 * 1024 * 1024:
        raise EvaluationError("resource limits must be positive and practical")
    if sandbox_wrapper is not None and allow_unsandboxed:
        raise EvaluationError("choose either --sandbox-wrapper-json or --allow-unsandboxed, not both")
    if sandbox_wrapper is not None:
        sandbox_wrapper = validate_sandbox_wrapper_argv(sandbox_wrapper)
    task = tasks[task_id]
    prepared: dict[str, tuple[Path, Path, bool]] = {}
    for arm, directory in candidates.items():
        if arm not in {"bash", "nushell", "execute_program"}:
            raise EvaluationError(f"unsupported candidate arm {arm!r}")
        candidate = _candidate_file(directory, task_id, arm)
        pinned = _is_pinned_public_reference(arm, task_id, directory, candidate)
        prepared[arm] = (directory, candidate, pinned)
    all_pinned = bool(prepared) and all(item[2] for item in prepared.values())
    if trusted_reference_fixtures:
        if not all_pinned:
            raise EvaluationError("--trusted-reference-fixtures accepts only byte-pinned checked-in reference artifacts")
    elif sandbox_wrapper is None and not allow_unsandboxed:
        raise EvaluationError(
            "refusing to execute candidate artifacts by default; use --trusted-reference-fixtures for exact pinned references, "
            "--sandbox-wrapper-json for an operator-supplied isolated runner, or explicitly opt in with --allow-unsandboxed"
        )
    arm_runs = []
    for arm in ("bash", "nushell", "execute_program"):
        prepared_candidate = prepared.get(arm)
        if prepared_candidate is None:
            arm_runs.append({"blind_arm_id": f"fixture_{arm}", "variant": arm, "status": "unavailable", "reason": "candidate directory not supplied"})
            continue
        _, candidate, _ = prepared_candidate
        cases = [
            _run_candidate(
                task, case, arm, candidate, execute_binary=execute_binary,
                nushell_binary=nushell_binary, timeout_s=timeout_s,
                output_limit=output_limit, cpu_s=cpu_s,
                memory_bytes=memory_bytes, sandbox_wrapper=sandbox_wrapper,
            )
            for case in task["cases"]
        ]
        statuses = {case["status"] for case in cases}
        arm_runs.append({
            "blind_arm_id": f"fixture_{arm}",
            "variant": arm,
            "status": "pass" if statuses == {"pass"} else "unavailable" if statuses == {"unavailable"} else "fail",
            "cases": cases,
        })
    return {
        "schema": FIXTURE_RESULT_SCHEMA,
        "trial_type": "fixture",
        "status": "fixture_only_not_an_ai_trial",
        "claim": "none",
        "task_id": task_id,
        "split": "development",
        "fixture_only": True,
        "prompt_sha256": digest_bytes(task["prompt"].encode("utf-8")),
        "resource_limits": {"timeout_s": timeout_s, "output_bytes_total": output_limit, "cpu_seconds": cpu_s, "address_space_bytes": memory_bytes},
        "execution_isolation": (
            "trusted_byte_pinned_public_reference_process_limits_only_not_a_security_sandbox"
            if trusted_reference_fixtures else
            "operator_supplied_wrapper_policy_unverified_by_evaluator"
            if sandbox_wrapper else
            "explicit_unsandboxed_execution_opt_in"
        ),
        "sandbox_wrapper_argv_sha256": digest_bytes(canonical(sandbox_wrapper)) if sandbox_wrapper else None,
        "sandbox_policy_verified": False,
        "enforced_process_limits": {
            "wall_timeout": True,
            "captured_stdout_stderr_bytes": True,
            "cpu_seconds": os.name == "posix",
            "per_file_size_bytes": os.name == "posix",
            "process_count": False,
            "address_space_bytes": platform.system() == "Linux" and hasattr(resource, "RLIMIT_AS"),
        },
        "environment": {"os": platform.platform(), "architecture": platform.machine(), "python": sys.version.splitlines()[0]},
        "arms": arm_runs,
        "provider_runs": "not_run",
        "independent_scoring": "not_run",
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    freeze = commands.add_parser("freeze-package", help="validate and hash an externally owned sealed task pack before any provider trial")
    freeze.add_argument("--package", required=True, type=Path)
    freeze.add_argument("--seed", type=int, required=True, help="must match the already frozen preregistration seed")
    freeze.add_argument("--out", required=True, type=Path)
    verify = commands.add_parser("verify-package", help="verify package bytes and freeze record against preregistration")
    verify.add_argument("--package", required=True, type=Path)
    verify.add_argument("--freeze", required=True, type=Path)
    roster = commands.add_parser("export-roster", help="export non-secret task IDs and commitments for the result importer")
    roster.add_argument("--package", required=True, type=Path)
    roster.add_argument("--freeze", required=True, type=Path)
    roster.add_argument("--out", required=True, type=Path)
    plan = commands.add_parser("plan", help="make a deterministic randomized blind schedule; does not call a provider")
    plan.add_argument("--package", required=True, type=Path)
    plan.add_argument("--freeze", required=True, type=Path)
    plan.add_argument("--arms", required=True, type=Path)
    plan.add_argument("--out", required=True, type=Path)
    dev = commands.add_parser("run-public", help="execute public fixture tasks; outputs are never importable as model trials")
    dev.add_argument("--task", required=True)
    dev.add_argument("--candidate-dir", action="append", default=[], metavar="ARM=DIR", help="repeat for bash, nushell, execute_program")
    dev.add_argument("--execute", type=Path, help="prebuilt execute CLI path")
    dev.add_argument("--nushell", default="nu", help="Nushell executable name or path")
    dev.add_argument("--timeout-seconds", type=float, default=5.0)
    dev.add_argument("--output-bytes", type=int, default=1_048_576)
    dev.add_argument("--cpu-seconds", type=int, default=5)
    dev.add_argument("--memory-bytes", type=int, default=512 * 1024 * 1024)
    mode = dev.add_mutually_exclusive_group(required=True)
    mode.add_argument(
        "--trusted-reference-fixtures", action="store_true",
        help="allow only exact byte-pinned checked-in reference artifacts",
    )
    mode.add_argument(
        "--sandbox-wrapper-json",
        help="JSON argv array for an operator-supplied isolated runner; use {workspace} and one {command} splice",
    )
    mode.add_argument(
        "--allow-unsandboxed", action="store_true",
        help="explicitly run candidate code with the current user's OS permissions",
    )
    dev.add_argument("--out", type=Path)
    args = parser.parse_args()
    try:
        if args.command == "freeze-package":
            result = freeze_external_package(args.package, args.seed, args.out)
        elif args.command == "verify-package":
            freeze, tasks = verify_external_package(args.package, args.freeze)
            result = {
                "schema": "execute.ai-productivity.package-verification/1",
                "status": "valid_pending_trials",
                "package_sha256": freeze["package_sha256"],
                "freeze_sha256": digest_file(args.freeze),
                "randomization_seed": freeze["randomization_seed"],
                "task_count": len(tasks),
                "claim": "none",
            }
        elif args.command == "export-roster":
            result = export_external_roster(args.package, args.freeze, args.out)
        elif args.command == "plan":
            result = build_run_plan(args.package, args.freeze, args.arms, args.out)
        else:
            candidates: dict[str, Path] = {}
            for value in args.candidate_dir:
                arm, separator, directory = value.partition("=")
                if not separator or arm not in {"bash", "nushell", "execute_program"} or arm in candidates:
                    raise EvaluationError("--candidate-dir must be unique ARM=DIR values for bash, nushell, or execute_program")
                candidates[arm] = Path(directory)
            result = run_public_fixture(
                args.task, candidates, execute_binary=args.execute,
                nushell_binary=args.nushell, timeout_s=args.timeout_seconds,
                output_limit=args.output_bytes, cpu_s=args.cpu_seconds,
                memory_bytes=args.memory_bytes,
                trusted_reference_fixtures=args.trusted_reference_fixtures,
                sandbox_wrapper=parse_sandbox_wrapper(args.sandbox_wrapper_json) if args.sandbox_wrapper_json else None,
                allow_unsandboxed=args.allow_unsandboxed,
            )
        encoded = json.dumps(result, sort_keys=True, indent=2) + "\n"
        out_path = getattr(args, "out", None)
        if args.command == "verify-package":
            sys.stdout.write(encoded)
        elif args.command in {"plan", "freeze-package", "export-roster"}:
            print(f"saved {args.command} record: {out_path}")
        elif out_path:
            out_path.write_text(encoded, encoding="utf-8")
        else:
            sys.stdout.write(encoded)
        return 0
    except (EvaluationError, OSError, ValueError, json.JSONDecodeError) as error:
        parser.error(str(error))
    return 2


if __name__ == "__main__":
    raise SystemExit(main())
