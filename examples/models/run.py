#!/usr/bin/env python3
"""Run model workflow contracts using only their checked-in local fixtures.

This runner deliberately makes no model-quality or provider claim.  It executes
the programs against their fixture bundle and checks exact, catalogued results.
"""
from __future__ import annotations

import argparse
import functools
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import selectors
import shutil
import signal
import subprocess
import sys
import tempfile
import time
from typing import Any


HERE = Path(__file__).resolve().parent
PROJECT = HERE.parents[1]
DEFAULT_CATALOG = HERE / "catalog.json"
TOTAL_WORKFLOWS = 100
MAX_SOURCE = 256 * 1024
MAX_FIXTURE_BYTES = 16 * 1024 * 1024
MAX_FILE_BYTES = 8 * 1024 * 1024
MAX_CAPTURE_BYTES = 2 * 1024 * 1024
DEFAULT_TIMEOUT = 30.0
MACOS_SANDBOX = Path("/usr/bin/sandbox-exec")
MACOS_OFFLINE_PROFILE = "(version 1) (allow default) (deny network*)"
MODEL_INVOCATION = re.compile(r"\b(?:jev\.(?:classify|judge|score)|llm\.(?:ask|extract))\b")


class ContractError(ValueError):
    """A catalog or run result violates the offline workflow contract."""


def _relative_path(raw: Any, *, label: str) -> PurePosixPath:
    if not isinstance(raw, str) or not raw or "\x00" in raw or "\\" in raw:
        raise ContractError(f"{label} must be a non-empty POSIX relative path")
    path = PurePosixPath(raw)
    if path.is_absolute() or any(part in {"", ".", ".."} for part in path.parts):
        raise ContractError(f"{label} escapes its owned directory: {raw!r}")
    return path


def _read_utf8_string(value: Any, *, label: str, nonempty: bool = True) -> bytes:
    if not isinstance(value, str):
        raise ContractError(f"{label} must be a UTF-8 string")
    try:
        encoded = value.encode("utf-8")
    except UnicodeEncodeError as error:
        raise ContractError(f"{label} is not valid UTF-8 text") from error
    if nonempty and not encoded:
        raise ContractError(f"{label} must not be empty")
    if len(encoded) > MAX_FILE_BYTES:
        raise ContractError(f"{label} exceeds the {MAX_FILE_BYTES}-byte limit")
    return encoded


def _catalog_items(document: Any) -> list[dict[str, Any]]:
    if isinstance(document, list):
        items = document
    elif isinstance(document, dict):
        if document.get("version") != 1:
            raise ContractError("catalog object must declare version: 1")
        items = document.get("workflows")
    else:
        raise ContractError("catalog must be an array or an object containing workflows")
    if not isinstance(items, list) or not all(isinstance(item, dict) for item in items):
        raise ContractError("catalog entries must be objects")
    return items


def _fixture_number(workflow_id: Any) -> str:
    if isinstance(workflow_id, bool) or not isinstance(workflow_id, int):
        raise ContractError(f"workflow ID must be an integer from 1 to 100: {workflow_id!r}")
    if not 1 <= workflow_id <= TOTAL_WORKFLOWS:
        raise ContractError(f"workflow ID {workflow_id!r} is outside 1..100")
    return f"{workflow_id:03d}"


def _program_path(program_dir: Path, filename: Any) -> Path:
    rel = _relative_path(filename, label="program file")
    if rel.suffix != ".program":
        raise ContractError(f"program file must have a .program suffix: {filename!r}")
    root = program_dir.resolve(strict=True)
    candidate = root / Path(*rel.parts)
    for parent in (candidate, *candidate.parents):
        if parent == root.parent:
            break
        if parent.is_symlink():
            raise ContractError(f"program path may not traverse a symlink: {filename!r}")
    path = candidate.resolve(strict=True)
    if not path.is_relative_to(root) or not path.is_file():
        raise ContractError(f"program file must resolve to a file inside {root}: {filename!r}")
    ordinal = path.name[:3]
    if not ordinal.isdigit() or path.name[3:4] != "-":
        raise ContractError(f"program filename must begin with NNN-: {filename!r}")
    raw = path.read_bytes()
    if not raw or len(raw) > MAX_SOURCE:
        raise ContractError(f"program source must be non-empty and at most {MAX_SOURCE} bytes: {filename!r}")
    try:
        raw.decode("utf-8")
    except UnicodeDecodeError as error:
        raise ContractError(f"program source is not UTF-8: {filename!r}") from error
    return path


