#!/usr/bin/env python3
"""Non-measuring case executor for the focused 100-script study.

This module contains no source-derived behavior. It validates a shared brief
package and a separate evaluator-only oracle package, creates fresh fixture
workspaces, executes one submitted artifact through a caller-supplied runtime
and isolation wrapper, and retains raw observations and exact filesystem and
mock-command effects. It never calls a model and never opens corpus sources.
"""

from __future__ import annotations

import argparse
import base64
import binascii
from collections import Counter
import copy
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import platform
import re
import selectors
import shlex
import shutil
import signal
import socket
import socketserver
import subprocess
import sys
import tempfile
import threading
from datetime import datetime, timezone
from typing import Any
import uuid


HERE = Path(__file__).resolve().parent
if str(HERE) not in sys.path:
    sys.path.insert(0, str(HERE))
import evaluate

BRIEF_SCHEMA = "execute.real-bash-100-task-package/1"
ORACLE_SCHEMA = "execute.real-bash-100-case-oracles/1"
ROSTER_SCHEMA = "execute.ai-productivity.real-bash-100-roster/1"
OBSERVATION_SCHEMA = "execute.real-bash-100-case-observation/1"
TASK_ID = re.compile(r"^[A-Za-z0-9_.-]{1,100}$")
CRITERION_ID = re.compile(r"^[a-z][a-z0-9_]{1,63}$")
SHA256 = re.compile(r"^[a-f0-9]{64}$")
ENV_NAME = re.compile(r"^[A-Z_][A-Z0-9_]*$")
MOCK_NAME = re.compile(r"^[A-Za-z0-9_.+-]{1,100}$")
CRITERION_OPERATIONS = {
    "process_io_exact",
    "filesystem_final_exact",
    "mock_command_effects_exact",
    "all_cases_observed",
}
RESERVED_WORKSPACE_NAME = ".execute-study-runner"
DEFAULT_LIMITS = {"timeout_seconds": 15, "cpu_seconds": 10, "memory_bytes": 1_073_741_824, "output_bytes": 1_048_576, "file_bytes": 1_048_576}
MAX_CASES = 128
MAX_TREE_NODES = 512
MAX_FIXTURE_BYTES = 16 * 1024 * 1024


class RunnerError(ValueError):
    """Package, contract, or observation error."""


class CandidateObservationLimit(RunnerError):
    """Candidate-created workspace exceeded a retained observation bound."""

    def __init__(self, code: str, message: str, nodes: list[dict[str, Any]], file_bytes: int, observed: int):
        super().__init__(message)
        self.code = code
        self.nodes = nodes
        self.file_bytes = file_bytes
        self.observed = observed


def canonical(value: Any) -> bytes:
    return json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=False, allow_nan=False).encode("utf-8")


def digest(raw: bytes) -> str:
    return hashlib.sha256(raw).hexdigest()


def digest_file(path: Path) -> str:
    return digest(path.read_bytes())


