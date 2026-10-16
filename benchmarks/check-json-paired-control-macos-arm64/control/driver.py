#!/usr/bin/env python3
"""Preregistered paired control for static `execute check [--json]` latency.

No Execute binary is run unless --confirm-idle-window is present. This driver
never builds product code and refuses unpinned binaries, modified fixtures, or
an existing report path. Timings are collected only via benchmarks/source_pair.py.
"""
from __future__ import annotations

import argparse
import base64
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import sys
import stat
from datetime import datetime, timezone
from typing import Any, Callable, Dict, Mapping, Optional, Sequence

HERE = Path(__file__).resolve().parent
FIXTURE_ROOT = HERE / "fixtures"
BASELINE_SOURCE_REVISION = "58b42f65d8c4de171baf8f3049ab70cf37c51af4"
CANDIDATE_SOURCE_LABEL = "staged-tree:ed2d0882d3d799cae321dca4f95fe6e6767f46b8"
BASELINE_EXECUTE_SHA256 = "c1b39031fa4f7924093179c1aeb57fe92d01ad85e1ecb834b0d72968c309ed69"
BASELINE_HELPER_SHA256 = "1ba4eb33c00ef969c6da035460adb3166316a8ddba5a32f25e2c18406f849e3f"
CANDIDATE_EXECUTE_SHA256 = "5a2d2d0eea72e7154f4cca26aaa7fa12e8329423917c2e22d45f346fe8b6a331"
CANDIDATE_HELPER_SHA256 = "56ecb62e828e55d83fc41e16f36a7c2ff91f01560daf5957d8d80f007f038636"
CANDIDATE_PATCH_SHA256 = "70867f9c719038cd7043e9c38cb948d65f73056ae42bd43917569aebcb9be362"
FIXTURE_SCHEMA = "execute.check-json-reuse-control-fixtures/1"
CELL_PLAN = (
    (1, "full75", False), (1, "full75", True),
    (1, "inert-cat", False), (1, "inert-cat", True),
    (100, "full75", False), (100, "full75", True),
    (100, "inert-cat", False), (100, "inert-cat", True),
)


def _sha256(payload: bytes) -> str:
    return hashlib.sha256(payload).hexdigest()


def _load_source_pair(path: Path) -> Any:
    sys.dont_write_bytecode = True
    spec = importlib.util.spec_from_file_location("execute_source_pair", path.resolve(strict=True))
    if spec is None or spec.loader is None:
        raise RuntimeError("cannot load the existing source_pair.py harness")
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


def _program_bytes(step_count: int) -> bytes:
    if step_count not in (1, 100):
        raise ValueError("only the preregistered 1- and 100-step programs are allowed")
    lines = ["accept path as text"]
    lines.extend(
        'run "execute-check-json-reuse-nonexistent-command" with arguments [path]'
        for _ in range(step_count)
    )
    lines.append('then save to "forbidden"')
    return ("\n".join(lines) + "\n").encode("utf-8")


def prepare_fixtures(primary_root: Path) -> Dict[str, Any]:
    """Create and freeze source-derived catalogs/programs; never invoke Execute."""
    primary = primary_root.expanduser().resolve(strict=True)
    source_catalog = primary / "manifests"
    if not source_catalog.is_dir():
        raise ValueError("primary source must contain the saved 75-manifest catalog")
    manifest_files = sorted(path for path in source_catalog.iterdir() if path.is_file())
    if len(manifest_files) != 75 or any(path.suffix != ".json" for path in manifest_files):
        raise ValueError("saved catalog must contain exactly 75 top-level JSON files")
    if FIXTURE_ROOT.exists():
        raise FileExistsError("refusing to replace frozen fixtures: " + str(FIXTURE_ROOT))
    catalog_full = FIXTURE_ROOT / "catalogs" / "full75"
    catalog_inert = FIXTURE_ROOT / "catalogs" / "inert-one-unused-cat"
    for directory in (catalog_full, catalog_inert):
        directory.mkdir(parents=True, exist_ok=False)
    for path in manifest_files:
        data = path.read_bytes()
        (catalog_full / path.name).write_bytes(data)
        if path.name == "cat.json":
            (catalog_inert / path.name).write_bytes(data)
    if not (catalog_inert / "cat.json").is_file():
        raise ValueError("saved catalog has no cat.json for inert-catalog control")
    for count in (1, 100):
        case_root = FIXTURE_ROOT / "cases" / ("steps-%03d" % count)
        program_dir = case_root / "programs"
        program_dir.mkdir(parents=True, exist_ok=False)
        (program_dir / "check.program").write_bytes(_program_bytes(count))
    # Freeze every fixture file and directory against accidental edits during collection.
    for path in FIXTURE_ROOT.rglob("*"):
        if path.is_file():
            path.chmod(0o444)
    for path in sorted((path for path in FIXTURE_ROOT.rglob("*") if path.is_dir()), reverse=True):
        path.chmod(0o555)
    FIXTURE_ROOT.chmod(0o555)
    return fixture_snapshot(_load_source_pair(primary / "benchmarks" / "source_pair.py"))