def _tree_sha256(root: Path) -> str:
    digest = hashlib.sha256()
    for path in sorted(root.rglob("*"), key=lambda item: item.relative_to(root).as_posix()):
        if path.is_file():
            relative = path.relative_to(root).as_posix().encode("utf-8")
            digest.update(len(relative).to_bytes(8, "big"))
            digest.update(relative)
            with path.open("rb") as stream:
                for chunk in iter(lambda: stream.read(1024 * 1024), b""):
                    digest.update(chunk)
    return digest.hexdigest()


def _fixture_tree(fixtures_dir: Path, ordinal: str) -> tuple[Path, int, str]:
    source = fixtures_dir / ordinal
    if not source.exists() or not source.is_dir() or source.is_symlink():
        raise ContractError(f"missing fixture directory: {source}")
    total = 0
    files = 0
    paths = sorted(source.rglob("*"), key=lambda item: item.relative_to(source).as_posix())
    for path in paths:
        if path.is_symlink():
            raise ContractError(f"fixture tree may not contain symlinks: {path}")
        if path.is_file():
            if path.name == ".env":
                raise ContractError(f"fixture bundles may not contain environment files: {path}")
            size = path.stat().st_size
            if size == 0:
                raise ContractError(f"empty fixture input: {path.relative_to(fixtures_dir).as_posix()}")
            if size > MAX_FILE_BYTES:
                raise ContractError(f"fixture exceeds {MAX_FILE_BYTES} bytes: {path.relative_to(fixtures_dir).as_posix()}")
            total += size
            files += 1
        elif not path.is_dir():
            raise ContractError(f"fixture tree contains a non-regular entry: {path}")
    if files == 0:
        raise ContractError(f"fixture directory has no non-empty files: {source}")
    if total > MAX_FIXTURE_BYTES:
        raise ContractError(f"fixture tree exceeds {MAX_FIXTURE_BYTES} bytes: {source}")
    return source, total, _tree_sha256(source)


def _fixture_references(source: str, inputs: dict[str, str], ordinal: str) -> tuple[set[str], set[str]]:
    """Find explicit fixture paths in defaults, source literals and supplied values."""
    prefix = f"fixtures/{ordinal}/"
    references: set[str] = set()
    input_references: set[str] = set()
    texts = [(source, False), *((value, True) for value in inputs.values())]
    for text, from_input in texts:
        for match in re.findall(r"fixtures/[0-9]{3}/[^\s\"'`]+", text):
            candidate = match.rstrip(".,:;)]}")
            if candidate.startswith(prefix):
                references.add(candidate)
                if from_input:
                    input_references.add(candidate)
            elif candidate.startswith("fixtures/"):
                raise ContractError(f"workflow {ordinal} references another workflow's fixture: {candidate}")
    if not input_references:
        raise ContractError(f"workflow {ordinal} must supply a CLI input that names a local fixtures/{ordinal}/ file")
    return references, input_references