def _reject_duplicates(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            raise RunnerError(f"duplicate JSON key {key!r}")
        result[key] = value
    return result


def read_json(path: Path, label: str) -> tuple[dict[str, Any], bytes]:
    try:
        raw = path.read_bytes()
        value = json.loads(raw, object_pairs_hook=_reject_duplicates, parse_constant=lambda value: (_ for _ in ()).throw(RunnerError(f"invalid JSON constant {value}")))
    except (OSError, UnicodeDecodeError, json.JSONDecodeError, RunnerError) as error:
        raise RunnerError(f"cannot read {label}: {error}") from error
    if not isinstance(value, dict):
        raise RunnerError(f"{label} must be a JSON object")
    return value, raw


def _keys(value: Any, required: set[str], label: str, optional: set[str] | None = None) -> dict[str, Any]:
    if not isinstance(value, dict):
        raise RunnerError(f"{label} must be an object")
    optional = optional or set()
    if required - value.keys() or value.keys() - required - optional:
        raise RunnerError(f"{label} must have exactly required keys {sorted(required)} and optional keys {sorted(optional)}")
    return value


def _sha(value: Any, label: str) -> str:
    if not isinstance(value, str) or not SHA256.fullmatch(value):
        raise RunnerError(f"{label} must be a lowercase SHA-256 digest")
    return value


def _b64(value: Any, label: str) -> bytes:
    if not isinstance(value, str):
        raise RunnerError(f"{label} must be base64 text")
    try:
        decoded = base64.b64decode(value, validate=True)
    except (ValueError, binascii.Error) as error:
        raise RunnerError(f"{label} is invalid base64") from error
    if base64.b64encode(decoded).decode("ascii") != value:
        raise RunnerError(f"{label} must use canonical base64")
    return decoded


def _safe_relative(value: Any, label: str, *, allow_dot: bool = False) -> PurePosixPath:
    if not isinstance(value, str) or not value or "\x00" in value:
        raise RunnerError(f"{label} must be a nonempty relative POSIX path")
    path = PurePosixPath(value)
    if path.is_absolute() or any(part in {"", ".", ".."} for part in value.split("/")):
        if not (allow_dot and value == "."):
            raise RunnerError(f"{label} must not be absolute, empty, dot, or parent-relative")
    if path.parts and path.parts[0] == RESERVED_WORKSPACE_NAME:
        raise RunnerError(f"{label} uses the reserved runner workspace directory")
    return path


def _valid_text_map(value: Any, label: str, *, reserved: set[str] = frozenset()) -> dict[str, str]:
    if not isinstance(value, dict):
        raise RunnerError(f"{label} must be an object")
    result = {}
    for key, text in value.items():
        if not isinstance(key, str) or not ENV_NAME.fullmatch(key) or key in reserved:
            raise RunnerError(f"{label} has an invalid or reserved variable name {key!r}")
        if not isinstance(text, str) or "\x00" in text:
            raise RunnerError(f"{label}[{key!r}] must be text without NUL")
        result[key] = text
    return result


def _validate_tree(nodes: Any, label: str) -> list[dict[str, Any]]:
    if not isinstance(nodes, list) or len(nodes) > MAX_TREE_NODES:
        raise RunnerError(f"{label} must be an array of at most {MAX_TREE_NODES} filesystem nodes")
    seen: set[str] = set()
    total = 0
    normalized = []
    for index, node in enumerate(nodes):
        row = _keys(node, {"path", "type"}, f"{label}[{index}]", {"mode", "data_base64", "target"})
        path = _safe_relative(row["path"], f"{label}[{index}].path")
        name = path.as_posix()
        if name in seen:
            raise RunnerError(f"{label} contains duplicate path {name!r}")
        seen.add(name)
        kind = row["type"]
        mode = row.get("mode", "0755" if kind == "directory" else "0644")
        if not isinstance(mode, str) or not re.fullmatch(r"[0-7]{4}", mode):
            raise RunnerError(f"{label}[{index}].mode must contain four octal digits")
        item: dict[str, Any] = {"path": name, "type": kind, "mode": mode}
        if kind == "file":
            if set(row) - {"path", "type", "mode", "data_base64"} or "data_base64" not in row:
                raise RunnerError(f"{label}[{index}] file needs only path, type, mode, and data_base64")
            data = _b64(row["data_base64"], f"{label}[{index}].data_base64")
            total += len(data)
            item["data_base64"] = base64.b64encode(data).decode("ascii")
        elif kind == "directory":
            if set(row) - {"path", "type", "mode"}:
                raise RunnerError(f"{label}[{index}] directory has unexpected content fields")
        elif kind == "symlink":
            if set(row) - {"path", "type", "target"} or "target" not in row or "mode" in row:
                raise RunnerError(f"{label}[{index}] symlink needs only path, type, and target")
            target = row["target"]
            if not isinstance(target, str) or not target or "\x00" in target or target.startswith("/"):
                raise RunnerError(f"{label}[{index}].target must be a nonempty relative link target")
            resolved_parts: list[str] = list(path.parent.parts)
            for part in target.split("/"):
                if part in {"", "."}:
                    continue
                if part == "..":
                    if not resolved_parts:
                        raise RunnerError(f"{label}[{index}].target escapes the owned fixture workspace")
                    resolved_parts.pop()
                else:
                    resolved_parts.append(part)
            item = {"path": name, "type": kind, "target": target}
        else:
            raise RunnerError(f"{label}[{index}].type must be file, directory, or symlink")
        normalized.append(item)
    if total > MAX_FIXTURE_BYTES:
        raise RunnerError(f"{label} exceeds {MAX_FIXTURE_BYTES} fixture bytes")
    by_name = {node["path"]: node for node in normalized}
    implicit_directories = {}
    for node in normalized:
        parent = PurePosixPath(node["path"]).parent
        while parent.as_posix() != ".":
            parent_name = parent.as_posix()
            existing = by_name.get(parent_name) or implicit_directories.get(parent_name)
            if existing is not None and existing["type"] != "directory":
                raise RunnerError(f"{label} path {node['path']!r} has a non-directory parent {parent_name!r}")
            if existing is None:
                implicit_directories[parent_name] = {"path": parent_name, "type": "directory", "mode": "0755"}
            parent = parent.parent
    normalized.extend(implicit_directories.values())
    if len(normalized) > MAX_TREE_NODES:
        raise RunnerError(f"{label} including parent directories exceeds {MAX_TREE_NODES} nodes")
    return sorted(normalized, key=lambda row: row["path"])


def _validate_effects(value: Any, label: str) -> list[dict[str, Any]]:
    if not isinstance(value, list) or len(value) > 64:
        raise RunnerError(f"{label} must be an array of at most 64 fixture effects")
    result = []
    for index, raw in enumerate(value):
        op = raw.get("op") if isinstance(raw, dict) else None
        if op in {"write_file", "append_file"}:
            row = _keys(raw, {"op", "path", "data_base64"}, f"{label}[{index}]")
            _safe_relative(row["path"], f"{label}[{index}].path")
            _b64(row["data_base64"], f"{label}[{index}].data_base64")
        elif op in {"remove", "mkdir"}:
            row = _keys(raw, {"op", "path"}, f"{label}[{index}]")
            _safe_relative(row["path"], f"{label}[{index}].path")
        elif op == "rename":
            row = _keys(raw, {"op", "source", "destination"}, f"{label}[{index}]")
            _safe_relative(row["source"], f"{label}[{index}].source")
            _safe_relative(row["destination"], f"{label}[{index}].destination")
        else:
            raise RunnerError(f"{label}[{index}].op is unsupported")
        result.append(dict(row))
    return result


def _validate_observed_effects(value: Any, label: str) -> list[dict[str, Any]]:
    if not isinstance(value, list):
        raise RunnerError(f"{label} must be an array")
    result = []
    for index, raw in enumerate(value):
        item_label = f"{label}[{index}]"
        op = raw.get("op") if isinstance(raw, dict) else None
        if op in {"write_file", "append_file"}:
            row = _keys(raw, {"op", "path", "sha256", "bytes"}, item_label)
            _safe_relative(row["path"], f"{item_label}.path")
            _sha(row["sha256"], f"{item_label}.sha256")
            if type(row["bytes"]) is not int or row["bytes"] < 0:
                raise RunnerError(f"{item_label}.bytes must be a nonnegative integer")
        elif op in {"remove", "mkdir"}:
            row = _keys(raw, {"op", "path"}, item_label)
            _safe_relative(row["path"], f"{item_label}.path")
        elif op == "rename":
            row = _keys(raw, {"op", "source", "destination"}, item_label)
            _safe_relative(row["source"], f"{item_label}.source")
            _safe_relative(row["destination"], f"{item_label}.destination")
        else:
            raise RunnerError(f"{item_label}.op is unsupported")
        result.append(dict(row))
    return result


def _validate_case(case: Any, label: str) -> dict[str, Any]:
    row = _keys(
        case,
        {"case_id", "cwd", "argv", "stdin_base64", "env", "filesystem_initial", "mock_commands", "expected"},
        label,
    )
    if not isinstance(row["case_id"], str) or not TASK_ID.fullmatch(row["case_id"]):
        raise RunnerError(f"{label}.case_id is malformed")
    _safe_relative(row["cwd"], f"{label}.cwd", allow_dot=True)
    if not isinstance(row["argv"], list) or any(not isinstance(arg, str) or "\x00" in arg for arg in row["argv"]):
        raise RunnerError(f"{label}.argv must be an array of strings without NUL")
    stdin_bytes = _b64(row["stdin_base64"], f"{label}.stdin_base64")
    if len(stdin_bytes) > MAX_FIXTURE_BYTES:
        raise RunnerError(f"{label}.stdin_base64 exceeds the input limit")
    env = _valid_text_map(row["env"], f"{label}.env", reserved={"PATH", "HOME", "TMPDIR", "LC_ALL", "LANG", "TZ", "EXECUTE_STUDY_MOCK_SOCKET", "EXECUTE_STUDY_WORKSPACE", "PYTHONDONTWRITEBYTECODE", "PYTHONPYCACHEPREFIX"})
    initial = _validate_tree(row["filesystem_initial"], f"{label}.filesystem_initial")
    initial_by_path = {node["path"]: node for node in initial}
    for reserved_path in ("home", "tmp"):
        if reserved_path in initial_by_path and initial_by_path[reserved_path]["type"] != "directory":
            raise RunnerError(f"{label}.filesystem_initial reserves {reserved_path!r} as a directory")
    if row["cwd"] != "." and initial_by_path.get(row["cwd"], {}).get("type") != "directory":
        raise RunnerError(f"{label}.cwd must name a directory in filesystem_initial")
    if not isinstance(row["mock_commands"], list):
        raise RunnerError(f"{label}.mock_commands must be an array")
    mock_names: set[str] = set()
    mocks = []
    for mock_index, raw_mock in enumerate(row["mock_commands"]):
        mock_label = f"{label}.mock_commands[{mock_index}]"
        mock = _keys(raw_mock, {"name", "steps"}, mock_label, {"observe_env"})
        if not isinstance(mock["name"], str) or not MOCK_NAME.fullmatch(mock["name"]) or mock["name"] in {".", ".."} or "/" in mock["name"] or mock["name"] in mock_names:
            raise RunnerError(f"{mock_label}.name must be a unique command basename")
        mock_names.add(mock["name"])
        observe_env = mock.get("observe_env", [])
        if not isinstance(observe_env, list) or any(not isinstance(name, str) or not ENV_NAME.fullmatch(name) for name in observe_env) or len(set(observe_env)) != len(observe_env):
            raise RunnerError(f"{mock_label}.observe_env must be unique environment variable names")
        if not isinstance(mock["steps"], list) or not mock["steps"]:
            raise RunnerError(f"{mock_label}.steps must be a nonempty array")
        steps = []
        for step_index, raw_step in enumerate(mock["steps"]):
            step_label = f"{mock_label}.steps[{step_index}]"
            step = _keys(raw_step, {"match", "response", "effects"}, step_label)
            match = _keys(step["match"], {"argv"}, f"{step_label}.match", {"stdin_sha256", "stdin_base64"})
            if not isinstance(match["argv"], list) or any(not isinstance(item, str) or "\x00" in item for item in match["argv"]):
                raise RunnerError(f"{step_label}.match.argv must be strings without NUL")
            if "stdin_sha256" in match:
                _sha(match["stdin_sha256"], f"{step_label}.match.stdin_sha256")
            if "stdin_base64" in match:
                match_stdin = _b64(match["stdin_base64"], f"{step_label}.match.stdin_base64")
                if len(match_stdin) > MAX_FIXTURE_BYTES:
                    raise RunnerError(f"{step_label}.match.stdin_base64 exceeds the input limit")
                if "stdin_sha256" in match and digest(match_stdin) != match["stdin_sha256"]:
                    raise RunnerError(f"{step_label}.match.stdin_sha256 does not match stdin_base64")
            response = _keys(step["response"], {"returncode", "stdout_base64", "stderr_base64"}, f"{step_label}.response")
            if type(response["returncode"]) is not int or not 0 <= response["returncode"] <= 255:
                raise RunnerError(f"{step_label}.response.returncode must be an integer from 0 through 255")
            _b64(response["stdout_base64"], f"{step_label}.response.stdout_base64")
            _b64(response["stderr_base64"], f"{step_label}.response.stderr_base64")
            effects = _validate_effects(step["effects"], f"{step_label}.effects")
            steps.append({"match": dict(match), "response": dict(response), "effects": effects})
        mocks.append({"name": mock["name"], "observe_env": observe_env, "steps": steps})
    expected = _keys(row["expected"], {"returncode", "stdout_base64", "stderr_base64", "filesystem_after", "mock_events"}, f"{label}.expected")
    if type(expected["returncode"]) is not int or not -255 <= expected["returncode"] <= 255:
        raise RunnerError(f"{label}.expected.returncode must be an integer from -255 through 255")
    _b64(expected["stdout_base64"], f"{label}.expected.stdout_base64")
    _b64(expected["stderr_base64"], f"{label}.expected.stderr_base64")
    final = _validate_tree(expected["filesystem_after"], f"{label}.expected.filesystem_after")
    if not isinstance(expected["mock_events"], list):
        raise RunnerError(f"{label}.expected.mock_events must be an array")
    event_fields = {"event_index", "command", "call_index", "matched_step", "argv", "stdin_sha256", "stdin_base64", "cwd", "env", "returncode", "stdout_sha256", "stderr_sha256", "matched", "applied_effects"}
    for event_index, raw_event in enumerate(expected["mock_events"]):
        event = _keys(raw_event, event_fields, f"{label}.expected.mock_events[{event_index}]")
        if type(event["event_index"]) is not int or event["event_index"] != event_index:
            raise RunnerError(f"{label}.expected.mock_events must have contiguous ordered event_index values")
        if not isinstance(event["command"], str) or not MOCK_NAME.fullmatch(event["command"]) or event["command"] in {".", ".."} or "/" in event["command"]:
            raise RunnerError(f"{label}.expected.mock_events[{event_index}].command is invalid")
        if not isinstance(event["argv"], list) or any(not isinstance(item, str) or "\x00" in item for item in event["argv"]):
            raise RunnerError(f"{label}.expected.mock_events[{event_index}].argv must be strings")
        if event["call_index"] is not None and (type(event["call_index"]) is not int or event["call_index"] < 0):
            raise RunnerError(f"{label}.expected.mock_events[{event_index}].call_index is invalid")
        if event["matched_step"] is not None and (type(event["matched_step"]) is not int or event["matched_step"] < 0):
            raise RunnerError(f"{label}.expected.mock_events[{event_index}].matched_step is invalid")
        _sha(event["stdin_sha256"], f"{label}.expected.mock_events[{event_index}].stdin_sha256")
        event_stdin = _b64(event["stdin_base64"], f"{label}.expected.mock_events[{event_index}].stdin_base64")
        if len(event_stdin) > MAX_FIXTURE_BYTES or digest(event_stdin) != event["stdin_sha256"]:
            raise RunnerError(f"{label}.expected.mock_events[{event_index}] stdin digest does not match its bytes")
        if not isinstance(event["cwd"], str) or "\x00" in event["cwd"] or not isinstance(event["matched"], bool):
            raise RunnerError(f"{label}.expected.mock_events[{event_index}] has malformed cwd/matched values")
        if not isinstance(event["env"], dict) or not all(isinstance(key, str) and ENV_NAME.fullmatch(key) and isinstance(value, str) and "\x00" not in value for key, value in event["env"].items()):
            raise RunnerError(f"{label}.expected.mock_events[{event_index}].env must be a text map")
        if type(event["returncode"]) is not int or not 0 <= event["returncode"] <= 255:
            raise RunnerError(f"{label}.expected.mock_events[{event_index}].returncode must be from 0 through 255")
        _sha(event["stdout_sha256"], f"{label}.expected.mock_events[{event_index}].stdout_sha256")
        _sha(event["stderr_sha256"], f"{label}.expected.mock_events[{event_index}].stderr_sha256")
        _validate_observed_effects(event["applied_effects"], f"{label}.expected.mock_events[{event_index}].applied_effects")
    return {
        "case_id": row["case_id"], "cwd": row["cwd"], "argv": list(row["argv"]),
        "stdin_base64": base64.b64encode(stdin_bytes).decode("ascii"), "env": env,
        "filesystem_initial": initial, "mock_commands": mocks,
        "expected": {"returncode": expected["returncode"], "stdout_base64": expected["stdout_base64"], "stderr_base64": expected["stderr_base64"], "filesystem_after": final, "mock_events": expected["mock_events"]},
    }


def validate_brief_package(document: Any, *, allow_fixture: bool = False) -> dict[str, dict[str, Any]]:
    package = _keys(document, {"schema", "status", "fixture_only", "protocol_sha256", "corpus_manifest_sha256", "tasks"}, "task package")
    if package["schema"] != BRIEF_SCHEMA:
        raise RunnerError(f"task package schema must be {BRIEF_SCHEMA}")
    fixture = package["fixture_only"] is True
    if type(package["fixture_only"]) is not bool or fixture and not allow_fixture:
        raise RunnerError("fixture-only packages are accepted only by explicitly fixture-scoped test calls")
    valid_status = {"draft_fixture"} if fixture else {"frozen_before_generation"}
    if package["status"] not in valid_status:
        raise RunnerError("task package status is not eligible for this execution mode")
    _sha(package["protocol_sha256"], "task package protocol_sha256")
    _sha(package["corpus_manifest_sha256"], "task package corpus_manifest_sha256")
    tasks = package["tasks"]
    expected_count = None if fixture else 100
    if not isinstance(tasks, list) or not tasks or expected_count is not None and len(tasks) != expected_count:
        raise RunnerError("task package must contain 100 tasks (or a nonempty explicitly fixture-only test set)")
    by_id: dict[str, dict[str, Any]] = {}
    fields = {"task_id", "family_id", "split", "stratum", "testability_stratum", "prompt", "behavior_brief", "prompt_sha256", "behavior_brief_sha256"}
    for index, raw_task in enumerate(tasks):
        label = f"task package.tasks[{index}]"
        task = _keys(raw_task, fields, label)
        for field in ("task_id", "family_id"):
            if not isinstance(task[field], str) or not TASK_ID.fullmatch(task[field]):
                raise RunnerError(f"{label}.{field} is malformed")
        if task["task_id"] in by_id:
            raise RunnerError(f"duplicate task ID {task['task_id']!r}")
        if not all(isinstance(task[field], str) and task[field] for field in ("testability_stratum", "prompt", "behavior_brief")):
            raise RunnerError(f"{label} must have nonempty testability_stratum, prompt, and behavior_brief text")
        if task["split"] != "held_out" or task["stratum"] != "workflow_cli":
            raise RunnerError(f"{label} must target the frozen held_out/workflow_cli study")
        for text_field, hash_field in (("prompt", "prompt_sha256"), ("behavior_brief", "behavior_brief_sha256")):
            actual = digest(task[text_field].encode("utf-8"))
            if _sha(task[hash_field], f"{label}.{hash_field}") != actual:
                raise RunnerError(f"{label}.{hash_field} does not match its exact UTF-8 text")
        by_id[task["task_id"]] = dict(task)
    return by_id


def oracle_material(task: dict[str, Any]) -> dict[str, Any]:
    return {"criteria": task["criteria"], "cases": task["cases"]}


def validate_oracle_package(document: Any, brief_sha256: str, task_ids: set[str], *, allow_fixture: bool = False) -> dict[str, dict[str, Any]]:
    package = _keys(document, {"schema", "status", "fixture_only", "task_package_sha256", "tasks"}, "oracle package")
    if package["schema"] != ORACLE_SCHEMA:
        raise RunnerError(f"oracle package schema must be {ORACLE_SCHEMA}")
    fixture = package["fixture_only"] is True
    if type(package["fixture_only"]) is not bool or fixture and not allow_fixture:
        raise RunnerError("fixture-only oracle packages are accepted only by explicitly fixture-scoped test calls")
    status = "draft_fixture" if fixture else "frozen_before_generation"
    if package["status"] != status:
        raise RunnerError("oracle package status is not eligible for this execution mode")
    if _sha(package["task_package_sha256"], "oracle package task_package_sha256") != brief_sha256:
        raise RunnerError("oracle package is bound to different shared brief package bytes")
    tasks = package["tasks"]
    if not isinstance(tasks, list) or len(tasks) != len(task_ids) or any(not isinstance(row, dict) for row in tasks):
        raise RunnerError("oracle package task IDs must exactly match the shared brief package")
    raw_task_ids = [row.get("task_id") for row in tasks]
    if any(not isinstance(task_id, str) for task_id in raw_task_ids) or set(raw_task_ids) != task_ids:
        raise RunnerError("oracle package task IDs must exactly match the shared brief package")
    by_id = {}
    for index, raw_task in enumerate(tasks):
        label = f"oracle package.tasks[{index}]"
        task = _keys(raw_task, {"task_id", "oracle_sha256", "criteria", "cases"}, label)
        task_id = task["task_id"]
        if task_id not in task_ids or task_id in by_id:
            raise RunnerError(f"{label}.task_id is unknown or duplicated")
        if not isinstance(task["criteria"], dict) or not task["criteria"]:
            raise RunnerError(f"{label}.criteria must map criterion IDs to observation operations")
        criteria = {}
        for key, operation in task["criteria"].items():
            if not isinstance(key, str) or not CRITERION_ID.fullmatch(key) or not isinstance(operation, str) or operation not in CRITERION_OPERATIONS:
                raise RunnerError(f"{label}.criteria has an invalid ID or unsupported observation operation")
            criteria[key] = operation
        if criteria.get("all_cases_observed") != "all_cases_observed" or list(criteria.values()).count("all_cases_observed") != 1:
            raise RunnerError(f"{label}.criteria must contain the reserved all_cases_observed ID/op exactly once")
        if not isinstance(task["cases"], list) or not 1 <= len(task["cases"]) <= MAX_CASES:
            raise RunnerError(f"{label}.cases must contain 1 through {MAX_CASES} cases")
        cases = []
        for case_index, raw_case in enumerate(task["cases"]):
            case = _validate_case(raw_case, f"{label}.cases[{case_index}]")
            case["_runner_template"] = copy.deepcopy(raw_case)
            case["_runner_template_sha256"] = digest(canonical(raw_case))
            cases.append(case)
        case_ids = [case["case_id"] for case in cases]
        if len(set(case_ids)) != len(case_ids):
            raise RunnerError(f"{label}.cases contains duplicate case IDs")
        normalized = {"task_id": task_id, "criteria": criteria, "cases": cases}
        computed = digest(canonical({"criteria": task["criteria"], "cases": task["cases"]}))
        if _sha(task["oracle_sha256"], f"{label}.oracle_sha256") != computed:
            raise RunnerError(f"{label}.oracle_sha256 does not match its criteria and case bytes")
        normalized["oracle_sha256"] = computed
        by_id[task_id] = normalized
    return by_id


def validate_packages(brief_document: Any, brief_raw: bytes, oracle_document: Any, *, allow_fixture: bool = False) -> tuple[dict[str, dict[str, Any]], dict[str, dict[str, Any]]]:
    try:
        parsed_brief = json.loads(
            brief_raw,
            object_pairs_hook=_reject_duplicates,
            parse_constant=lambda value: (_ for _ in ()).throw(RunnerError(f"invalid JSON constant {value}")),
        )
    except (UnicodeDecodeError, json.JSONDecodeError, RunnerError) as error:
        raise RunnerError(f"shared task package bytes are invalid: {error}") from error
    if parsed_brief != brief_document:
        raise RunnerError("shared task package document does not match its exact bound bytes")
    brief_tasks = validate_brief_package(brief_document, allow_fixture=allow_fixture)
    oracle_tasks = validate_oracle_package(oracle_document, digest(brief_raw), set(brief_tasks), allow_fixture=allow_fixture)
    if bool(brief_document["fixture_only"]) != bool(oracle_document["fixture_only"]):
        raise RunnerError("task and oracle package fixture_only flags must match")
    return brief_tasks, oracle_tasks


def validate_roster_binding(
    roster_document: Any, roster_raw: bytes, brief_raw: bytes,
    brief_tasks: dict[str, dict[str, Any]], oracle_tasks: dict[str, dict[str, Any]],
) -> str:
    """Bind every private oracle criterion map and public brief to the frozen ID roster."""
    try:
        parsed = json.loads(
            roster_raw,
            object_pairs_hook=_reject_duplicates,
            parse_constant=lambda value: (_ for _ in ()).throw(RunnerError(f"invalid JSON constant {value}")),
        )
    except (UnicodeDecodeError, json.JSONDecodeError, RunnerError) as error:
        raise RunnerError(f"frozen roster bytes are invalid: {error}") from error
    if parsed != roster_document:
        raise RunnerError("frozen roster document does not match its exact bound bytes")
    roster = _keys(
        roster_document,
        {"schema", "status", "frozen_utc", "protocol_sha256", "corpus_manifest_sha256", "digest_receipt_sha256", "task_package_sha256", "tasks"},
        "frozen roster",
    )
    if roster["schema"] != ROSTER_SCHEMA or roster["status"] != "frozen_before_generation":
        raise RunnerError("frozen roster must use the real-bash-100 frozen roster schema/status")
    if not isinstance(roster["frozen_utc"], str) or not roster["frozen_utc"]:
        raise RunnerError("frozen roster frozen_utc must be nonempty text")
    for field in ("protocol_sha256", "corpus_manifest_sha256", "digest_receipt_sha256", "task_package_sha256"):
        _sha(roster[field], f"frozen roster {field}")
    if roster["task_package_sha256"] != digest(brief_raw):
        raise RunnerError("frozen roster is bound to different shared task-package bytes")
    if not isinstance(roster["tasks"], list) or len(roster["tasks"]) != 100:
        raise RunnerError("frozen roster must contain exactly 100 tasks")
    task_fields = {
        "task_id", "family_id", "split", "stratum", "testability_stratum", "repo_url", "source_commit", "source_path",
        "source_sha256", "manifest_record_sha256", "prompt_sha256", "behavior_brief_sha256", "oracle_sha256", "oracle_ref",
        "criteria", "timing_case_set_sha256", "timing_case_ref",
    }
    roster_tasks: dict[str, dict[str, Any]] = {}
    for index, raw_task in enumerate(roster["tasks"]):
        label = f"frozen roster.tasks[{index}]"
        task = _keys(raw_task, task_fields, label)
        task_id = task["task_id"]
        if not isinstance(task_id, str) or task_id in roster_tasks:
            raise RunnerError(f"{label}.task_id is malformed or duplicated")
        criteria = task["criteria"]
        if not isinstance(criteria, dict) or not criteria or any(
            not isinstance(item, str)
            or not CRITERION_ID.fullmatch(item)
            or not isinstance(operation, str)
            or operation not in CRITERION_OPERATIONS
            for item, operation in criteria.items()
        ):
            raise RunnerError(f"{label}.criteria must map stable criterion IDs to supported observation operations")
        if criteria.get("all_cases_observed") != "all_cases_observed" or list(criteria.values()).count("all_cases_observed") != 1:
            raise RunnerError(f"{label}.criteria must contain the reserved all_cases_observed ID/op exactly once")
        roster_tasks[task_id] = task
    if set(roster_tasks) != set(brief_tasks) or set(roster_tasks) != set(oracle_tasks):
        raise RunnerError("frozen roster, shared task package, and evaluator oracle task IDs must match exactly")
    for task_id, roster_task in roster_tasks.items():
        brief_task = brief_tasks[task_id]
        oracle_task = oracle_tasks[task_id]
        expected = {
            "family_id": brief_task["family_id"],
            "split": brief_task["split"],
            "stratum": brief_task["stratum"],
            "testability_stratum": brief_task["testability_stratum"],
            "prompt_sha256": brief_task["prompt_sha256"],
            "behavior_brief_sha256": brief_task["behavior_brief_sha256"],
            "oracle_sha256": oracle_task["oracle_sha256"],
        }
        for field, value in expected.items():
            if roster_task[field] != value:
                raise RunnerError(f"frozen roster task {task_id!r} has a mismatched {field}")
        if roster_task["criteria"] != oracle_task["criteria"]:
            raise RunnerError(f"frozen roster task {task_id!r} criterion ID-to-operation map does not exactly match evaluator operations")
    return digest(roster_raw)


def _materialize_tree(root: Path, nodes: list[dict[str, Any]]) -> None:
    directories = [node for node in nodes if node["type"] == "directory"]
    files = [node for node in nodes if node["type"] == "file"]
    links = [node for node in nodes if node["type"] == "symlink"]
    for node in directories:
        path = root.joinpath(*PurePosixPath(node["path"]).parts)
        path.mkdir(parents=True, exist_ok=True)
    for node in files:
        path = root.joinpath(*PurePosixPath(node["path"]).parts)
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(_b64(node["data_base64"], f"fixture {node['path']}"))
        path.chmod(int(node["mode"], 8))
    for node in links:
        path = root.joinpath(*PurePosixPath(node["path"]).parts)
        path.parent.mkdir(parents=True, exist_ok=True)
        path.symlink_to(node["target"])
    for node in sorted(directories, key=lambda row: row["path"].count("/"), reverse=True):
        root.joinpath(*PurePosixPath(node["path"]).parts).chmod(int(node["mode"], 8))


PLACEHOLDER_NAMES = ("{workspace}", "{home}", "{tmp}")


def _expand_case_template(template: dict[str, Any], workspace: Path) -> dict[str, Any]:
    """Bind exact path tokens throughout one private evaluator case template."""
    bindings = {
        "{workspace}": str(workspace),
        "{home}": str(workspace / "home"),
        "{tmp}": str(workspace / "tmp"),
    }
    byte_bindings = {token.encode("ascii"): value.encode("utf-8") for token, value in bindings.items()}

    def visit(value: Any, key: str | None = None) -> Any:
        if isinstance(value, dict):
            return {name: visit(item, name) for name, item in value.items()}
        if isinstance(value, list):
            return [visit(item, key) for item in value]
        if isinstance(value, str):
            if key == "case_id":
                return value
            if key is not None and key.endswith("_base64"):
                raw = _b64(value, f"template.{key}")
                for token, replacement in byte_bindings.items():
                    raw = raw.replace(token, replacement)
                return base64.b64encode(raw).decode("ascii")
            for token, replacement in bindings.items():
                value = value.replace(token, replacement)
            return value
        return value

    expanded = visit(template)
    if not isinstance(expanded, dict):
        raise RunnerError("materialized case template must remain an object")
    return expanded


def _normalize_observed_env(values: Any, root: Path) -> dict[str, str]:
    if not isinstance(values, dict):
        return {}
    normalized = {}
    resolved_root = root.resolve()
    for key, value in values.items():
        if not isinstance(key, str) or not isinstance(value, str):
            continue
        try:
            relative = Path(value).resolve(strict=False).relative_to(resolved_root)
            normalized[key] = "." if relative.as_posix() == "." else "$WORKSPACE/" + relative.as_posix()
        except (OSError, ValueError):
            normalized[key] = value
    return normalized


def _observed_effect(effect: dict[str, Any]) -> dict[str, Any]:
    operation = effect["op"]
    if operation in {"write_file", "append_file"}:
        data = _b64(effect["data_base64"], "materialized mock effect data")
        return {"op": operation, "path": effect["path"], "sha256": digest(data), "bytes": len(data)}
    if operation in {"remove", "mkdir"}:
        return {"op": operation, "path": effect["path"]}
    if operation == "rename":
        return {"op": operation, "source": effect["source"], "destination": effect["destination"]}
    raise RunnerError(f"unsupported mock effect {operation!r}")


def _refresh_materialized_case_expectations(case: dict[str, Any], workspace: Path) -> None:
    """Recompute digest fields derived from path-expanded bytes and mock rules."""
    mock_specs = {mock["name"]: mock for mock in case["mock_commands"]}
    for mock in case["mock_commands"]:
        for step in mock["steps"]:
            match = step["match"]
            if "stdin_base64" in match:
                input_bytes = _b64(match["stdin_base64"], "materialized match stdin")
                computed = digest(input_bytes)
                if "stdin_sha256" in match and match["stdin_sha256"] != computed:
                    # A hash in the template describes the original bytes and
                    # cannot bind a path after materialization. Keep the pair
                    # internally consistent with the resolved byte field.
                    match["stdin_sha256"] = computed
            elif "stdin_sha256" in match:
                _sha(match["stdin_sha256"], "materialized match stdin_sha256")
    expected = case["expected"]
    for event in expected["mock_events"]:
        event_stdin = _b64(event["stdin_base64"], "materialized expected event stdin")
        event["stdin_sha256"] = digest(event_stdin)
        event["env"] = _normalize_observed_env(event["env"], workspace)
        if not event["matched"]:
            continue
        spec = mock_specs.get(event["command"])
        step_index = event["matched_step"]
        if spec is None or type(step_index) is not int or not 0 <= step_index < len(spec["steps"]):
            raise RunnerError("materialized matched mock event has no corresponding command step")
        step = spec["steps"][step_index]
        response = step["response"]
        event["returncode"] = response["returncode"]
        event["stdout_sha256"] = digest(_b64(response["stdout_base64"], "materialized mock stdout"))
        event["stderr_sha256"] = digest(_b64(response["stderr_base64"], "materialized mock stderr"))
        event["applied_effects"] = [_observed_effect(effect) for effect in step["effects"]]


def _tree_snapshot(root: Path) -> list[dict[str, Any]]:
    rows = []
    total_file_bytes = 0
    for directory, dirnames, filenames in os.walk(root, topdown=True, followlinks=False):
        current = Path(directory)
        if current == root:
            dirnames[:] = sorted(name for name in dirnames if name != RESERVED_WORKSPACE_NAME)
        else:
            dirnames.sort()
        for name in sorted(dirnames + filenames):
            path = current / name
            rel = path.relative_to(root).as_posix()
            if rel == RESERVED_WORKSPACE_NAME:
                continue
            if len(rows) >= MAX_TREE_NODES:
                observed = len(rows) + 1
                raise CandidateObservationLimit(
                    "node_limit",
                    f"candidate workspace exceeds {MAX_TREE_NODES} retained nodes (at least {observed})",
                    rows,
                    total_file_bytes,
                    observed,
                )
            info = path.lstat()
            if path.is_symlink():
                row = {"path": rel, "type": "symlink", "target": os.readlink(path)}
            elif path.is_dir():
                row = {"path": rel, "type": "directory", "mode": f"{info.st_mode & 0o7777:04o}"}
            elif path.is_file():
                file_size = info.st_size
                if total_file_bytes + file_size > MAX_FIXTURE_BYTES:
                    observed = total_file_bytes + file_size
                    raise CandidateObservationLimit(
                        "file_byte_limit",
                        f"candidate workspace exceeds {MAX_FIXTURE_BYTES} retained file bytes (at least {observed})",
                        rows,
                        total_file_bytes,
                        observed,
                    )
                data = path.read_bytes()
                total_file_bytes += len(data)
                if len(data) != file_size:
                    raise OSError(f"workspace file changed while being observed: {rel}")
                row = {"path": rel, "type": "file", "mode": f"{info.st_mode & 0o7777:04o}", "data_base64": base64.b64encode(data).decode("ascii")}
            else:
                row = {"path": rel, "type": "other", "mode": f"{info.st_mode & 0o7777:04o}"}
            rows.append(row)
    return sorted(rows, key=lambda row: row["path"])


def _node_fingerprint(nodes: list[dict[str, Any]]) -> str:
    return digest(canonical(nodes))


def _apply_effect(root: Path, effect: dict[str, Any]) -> dict[str, Any]:
    operation = effect["op"]
    if operation in {"write_file", "append_file"}:
        rel = _safe_relative(effect["path"], "mock effect path")
        path = root.joinpath(*rel.parts)
        path.parent.mkdir(parents=True, exist_ok=True)
        data = _b64(effect["data_base64"], "mock effect data")
        if operation == "append_file":
            with path.open("ab") as stream:
                stream.write(data)
        else:
            path.write_bytes(data)
        return {"op": operation, "path": rel.as_posix(), "sha256": digest(data), "bytes": len(data)}
    if operation == "remove":
        rel = _safe_relative(effect["path"], "mock effect path")
        path = root.joinpath(*rel.parts)
        if path.is_dir() and not path.is_symlink():
            path.rmdir()
        else:
            path.unlink()
        return {"op": operation, "path": rel.as_posix()}
    if operation == "mkdir":
        rel = _safe_relative(effect["path"], "mock effect path")
        root.joinpath(*rel.parts).mkdir(parents=True, exist_ok=True)
        return {"op": operation, "path": rel.as_posix()}
    if operation == "rename":
        source = root.joinpath(*_safe_relative(effect["source"], "mock effect source").parts)
        destination_rel = _safe_relative(effect["destination"], "mock effect destination")
        destination = root.joinpath(*destination_rel.parts)
        destination.parent.mkdir(parents=True, exist_ok=True)
        source.rename(destination)
        return {"op": operation, "source": effect["source"], "destination": destination_rel.as_posix()}
    raise RunnerError(f"unsupported mock effect {operation!r}")


class _MockServer(socketserver.ThreadingUnixStreamServer):
    allow_reuse_address = False
    daemon_threads = False
    block_on_close = True

    def __init__(self, path: str, root: Path, specs: list[dict[str, Any]]):
        self.root = root
        self.specs = {spec["name"]: spec for spec in specs}
        self.indices: Counter[str] = Counter()
        self.events: list[dict[str, Any]] = []
        self._event_lock = threading.Lock()
        self._dispatch_lock = threading.Lock()
        super().__init__(path, _MockHandler)

    def _normalized_env(self, request: dict[str, Any]) -> dict[str, str]:
        return _normalize_observed_env(request.get("env", {}), self.root)

    def dispatch(self, request: dict[str, Any]) -> dict[str, Any]:
        with self._dispatch_lock:
            return self._dispatch_one(request)

    def _dispatch_one(self, request: dict[str, Any]) -> dict[str, Any]:
        command = request.get("command")
        spec = self.specs.get(command)
        if spec is None:
            return self._error_event(request, "unknown mock command")
        argv = request.get("argv")
        stdin = _b64(request.get("stdin_base64"), "mock request stdin")
        index = self.indices[command]
        self.indices[command] += 1
        selected = None
        for step_index, step in enumerate(spec["steps"]):
            match = step["match"]
            if index != step_index or argv != match["argv"]:
                continue
            if "stdin_base64" in match and stdin != _b64(match["stdin_base64"], "mock match stdin"):
                continue
            if "stdin_sha256" in match and digest(stdin) != match["stdin_sha256"]:
                continue
            selected = (step_index, step)
            break
        if selected is None:
            reply = {"returncode": 125, "stdout_base64": "", "stderr_base64": base64.b64encode(b"mock protocol error: unexpected command invocation\n").decode("ascii")}
            event = {
                "command": command, "call_index": index, "matched_step": None, "argv": argv, "stdin_sha256": digest(stdin),
                "stdin_base64": base64.b64encode(stdin).decode("ascii"), "cwd": request.get("cwd"), "env": self._normalized_env(request), "returncode": 125, "stdout_sha256": digest(b""),
                "stderr_sha256": digest(_b64(reply["stderr_base64"], "mock stderr")), "matched": False,
                "applied_effects": [],
            }
        else:
            step_index, step = selected
            response = step["response"]
            stdout = _b64(response["stdout_base64"], "mock stdout")
            stderr = _b64(response["stderr_base64"], "mock stderr")
            applied = []
            try:
                for effect in step["effects"]:
                    applied.append(_apply_effect(self.root, effect))
                returncode = response["returncode"]
                matched = True
            except (OSError, RunnerError) as error:
                returncode = 125
                stderr += f"mock fixture effect failed: {error}\n".encode("utf-8", errors="replace")
                matched = False
            reply = {"returncode": returncode, "stdout_base64": base64.b64encode(stdout).decode("ascii"), "stderr_base64": base64.b64encode(stderr).decode("ascii")}
            event = {
                "command": command, "call_index": index, "matched_step": step_index, "argv": argv,
                "stdin_sha256": digest(stdin), "stdin_base64": base64.b64encode(stdin).decode("ascii"), "cwd": request.get("cwd"), "env": self._normalized_env(request), "returncode": returncode,
                "stdout_sha256": digest(stdout), "stderr_sha256": digest(stderr), "matched": matched,
                "applied_effects": applied,
            }
        with self._event_lock:
            event["event_index"] = len(self.events)
            self.events.append(event)
        return reply

    def _error_event(self, request: dict[str, Any], message: str) -> dict[str, Any]:
        stderr = f"mock protocol error: {message}\n".encode()
        try:
            stdin = _b64(request.get("stdin_base64"), "mock request stdin")
        except RunnerError:
            stdin = b""
        with self._event_lock:
            self.events.append({"event_index": len(self.events), "command": request.get("command"), "call_index": None, "matched_step": None, "argv": request.get("argv"), "stdin_sha256": digest(stdin), "stdin_base64": base64.b64encode(stdin).decode("ascii"), "cwd": request.get("cwd"), "env": self._normalized_env(request), "returncode": 125, "stdout_sha256": digest(b""), "stderr_sha256": digest(stderr), "matched": False, "applied_effects": []})
        return {"returncode": 125, "stdout_base64": "", "stderr_base64": base64.b64encode(stderr).decode("ascii")}

    def _transport_error(self, message: str) -> dict[str, Any]:
        return self._error_event(
            {"command": "mock_protocol_error", "argv": [], "stdin_base64": "", "cwd": ".", "env": {}},
            message,
        )


class _MockHandler(socketserver.StreamRequestHandler):
    def handle(self) -> None:
        line = self.rfile.readline(2 * MAX_FIXTURE_BYTES + 65536)
        try:
            request = json.loads(line, object_pairs_hook=_reject_duplicates)
            if not isinstance(request, dict):
                raise RunnerError("mock request must be a JSON object")
            response = self.server.dispatch(request)  # type: ignore[attr-defined]
        except Exception as error:  # the mock protocol reports errors as explicit failed command outcomes
            response = self.server._transport_error(str(error))  # type: ignore[attr-defined]
        self.wfile.write(canonical(response) + b"\n")


MOCK_CLIENT = r'''#!/usr/bin/env python3
import base64, json, os, socket, sys
try:
    sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    sock.connect(os.environ["EXECUTE_STUDY_MOCK_SOCKET"])
    request = {
        "command": sys.argv[1],
        "argv": sys.argv[2:],
        "stdin_base64": base64.b64encode(sys.stdin.buffer.read()).decode("ascii"),
        "cwd": os.path.relpath(os.path.realpath(os.getcwd()), os.path.realpath(os.environ["EXECUTE_STUDY_WORKSPACE"])),
        "env": {key: os.environ.get(key, "") for key in __OBSERVED_ENV__},
    }
    sock.sendall(json.dumps(request, separators=(",", ":")).encode("utf-8") + b"\n")
    stream = sock.makefile("rb")
    response = json.loads(stream.readline())
    sys.stdout.buffer.write(base64.b64decode(response["stdout_base64"], validate=True))
    sys.stderr.buffer.write(base64.b64decode(response["stderr_base64"], validate=True))
    raise SystemExit(int(response["returncode"]))
except Exception as error:
    sys.stderr.write("mock command transport error: " + str(error) + "\n")
    raise SystemExit(126)
'''


def _install_mock_clients(private_root: Path, specs: list[dict[str, Any]]) -> Path:
    mock_bin = private_root / "mock-bin"
    mock_bin.mkdir(parents=True)
    for spec in specs:
        path = mock_bin / spec["name"]
        client = MOCK_CLIENT.replace("__OBSERVED_ENV__", json.dumps(spec["observe_env"]))
        shim = f"#!/bin/sh\nexec {shlex.quote(sys.executable)} -c {shlex.quote(client)} {shlex.quote(spec['name'])} \"$@\"\n"
        path.write_text(shim, encoding="utf-8")
        path.chmod(0o755)
    return mock_bin


def _snapshot_write(directory: Path, filename: str, nodes: list[dict[str, Any]]) -> tuple[str, Path]:
    path = directory / filename
    raw = canonical({"schema": "execute.real-bash-100-filesystem-snapshot/1", "nodes": nodes})
    path.write_bytes(raw)
    return digest(raw), path


def _write_archive_manifest(output_dir: Path) -> tuple[dict[str, Any], str, str]:
    files = []
    manifest_path = output_dir / "archive-manifest.json"
    for path in sorted(output_dir.rglob("*"), key=lambda item: item.relative_to(output_dir).as_posix()):
        if path == manifest_path:
            continue
        if path.is_symlink():
            raise RunnerError(f"attempt archive contains a symlink: {path.relative_to(output_dir)}")
        if path.is_dir():
            continue
        if not path.is_file():
            raise RunnerError(f"attempt archive contains a non-regular file: {path.relative_to(output_dir)}")
        raw = path.read_bytes()
        files.append({"path": path.relative_to(output_dir).as_posix(), "sha256": digest(raw), "bytes": len(raw)})
    paths = [item["path"] for item in files]
    if len(paths) != len(set(paths)):
        raise RunnerError("attempt archive contains duplicate relative file paths")
    manifest = {"schema": "execute.real-bash-100-attempt-archive-manifest/1", "files": files}
    manifest_canonical = canonical(manifest)
    manifest_path.write_bytes(manifest_canonical + b"\n")
    observation_path = output_dir / "observation.json"
    if not observation_path.is_file():
        raise RunnerError("attempt archive is missing observation.json")
    return manifest, digest(manifest_canonical), digest(observation_path.read_bytes())


def _run_capture(command: list[str], cwd: Path, env: dict[str, str], stdin_data: bytes, limits: dict[str, int]) -> dict[str, Any]:
    """Capture exact bytes with limits; elapsed time is intentionally discarded."""
    try:
        if os.name != "posix":
            return {"status": "infrastructure_error", "returncode": None, "stdout": b"", "stderr": b"", "error": "runner requires a POSIX disposable worker"}
        import resource

        with tempfile.TemporaryFile() as input_file:
            input_file.write(stdin_data)
            input_file.seek(0)

            def apply_limits() -> None:
                os.umask(0o022)
                resource.setrlimit(resource.RLIMIT_CORE, (0, 0))
                resource.setrlimit(resource.RLIMIT_CPU, (limits["cpu_seconds"], limits["cpu_seconds"] + 1))
                resource.setrlimit(resource.RLIMIT_FSIZE, (limits["file_bytes"], limits["file_bytes"]))
                resource.setrlimit(resource.RLIMIT_NOFILE, (128, 128))
                if hasattr(resource, "RLIMIT_AS") and platform.system() == "Linux":
                    resource.setrlimit(resource.RLIMIT_AS, (limits["memory_bytes"], limits["memory_bytes"]))

            try:
                process = subprocess.Popen(
                    command, cwd=cwd, env=env, stdin=input_file,
                    stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                    start_new_session=True, preexec_fn=apply_limits,
                )
            except OSError as error:
                return {"status": "launch_error", "returncode": None, "stdout": b"", "stderr": b"", "error": str(error)}
            assert process.stdout is not None and process.stderr is not None
            selector = selectors.DefaultSelector()
            captured = {"stdout": bytearray(), "stderr": bytearray()}
            for stream, name in ((process.stdout, "stdout"), (process.stderr, "stderr")):
                os.set_blocking(stream.fileno(), False)
                selector.register(stream, selectors.EVENT_READ, name)
            total = 0
            timed_out = False
            output_limited = False
            import time
            deadline = time.monotonic() + limits["timeout_seconds"]
            while selector.get_map() or process.poll() is None:
                remaining = deadline - time.monotonic()
                if remaining <= 0 and not timed_out and (process.poll() is None or selector.get_map()):
                    timed_out = True
                    try:
                        os.killpg(process.pid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
                events = selector.select(max(0.005, min(0.05, remaining)) if remaining > 0 else 0.01)
                for key, _ in events:
                    try:
                        chunk = os.read(key.fileobj.fileno(), 65536)
                    except BlockingIOError:
                        continue
                    if not chunk:
                        selector.unregister(key.fileobj)
                        key.fileobj.close()
                        continue
                    total += len(chunk)
                    retained = len(captured["stdout"]) + len(captured["stderr"])
                    room = max(0, limits["output_bytes"] - retained)
                    captured[key.data].extend(chunk[:room])
                    if total > limits["output_bytes"] and not output_limited:
                        output_limited = True
                        try:
                            os.killpg(process.pid, signal.SIGKILL)
                        except ProcessLookupError:
                            pass
                if process.poll() is not None and not selector.get_map():
                    break
            try:
                returncode = process.wait(timeout=1)
            except subprocess.TimeoutExpired:
                try:
                    os.killpg(process.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                returncode = process.wait()
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            selector.close()
            if timed_out:
                status = "timeout"
            elif output_limited:
                status = "output_limit"
            elif returncode in (-signal.SIGXCPU, -signal.SIGXFSZ):
                status = "resource_limit"
            else:
                status = "completed"
            return {"status": status, "returncode": returncode, "stdout": bytes(captured["stdout"]), "stderr": bytes(captured["stderr"]), "error": None}
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        return {"status": "infrastructure_error", "returncode": None, "stdout": b"", "stderr": b"", "error": str(error)}


def _validated_runtime_command(runtime_argv: list[str], candidate_path: Path, case_argv: list[str], workspace: Path, wrapper: list[str] | None) -> list[str]:
    expanded = []
    for item in runtime_argv:
        if item == "{candidate}":
            expanded.append(str(candidate_path))
        elif item == "{workspace}":
            expanded.append(str(workspace))
        elif "{candidate}" in item or "{workspace}" in item:
            expanded.append(item.replace("{candidate}", str(candidate_path)).replace("{workspace}", str(workspace)))
        else:
            expanded.append(item)
    if not expanded:
        raise RunnerError("runtime argv must name an executable")
    command = expanded + case_argv
    return evaluate._wrapped_command(wrapper, workspace, command) if wrapper else command


def _validate_wrapper(value: Any) -> list[str]:
    try:
        return evaluate.validate_sandbox_wrapper_argv(value)
    except evaluate.EvaluationError as error:
        raise RunnerError(f"sandbox wrapper does not satisfy the shared external-driver contract: {error}") from error


def _runtime_receipt(runtime_argv: list[str], version_label: str, wrapper_argv: list[str] | None) -> dict[str, Any]:
    first = runtime_argv[0]
    resolved = shutil.which(first) if not os.path.isabs(first) else first
    runtime_path = Path(resolved).resolve(strict=True) if resolved else None
    if runtime_path is None or not runtime_path.is_file():
        raise RunnerError("runtime executable cannot be resolved to a regular file")
    machine = {"system": platform.system(), "release": platform.release(), "machine": platform.machine(), "python": platform.python_version()}
    source_hashes = {
        "real_bash_100_runner.py": digest_file(Path(__file__).resolve()),
        "evaluate.py": digest_file(Path(evaluate.__file__).resolve()),
    }
    wrapper_executable_sha256 = None
    if wrapper_argv is not None:
        wrapper_path = Path(wrapper_argv[0]).resolve(strict=True)
        if not wrapper_path.is_file() or not os.access(wrapper_path, os.X_OK):
            raise RunnerError("sandbox wrapper executable cannot be resolved to an executable file")
        wrapper_executable_sha256 = digest_file(wrapper_path)
    return {
        "version_label": version_label,
        "version_label_sha256": digest(version_label.encode("utf-8")),
        "runtime_argv_sha256": digest(canonical(runtime_argv)),
        "runtime_binary_sha256": digest_file(runtime_path),
        "sandbox_wrapper_executable_sha256": wrapper_executable_sha256,
        "machine": machine,
        "machine_sha256": digest(canonical(machine)),
        "python_executable_sha256": digest_file(Path(sys.executable).resolve()),
        "runner_source_files": source_hashes,
        "runner_source_sha256": digest(canonical(source_hashes)),
    }


def _archive_runner_sources(output_dir: Path, receipt: dict[str, Any]) -> None:
    source_paths = {
        "real_bash_100_runner.py": Path(__file__).resolve(),
        "evaluate.py": Path(evaluate.__file__).resolve(),
    }
    destination = output_dir / "runner-source"
    destination.mkdir()
    copied_hashes = {}
    for filename, source_path in source_paths.items():
        target = destination / filename
        shutil.copyfile(source_path, target)
        copied_hashes[filename] = digest_file(target)
    if copied_hashes != receipt["runner_source_files"]:
        raise RunnerError("archived runner source bytes differ from the runtime receipt")
    if digest(canonical(copied_hashes)) != receipt["runner_source_sha256"]:
        raise RunnerError("archived runner source aggregate differs from the runtime receipt")


def run_attempt(
    *, brief_package: dict[str, Any], brief_raw: bytes, oracle_package: dict[str, Any], oracle_raw: bytes | None = None,
    roster_package: dict[str, Any] | None = None, roster_raw: bytes | None = None,
    task_id: str, blind_arm_id: str, dispatch_index: int, stage: str, attempt_index: int,
    candidate: Path, runtime_argv: list[str], runtime_version: str, output_dir: Path,
    sandbox_wrapper: list[str] | None, fixture_only: bool = False,
    limits: dict[str, int] | None = None,
) -> dict[str, Any]:
    """Execute a single candidate attempt over every frozen functional case."""
    if not sandbox_wrapper and not fixture_only:
        raise RunnerError("production execution requires the external reviewed sandbox wrapper")
    if not fixture_only and platform.system() != "Linux":
        raise RunnerError("production execution requires a disposable Linux worker; local fixture-only tests remain available")
    if sandbox_wrapper is not None:
        sandbox_wrapper = _validate_wrapper(sandbox_wrapper)
    if stage not in {"generate", "repair"} or type(attempt_index) is not int or attempt_index < 1 or attempt_index > 3:
        raise RunnerError("attempt stage/index is invalid")
    if stage == "generate" and attempt_index != 1 or stage == "repair" and attempt_index < 2:
        raise RunnerError("generation must be attempt 1; repairs must be attempts 2 or 3")
    if not TASK_ID.fullmatch(task_id) or not re.fullmatch(r"^arm_[a-z0-9]{8,32}$", blind_arm_id):
        raise RunnerError("task or blind arm ID is malformed")
    if type(dispatch_index) is not int or not 1 <= dispatch_index <= 300:
        raise RunnerError("dispatch index must be from 1 through 300")
    if not isinstance(runtime_argv, list) or not runtime_argv or any(not isinstance(item, str) or not item or "\x00" in item for item in runtime_argv):
        raise RunnerError("runtime argv must be an array of nonempty strings without NUL")
    if not runtime_version or "\x00" in runtime_version:
        raise RunnerError("runtime version label must be nonempty text")
    brief_tasks, oracle_tasks = validate_packages(brief_package, brief_raw, oracle_package, allow_fixture=fixture_only)
    if fixture_only is not bool(brief_package["fixture_only"]):
        raise RunnerError("fixture_only execution mode must match both package declarations")
    if oracle_raw is None:
        if not fixture_only:
            raise RunnerError("production execution requires exact evaluator oracle package bytes")
        oracle_raw = canonical(oracle_package)
    try:
        parsed_oracle = json.loads(
            oracle_raw,
            object_pairs_hook=_reject_duplicates,
            parse_constant=lambda value: (_ for _ in ()).throw(RunnerError(f"invalid JSON constant {value}")),
        )
    except (UnicodeDecodeError, json.JSONDecodeError, RunnerError) as error:
        raise RunnerError(f"evaluator oracle package bytes are invalid: {error}") from error
    if parsed_oracle != oracle_package:
        raise RunnerError("evaluator oracle package document does not match its exact bound bytes")
    if not fixture_only:
        if roster_package is None or roster_raw is None:
            raise RunnerError("production execution requires exact frozen roster package bytes")
        roster_sha256 = validate_roster_binding(roster_package, roster_raw, brief_raw, brief_tasks, oracle_tasks)
    else:
        if (roster_package is None) != (roster_raw is None):
            raise RunnerError("roster document and exact roster bytes must be provided together")
        roster_sha256 = digest(roster_raw) if roster_raw is not None else None
    if task_id not in brief_tasks:
        raise RunnerError(f"unknown task ID {task_id!r}")
    task = oracle_tasks[task_id]
    source = candidate.resolve(strict=True)
    if not source.is_file() or candidate.is_symlink():
        raise RunnerError("candidate artifact must be a regular non-symlink file")
    if output_dir.exists() or output_dir.is_symlink():
        raise RunnerError("output directory already exists; attempts are immutable and never overwritten")
    selected_limits = dict(DEFAULT_LIMITS)
    if limits is not None:
        if set(limits) != set(DEFAULT_LIMITS) or any(type(value) is not int or value <= 0 for value in limits.values()):
            raise RunnerError("limits must provide positive integers for every frozen limit field")
        selected_limits = dict(limits)
    if sum(item.count("{candidate}") for item in runtime_argv) != 1:
        raise RunnerError("runtime argv must contain exactly one {candidate} placeholder")
    runtime_receipt = _runtime_receipt(runtime_argv, runtime_version, sandbox_wrapper)
    output_dir = output_dir.absolute()
    output_dir.mkdir(parents=True)
    _archive_runner_sources(output_dir, runtime_receipt)
    candidate_copy = output_dir / ("candidate" + (source.suffix or ".artifact"))
    shutil.copyfile(source, candidate_copy)
    candidate_copy.chmod(source.stat().st_mode & 0o7777)
    candidate_sha = digest_file(candidate_copy)
    runner_invocation = {
        "schema": OBSERVATION_SCHEMA,
        "trial_type": "fixture" if fixture_only else "candidate_observation_not_trial",
        "run_id": str(uuid.uuid4()),
        "session_id": output_dir.name,
        "task_id": task_id,
        "blind_arm_id": blind_arm_id,
        "dispatch_index": dispatch_index,
        "attempt": {"stage": stage, "attempt_index": attempt_index, "artifact_sha256": candidate_sha, "artifact_ref": candidate_copy.name},
        "task_package_sha256": digest(brief_raw),
        "oracle_package_sha256": digest(oracle_raw),
        "task_roster_sha256": roster_sha256,
        "oracle_sha256": task["oracle_sha256"],
        "criteria": task["criteria"],
        "runner_source_sha256": runtime_receipt["runner_source_sha256"],
        "runtime": runtime_receipt,
        "limits": selected_limits,
        "sandbox_wrapper_sha256": digest(canonical(sandbox_wrapper)) if sandbox_wrapper else None,
        "sandbox_policy_verified": False,
        "started_utc": datetime.now(timezone.utc).isoformat().replace("+00:00", "Z"),
        "cases": [],
    }
    criterion_case_results: dict[str, list[bool | None]] = {key: [] for key in task["criteria"]}
    with tempfile.TemporaryDirectory(prefix="rb100-", dir="/tmp") as temp_root:
        private_root = Path(temp_root)
        for case_index, case in enumerate(task["cases"]):
            case_dir = output_dir / "cases" / case["case_id"]
            case_dir.mkdir(parents=True)
            # Short deterministic temp paths keep AF_UNIX socket names under
            # platform limits without leaking long case IDs into the runtime.
            workspace = private_root / f"w{case_index}"
            workspace.mkdir(parents=True)
            template = case["_runner_template"]
            template_hash = case["_runner_template_sha256"]
            template_file = case_dir / "case-template.json"
            template_bytes = canonical(template)
            template_file.write_bytes(template_bytes + b"\n")
            materialized_template = _expand_case_template(template, workspace)
            _refresh_materialized_case_expectations(materialized_template, workspace)
            case = _validate_case(materialized_template, f"materialized case {case['case_id']}")
            _refresh_materialized_case_expectations(case, workspace)
            materialized_bytes = canonical(case)
            materialized_hash = digest(materialized_bytes)
            materialized_file = case_dir / "case-materialized.json"
            materialized_file.write_bytes(materialized_bytes + b"\n")
            _materialize_tree(workspace, case["filesystem_initial"])
            (workspace / "home").mkdir(exist_ok=True)
            (workspace / "tmp").mkdir(exist_ok=True)
            reserved = workspace / RESERVED_WORKSPACE_NAME
            reserved.mkdir()
            candidate_in_workspace = reserved / candidate_copy.name
            shutil.copyfile(candidate_copy, candidate_in_workspace)
            candidate_in_workspace.chmod(source.stat().st_mode & 0o7777)
            mock_bin = _install_mock_clients(reserved, case["mock_commands"])
            socket_path = reserved / "s"
            cwd = workspace if case["cwd"] == "." else workspace.joinpath(*PurePosixPath(case["cwd"]).parts)
            if not cwd.is_dir() or cwd.is_symlink():
                raise RunnerError(f"case {case['case_id']!r} working directory must exist as an owned fixture directory")
            before_nodes = _tree_snapshot(workspace)
            before_sha, before_file = _snapshot_write(case_dir, "filesystem-before.json", before_nodes)
            server = _MockServer(str(socket_path), workspace, case["mock_commands"])
            server_thread = threading.Thread(target=server.serve_forever, daemon=True)
            server_thread.start()
            env = {
                "PATH": os.pathsep.join((str(mock_bin), "/usr/bin", "/bin")),
                "HOME": str(workspace / "home"),
                "TMPDIR": str(workspace / "tmp"),
                "LC_ALL": "C", "LANG": "C", "TZ": "UTC",
                "EXECUTE_STUDY_MOCK_SOCKET": str(socket_path),
                "EXECUTE_STUDY_WORKSPACE": str(workspace),
                "PYTHONDONTWRITEBYTECODE": "1",
                "PYTHONPYCACHEPREFIX": str(reserved / "pycache"),
            }
            env.update(case["env"])
            command = _validated_runtime_command(runtime_argv, candidate_in_workspace, case["argv"], workspace, sandbox_wrapper)
            captured = _run_capture(command, cwd, env, _b64(case["stdin_base64"], "case stdin"), selected_limits)
            process_status = captured["status"]
            server.shutdown()
            server.server_close()
            server_thread.join(timeout=2)
            mock_events = sorted(server.events, key=lambda item: item["event_index"])
            snapshot_error = None
            snapshot_limit: CandidateObservationLimit | None = None
            filesystem_observation_status = "complete"
            try:
                after_nodes = _tree_snapshot(workspace)
            except CandidateObservationLimit as error:
                after_nodes = error.nodes
                snapshot_error = f"filesystem observation candidate limit: {error}"
                snapshot_limit = error
                filesystem_observation_status = "candidate_resource_limit"
                captured["status"] = "resource_limit"
                captured["error"] = snapshot_error
            except (OSError, RunnerError) as error:
                after_nodes = []
                snapshot_error = f"filesystem observation failed: {error}"
                filesystem_observation_status = "infrastructure_error"
                captured["status"] = "infrastructure_error"
                captured["error"] = snapshot_error
            after_sha, after_file = _snapshot_write(case_dir, "filesystem-after.json", after_nodes)
            stdout_file = case_dir / "stdout.bin"
            stderr_file = case_dir / "stderr.bin"
            stdout_file.write_bytes(captured["stdout"])
            stderr_file.write_bytes(captured["stderr"])
            mock_file = case_dir / "mock-events.jsonl"
            mock_file.write_bytes(b"".join(canonical(event) + b"\n" for event in mock_events))
            expected = case["expected"]
            # A later filesystem-observer infrastructure failure must not erase
            # process evidence that was already captured from the candidate.
            observation_available = process_status not in {"launch_error", "infrastructure_error"}
            process_match = None if not observation_available else (
                captured["returncode"] == expected["returncode"]
                and captured["stdout"] == _b64(expected["stdout_base64"], "expected stdout")
                and captured["stderr"] == _b64(expected["stderr_base64"], "expected stderr")
            )
            if not observation_available or filesystem_observation_status == "infrastructure_error":
                filesystem_match = None
            elif snapshot_limit is not None:
                filesystem_match = False
            else:
                filesystem_match = after_nodes == expected["filesystem_after"]
            mocks_match = None if not observation_available else mock_events == expected["mock_events"]
            case_observation = {
                "case_id": case["case_id"], "status": captured["status"], "returncode": captured["returncode"],
                "process_status": process_status,
                "runner_error": captured["error"],
                "path_bindings": {"workspace": str(workspace), "home": str(workspace / "home"), "tmp": str(workspace / "tmp")},
                "filesystem_observation_status": filesystem_observation_status,
                "filesystem_observation_limit": None if snapshot_limit is None else {
                    "code": snapshot_limit.code,
                    "limit": MAX_TREE_NODES if snapshot_limit.code == "node_limit" else MAX_FIXTURE_BYTES,
                    "observed_at_least": snapshot_limit.observed,
                    "retained_nodes": len(snapshot_limit.nodes),
                    "retained_file_bytes": snapshot_limit.file_bytes,
                },
                "case_template": {"artifact_ref": str(template_file.relative_to(output_dir)), "sha256": template_hash},
                "case_materialized": {"artifact_ref": str(materialized_file.relative_to(output_dir)), "sha256": materialized_hash},
                "stdout": {"artifact_ref": str(stdout_file.relative_to(output_dir)), "sha256": digest(captured["stdout"]), "bytes": len(captured["stdout"])},
                "stderr": {"artifact_ref": str(stderr_file.relative_to(output_dir)), "sha256": digest(captured["stderr"]), "bytes": len(captured["stderr"])},
                "filesystem_before": {"artifact_ref": str(before_file.relative_to(output_dir)), "sha256": before_sha},
                "filesystem_after": {"artifact_ref": str(after_file.relative_to(output_dir)), "sha256": after_sha},
                "mock_events": {"artifact_ref": str(mock_file.relative_to(output_dir)), "sha256": digest(mock_file.read_bytes()), "count": len(mock_events)},
                "criteria_results": {"process_io_exact": process_match, "filesystem_final_exact": filesystem_match, "mock_command_effects_exact": mocks_match},
            }
            runner_invocation["cases"].append(case_observation)
            for criterion_id, operation in task["criteria"].items():
                if operation == "process_io_exact":
                    value = process_match
                elif operation == "filesystem_final_exact":
                    value = filesystem_match
                elif operation == "mock_command_effects_exact":
                    value = mocks_match
                else:
                    if not observation_available:
                        value = None
                    elif process_status != "completed" or filesystem_observation_status == "candidate_resource_limit":
                        value = False
                    elif filesystem_observation_status == "infrastructure_error":
                        value = None
                    else:
                        value = True
                criterion_case_results[criterion_id].append(value)
    runner_invocation["criteria_results"] = {
        key: (False if False in values else None if any(value is None for value in values) else True)
        for key, values in criterion_case_results.items()
    }
    runner_invocation["ended_utc"] = datetime.now(timezone.utc).isoformat().replace("+00:00", "Z")
    result_file = output_dir / "observation.json"
    result_file.write_bytes(canonical(runner_invocation) + b"\n")
    _manifest, manifest_sha256, observation_sha256 = _write_archive_manifest(output_dir)
    runner_invocation["archive_manifest_ref"] = "archive-manifest.json"
    runner_invocation["archive_manifest_sha256"] = manifest_sha256
    runner_invocation["observation_sha256"] = observation_sha256
    return runner_invocation


def _cli() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    validate = commands.add_parser("validate-packages", help="validate brief/oracle split and their hashes without executing a candidate")
    validate.add_argument("--brief-package", type=Path, required=True)
    validate.add_argument("--oracle-package", type=Path, required=True)
    validate.add_argument("--roster-package", type=Path, help="frozen hash-only roster; required for production package validation")
    validate.add_argument("--allow-fixture", action="store_true")
    run = commands.add_parser("run-attempt", help="execute one candidate attempt and retain raw case observations")
    run.add_argument("--brief-package", type=Path, required=True)
    run.add_argument("--oracle-package", type=Path, required=True)
    run.add_argument("--roster-package", type=Path, help="frozen hash-only roster; required for production execution")
    run.add_argument("--task-id", required=True)
    run.add_argument("--blind-arm-id", required=True)
    run.add_argument("--dispatch-index", type=int, required=True)
    run.add_argument("--stage", choices=("generate", "repair"), required=True)
    run.add_argument("--attempt-index", type=int, required=True)
    run.add_argument("--candidate", type=Path, required=True)
    run.add_argument("--runtime-argv-json", required=True, help="direct argv JSON array; include {candidate} exactly as required by the runtime")
    run.add_argument("--runtime-version", required=True, help="frozen human-readable runtime version label")
    run.add_argument("--sandbox-wrapper-json", required=True, help="reviewed external wrapper argv using {workspace} and standalone {command}; no shell string")
    run.add_argument("--out", type=Path, required=True)
    run.add_argument("--fixture-only", action="store_true", help="accept only packages marked fixture_only; never produces evaluation evidence")
    return parser


def main() -> int:
    args = _cli().parse_args()
    try:
        brief, brief_raw = read_json(args.brief_package, "shared task package")
        oracle, oracle_raw = read_json(args.oracle_package, "evaluator-only oracle package")
        roster = None
        roster_raw = None
        if args.roster_package is not None:
            roster, roster_raw = read_json(args.roster_package, "frozen hash-only roster")
        if args.command == "validate-packages":
            tasks, oracle_tasks = validate_packages(brief, brief_raw, oracle, allow_fixture=args.allow_fixture)
            roster_sha256 = None
            if not brief["fixture_only"]:
                if roster is None or roster_raw is None:
                    raise RunnerError("production package validation requires --roster-package")
                roster_sha256 = validate_roster_binding(roster, roster_raw, brief_raw, tasks, oracle_tasks)
            print(json.dumps({"status": "package_contract_validated_only", "trial_type": "fixture" if brief["fixture_only"] else "not_a_trial", "tasks": len(tasks), "oracle_tasks": len(oracle_tasks), "task_package_sha256": digest(brief_raw), "oracle_package_sha256": digest(oracle_raw), "task_roster_sha256": roster_sha256}))
            return 0
        try:
            runtime_argv = json.loads(args.runtime_argv_json)
        except json.JSONDecodeError as error:
            raise RunnerError("runtime argv must be a JSON array") from error
        if not isinstance(runtime_argv, list) or not runtime_argv or not all(isinstance(item, str) and item for item in runtime_argv):
            raise RunnerError("runtime argv must be a nonempty JSON array of nonempty strings")
        wrapper = _validate_wrapper(json.loads(args.sandbox_wrapper_json))
        result = run_attempt(
            brief_package=brief, brief_raw=brief_raw, oracle_package=oracle, oracle_raw=oracle_raw,
            roster_package=roster, roster_raw=roster_raw,
            task_id=args.task_id, blind_arm_id=args.blind_arm_id, dispatch_index=args.dispatch_index,
            stage=args.stage, attempt_index=args.attempt_index, candidate=args.candidate,
            runtime_argv=runtime_argv, runtime_version=args.runtime_version, output_dir=args.out,
            sandbox_wrapper=wrapper, fixture_only=args.fixture_only,
        )
        print(json.dumps({"status": "observation_retained", "trial_type": result["trial_type"], "output": str(args.out), "archive_manifest_ref": result["archive_manifest_ref"], "archive_manifest_sha256": result["archive_manifest_sha256"], "observation_sha256": result["observation_sha256"], "criteria_results": result["criteria_results"]}, sort_keys=True))
        return 0
    except (RunnerError, OSError, json.JSONDecodeError) as error:
        print(f"runner error: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