def fixture_snapshot(sp: Any) -> Dict[str, Any]:
    full = FIXTURE_ROOT / "catalogs" / "full75"
    inert = FIXTURE_ROOT / "catalogs" / "inert-one-unused-cat"
    if not full.is_dir() or not inert.is_dir():
        raise ValueError("fixtures are missing; use --prepare-fixtures before a measurement")
    full_result = sp.directory_snapshot(full, "full 75-manifest catalog")
    inert_result = sp.directory_snapshot(inert, "inert one-manifest catalog")
    if full_result["file_count"] != 75 or inert_result["file_count"] != 1:
        raise ValueError("frozen catalog counts differ from the preregistered 75/1 plan")
    programs: Dict[str, Any] = {}
    for count in (1, 100):
        case_root = FIXTURE_ROOT / "cases" / ("steps-%03d" % count)
        program = case_root / "programs" / "check.program"
        expected = _program_bytes(count)
        if program.read_bytes() != expected:
            raise ValueError("program fixture does not match preregistered bytes: " + str(program))
        if b'run "cat"' in expected:
            raise ValueError("the inert cat manifest must remain unused by every program")
        programs[str(count)] = sp.file_snapshot(program, "%d-step .program" % count)
    return {
        "schema": FIXTURE_SCHEMA,
        "catalogs": {"full75": full_result, "inert-one-unused-cat": inert_result},
        "programs": programs,
    }


def check_json_semantic_oracle(path_argument: str) -> Callable[[bytes], Mapping[str, Any]]:
    expected_path_bytes = list(path_argument.encode("utf-8"))

    def validate(payload: bytes) -> Mapping[str, Any]:
        try:
            document = json.loads(payload.decode("utf-8"))
        except (UnicodeDecodeError, json.JSONDecodeError) as error:
            raise AssertionError("check --json output must be a UTF-8 JSON document") from error
        expected = {
            "protocol": "execute.check/1",
            "validation": "static",
            "ok": True,
            "path": path_argument,
            "path_bytes": expected_path_bytes,
            "diagnostics": [],
        }
        if document != expected:
            raise AssertionError("check --json fields differ from the independent static oracle")
        return document

    return validate


def _commands(binary: Path, count: int, catalog: Path, json_mode: bool) -> Sequence[str]:
    relative_program = "programs/check.program"
    command = [str(binary), "check", relative_program]
    if json_mode:
        command.append("--json")
    command.extend(("--manifest-dir", str(catalog)))
    return command


def _make_workload(sp: Any, count: int, catalog_kind: str, json_mode: bool) -> tuple[Any, Path, Path]:
    case_root = FIXTURE_ROOT / "cases" / ("steps-%03d" % count)
    program = case_root / "programs" / "check.program"
    catalog = FIXTURE_ROOT / "catalogs" / (
        "full75" if catalog_kind == "full75" else "inert-one-unused-cat"
    )
    if not program.is_file() or not catalog.is_dir():
        raise ValueError("frozen benchmark fixture is missing")
    forbidden = case_root / "forbidden"
    expected_effects = sp._tree_effect(case_root)
    if forbidden.exists():
        raise ValueError("forbidden save output exists before measurement")

    def effects() -> Mapping[str, Any]:
        observed = sp._tree_effect(case_root)
        if forbidden.exists():
            raise AssertionError("static check created the forbidden save output")
        return observed

    name = "%03d_steps-%s-%s" % (count, catalog_kind, "json" if json_mode else "ordinary")
    workload = sp.Workload(
        name=name,
        program=program,
        extra_args=[],
        expected_stdout=None if json_mode else b"",
        expected_stderr=b"",
        effect_check=effects,
        semantic_check=check_json_semantic_oracle("programs/check.program") if json_mode else None,
        manifest_directory=catalog,
    )
    if effects() != expected_effects:
        raise ValueError("fixture effects changed while constructing workload")
    return workload, case_root, catalog