def _validate_fixture_backed_model_calls(
    source: str,
    ordinal: str,
    workflow_id: int,
    *,
    require_live_control: bool = False,
) -> None:
    prefix = f"fixtures/{ordinal}/"
    declared: dict[str, str] = {}
    declared_types: dict[str, str] = {}
    for line in source.splitlines():
        accepted = re.match(
            r'^\s*accept\s+([A-Za-z_][A-Za-z0-9_]*)\s+as\s+(\w+)\s+with\s+default\s+(.+?)\s*$',
            line,
        )
        if accepted:
            raw_default = accepted.group(3)
            string_default = re.fullmatch(r'"([^"\x00]*)"', raw_default)
            name = accepted.group(1)
            default = string_default.group(1) if string_default else raw_default
            declared[name] = default
            declared_types[name] = accepted.group(2).lower()
            if name.lower() in {"env_file", "env_file_path"} and default != ".env":
                raise ContractError(f"workflow {workflow_id}: env file input must default to the private .env")
    has_valid_live_control = declared.get("live") == "false" and declared_types.get("live") in {"boolean", "bool"}
    if require_live_control and not has_valid_live_control:
        raise ContractError(f"workflow {workflow_id}: accept live as boolean with default false is required")
    if "live" in declared and not has_valid_live_control:
        raise ContractError(f"workflow {workflow_id}: live must be boolean with default false")

    conditions: list[dict[str, Any]] = []
    call_count = 0
    fixture_call_count = 0
    live_call_count = 0
    for line_number, line in enumerate(source.splitlines(), 1):
        empty_branch = re.search(r'\b([A-Za-z_][A-Za-z0-9_]*fixture[A-Za-z0-9_]*)\s+is\s+""', line)
        if empty_branch:
            raise ContractError(
                f"workflow {workflow_id}: empty-fixture live fallback at source line {line_number}"
            )
        stripped = line.strip()
        indent = len(line) - len(line.lstrip(" "))
        if stripped == "otherwise":
            while conditions and conditions[-1]["indent"] > indent:
                conditions.pop()
            if conditions and conditions[-1]["indent"] == indent:
                conditions[-1]["then_active"] = False
            continue
        conditional = re.match(r"if\s+(.+?)\s*$", stripped)
        if conditional:
            while conditions and conditions[-1]["indent"] >= indent:
                conditions.pop()
            conditions.append({
                "indent": indent,
                "then_active": True,
                "is_live": conditional.group(1) == "live is true",
            })
            continue
        while conditions and indent <= conditions[-1]["indent"]:
            conditions.pop()
        if not MODEL_INVOCATION.search(line):
            continue
        call_count += 1
        fixture_arg = re.search(r"\band\s+fixture\s+([A-Za-z_][A-Za-z0-9_]*)\b", line)
        if not fixture_arg:
            live_guarded = any(frame["is_live"] and frame["then_active"] for frame in conditions)
            if not live_guarded or not has_valid_live_control:
                raise ContractError(
                    f"workflow {workflow_id}: model call at source line {line_number} must pass a fixture unless guarded by `if live is true`"
                )
            live_call_count += 1
            continue
        fixture_name = fixture_arg.group(1)
        fixture_call_count += 1
        default = declared.get(fixture_name, "")
        if not default.startswith(prefix):
            raise ContractError(
                f"workflow {workflow_id}: model fixture {fixture_name!r} must default to a local {prefix} file"
            )
        env_arg = re.search(r"\band\s+env_file\s+([A-Za-z_][A-Za-z0-9_]*)\b", line)
        if env_arg and declared.get(env_arg.group(1)) != ".env":
            raise ContractError(f"workflow {workflow_id}: model env_file must use the runner's private empty .env")
    if call_count == 0:
        raise ContractError(f"workflow {workflow_id}: catalog entry does not contain a model invocation")
    if fixture_call_count == 0:
        raise ContractError(f"workflow {workflow_id}: default execution must contain a fixture-backed model invocation")
    if require_live_control and live_call_count == 0:
        raise ContractError(f"workflow {workflow_id}: explicit live mode must contain a guarded model invocation")


