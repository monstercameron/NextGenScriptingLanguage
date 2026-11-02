#!/usr/bin/env python3
"""Paired saved-source/current-source CLI benchmark for owned .program workloads.

The harness never builds either executable. Each invocation is a fresh process;
correctness and effect checks run before timing and after every warmup/sample.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import shutil
import signal
import statistics
import subprocess
import sys
import tempfile
import time
from dataclasses import dataclass
from datetime import datetime, timezone
from typing import Any, Callable, Dict, List, Mapping, Optional, Sequence, Tuple


ROOT = Path(__file__).resolve().parents[1]
DEFAULT_SAMPLES = 30
DEFAULT_WARMUPS = 5
MAX_ARGV_COUNT = 4096
MAX_TRANSFORM_LINES = 65536
MATCH_CATALOG_SIZE = 1000
MATCH_RESULT_LIMIT = 100
MAX_STDERR_DIAGNOSTIC_BYTES = 512
MAX_OUTER_TIMEOUT_SECONDS = 120
SOURCE_ROOTS = ("src",)
SOURCE_FILES = (
    "Cargo.toml",
    "Cargo.lock",
    "build.rs",
    ".cargo/config.toml",
    "rust-toolchain",
    "rust-toolchain.toml",
    "rustfmt.toml",
)
STARTUP_ENVIRONMENT = {
    "BASH_ENV",
    "ENV",
    "SHELLOPTS",
    "BASHOPTS",
    "CDPATH",
    "GLOBIGNORE",
    "PYTHONPATH",
    "PYTHONHOME",
    "PYTHONSTARTUP",
}
REALISTIC_PROGRAM = ROOT / "examples/omarchy-rewrites/03-update-analyze-logs.program"
REALISTIC_FIXTURE = ROOT / "tests/fixtures/omarchy-rewrites/03-update-analyze-logs/success.log"


class BenchmarkError(RuntimeError):
    """A precondition, oracle, timeout, or artifact-stability failure."""


@dataclass
class Completed:
    status: int
    stdout: bytes
    stderr: bytes
    elapsed_ns: int


@dataclass
class Workload:
    name: str
    program: Optional[Path]
    extra_args: List[str]
    expected_stdout: Optional[bytes]
    expected_stderr: bytes
    effect_check: Callable[[], Mapping[str, Any]]
    semantic_check: Optional[Callable[[bytes], Any]] = None
    direct_argv: Optional[List[str]] = None
    manifest_directory: Optional[Path] = None


def sha256_bytes(payload: bytes) -> str:
    return hashlib.sha256(payload).hexdigest()


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for block in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def combined_hash(files: Mapping[str, str]) -> str:
    digest = hashlib.sha256()
    for name, content_hash in sorted(files.items()):
        digest.update(name.encode("utf-8"))
        digest.update(b"\0")
        digest.update(content_hash.encode("ascii"))
        digest.update(b"\n")
    return digest.hexdigest()


def percentile_nearest_rank(samples: Sequence[int], percentile: float) -> int:
    if not samples:
        raise ValueError("percentile requires at least one sample")
    if not 0 < percentile <= 1:
        raise ValueError("percentile must be in (0, 1]")
    return sorted(samples)[math.ceil(percentile * len(samples)) - 1]


def summarize(samples: Sequence[int]) -> Dict[str, Any]:
    if not samples:
        raise ValueError("summary requires at least one sample")
    return {
        "sample_count": len(samples),
        "median_ns": statistics.median(samples),
        "p95_nearest_rank_ns": percentile_nearest_rank(samples, 0.95),
        "minimum_ns": min(samples),
        "maximum_ns": max(samples),
    }


def interleaved_order(round_index: int) -> Tuple[str, str]:
    if round_index % 2 == 0:
        return ("baseline", "candidate")
    return ("candidate", "baseline")


def validate_report_path(path: Path) -> Path:
    destination = path.expanduser().absolute()
    if destination.exists() or destination.is_symlink():
        raise FileExistsError("refusing to overwrite report: " + str(destination))
    if not destination.parent.is_dir():
        raise ValueError("report parent directory must already exist: " + str(destination.parent))
    return destination


def assert_artifacts_unchanged(before: Mapping[str, Any], after: Mapping[str, Any]) -> None:
    if dict(before) != dict(after):
        raise BenchmarkError(
            "a binary, source file, helper, catalog, program or native tool changed during measurement"
        )


def verify_executable(path: Path, label: str) -> Path:
    resolved = path.expanduser().resolve(strict=True)
    if not resolved.is_file() or not os.access(resolved, os.X_OK):
        raise ValueError(label + " must be an executable regular file: " + str(resolved))
    return resolved


def helper_path(binary: Path) -> Path:
    helper_name = "execute-applet.exe" if os.name == "nt" else "execute-applet"
    return verify_executable(binary.with_name(helper_name), "version-matched execute-applet helper")


def _git_output(root: Path, arguments: Sequence[str], timeout: float = 30.0) -> bytes:
    git = shutil.which("git")
    if not git:
        raise BenchmarkError("Git is required to fingerprint the saved baseline source revision")
    try:
        result = subprocess.run(
            [git, "-C", str(root), *arguments],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=timeout,
            check=False,
        )
    except subprocess.TimeoutExpired as error:
        raise BenchmarkError("timed out fingerprinting source with Git") from error
    if result.returncode != 0:
        diagnostic = result.stderr.decode("utf-8", errors="replace").strip()
        raise BenchmarkError("Git source fingerprint failed: " + diagnostic)
    return result.stdout


def baseline_source_snapshot(root: Path, revision: str) -> Dict[str, Any]:
    """Hash the Rust/Cargo source closure as stored at a Git revision."""
    tree = _git_output(root, ["rev-parse", "--verify", revision + "^{tree}"]).decode("ascii").strip()
    listing = _git_output(
        root,
        ["ls-tree", "-r", "--name-only", revision, "--", *SOURCE_FILES, *SOURCE_ROOTS],
    ).decode("utf-8")
    names = [name for name in listing.splitlines() if name]
    files: Dict[str, str] = {}
    for name in names:
        content = _git_output(root, ["show", revision + ":" + name])
        files[name] = sha256_bytes(content)
    if not files:
        raise BenchmarkError("saved source revision has no Cargo or Rust source files")
    return {
        "revision": revision,
        "git_tree": tree,
        "file_count": len(files),
        "files_sha256": files,
        "combined_sha256": combined_hash(files),
    }


def candidate_source_snapshot(root: Path, revision_label: str) -> Dict[str, Any]:
    """Hash the current source closure, including tracked and untracked Rust files."""
    paths = set()
    for name in SOURCE_FILES:
        path = root / name
        if path.is_file():
            paths.add(path)
    for source_root in SOURCE_ROOTS:
        directory = root / source_root
        if directory.is_dir():
            paths.update(path for path in directory.rglob("*.rs") if path.is_file())
    files: Dict[str, str] = {}
    for path in sorted(paths):
        if path.is_symlink():
            raise BenchmarkError("source closure contains a symlink: " + str(path))
        files[path.relative_to(root).as_posix()] = sha256_file(path)
    if not files:
        raise BenchmarkError("candidate source root has no Cargo or Rust source files")
    return {
        "revision_label": revision_label,
        "file_count": len(files),
        "files_sha256": files,
        "combined_sha256": combined_hash(files),
    }


def directory_snapshot(directory: Path, label: str) -> Dict[str, Any]:
    root = directory.expanduser().resolve(strict=True)
    if not root.is_dir():
        raise ValueError(label + " must be a directory: " + str(root))
    files: Dict[str, str] = {}
    for path in sorted(root.rglob("*")):
        if path.is_symlink():
            raise BenchmarkError(label + " contains a symlink: " + str(path))
        if path.is_file():
            files[path.relative_to(root).as_posix()] = sha256_file(path)
    if not files:
        raise ValueError(label + " contains no files: " + str(root))
    return {
        "path": str(root),
        "file_count": len(files),
        "files_sha256": files,
        "combined_sha256": combined_hash(files),
    }


def file_snapshot(path: Path, label: str) -> Dict[str, Any]:
    resolved = path.resolve(strict=True)
    if not resolved.is_file():
        raise ValueError(label + " must be a regular file: " + str(resolved))
    return {
        "path": str(resolved),
        "bytes": resolved.stat().st_size,
        "sha256": sha256_file(resolved),
    }


def _program_hashes(workloads: Sequence[Workload]) -> Dict[str, Dict[str, Any]]:
    return {
        workload.name: {
            "path": str(workload.program.resolve()),
            "bytes": workload.program.stat().st_size,
            "sha256": sha256_file(workload.program),
        }
        for workload in workloads
        if workload.program is not None
    }


def _workload_inputs(workloads: Sequence[Workload]) -> Dict[str, Dict[str, Any]]:
    inputs: Dict[str, Dict[str, Any]] = {}
    for workload in workloads:
        if workload.program is not None:
            inputs[workload.name] = {
                "kind": "program",
                "path": str(workload.program.resolve()),
                "bytes": workload.program.stat().st_size,
                "sha256": sha256_file(workload.program),
            }
        elif workload.direct_argv is not None:
            encoded = json.dumps(workload.direct_argv, ensure_ascii=False, separators=(",", ":")).encode("utf-8")
            inputs[workload.name] = {
                "kind": "direct_cli_argv",
                "argv": workload.direct_argv,
                "bytes": len(encoded),
                "sha256": sha256_bytes(encoded),
            }
    return inputs


def _safe_environment() -> Dict[str, str]:
    env = {
        name: value
        for name, value in os.environ.items()
        if name not in STARTUP_ENVIRONMENT
        and not name.startswith("BASH_FUNC_")
        and not value.lstrip().startswith("() {")
    }
    env["LC_ALL"] = "C"
    env["LANG"] = "C"
    env["TZ"] = "UTC"
    env["PYTHONDONTWRITEBYTECODE"] = "1"
    return env


def run_process(
    command: Sequence[str],
    cwd: Path,
    environment: Mapping[str, str],
    timeout_seconds: float,
) -> Completed:
    """Run one fresh CLI process and kill its owned process group on timeout."""
    started = time.perf_counter_ns()
    popen_options: Dict[str, Any] = {}
    if os.name == "nt":
        popen_options["creationflags"] = getattr(subprocess, "CREATE_NEW_PROCESS_GROUP", 0)
    else:
        popen_options["start_new_session"] = True
    child = subprocess.Popen(
        list(command),
        cwd=str(cwd),
        env=dict(environment),
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        **popen_options,
    )
    try:
        stdout, stderr = child.communicate(timeout=timeout_seconds)
    except subprocess.TimeoutExpired as error:
        if os.name == "nt":
            child.kill()
            stdout, stderr = child.communicate()
        else:
            try:
                os.killpg(child.pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
            try:
                stdout, stderr = child.communicate(timeout=0.5)
            except subprocess.TimeoutExpired:
                try:
                    os.killpg(child.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                stdout, stderr = child.communicate()
        raise BenchmarkError(
            "subprocess timed out after " + str(timeout_seconds) + " seconds: " + " ".join(command)
        ) from error
    elapsed_ns = time.perf_counter_ns() - started
    return Completed(child.returncode, stdout, stderr, elapsed_ns)


def _observed_effects(workload: Workload) -> Dict[str, Any]:
    try:
        return dict(workload.effect_check())
    except Exception as error:
        raise BenchmarkError("effect oracle failed for " + workload.name + ": " + str(error)) from error


def _signature(observation: Mapping[str, Any]) -> Tuple[Any, ...]:
    return (
        observation["status"],
        observation.get("normalized_stdout_sha256", observation["stdout_sha256"]),
        observation["stderr_sha256"],
        observation.get("normalized_stdout_bytes", observation["stdout_bytes"]),
        observation["stderr_bytes"],
        json.dumps(observation["effects"], sort_keys=True, separators=(",", ":")),
    )


def _stderr_excerpt(stderr: bytes) -> str:
    if not stderr:
        return ""
    sample = stderr[:MAX_STDERR_DIAGNOSTIC_BYTES].decode("utf-8", errors="replace")
    printable = "".join(character if character.isprintable() else " " for character in sample)
    suffix = " [truncated]" if len(stderr) > MAX_STDERR_DIAGNOSTIC_BYTES else ""
    return "; stderr excerpt=" + repr(printable) + suffix


def _check_output(workload: Workload, result: Completed) -> Any:
    if result.status != 0:
        raise BenchmarkError("exited with status " + str(result.status) + _stderr_excerpt(result.stderr))
    if result.stderr != workload.expected_stderr:
        raise BenchmarkError("stderr differs from its exact oracle" + _stderr_excerpt(result.stderr))
    if workload.expected_stdout is not None and result.stdout != workload.expected_stdout:
        raise BenchmarkError("stdout differs from its exact byte oracle" + _stderr_excerpt(result.stderr))
    if workload.semantic_check is not None:
        try:
            return workload.semantic_check(result.stdout)
        except Exception as error:
            raise BenchmarkError("output failed its typed oracle: " + str(error)) from error
    return None


def _capture_observation(
    workload: Workload,
    command: Sequence[str],
    cwd: Path,
    environment: Mapping[str, str],
    timeout_seconds: float,
    context: str,
    expected_effects: Optional[Mapping[str, Any]] = None,
    process_runner: Callable[[Sequence[str], Path, Mapping[str, str], float], Completed] = run_process,
) -> Dict[str, Any]:
    try:
        result = process_runner(command, cwd, environment, timeout_seconds)
        normalized_output = _check_output(workload, result)
        effects = _observed_effects(workload)
        if expected_effects is not None and effects != dict(expected_effects):
            raise BenchmarkError("observed effects changed during an invocation")
    except Exception as error:
        if isinstance(error, BenchmarkError):
            detail = str(error)
        else:
            detail = "subprocess invocation failed: " + str(error)
        raise BenchmarkError(context + ": " + detail) from error
    observation = {
        "status": result.status,
        "stdout_bytes": len(result.stdout),
        "stdout_sha256": sha256_bytes(result.stdout),
        "stderr_bytes": len(result.stderr),
        "stderr_sha256": sha256_bytes(result.stderr),
        "effects": effects,
        "elapsed_ns": result.elapsed_ns,
    }
    if normalized_output is not None:
        encoded_normalized = json.dumps(
            normalized_output, ensure_ascii=False, sort_keys=True, separators=(",", ":")
        ).encode("utf-8")
        observation["normalized_stdout_bytes"] = len(encoded_normalized)
        observation["normalized_stdout_sha256"] = sha256_bytes(encoded_normalized)
    return observation


def run_paired_workload(
    workload: Workload,
    commands: Mapping[str, Sequence[str]],
    cwd: Path,
    environment: Mapping[str, str],
    samples: int,
    warmups: int,
    timeout_seconds: float,
    process_runner: Callable[[Sequence[str], Path, Mapping[str, str], float], Completed] = run_process,
) -> Dict[str, Any]:
    """Oracle each side first, then alternate warmups and measured invocations."""
    if set(commands) != {"baseline", "candidate"}:
        raise ValueError("paired workload needs baseline and candidate commands")
    initial_effects = _observed_effects(workload)
    preflight: Dict[str, Dict[str, Any]] = {}
    for side in ("baseline", "candidate"):
        preflight[side] = _capture_observation(
            workload,
            commands[side],
            cwd,
            environment,
            timeout_seconds,
            context=workload.name + " side=" + side + " phase=preflight",
            expected_effects=initial_effects,
            process_runner=process_runner,
        )
        preflight[side].pop("elapsed_ns", None)
    if _signature(preflight["baseline"]) != _signature(preflight["candidate"]):
        raise BenchmarkError(workload.name + " baseline/candidate preflight results differ")

    expected_effects = initial_effects
    warmup_rows: List[Dict[str, Any]] = []
    for round_index in range(warmups):
        for position, side in enumerate(interleaved_order(round_index)):
            observation = _capture_observation(
                workload,
                commands[side],
                cwd,
                environment,
                timeout_seconds,
                context=workload.name + " side=" + side + " phase=warmup round=" + str(round_index + 1),
                expected_effects=expected_effects,
                process_runner=process_runner,
            )
            if _signature(observation) != _signature(preflight[side]):
                raise BenchmarkError(
                    workload.name + " side=" + side + " phase=warmup round=" + str(round_index + 1)
                    + " differs from its preflight oracle"
                )
            warmup_rows.append(
                {"round": round_index + 1, "order_position": position + 1, "side": side, **observation}
            )

    sample_rows: List[Dict[str, Any]] = []
    for round_index in range(samples):
        for position, side in enumerate(interleaved_order(round_index)):
            observation = _capture_observation(
                workload,
                commands[side],
                cwd,
                environment,
                timeout_seconds,
                context=workload.name + " side=" + side + " phase=sample pair=" + str(round_index + 1),
                expected_effects=expected_effects,
                process_runner=process_runner,
            )
            if _signature(observation) != _signature(preflight[side]):
                raise BenchmarkError(
                    workload.name + " side=" + side + " phase=sample pair=" + str(round_index + 1)
                    + " differs from its preflight oracle"
                )
            sample_rows.append(
                {"pair": round_index + 1, "order_position": position + 1, "side": side, **observation}
            )

    by_side = {
        side: summarize([row["elapsed_ns"] for row in sample_rows if row["side"] == side])
        for side in ("baseline", "candidate")
    }
    paired_samples: List[Dict[str, Any]] = []
    for pair_number in range(1, samples + 1):
        pair = {row["side"]: row for row in sample_rows if row["pair"] == pair_number}
        baseline_ns = pair["baseline"]["elapsed_ns"]
        candidate_ns = pair["candidate"]["elapsed_ns"]
        paired_samples.append(
            {
                "pair": pair_number,
                "baseline_ns": baseline_ns,
                "candidate_ns": candidate_ns,
                "candidate_minus_baseline_ns": candidate_ns - baseline_ns,
                "candidate_over_baseline": candidate_ns / baseline_ns if baseline_ns else None,
            }
        )
    return {
        "preflight": preflight,
        "warmup_order": [
            {"round": index + 1, "order": list(interleaved_order(index))}
            for index in range(warmups)
        ],
        "warmup_observations": warmup_rows,
        "paired_order": [
            {"pair": index + 1, "order": list(interleaved_order(index))}
            for index in range(samples)
        ],
        "raw_paired_samples": sample_rows,
        "summary": by_side,
        "paired_delta_summary": summarize(
            [pair["candidate_minus_baseline_ns"] for pair in paired_samples]
        ),
        "paired_comparisons": paired_samples,
    }


def _shell_text(value: str) -> str:
    return json.dumps(value, ensure_ascii=False)


def _program_with_arguments(executable: str, arguments: Sequence[str]) -> bytes:
    encoded = json.dumps(list(arguments), ensure_ascii=False, separators=(",", ":"))
    return ("run " + _shell_text(executable) + " with arguments " + encoded + "\n").encode("utf-8")


def _tree_effect(root: Path) -> Dict[str, str]:
    """Hash the named immutable fixture files that a workload can access."""
    result: Dict[str, str] = {}
    for path in sorted(root.rglob("*")):
        relative = path.relative_to(root).as_posix()
        if path.is_symlink():
            result[relative] = "symlink:" + os.readlink(path)
        elif path.is_dir():
            result[relative + "/"] = "directory"
        elif path.is_file():
            result[relative] = sha256_file(path)
    return result


def _graph_oracle(payload: bytes) -> Dict[str, Any]:
    try:
        report = json.loads(payload.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise AssertionError("graph output must be UTF-8 JSON") from error
    if not isinstance(report, dict):
        raise AssertionError("graph output must be a JSON object")
    if report.get("protocol") != "execute.graph-run/1":
        raise AssertionError("unexpected graph output protocol")
    generation = report.get("generation")
    revision = report.get("revision")
    if type(generation) is not int or generation != 1 or type(revision) is not int or revision != 1:
        raise AssertionError("graph report generation/revision identity changed")
    dropped_messages = report.get("dropped_messages")
    launched_processes = report.get("launched_processes")
    if (
        type(dropped_messages) is not int
        or dropped_messages != 0
        or type(launched_processes) is not int
        or launched_processes != 0
    ):
        raise AssertionError("typed in-process graph should drop and launch no work")
    outputs = report.get("outputs")
    expected_ports = {
        "joined.status": {"kind": "value", "value": 0},
        "cleaned.status": {"kind": "value", "value": 0},
        "cleaned.value": {"kind": "value", "value": "paired graph"},
    }
    if not isinstance(outputs, list) or len(outputs) != len(expected_ports):
        raise AssertionError("typed graph should emit its two node statuses and terminal value")

    observed: Dict[str, Dict[str, Any]] = {}
    previous_sequence = 0
    for event in outputs:
        if not isinstance(event, dict):
            raise AssertionError("graph terminal rows must be objects")
        sequence = event.get("sequence")
        if type(sequence) is not int or sequence <= previous_sequence:
            raise AssertionError("graph terminal rows must retain strictly increasing sequence order")
        previous_sequence = sequence
        if (
            type(event.get("generation")) is not int
            or event.get("generation") != generation
            or type(event.get("revision")) is not int
            or event.get("revision") != revision
        ):
            raise AssertionError("graph terminal row identity must match its report")
        port = event.get("port")
        if (
            not isinstance(port, dict)
            or set(port) != {"node", "port"}
            or not isinstance(port.get("node"), str)
            or not isinstance(port.get("port"), str)
        ):
            raise AssertionError("graph terminal row has an invalid port identity")
        key = port["node"] + "." + port["port"]
        if key not in expected_ports or key in observed:
            raise AssertionError("graph terminal ports must be unique and expected")
        if event.get("payload") != expected_ports[key]:
            raise AssertionError("typed graph payload/status changed for " + key)
        if event.get("sensitivity") != ["unknown"]:
            raise AssertionError("typed graph terminal sensitivity changed for " + key)
        # Output-list order and global sequence values are captured in the raw
        # stdout hash; pair parity follows typed values by port instead.
        observed[key] = {
            "payload": event["payload"],
            "sensitivity": event["sensitivity"],
        }
    if set(observed) != set(expected_ports):
        raise AssertionError("typed graph terminal port set changed")
    return {
        "generation": generation,
        "revision": revision,
        "dropped_messages": 0,
        "launched_processes": 0,
        "outputs_by_port": observed,
    }


def _match_request_json() -> str:
    # Schema selectors only affect ranking. Omitting applet/port selectors
    # leaves the complete generated endpoint catalog in the search.
    request = {
        "source": {"schema": {"type": "text"}},
        "target": {"schema": {"type": "text"}},
        "limit": MATCH_RESULT_LIMIT,
    }
    return json.dumps(request, ensure_ascii=False, sort_keys=True, separators=(",", ":"))


def generate_match_catalog(directory: Path, count: int = MATCH_CATALOG_SIZE) -> Dict[str, Dict[str, Any]]:
    """Write a deterministic metadata-only catalog for the discovery benchmark."""
    if count <= 0:
        raise ValueError("match catalog size must be positive")
    directory.mkdir(parents=True, exist_ok=False)
    retained: Dict[str, Dict[str, Any]] = {}
    for index in range(count):
        name = "benchmark.catalog.command_" + str(index).zfill(4)
        manifest = {
            "name": name,
            "binary": "benchmark-only-never-invoked",
            "parameters": {"payload": {"type": "string"}},
            "arguments": [],
            "contract": {
                "version": 1,
                "backend": "command",
                "lifecycle": {"kind": "oneshot"},
                "output": {"type": "text", "encoding": "raw"},
                "ports": {
                    "inputs": {
                        "payload": {
                            "schema": {"type": "text"},
                            "delivery": "single",
                        }
                    }
                },
            },
        }
        source = json.dumps(manifest, ensure_ascii=False, indent=2, sort_keys=True) + "\n"
        path = directory / (name + ".json")
        encoded = source.encode("utf-8")
        path.write_bytes(encoded)
        retained[path.name] = {
            "bytes": len(encoded),
            "sha256": sha256_bytes(encoded),
            "source_utf8": source,
        }
    return retained


def _catalog_names_for_match_oracle(directory: Path) -> List[str]:
    """Validate the declared generated contracts and return lexical applet names."""
    manifest_paths = sorted(directory.glob("*.json"))
    if len(manifest_paths) != MATCH_CATALOG_SIZE:
        raise AssertionError(
            "match oracle expected " + str(MATCH_CATALOG_SIZE) + " generated manifests"
        )
    names: List[str] = []
    seen = set()
    for path in manifest_paths:
        manifest = json.loads(path.read_text(encoding="utf-8"))
        name = manifest.get("name")
        if not isinstance(name, str) or not name or name in seen:
            raise AssertionError("match catalog applet names must be nonempty and unique")
        seen.add(name)
        if path.name != name + ".json":
            raise AssertionError("match catalog filename must match its canonical applet name")
        contract = manifest.get("contract", {})
        if (
            manifest.get("binary") != "benchmark-only-never-invoked"
            or manifest.get("parameters") != {"payload": {"type": "string"}}
            or contract.get("version") != 1
            or contract.get("backend") != "command"
            or contract.get("lifecycle") != {"kind": "oneshot"}
            or contract.get("output") != {"type": "text", "encoding": "raw"}
        ):
            raise AssertionError("match catalog manifest does not match its versioned command fixture")
        ports = contract.get("ports", {})
        if ports != {
            "inputs": {
                "payload": {"schema": {"type": "text"}, "delivery": "single"}
            }
        }:
            raise AssertionError("match catalog must declare one optional single text input")
        if manifest.get("arguments") != []:
            raise AssertionError("match catalog fixture must have no invocation arguments")
        names.append(name)
    return sorted(names)


def _expected_match_row(name: str, score: int) -> Dict[str, Any]:
    return {
        "source": {
            "applet": name,
            "port": "stdout",
            "schema": {"type": "text"},
            "delivery": "single",
        },
        "target": {
            "applet": name,
            "port": "payload",
            "schema": {"type": "text"},
            "delivery": "single",
        },
        "compatible": True,
        "score": score,
        "source_matches_request": True,
        "target_matches_request": True,
        "issues": [],
        "suggested_converters": [],
    }


def match_top_k_oracle(payload: bytes, catalog_directory: Path) -> None:
    """Independently check the exact top 100 compatible endpoint pairs.

    Every fixture contributes one text stdout endpoint and one optional text
    input endpoint. Identical text schemas score 100 in the pair, 25 on each
    query side, and 10 for matching single delivery. A same-applet pair gets
    the documented 3-point preference, so all 1,000 diagonal pairs score 213
    and rank before every off-diagonal pair (210). The public lexical tie order
    then selects the first 100 canonical names. No applet binary is started.
    """
    names = _catalog_names_for_match_oracle(catalog_directory)
    if len(names) != MATCH_CATALOG_SIZE:
        raise AssertionError("match catalog must contain 1,000 canonical names")
    try:
        actual = json.loads(payload.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise AssertionError("match output must be UTF-8 JSON") from error
    expected = {
        "protocol": "execute.applet-matches/1",
        "validation": "declared_contracts",
        "matches": [_expected_match_row(name, 213) for name in names[:MATCH_RESULT_LIMIT]],
    }
    if actual != expected:
        raise AssertionError("match output differs from the exact lexical compatible top-100 oracle")


def _parse_counts(raw: str) -> List[int]:
    try:
        counts = [int(part.strip()) for part in raw.split(",") if part.strip()]
    except ValueError as error:
        raise argparse.ArgumentTypeError("argv counts must be comma-separated integers") from error
    if not counts or any(count <= 0 or count > MAX_ARGV_COUNT for count in counts):
        raise argparse.ArgumentTypeError("argv counts must be between 1 and " + str(MAX_ARGV_COUNT))
    if len(set(counts)) != len(counts):
        raise argparse.ArgumentTypeError("argv counts must not repeat")
    return counts


def build_workloads(
    root: Path,
    tools: Mapping[str, Path],
    transform_lines: int,
    argv_counts: Sequence[int],
) -> Tuple[Path, List[Workload], Dict[str, Dict[str, Any]]]:
    """Prepare small literal .program inputs and immutable owned fixtures."""
    root.mkdir(parents=True, exist_ok=True)
    home = root / "home"
    home.mkdir()
    programs_dir = root / "programs"
    programs_dir.mkdir()
    fixture_dir = root / "fixtures"
    fixture_dir.mkdir()
    environment = _safe_environment()
    environment["HOME"] = str(home)

    workloads: List[Workload] = []
    program_sources: Dict[str, bytes] = {}

    def write_program(name: str, source: bytes) -> Path:
        path = programs_dir / (name + ".program")
        path.write_bytes(source)
        program_sources[name] = source
        return path

    no_op = write_program("noop_direct_true", _program_with_arguments(str(tools["true"]), []))
    workloads.append(
        Workload(
            "fresh_noop_direct_true",
            no_op,
            [],
            b"",
            b"",
            lambda: _tree_effect(root),
        )
    )

    for count in argv_counts:
        values = ["literal-" + str(index).zfill(4) for index in range(count)]
        special = ["", "two words", "$HOME", "$(touch never-run)", ";"]
        values[: min(len(values), len(special))] = special[: min(len(values), len(special))]
        arguments = ["%s\\n", *values]
        name = "literal_argv_" + str(count)
        program = write_program(name, _program_with_arguments(str(tools["printf"]), arguments))
        expected = "".join(value + "\n" for value in values).encode("utf-8")
        workloads.append(Workload(name, program, [], expected, b"", lambda: _tree_effect(root)))

    transform_path = fixture_dir / "lowercase-lines.txt"
    transform_input = b"paired native pipe benchmark line 0123456789\n" * transform_lines
    transform_path.write_bytes(transform_input)
    transform_program = write_program(
        "native_pipe_cat_tr",
        _program_with_arguments(str(tools["cat"]), ["--", str(transform_path)])
        + ("pipe to " + _shell_text(str(tools["tr"])) + ' with arguments ["[:lower:]","[:upper:]"]\n').encode("utf-8"),
    )
    transformed = transform_input.translate(bytes.maketrans(b"abcdefghijklmnopqrstuvwxyz", b"ABCDEFGHIJKLMNOPQRSTUVWXYZ"))
    workloads.append(
        Workload(
            "native_pipe_cat_to_tr",
            transform_program,
            [],
            transformed,
            b"",
            lambda: _tree_effect(root),
        )
    )

    graph_source = (
        "graph paired_typed_graph\n"
        '  run text.concat with parts ["paired", "graph"] and separator " " as joined\n'
        "  run text.trim_final_lf as cleaned\n"
        "  bind joined.value to cleaned.text with capacity 1 lossless\n"
        "end graph\n"
    ).encode("utf-8")
    graph_program = write_program("legacy_typed_graph_inprocess", graph_source)
    workloads.append(
        Workload(
            "legacy_typed_graph_two_inprocess_nodes",
            graph_program,
            [],
            None,
            b"",
            lambda: _tree_effect(root),
            _graph_oracle,
        )
    )

    workflow_source = REALISTIC_PROGRAM.read_bytes()
    workflow_program = write_program("existing_omarchy_update_log_success", workflow_source)
    workflow_log = fixture_dir / "update-success.log"
    workflow_log.write_bytes(REALISTIC_FIXTURE.read_bytes())
    workloads.append(
        Workload(
            "existing_omarchy_update_log_success",
            workflow_program,
            ["--input", "update_log=" + str(workflow_log)],
            b"",
            b"",
            lambda: _tree_effect(root),
        )
    )

    match_catalog = root / "match-catalog-1000-command-applets"
    match_catalog_sources = generate_match_catalog(match_catalog)
    match_request = _match_request_json()
    match_workload = Workload(
        "applet_match_1000_catalog_top_100",
        None,
        [],
        None,
        b"",
        lambda: _tree_effect(root),
        lambda payload: match_top_k_oracle(payload, match_catalog),
        direct_argv=["applets", "match", "--json", match_request],
        manifest_directory=match_catalog,
    )
    workloads.append(match_workload)

    payload_inventory = {
        "programs": {
            name: {
                "bytes": len(payload),
                "sha256": sha256_bytes(payload),
                "source_utf8": payload.decode("utf-8"),
            }
            for name, payload in sorted(program_sources.items())
        },
        "fixtures": {
            "lowercase-lines.txt": {"bytes": len(transform_input), "sha256": sha256_bytes(transform_input)},
            "update-success.log": {
                "bytes": workflow_log.stat().st_size,
                "sha256": sha256_file(workflow_log),
            },
        },
        "original_existing_program": file_snapshot(REALISTIC_PROGRAM, "existing benchmark program"),
        "original_existing_fixture": file_snapshot(REALISTIC_FIXTURE, "existing benchmark fixture"),
        "match_catalog": {
            "manifest_count": len(match_catalog_sources),
            "files": match_catalog_sources,
            "request_json": match_request,
            "request_sha256": sha256_bytes(match_request.encode("utf-8")),
            "scope": "1,000 versioned command-applet metadata manifests; no applet binary is invoked",
        },
    }
    return home, workloads, payload_inventory


def _cli_command(
    binary: Path,
    manifest_directory: Path,
    workload: Workload,
    command_timeout_ms: int,
) -> List[str]:
    selected_manifest_directory = workload.manifest_directory or manifest_directory
    selected_manifest_directory = selected_manifest_directory.expanduser().resolve(strict=True)
    if workload.direct_argv is not None:
        if workload.program is not None:
            raise BenchmarkError("a direct CLI workload cannot also declare a .program input")
        return [
            str(binary),
            *workload.direct_argv,
            "--manifest-dir",
            str(selected_manifest_directory),
        ]
    if workload.program is None:
        raise BenchmarkError("a .program workload requires its source path")
    return [
        str(binary),
        "run",
        str(workload.program),
        "--manifest-dir",
        str(selected_manifest_directory),
        "--max-output-bytes",
        "1048576",
        "--command-timeout-ms",
        str(command_timeout_ms),
        *workload.extra_args,
    ]


def _environment_report(environment: Mapping[str, str]) -> Dict[str, Any]:
    visible = {name: environment.get(name) for name in ("LC_ALL", "LANG", "TZ")}
    path_bytes = environment.get("PATH", "").encode("utf-8")
    return {
        "os": platform.system(),
        "os_release": platform.release(),
        "platform": platform.platform(),
        "architecture": platform.machine(),
        "processor": platform.processor() or None,
        "logical_cpu_count": os.cpu_count(),
        "python": sys.version.splitlines()[0],
        "environment": visible,
        "path_sha256": sha256_bytes(path_bytes),
    }


def _artifact_snapshot(
    args: argparse.Namespace,
    binaries: Mapping[str, Path],
    helpers: Mapping[str, Path],
    workloads: Sequence[Workload],
    tools: Mapping[str, Path],
) -> Dict[str, Any]:
    return {
        "binaries": {side: file_snapshot(path, side + " interpreter") for side, path in binaries.items()},
        "helpers": {side: file_snapshot(path, side + " execute-applet") for side, path in helpers.items()},
        "source": {
            "baseline": baseline_source_snapshot(ROOT, args.baseline_revision),
            "candidate": candidate_source_snapshot(ROOT, args.candidate_revision),
        },
        "catalogs": {
            "baseline": directory_snapshot(args.baseline_manifest_dir, "baseline manifest catalog"),
            "candidate": directory_snapshot(args.candidate_manifest_dir, "candidate manifest catalog"),
        },
        "workload_manifest_catalogs": {
            workload.name: directory_snapshot(
                workload.manifest_directory, workload.name + " generated manifest catalog"
            )
            for workload in workloads
            if workload.manifest_directory is not None
        },
        "program_payloads": _program_hashes(workloads),
        "workload_inputs": _workload_inputs(workloads),
        "tool_binaries": {name: file_snapshot(path, name) for name, path in tools.items()},
        "existing_workflow_sources": {
            "program": file_snapshot(REALISTIC_PROGRAM, "existing benchmark program"),
            "fixture": file_snapshot(REALISTIC_FIXTURE, "existing benchmark fixture"),
        },
        "harness": file_snapshot(Path(__file__), "benchmark harness"),
    }


def _tool_paths() -> Dict[str, Path]:
    resolved: Dict[str, Path] = {}
    for name in ("true", "printf", "cat", "tr", "grep"):
        found = shutil.which(name)
        if not found:
            raise BenchmarkError("required native tool is not on PATH: " + name)
        resolved[name] = Path(found).resolve(strict=True)
    return resolved


def build_report(
    args: argparse.Namespace,
    process_runner: Optional[Callable[[Sequence[str], Path, Mapping[str, str], float], Completed]] = None,
) -> Dict[str, Any]:
    if not args.candidate_build_confirmed:
        raise BenchmarkError("pass --candidate-build-confirmed after the candidate build is complete")
    if not args.measurement_window_idle:
        raise BenchmarkError("pass --measurement-window-idle only after builds, tests and other benchmarks stop")
    if args.samples < 10:
        raise ValueError("--samples must be at least 10")
    if args.warmups < 0:
        raise ValueError("--warmups must be zero or greater")
    if not 0 < args.timeout_seconds <= MAX_OUTER_TIMEOUT_SECONDS:
        raise ValueError("--timeout-seconds must be in (0, " + str(MAX_OUTER_TIMEOUT_SECONDS) + "]")
    if not 0 < args.command_timeout_ms <= int(args.timeout_seconds * 1000):
        raise ValueError("--command-timeout-ms must be positive and no greater than the outer timeout")
    if not 0 < args.transform_lines <= MAX_TRANSFORM_LINES:
        raise ValueError("--transform-lines must be between 1 and " + str(MAX_TRANSFORM_LINES))

    report_path = validate_report_path(args.report)
    binaries = {
        "baseline": verify_executable(args.baseline_binary, "baseline interpreter"),
        "candidate": verify_executable(args.candidate_binary, "candidate interpreter"),
    }
    helpers = {side: helper_path(path) for side, path in binaries.items()}
    for attribute, label in (
        ("baseline_manifest_dir", "baseline manifest catalog"),
        ("candidate_manifest_dir", "candidate manifest catalog"),
    ):
        directory = getattr(args, attribute).expanduser().resolve(strict=True)
        if not directory.is_dir():
            raise ValueError(label + " does not exist or is not a directory: " + str(directory))
        # CLI commands run with a temporary cwd, so relative user paths must
        # be made absolute here before their meaning can change.
        setattr(args, attribute, directory)

    tools = _tool_paths()
    runner = process_runner or run_process
    environment = _safe_environment()
    observations: Dict[str, Any] = {}
    with tempfile.TemporaryDirectory(prefix="execute-source-pair-") as temp_name:
        cwd = Path(temp_name)
        home, workloads, payload_inventory = build_workloads(
            cwd, tools, args.transform_lines, args.argv_counts
        )
        environment["HOME"] = str(home)
        initial_artifacts = _artifact_snapshot(args, binaries, helpers, workloads, tools)
        for workload in workloads:
            commands = {
                "baseline": _cli_command(
                    binaries["baseline"], args.baseline_manifest_dir, workload, args.command_timeout_ms
                ),
                "candidate": _cli_command(
                    binaries["candidate"], args.candidate_manifest_dir, workload, args.command_timeout_ms
                ),
            }
            observations[workload.name] = run_paired_workload(
                workload,
                commands,
                cwd,
                environment,
                args.samples,
                args.warmups,
                args.timeout_seconds,
                process_runner=runner,
            )
        final_artifacts = _artifact_snapshot(args, binaries, helpers, workloads, tools)
        assert_artifacts_unchanged(initial_artifacts, final_artifacts)
        return {
            "schema": "execute.source-pair-benchmark/1",
            "recorded_at_utc": datetime.now(timezone.utc).isoformat(),
            "revisions": {
                "baseline": args.baseline_revision,
                "candidate": args.candidate_revision,
            },
            "binaries": initial_artifacts["binaries"],
            "helpers": initial_artifacts["helpers"],
            "source_fingerprints": initial_artifacts["source"],
            "manifest_catalogs": initial_artifacts["catalogs"],
            "workload_manifest_catalogs": initial_artifacts["workload_manifest_catalogs"],
            "program_payloads": payload_inventory["programs"],
            "workload_input_payloads": initial_artifacts["workload_inputs"],
            "fixture_payloads": payload_inventory["fixtures"],
            "retained_workload_catalogs": {"applet_match_1000_catalog_top_100": payload_inventory["match_catalog"]},
            "existing_workflow_sources": {
                "program": payload_inventory["original_existing_program"],
                "fixture": payload_inventory["original_existing_fixture"],
            },
            "native_tools": initial_artifacts["tool_binaries"],
            "method": {
                "fresh_process_per_invocation": True,
                "sampling": "Independent per-side oracle first, then alternating baseline-first/candidate-first warmups and paired measured rounds.",
                "timed_boundary": "Python starts one Execute process and captures stdout/stderr through exit. Oracle checks and filesystem snapshots happen after each process exits and are excluded from the elapsed interval.",
                "verification": "Every preflight, warmup and measured invocation checks status, exact output or typed graph payload, stderr, and read-only fixture effects. Baseline and candidate preflight observations must match.",
                "graph_pair_parity": "The graph oracle independently validates protocol, generation/revision identity, zero drops/process launches, all three typed payload/status values and port sensitivities. Terminal rows must be sequence-ordered, while cross-side parity uses the canonical payload map by port; per-invocation raw stdout byte counts and SHA-256 hashes are retained so legitimate event ordering/global-sequence differences remain visible.",
                "scope": "The local workload families measure their named CLI paths only. The top-K case measures matching over a 1,000-manifest metadata catalog, not 1,000 independently implemented or concurrent applets. Results do not establish Bash compatibility, broad application latency, CPU use, peak RSS, or a universal performance ranking.",
                "applet_match_top_k": "Discovery only via execute applets match --json. Every catalog manifest declares a versioned command contract with an optional text input and a text stdout output treated as single delivery. The same generated catalog is passed to both binaries. The independent oracle validates exactly 100 lexically ranked compatible pairs and every public result field before timing and after every invocation.",
                "resource_measurements": "Only end-to-end wall time is recorded. Peak RSS and CPU time are not measured or inferred from allocation counts or wall time.",
                "cache_state": "Warmups run before measured pairs; OS cache is not flushed and cold-cache behavior is not measured.",
            },
            "parameters": {
                "samples_per_side_per_workload": args.samples,
                "warmups_per_side_per_workload": args.warmups,
                "timeout_seconds": args.timeout_seconds,
                "command_timeout_ms": args.command_timeout_ms,
                "argv_counts": args.argv_counts,
                "transform_lines": args.transform_lines,
            },
            "environment": _environment_report(environment),
            "artifact_fingerprints": {
                "pre_measurement": initial_artifacts,
                "post_measurement": final_artifacts,
                "unchanged": True,
            },
            "workloads": observations,
            "report_path": str(report_path),
        }


def write_report(path: Path, report: Mapping[str, Any]) -> Path:
    destination = validate_report_path(path)
    payload = (json.dumps(report, indent=2, sort_keys=True) + "\n").encode("utf-8")
    descriptor = os.open(destination, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(descriptor, "wb") as output:
        output.write(payload)
        output.flush()
        os.fsync(output.fileno())
    return destination


def make_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--baseline-binary", type=Path, required=True, help="preserved baseline Execute binary")
    parser.add_argument("--candidate-binary", type=Path, required=True, help="already-built candidate Execute binary")
    parser.add_argument("--baseline-manifest-dir", type=Path, required=True, help="explicit baseline applet catalog directory")
    parser.add_argument("--candidate-manifest-dir", type=Path, required=True, help="explicit candidate applet catalog directory")
    parser.add_argument("--baseline-revision", required=True, help="Git revision that produced the saved baseline source")
    parser.add_argument("--candidate-revision", required=True, help="label for the candidate source/build")
    parser.add_argument("--report", type=Path, required=True, help="fresh report path; existing files are never replaced")
    parser.add_argument("--samples", type=int, default=DEFAULT_SAMPLES)
    parser.add_argument("--warmups", type=int, default=DEFAULT_WARMUPS)
    parser.add_argument("--argv-counts", type=_parse_counts, default=[8, 64, 256, 1024])
    parser.add_argument("--transform-lines", type=int, default=8192)
    parser.add_argument("--timeout-seconds", type=float, default=30.0)
    parser.add_argument("--command-timeout-ms", type=int, default=10000)
    parser.add_argument(
        "--candidate-build-confirmed",
        action="store_true",
        help="confirm the exact candidate build is complete before measurement",
    )
    parser.add_argument(
        "--measurement-window-idle",
        action="store_true",
        help="confirm all builds, tests and other benchmarks have stopped",
    )
    return parser


def main(argv: Optional[Sequence[str]] = None) -> int:
    parser = make_parser()
    args = parser.parse_args(argv)
    try:
        report = build_report(args)
        destination = write_report(args.report, report)
        print("Wrote " + str(destination))
        for name, workload in report["workloads"].items():
            print(name + ": " + json.dumps(workload["summary"], sort_keys=True))
        return 0
    except (AssertionError, BenchmarkError, FileExistsError, OSError, RuntimeError, ValueError, json.JSONDecodeError) as error:
        print("Source pair benchmark failed: " + str(error), file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