def _attach_raw_bytes(sp: Any, rows: Mapping[str, Any], captures: list[Dict[str, Any]]) -> Dict[str, Any]:
    sequence = []
    for side in ("baseline", "candidate"):
        sequence.append(("preflight", side, rows["preflight"][side]))
    for row in rows["warmup_observations"]:
        sequence.append(("warmup", row["side"], row))
    for row in rows["raw_paired_samples"]:
        sequence.append(("sample", row["side"], row))
    if len(sequence) != len(captures):
        raise RuntimeError("raw byte recorder and source_pair observation counts diverged")
    for (phase, side, row), capture in zip(sequence, captures):
        if capture.get("state") != "completed":
            raise RuntimeError("completed cell contains a failed or unfinished invocation record")
        for field in ("status", "stdout_bytes", "stdout_sha256", "stderr_bytes", "stderr_sha256", "elapsed_ns"):
            if field in row and capture[field] != row[field]:
                raise RuntimeError("raw byte recorder disagrees with source_pair observation at " + phase)
        if capture.get("phase") != phase or capture.get("side") != side:
            raise RuntimeError("raw byte recorder phase or side disagrees with source_pair order")
        capture["pair"] = row.get("pair")
        capture["round"] = row.get("round")
    # The output is contractually byte-identical as well as semantically valid.
    expected_stdout: Optional[bytes] = None
    expected_stderr: Optional[bytes] = None
    for capture in captures:
        stdout = base64.b64decode(capture["stdout_base64"])
        stderr = base64.b64decode(capture["stderr_base64"])
        if expected_stdout is None:
            expected_stdout = stdout
            expected_stderr = stderr
        elif stdout != expected_stdout or stderr != expected_stderr:
            raise RuntimeError("exact stdout/stderr byte parity failed across invocations")
    return {
        **dict(rows),
        "raw_invocations": captures,
        "raw_bytes_retained": True,
        "exact_stdout_stderr_byte_parity": True,
    }


def _invocation_context(sp: Any, index: int, samples: int, warmups: int) -> Dict[str, Any]:
    if index < 2:
        return {"phase": "preflight", "side": ("baseline", "candidate")[index]}
    index -= 2
    if index < warmups * 2:
        round_index, position = divmod(index, 2)
        return {
            "phase": "warmup",
            "side": sp.interleaved_order(round_index)[position],
            "round": round_index + 1,
        }
    index -= warmups * 2
    if index < samples * 2:
        round_index, position = divmod(index, 2)
        return {
            "phase": "sample",
            "side": sp.interleaved_order(round_index)[position],
            "pair": round_index + 1,
        }
    raise RuntimeError("source_pair invoked more processes than the registered plan")


def _make_capture_runner(
    sp: Any,
    captures: list[Dict[str, Any]],
    samples: int = 48,
    warmups: int = 5,
    process_implementation: Optional[Callable[..., Any]] = None,
) -> Callable[..., Any]:
    bounded_process = process_implementation or sp.run_process

    def capture(command: Sequence[str], cwd: Path, environment: Mapping[str, str], timeout_seconds: float) -> Any:
        # Preserve the existing harness's bounded process implementation and timing boundary.
        context = _invocation_context(sp, len(captures), samples, warmups)
        record: Dict[str, Any] = {
            **context,
            "state": "running",
            "command": list(command),
            "timeout_seconds": timeout_seconds,
        }
        try:
            completed = bounded_process(command, cwd, environment, timeout_seconds)
        except Exception as error:
            chain = []
            cause: Optional[BaseException] = error
            while cause is not None:
                chain.append({"type": type(cause).__name__, "message": str(cause)})
                cause = cause.__cause__
            record.update({
                "state": "failed",
                "status": None,
                "stdout_bytes": None,
                "stdout_sha256": None,
                "stdout_base64": None,
                "stderr_bytes": None,
                "stderr_sha256": None,
                "stderr_base64": None,
                "elapsed_ns": None,
                "error_type": type(error).__name__,
                "error": str(error),
                "exception_chain": chain,
                "output_available": False,
            })
            captures.append(record)
            raise
        record.update({
            "state": "completed",
            "status": completed.status,
            "stdout_bytes": len(completed.stdout),
            "stdout_sha256": sp.sha256_bytes(completed.stdout),
            "stdout_base64": base64.b64encode(completed.stdout).decode("ascii"),
            "stderr_bytes": len(completed.stderr),
            "stderr_sha256": sp.sha256_bytes(completed.stderr),
            "stderr_base64": base64.b64encode(completed.stderr).decode("ascii"),
            "elapsed_ns": completed.elapsed_ns,
            "output_available": True,
        })
        captures.append(record)
        return completed
    return capture