def validate_catalog(
    document: Any,
    *,
    program_dir: Path,
    fixtures_dir: Path,
    strict_total: bool = False,
) -> list[dict[str, Any]]:
    workflows = _catalog_items(document)
    if not workflows:
        raise ContractError("catalog has no workflows")
    if strict_total and len(workflows) != TOTAL_WORKFLOWS:
        raise ContractError(f"default catalog must contain exactly {TOTAL_WORKFLOWS} workflows; found {len(workflows)}")

    seen: set[int] = set()
    normalized: list[dict[str, Any]] = []
    for workflow in workflows:
        workflow_id = workflow.get("id")
        ordinal = _fixture_number(workflow_id)
        workflow_id = int(ordinal)
        if workflow_id in seen:
            raise ContractError(f"duplicate workflow id: {workflow_id}")
        seen.add(workflow_id)
        for key in ("title", "description"):
            if not isinstance(workflow.get(key), str) or not workflow[key].strip():
                raise ContractError(f"workflow {workflow_id}: {key} must be a non-empty string")
        program = _program_path(program_dir, workflow.get("file"))
        if program.name[:3] != ordinal:
            raise ContractError(f"workflow {workflow_id}: source filename must begin {ordinal}-")
        fixtures, fixture_bytes, fixture_sha256 = _fixture_tree(fixtures_dir, ordinal)
        try:
            source = program.read_text(encoding="utf-8")
        except UnicodeDecodeError as error:
            raise ContractError(f"workflow {workflow_id}: program source is not UTF-8") from error
        _validate_fixture_backed_model_calls(
            source,
            ordinal,
            workflow_id,
            require_live_control=strict_total,
        )
        cases = workflow.get("cases")
        if not isinstance(cases, list) or len(cases) < 2:
            raise ContractError(f"workflow {workflow_id}: at least two fixture cases are required")
        case_names: set[str] = set()
        normalized_cases: list[dict[str, Any]] = []
        for index, case in enumerate(cases, 1):
            if not isinstance(case, dict):
                raise ContractError(f"workflow {workflow_id} case {index} must be an object")
            name = case.get("name")
            if not isinstance(name, str) or not name.strip() or name in case_names:
                raise ContractError(f"workflow {workflow_id}: cases need distinct non-empty names")
            case_names.add(name)
            inputs = case.get("inputs")
            if not isinstance(inputs, dict) or not inputs:
                raise ContractError(f"workflow {workflow_id}/{name}: inputs must be a non-empty object")
            clean_inputs: dict[str, str] = {}
            for key, value in inputs.items():
                if not isinstance(key, str) or not re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*", key) or key in {"true", "false"}:
                    raise ContractError(f"workflow {workflow_id}/{name}: invalid input name {key!r}")
                if not isinstance(value, str) or not value.strip() or "\x00" in value:
                    raise ContractError(f"workflow {workflow_id}/{name}: input {key!r} must be non-empty text")
                if key.lower() == "live" and value.lower() not in {"false", "0"}:
                    raise ContractError(f"workflow {workflow_id}/{name}: fixture cases must set live to false")
                clean_inputs[key] = value
            expected_stdout = _read_utf8_string(case.get("expected_stdout"), label=f"{workflow_id}/{name} expected_stdout")
            outputs = case.get("outputs")
            if not isinstance(outputs, dict) or not outputs:
                raise ContractError(f"workflow {workflow_id}/{name}: outputs must be a non-empty object")
            clean_outputs: dict[str, bytes] = {}
            for raw_path, content in outputs.items():
                rel = _relative_path(raw_path, label="output path")
                if not rel.parts or rel.parts[0] != "out" or len(rel.parts) < 2:
                    raise ContractError(f"workflow {workflow_id}/{name}: outputs must be files under out/")
                clean_outputs[rel.as_posix()] = _read_utf8_string(content, label=f"{workflow_id}/{name} output {raw_path}")
            references, input_references = _fixture_references(source, clean_inputs, ordinal)
            for key, value in clean_inputs.items():
                key_lower = key.lower()
                if any(marker in key_lower for marker in ("path", "file", "directory", "dir")):
                    if key_lower in {"env_file", "env_file_path"}:
                        if value != ".env":
                            raise ContractError(f"workflow {workflow_id}/{name}: env_file must be the runner's private empty .env")
                    elif key_lower in {"report_path", "output_path", "destination_path"}:
                        out_path = _relative_path(value, label=f"{workflow_id}/{name} {key}")
                        if out_path.parts[0] != "out" or out_path.as_posix() not in clean_outputs:
                            raise ContractError(f"workflow {workflow_id}/{name}: output input {key!r} must name a declared out/ oracle")
                    elif not any(value == reference or value.startswith(reference + "/") for reference in input_references):
                        raise ContractError(f"workflow {workflow_id}/{name}: path input {key!r} must name a non-empty local fixture")
            for reference in references:
                rel = _relative_path(reference, label="fixture input path")
                if len(rel.parts) < 3 or rel.parts[0] != "fixtures" or rel.parts[1] != ordinal:
                    raise ContractError(f"workflow {workflow_id}/{name}: invalid fixture path {reference}")
                referenced = fixtures_dir.joinpath(*rel.parts[1:])
                if not referenced.is_file() or referenced.stat().st_size == 0:
                    raise ContractError(f"workflow {workflow_id}/{name}: missing or empty fixture input {reference}")
            normalized_cases.append({
                "name": name,
                "inputs": clean_inputs,
                "expected_stdout": expected_stdout,
                "outputs": clean_outputs,
            })
        normalized.append({
            "id": workflow_id,
            "ordinal": ordinal,
            "title": workflow["title"],
            "description": workflow["description"],
            "file": program,
            "source_sha256": hashlib.sha256(program.read_bytes()).hexdigest(),
            "fixture_bytes": fixture_bytes,
            "fixture_sha256": fixture_sha256,
            "cases": normalized_cases,
        })
    if strict_total:
        expected = {f"{number:03d}" for number in range(1, TOTAL_WORKFLOWS + 1)}
        ordinals = [item["ordinal"] for item in normalized]
        ordered = [f"{number:03d}" for number in range(1, TOTAL_WORKFLOWS + 1)]
        if set(ordinals) != expected or len(set(ordinals)) != TOTAL_WORKFLOWS or ordinals != ordered:
            raise ContractError("default catalog must cover workflow ordinals 001..100 exactly once in order")
    return normalized


def safe_environment(cwd: Path, binary: Path) -> dict[str, str]:
    # Do not inherit proxy, cloud credentials, user startup hooks or shell exports.
    path_parts = [str(binary.parent)]
    for item in ("/usr/local/bin", "/opt/homebrew/bin", "/usr/bin", "/bin", "/usr/sbin", "/sbin"):
        if item not in path_parts:
            path_parts.append(item)
    return {
        "PATH": os.pathsep.join(path_parts),
        "LC_ALL": "C",
        "LANG": "C",
        "HOME": str(cwd),
        "TMPDIR": str(cwd),
        "TZ": "UTC",
        "EXECUTE_MODELS_OFFLINE": "1",
    }


def terminate_owned_group(process: subprocess.Popen[bytes]) -> None:
    # The leader may have exited while a descendant still holds its pipes.
    # Signal the owned group regardless of the leader's status.
    try:
        os.killpg(process.pid, signal.SIGTERM)
    except ProcessLookupError:
        pass
    try:
        process.wait(timeout=1.0)
    except subprocess.TimeoutExpired:
        pass
    # Runtime applets usually live in their own groups and are reaped by the
    # runner's TERM handler.  This final escalation catches leaked descendants
    # that remained in the launcher's owned group after its leader exited.
    try:
        os.killpg(process.pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    try:
        process.wait(timeout=2.0)
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait(timeout=1.0)


@functools.lru_cache(maxsize=1)
def discover_network_sandbox() -> tuple[str, tuple[str, ...]] | None:
    """Find an unprivileged network sandbox without making it a Linux prerequisite."""
    if sys.platform == "darwin":
        if MACOS_SANDBOX.is_file():
            return "macos-sandbox-exec", (str(MACOS_SANDBOX), "-p", MACOS_OFFLINE_PROFILE)
    if sys.platform.startswith("linux"):
        unshare = shutil.which("unshare", path="/usr/bin:/bin")
        if unshare is not None:
            try:
                probe = subprocess.run(
                    [unshare, "--net", "--", "/bin/true"],
                    stdin=subprocess.DEVNULL,
                    stdout=subprocess.DEVNULL,
                    stderr=subprocess.DEVNULL,
                    env={"PATH": "/usr/bin:/bin", "LC_ALL": "C"},
                    timeout=2,
                    check=False,
                )
            except (OSError, subprocess.SubprocessError):
                probe = None
            if probe is not None and probe.returncode == 0:
                return "linux-unshare-net", (unshare, "--net", "--")
    return None


def isolated_command(binary: Path, arguments: list[str], *, required: bool = False) -> list[str]:
    sandbox = discover_network_sandbox()
    if sandbox is None:
        if required:
            raise ContractError("requested network isolation is unavailable on this host")
        return [str(binary), *arguments]
    return [*sandbox[1], str(binary), *arguments]


def invoke(
    binary: Path,
    cwd: Path,
    arguments: list[str],
    timeout: float = DEFAULT_TIMEOUT,
    *,
    require_network_isolation: bool = False,
) -> dict[str, Any]:
    process = subprocess.Popen(
        isolated_command(binary, arguments, required=require_network_isolation),
        cwd=cwd,
        env=safe_environment(cwd, binary),
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        start_new_session=True,
        bufsize=0,
    )
    if process.stdout is None or process.stderr is None:
        terminate_owned_group(process)
        raise ContractError("runner pipes were not created")
    selector = selectors.DefaultSelector()
    stdout_fd = process.stdout.fileno()
    stderr_fd = process.stderr.fileno()
    captured: dict[int, bytearray] = {
        stdout_fd: bytearray(),
        stderr_fd: bytearray(),
    }
    streams = {stream.fileno(): stream for stream in (process.stdout, process.stderr)}
    for stream in streams.values():
        os.set_blocking(stream.fileno(), False)
        selector.register(stream, selectors.EVENT_READ)
    deadline = time.monotonic() + timeout
    started = time.perf_counter_ns()
    try:
        while selector.get_map():
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError(f"workflow exceeded {timeout:g}s timeout")
            for key, _ in selector.select(min(remaining, 0.1)):
                try:
                    chunk = os.read(key.fd, 65_536)
                except BlockingIOError:
                    continue
                if not chunk:
                    selector.unregister(key.fileobj)
                    key.fileobj.close()
                    continue
                captured[key.fd].extend(chunk)
                size = sum(len(value) for value in captured.values())
                if size > MAX_CAPTURE_BYTES:
                    raise ContractError(f"combined stdout/stderr exceeded {MAX_CAPTURE_BYTES} bytes")
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise TimeoutError(f"workflow exceeded {timeout:g}s timeout")
        process.wait(timeout=remaining)
        return {
            "code": process.returncode,
            "stdout": bytes(captured[stdout_fd]),
            "stderr": bytes(captured[stderr_fd]),
            "elapsed_ms": (time.perf_counter_ns() - started) / 1_000_000,
        }
    except BaseException:
        terminate_owned_group(process)
        raise
    finally:
        selector.close()
        for stream in (process.stdout, process.stderr):
            try:
                stream.close()
            except OSError:
                pass


def run_case(
    binary: Path,
    manifest_dir: Path,
    workflow: dict[str, Any],
    case: dict[str, Any],
    fixtures_dir: Path,
    timeout: float = DEFAULT_TIMEOUT,
    *,
    require_network_isolation: bool = False,
) -> dict[str, Any]:
    with tempfile.TemporaryDirectory(prefix=f"execute-model-{workflow['ordinal']}-") as temporary:
        if _sha256(workflow["file"]) != workflow["source_sha256"]:
            raise ContractError("workflow source changed after catalog validation")
        cwd = Path(temporary)
        fixture_src = fixtures_dir / workflow["ordinal"]
        fixture_dest = cwd / "fixtures" / workflow["ordinal"]
        fixture_dest.parent.mkdir(parents=True, exist_ok=True)
        shutil.copytree(fixture_src, fixture_dest, symlinks=False)
        fixture_copy_hash = _tree_sha256(fixture_dest)
        # The adapters accept an explicit env file.  Supplying a new empty file
        # in the owned cwd prevents them from resolving a developer's .env.
        (cwd / ".env").write_bytes(b"")
        for relative in case["outputs"]:
            output_parent = cwd.joinpath(*PurePosixPath(relative).parts).parent
            output_parent.mkdir(parents=True, exist_ok=True)
        args: list[str] = [str(workflow["file"]), "--manifest-dir", str(manifest_dir.resolve())]
        for name, value in case["inputs"].items():
            args.extend(("--input", f"{name}={value}"))
        source_hash_before = _sha256(workflow["file"])
        result = invoke(binary.resolve(), cwd, args, timeout, require_network_isolation=require_network_isolation)
        if _tree_sha256(fixture_dest) != fixture_copy_hash:
            raise ContractError("workflow modified its owned fixture copy")
        if _sha256(workflow["file"]) != source_hash_before or source_hash_before != workflow["source_sha256"]:
            raise ContractError("workflow modified its read-only source program")
        if (cwd / ".env").read_bytes() != b"":
            raise ContractError("workflow modified the private empty .env file")
        if result["code"] != 0:
            raise ContractError(f"exit code {result['code']}: {result['stderr'][:4096].decode(errors='replace')}")
        if result["stderr"]:
            raise ContractError(f"unexpected stderr: {result['stderr'][:4096].decode(errors='replace')}")
        if result["stdout"] != case["expected_stdout"]:
            raise ContractError(
                "stdout mismatch: "
                f"expected {case['expected_stdout'][:512]!r}, got {result['stdout'][:512]!r}"
            )
        output_evidence: dict[str, dict[str, Any]] = {}
        allowed = set(case["outputs"]) | {".env"}
        for relative, expected in case["outputs"].items():
            path = cwd.joinpath(*PurePosixPath(relative).parts)
            if not path.is_file() or path.is_symlink():
                raise ContractError(f"expected output file was not created: {relative}")
            for parent in path.parents:
                if parent == cwd.parent:
                    break
                if parent.is_symlink() or not parent.resolve().is_relative_to(cwd.resolve()):
                    raise ContractError(f"output resolves outside the owned directory: {relative}")
            actual = path.read_bytes()
            if actual != expected:
                raise ContractError(f"output file mismatch: {relative}")
            output_evidence[relative] = {"bytes": len(actual), "sha256": hashlib.sha256(actual).hexdigest()}
        for path in cwd.rglob("*"):
            if path.is_symlink():
                raise ContractError(f"workflow created a symlink in its owned directory: {path.relative_to(cwd)}")
            if path.is_file():
                relative = path.relative_to(cwd).as_posix()
                if relative.startswith("fixtures/"):
                    continue
                if relative not in allowed:
                    raise ContractError(f"workflow created undeclared output: {relative}")
        return {
            "name": case["name"],
            "passed": True,
            "elapsed_ms": round(result["elapsed_ms"], 3),
            "stdout_bytes": len(result["stdout"]),
            "stdout_sha256": hashlib.sha256(result["stdout"]).hexdigest(),
            "outputs": output_evidence,
        }


def _sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def main(argv: list[str] | None = None) -> int:
    def interrupt(signum: int, _frame: Any) -> None:
        raise KeyboardInterrupt(f"runner interrupted by signal {signum}")

    signal.signal(signal.SIGTERM, interrupt)
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True, help="path to the execute binary")
    parser.add_argument("--manifest-dir", type=Path, default=PROJECT / "manifests")
    parser.add_argument("--report", type=Path, required=True)
    parser.add_argument("--catalog", type=Path, default=DEFAULT_CATALOG)
    parser.add_argument("--program-dir", type=Path, default=HERE)
    parser.add_argument("--fixtures-dir", type=Path, default=HERE / "fixtures")
    parser.add_argument("--case", action="append", help="run a selected workflow ID; repeatable")
    parser.add_argument("--limit", type=int, help="run the first N workflows as a builder slice")
    parser.add_argument("--allow-partial", action="store_true", help="explicitly permit fewer than 100 workflows")
    parser.add_argument("--timeout", type=float, default=DEFAULT_TIMEOUT)
    parser.add_argument("--isolate-network", action="store_true", help="require an OS-level network sandbox for child processes")
    args = parser.parse_args(argv)

    try:
        if args.timeout <= 0 or args.timeout > 120:
            raise ContractError("timeout must be within 0..120 seconds")
        network_sandbox = discover_network_sandbox()
        if args.isolate_network and network_sandbox is None:
            raise ContractError("--isolate-network was requested but this host cannot provide a network sandbox")
        binary = args.binary.resolve(strict=True)
        if not binary.is_file() or not os.access(binary, os.X_OK):
            raise ContractError(f"execute binary is missing or not executable: {binary}")
        model_helper = binary.with_name("execute-model")
        if not model_helper.is_file() or not os.access(model_helper, os.X_OK):
            raise ContractError(f"sibling execute-model helper is missing or not executable: {model_helper}")
        manifest_dir = args.manifest_dir.resolve(strict=True)
        if not manifest_dir.is_dir():
            raise ContractError(f"manifest path is not a directory: {manifest_dir}")
        catalog_path = args.catalog.resolve(strict=True)
        if catalog_path != DEFAULT_CATALOG.resolve() and args.case is None and not args.allow_partial:
            raise ContractError("a non-default range catalog requires --allow-partial or an explicit --case selection")
        document = json.loads(catalog_path.read_text(encoding="utf-8"))
        binary_sha256 = _sha256(binary)
        model_helper_sha256 = _sha256(model_helper)
        catalog_sha256 = _sha256(catalog_path)
        # The canonical catalog is always held to the complete 001..100 contract,
        # including when a developer asks to execute only selected cases.
        strict_total = catalog_path == DEFAULT_CATALOG.resolve()
        workflows = validate_catalog(
            document,
            program_dir=args.program_dir.resolve(strict=True),
            fixtures_dir=args.fixtures_dir.resolve(strict=True),
            strict_total=strict_total,
        )
        if args.case:
            try:
                selected_ids = {int(item) for item in args.case}
            except ValueError as error:
                raise ContractError("--case IDs must be decimal integers from 1 to 100") from error
            if any(not 1 <= item <= TOTAL_WORKFLOWS for item in selected_ids):
                raise ContractError("--case IDs must be decimal integers from 1 to 100")
            unknown = selected_ids - {item["id"] for item in workflows}
            if unknown:
                raise ContractError(f"unknown workflow ID(s): {', '.join(map(str, sorted(unknown)))}")
            workflows = [item for item in workflows if item["id"] in selected_ids]
        if args.limit is not None:
            if args.limit < 1:
                raise ContractError("--limit must be at least 1")
            if not args.allow_partial:
                raise ContractError("--limit requires --allow-partial")
            workflows = workflows[:args.limit]
        if not workflows:
            raise ContractError("selection contains no workflows")

        results: list[dict[str, Any]] = []
        case_count = 0
        for workflow in workflows:
            item = {
                "id": workflow["id"],
                "ordinal": workflow["ordinal"],
                "title": workflow["title"],
                "file": workflow["file"].relative_to(args.program_dir.resolve()).as_posix(),
                "source_sha256": workflow["source_sha256"],
                "fixture_bytes": workflow["fixture_bytes"],
                "fixture_sha256": workflow["fixture_sha256"],
                "cases": [],
                "passed": True,
            }
            for case in workflow["cases"]:
                case_count += 1
                evidence: dict[str, Any] = {"name": case["name"], "passed": False}
                try:
                    evidence.update(run_case(
                        binary,
                        manifest_dir,
                        workflow,
                        case,
                        args.fixtures_dir.resolve(),
                        args.timeout,
                        require_network_isolation=args.isolate_network,
                    ))
                    print(f"{workflow['ordinal']} {case['name']} PASS", flush=True)
                except Exception as error:
                    item["passed"] = False
                    evidence["error"] = str(error)[:4096]
                    print(f"{workflow['ordinal']} {case['name']} FAIL: {evidence['error']}", flush=True)
                item["cases"].append(evidence)
            item["passed"] = item["passed"] and all(case["passed"] for case in item["cases"])
            results.append(item)

        if _sha256(binary) != binary_sha256:
            raise ContractError("execute binary changed during workflow execution")
        if _sha256(model_helper) != model_helper_sha256:
            raise ContractError("execute-model helper changed during workflow execution")
        if _sha256(catalog_path) != catalog_sha256:
            raise ContractError("workflow catalog changed during workflow execution")
        for workflow in workflows:
            _, _, current_fixture_hash = _fixture_tree(args.fixtures_dir.resolve(), workflow["ordinal"])
            if current_fixture_hash != workflow["fixture_sha256"]:
                raise ContractError(f"workflow {workflow['ordinal']} fixture bundle changed during execution")
        with tempfile.TemporaryDirectory(prefix="execute-model-version-") as version_cwd:
            version_result = invoke(
                binary,
                Path(version_cwd),
                ["--version"],
                timeout=5,
                require_network_isolation=args.isolate_network,
            )
        if version_result["code"] != 0 or version_result["stderr"]:
            raise ContractError("execute --version failed")
        report = {
            "execute_version": version_result["stdout"].decode("utf-8", errors="replace").strip(),
            "binary_sha256": binary_sha256,
            "model_helper_sha256": model_helper_sha256,
            "catalog_path": str(catalog_path),
            "catalog_sha256": catalog_sha256,
            "platform": sys.platform,
            "network_isolation": network_sandbox[0] if network_sandbox else "unavailable; offline guard only",
            "workflows_run": len(results),
            "workflows_passed": sum(item["passed"] for item in results),
            "cases_run": case_count,
            "cases_passed": sum(case["passed"] for item in results for case in item["cases"]),
            "scope": "full-100" if len(results) == TOTAL_WORKFLOWS and {item["ordinal"] for item in results} == {f"{n:03d}" for n in range(1, TOTAL_WORKFLOWS + 1)} else "partial",
            "method": "local fixture-only model cases; execute-model offline guard enabled; exact stdout and declared file oracles; child environment excludes credentials; network sandbox status is reported separately; no provider quality claim; bounded execution time and output capture",
            "results": results,
        }
        args.report.parent.mkdir(parents=True, exist_ok=True)
        args.report.write_text(json.dumps(report, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
        print(f"{report['workflows_passed']}/{len(results)} workflows and {report['cases_passed']}/{case_count} cases passed", flush=True)
        return 0 if report["workflows_passed"] == len(results) and report["cases_passed"] == case_count else 1
    except (ContractError, OSError, json.JSONDecodeError, subprocess.SubprocessError) as error:
        parser.error(str(error))
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