class IncrementalReport:
    """Own a fresh report file and durably update its progress in place."""

    def __init__(self, sp: Any, path: Path) -> None:
        self.path = sp.validate_report_path(path)
        self.descriptor = os.open(self.path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        identity = os.fstat(self.descriptor)
        self.device = identity.st_dev
        self.inode = identity.st_ino

    def persist(self, report: Dict[str, Any]) -> None:
        try:
            current = os.lstat(self.path)
        except OSError as error:
            raise RuntimeError("running report path disappeared during collection") from error
        if stat.S_ISLNK(current.st_mode) or (current.st_dev, current.st_ino) != (self.device, self.inode):
            raise RuntimeError("running report path was replaced during collection")
        report["updated_at_utc"] = datetime.now(timezone.utc).isoformat()
        payload = (json.dumps(report, indent=2, sort_keys=True) + "\n").encode("utf-8")
        os.lseek(self.descriptor, 0, os.SEEK_SET)
        os.ftruncate(self.descriptor, 0)
        offset = 0
        while offset < len(payload):
            offset += os.write(self.descriptor, payload[offset:])
        os.fsync(self.descriptor)

    def close(self) -> None:
        if self.descriptor >= 0:
            os.close(self.descriptor)
            self.descriptor = -1


def _error_record(error: Exception) -> Dict[str, str]:
    return {"type": type(error).__name__, "message": str(error)}


def _collect_cells(
    sp: Any,
    report: Dict[str, Any],
    report_writer: IncrementalReport,
    binaries: Mapping[str, Path],
    environment: Mapping[str, str],
    samples: int = 48,
    warmups: int = 5,
    timeout_seconds: float = 30.0,
    plan: Sequence[tuple[int, str, bool]] = CELL_PLAN,
    process_implementation: Optional[Callable[..., Any]] = None,
) -> int:
    for count, catalog_kind, json_mode in plan:
        name = "%03d_steps-%s-%s" % (count, catalog_kind, "json" if json_mode else "ordinary")
        captures: list[Dict[str, Any]] = []
        current = {
            "name": name,
            "state": "running",
            "program_steps": count,
            "catalog": catalog_kind,
            "mode": "check --json" if json_mode else "ordinary check",
            "raw_invocations": captures,
        }
        report["current_cell"] = current
        report_writer.persist(report)
        try:
            workload, cwd, catalog = _make_workload(sp, count, catalog_kind, json_mode)
            commands = {
                side: _commands(binaries[side], count, catalog, json_mode)
                for side in ("baseline", "candidate")
            }
            rows = sp.run_paired_workload(
                workload=workload,
                commands=commands,
                cwd=cwd,
                environment=environment,
                samples=samples,
                warmups=warmups,
                timeout_seconds=timeout_seconds,
                process_runner=_make_capture_runner(
                    sp, captures, samples, warmups, process_implementation
                ),
            )
            raw = _attach_raw_bytes(sp, rows, captures)
            report["workloads"][name] = {
                "program_steps": count,
                "catalog": catalog_kind,
                "mode": "check --json" if json_mode else "ordinary check",
                "commands": {side: list(commands[side]) for side in ("baseline", "candidate")},
                "summary": rows["summary"],
                "paired_delta_summary": rows["paired_delta_summary"],
                "paired_comparisons": rows["paired_comparisons"],
                "warmup_order": rows["warmup_order"],
                "paired_order": rows["paired_order"],
                "preflight": rows["preflight"],
                "warmup_observations": rows["warmup_observations"],
                "raw_paired_samples": rows["raw_paired_samples"],
                "raw_invocations": raw["raw_invocations"],
                "oracle": {
                    "ordinary_check_stdout": "empty bytes, derived from src/main.rs check => Ok() branch",
                    "stderr": "empty bytes",
                    "status": 0,
                    "json": "exact execute.check/1 object: validation=static, ok=true, requested path/path_bytes, diagnostics=[]",
                    "effects": "fixture tree byte-identical; forbidden save output absent, checked before/after every invocation",
                    "pair_parity": "stdout and stderr bytes identical across all sides and phases",
                },
            }
            report["completed_cells"].append(name)
            current["state"] = "completed"
            report["current_cell"] = None
            report_writer.persist(report)
        except Exception as error:
            current["state"] = "failed"
            current["error"] = _error_record(error)
            report["status"] = "failed"
            report["error"] = _error_record(error)
            report_writer.persist(report)
            print("Check JSON reuse control failed in " + name + ": " + str(error), file=sys.stderr)
            return 1
    return 0


def artifact_snapshot(sp: Any, args: argparse.Namespace, binaries: Mapping[str, Path], helpers: Mapping[str, Path]) -> Dict[str, Any]:
    fixture_data = fixture_snapshot(sp)
    return {
        "binaries": {side: sp.file_snapshot(path, side + " execute") for side, path in binaries.items()},
        "helpers": {side: sp.file_snapshot(path, side + " execute-applet") for side, path in helpers.items()},
        "source": {
            "baseline": sp.baseline_source_snapshot(args.primary_root, BASELINE_SOURCE_REVISION),
            "candidate": sp.candidate_source_snapshot(args.candidate_root, CANDIDATE_SOURCE_LABEL),
        },
        "candidate_reviewed_patch": sp.file_snapshot(args.candidate_patch, "reviewed candidate patch"),
        "source_pair_harness": sp.file_snapshot(args.harness, "existing source_pair.py harness"),
        "control_driver": sp.file_snapshot(Path(__file__), "paired control driver"),
        "control_tests": sp.file_snapshot(HERE / "test_control.py", "fake-run control tests"),
        "fixture_data": fixture_data,
    }


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--primary-root", type=Path, required=True, help="saved production source checkout")
    parser.add_argument("--candidate-root", type=Path, required=True, help="reviewed candidate source worktree")
    parser.add_argument("--baseline-binary", type=Path, help="saved production release executable")
    parser.add_argument("--candidate-binary", type=Path, help="already-built reviewed candidate executable")
    parser.add_argument("--candidate-patch", type=Path, default=HERE.parent / "evidence" / "check-json-reuse-final.patch")
    parser.add_argument("--harness", type=Path, help="existing source_pair.py; defaults to primary-root/benchmarks/source_pair.py")
    parser.add_argument("--report", type=Path, help="new report destination; it must not exist")
    parser.add_argument("--prepare-fixtures", action="store_true", help="copy and freeze source-derived 75/1 catalogs and owned programs")
    parser.add_argument("--confirm-idle-window", action="store_true", help="root has confirmed all builds/tests/other benchmarks are stopped")
    return parser


def main(argv: Optional[Sequence[str]] = None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)
    report_writer: Optional[IncrementalReport] = None
    report: Optional[Dict[str, Any]] = None
    try:
        if args.prepare_fixtures:
            if args.confirm_idle_window or args.report:
                raise ValueError("fixture preparation does not accept measurement flags")
            args.primary_root = args.primary_root.expanduser().resolve(strict=True)
            print(json.dumps(prepare_fixtures(args.primary_root), indent=2, sort_keys=True))
            return 0
        if not args.confirm_idle_window:
            parser.error("refusing to launch Execute: pass --confirm-idle-window only after the root opens the measurement window")
        args.primary_root = args.primary_root.expanduser().resolve(strict=True)
        args.candidate_root = args.candidate_root.expanduser().resolve(strict=True)
        args.harness = (args.harness or args.primary_root / "benchmarks" / "source_pair.py").expanduser().resolve(strict=True)
        if args.report is None or args.baseline_binary is None or args.candidate_binary is None:
            parser.error("measurement requires --report, --baseline-binary and --candidate-binary")
        sp = _load_source_pair(args.harness)
        report_path = sp.validate_report_path(args.report)
        binaries = {
            "baseline": sp.verify_executable(args.baseline_binary, "baseline Execute binary"),
            "candidate": sp.verify_executable(args.candidate_binary, "candidate Execute binary"),
        }
        helpers = {side: sp.helper_path(binary) for side, binary in binaries.items()}
        expected_hashes = {
            "baseline": (BASELINE_EXECUTE_SHA256, BASELINE_HELPER_SHA256),
            "candidate": (CANDIDATE_EXECUTE_SHA256, CANDIDATE_HELPER_SHA256),
        }
        for side in ("baseline", "candidate"):
            actual = (sp.sha256_file(binaries[side]), sp.sha256_file(helpers[side]))
            if actual != expected_hashes[side]:
                raise ValueError(side + " executable/helper hash differs from preregistered reviewed artifact")
        args.candidate_patch = args.candidate_patch.expanduser().resolve(strict=True)
        if sp.sha256_file(args.candidate_patch) != CANDIDATE_PATCH_SHA256:
            raise ValueError("candidate patch differs from reviewed patch SHA-256")
        before = artifact_snapshot(sp, args, binaries, helpers)
        environment = sp._safe_environment()
        report = {
            "schema": "execute.check-json-reuse-paired-control/1",
            "status": "running",
            "recorded_at_utc": datetime.now(timezone.utc).isoformat(),
            "provenance": {
                "baseline_source_revision": BASELINE_SOURCE_REVISION,
                "baseline_binary": before["binaries"]["baseline"],
                "baseline_helper": before["helpers"]["baseline"],
                "candidate_source_label": CANDIDATE_SOURCE_LABEL,
                "candidate_binary": before["binaries"]["candidate"],
                "candidate_helper": before["helpers"]["candidate"],
                "reviewed_patch": before["candidate_reviewed_patch"],
                "candidate_validation_reported_by_root": "all-native and Windows strict checks plus six check_json tests passed before this control",
            },
            "method": {
                "samples_per_side_per_cell": 48,
                "excluded_warmups_per_side_per_cell": 5,
                "cells": 8,
                "order": "per cell, 48 pairs alternate baseline-first and candidate-first; warmups use the same alternation",
                "process": "fresh bounded Execute process for each check; existing source_pair.run_process controls timeout and records end-to-end wall time",
                "correctness": "independent static JSON field oracle or exact empty ordinary-check stdout oracle; status, stderr, effects checked by source_pair before every phase; raw stdout/stderr bytes retained and exact pair/phase parity enforced",
                "targeted_input": "quoted nonexistent command repeated 1 or 100 times followed by save to forbidden; --check only parses/validates and must not invoke command or save step",
                "scope": "This compares the static check code path with and without JSON reporting. It makes no general performance claim, no statement about runtime applets/model/network, and no inference about CPU or memory from wall time.",
            },
            "environment": sp._environment_report(environment),
            "fingerprints": {"before": before, "after": None, "unchanged": None},
            "workloads": {},
            "completed_cells": [],
            "current_cell": None,
            "report_path": str(report_path),
        }
        # Exclusively create and flush a valid running report before the first Execute invocation.
        report_writer = IncrementalReport(sp, report_path)
        report_writer.persist(report)
        result = _collect_cells(
            sp=sp,
            report=report,
            report_writer=report_writer,
            binaries=binaries,
            environment=environment,
            samples=48,
            warmups=5,
            timeout_seconds=30.0,
        )
        if result:
            return result
        after = artifact_snapshot(sp, args, binaries, helpers)
        sp.assert_artifacts_unchanged(before, after)
        report["fingerprints"] = {"before": before, "after": after, "unchanged": True}
        report["status"] = "completed"
        report["current_cell"] = None
        report_writer.persist(report)
        print("Completed " + str(report_path))
        for name, cell in report["workloads"].items():
            print(name + ": " + json.dumps(cell["summary"], sort_keys=True))
        return 0
    except Exception as error:
        if report_writer is not None and report is not None:
            report["status"] = "failed"
            report["error"] = _error_record(error)
            if isinstance(report.get("current_cell"), dict):
                report["current_cell"]["state"] = "failed"
                report["current_cell"]["error"] = _error_record(error)
            try:
                report_writer.persist(report)
            except Exception as persistence_error:
                print("Could not persist failure update: " + str(persistence_error), file=sys.stderr)
        print("Check JSON reuse control failed: " + str(error), file=sys.stderr)
        return 1
    finally:
        if report_writer is not None:
            report_writer.close()


if __name__ == "__main__":
    raise SystemExit(main())
