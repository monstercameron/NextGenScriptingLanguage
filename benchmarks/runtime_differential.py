#!/usr/bin/env python3
"""Differentially exercise finite runtime semantics with bounded local fixtures.

One fixed case sweep is the default smoke test. Add --duration-seconds to repeat
seeded, deterministically shuffled case pairs as a bounded soak. Every workload
runs in a private temporary cwd and invokes only the supplied Execute binaries
and the five read-only Unix applets named by the explicit manifest directory.
"""
from __future__ import annotations

import argparse
import base64
import datetime as dt
import hashlib
import json
import os
import platform
import random
import re
import resource
import selectors
import signal
import subprocess
import sys
import tempfile
import time
from dataclasses import dataclass
from pathlib import Path, PurePosixPath
from typing import Any

MIB = 1024 * 1024
DEFAULT_OUTPUT_LIMIT = 4 * MIB
HARD_OUTPUT_LIMIT = 64 * MIB
MAX_CASE_INPUT_BYTES = 2 * MIB + 1024
MAX_DURATION_SECONDS = 24 * 60 * 60
MAX_ROUNDS = 1_000
MAX_TIMEOUT_SECONDS = 120.0
MAX_COMMAND_TIMEOUT_MS = 60_000
CAPTURE_PREFIX_BYTES = 64 * 1024
TOOLS = ("cat", "tr", "sort", "wc", "printf")
PREFERRED_BINARIES = {
    "cat": ("/bin/cat", "/usr/bin/cat"),
    "tr": ("/usr/bin/tr", "/bin/tr"),
    "sort": ("/usr/bin/sort", "/bin/sort"),
    "wc": ("/usr/bin/wc", "/bin/wc"),
    "printf": ("/usr/bin/printf", "/bin/printf"),
}
TRUSTED_MANIFEST_SHA256 = {
    "cat": "483117c32690b423ab88e9443cc7ce0787545f07833576d678b1d0472f19ab3a",
    "tr": "b093a6aa91b7657ea4890279f9c3e97e2738114d31f9232ed0501b780ca78c5e",
    "sort": "6d953c33eed06e702c5781290d76929b8e19d62d283b999321e42de19d779b50",
    "wc": "be82bb3ac877a6eb53d3b2c699b1de84dcc6fd1d7cb9b05b784edc45d7786579",
    "printf": "abe61c4481c94396e230406897d253ff8ba0f9c854fb09ecc098b85870b0237e",
}


@dataclass(frozen=True)
class Case:
    case_id: str
    source: str
    inputs: dict[str, bytes]
    output_files: dict[str, bytes]
    expected_stdout: bytes | None = None
    expected_wc_lines: int | None = None


@dataclass
class Observation:
    returncode: int | None
    stdout: bytes
    stderr: bytes
    elapsed_ns: int
    error: str | None
    output_files: dict[str, bytes | None]
    cwd: str
    cleanup_status: str


class RunnerInterrupted(Exception):
    def __init__(self, cleanup_status: str) -> None:
        super().__init__("interrupted while running Execute")
        self.cleanup_status = cleanup_status


def sha256_bytes(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def file_record(data: bytes) -> dict[str, int | str]:
    return {"bytes": len(data), "sha256": sha256_bytes(data)}


def pipe_buf_bytes() -> int:
    read_fd, write_fd = os.pipe()
    try:
        try:
            size = int(os.fpathconf(write_fd, "PC_PIPE_BUF"))
        except (OSError, ValueError):
            size = 4096
    finally:
        os.close(read_fd)
        os.close(write_fd)
    if not 1 <= size <= 64 * 1024:
        raise ValueError(f"unexpected PIPE_BUF size: {size}")
    return size


def binary_payload(size: int, seed: int) -> bytes:
    pattern = bytes((index + seed) & 0xFF for index in range(256))
    return (pattern * ((size + len(pattern) - 1) // len(pattern)))[:size]


def dsl_quote(value: str) -> str:
    return '"' + value.replace("\\", "\\\\").replace('"', '\\"') + '"'


def make_cases(seed: int, pipe_buf: int) -> list[Case]:
    sizes = sorted({0, 1, max(0, pipe_buf - 1), pipe_buf, pipe_buf + 1,
                    MIB - 1, MIB, MIB + 1})
    cases: list[Case] = []
    for size in sizes:
        payload = binary_payload(size, seed)
        label = "empty" if size == 0 else str(size)
        cases.append(Case(
            case_id=f"buffer-cat-{label}",
            source='run cat with files ["payload raw.bin"]\nthen cat\n',
            inputs={"payload raw.bin": payload},
            output_files={},
            expected_stdout=payload,
        ))

    large = binary_payload(MIB + 1, seed ^ 0x4F1BBCDC)
    cases.append(Case(
        case_id="then-cat-1m-plus-1",
        source='read "large payload.bin"\nthen cat\n',
        inputs={"large payload.bin": large},
        output_files={},
        expected_stdout=large,
    ))

    lower = b"abcdefghijklmnopqrstuvwxyz"
    upper = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ"
    translation = bytes.maketrans(lower, upper)
    raw_text = "Résumé Ω\nASCII lower case\n".encode("utf-8") + b"\x00z\xff"
    translated = raw_text.translate(translation)
    cases.append(Case(
        case_id="pipe-tr-binary-utf8",
        source=(
            'read "mixed Ω input.bin"\n'
            'pipe to tr with from "[:lower:]" and to "[:upper:]"\n'
        ),
        inputs={"mixed Ω input.bin": raw_text},
        output_files={},
        expected_stdout=translated,
    ))

    lines = [b"zulu\n", b"alpha\n", b"delta\n", b"bravo\n", b"echo\n"]
    sort_input = b"".join(lines)
    sorted_lines = b"".join(sorted(lines))
    cases.append(Case(
        case_id="pipe-sort-lines",
        source='read "sort input.txt"\npipe to sort\n',
        inputs={"sort input.txt": sort_input},
        output_files={},
        expected_stdout=sorted_lines,
    ))

    wc_input = b"one\ntwo\nthree\nfour\n"
    cases.append(Case(
        case_id="pipe-wc-line-count",
        source='read "line count Ω.txt"\npipe to wc with flags ["-l"]\n',
        inputs={"line count Ω.txt": wc_input},
        output_files={},
        expected_wc_lines=4,
    ))

    unicode_input = "Résumé 🐈\nLF stays\n".encode("utf-8")
    named_output = unicode_input + b"\n"
    named_file = "owned outputs/résultat Ω.txt"
    cases.append(Case(
        case_id="named-utf8-lf-printf-save",
        source=(
            f"read {dsl_quote('unicode input Ω.txt')} as greeting\n"
            "run printf with message greeting\n"
            f"then save to file {dsl_quote(named_file)}\n"
        ),
        inputs={"unicode input Ω.txt": unicode_input},
        output_files={named_file: named_output},
        expected_stdout=named_output,
    ))

    transform_input = b"zebra\nalpha\nBETA\n"
    transformed = transform_input.translate(translation)
    transform_file = "owned outputs/transformed.txt"
    cases.append(Case(
        case_id="then-tr-save-owned-output",
        source=(
            f"read {dsl_quote('text input Ω.txt')}\n"
            'then tr with from "[:lower:]" and to "[:upper:]"\n'
            f"then save to file {dsl_quote(transform_file)}\n"
        ),
        inputs={"text input Ω.txt": transform_input},
        output_files={transform_file: transformed},
        expected_stdout=transformed,
    ))

    printf_text = "Résumé 🐈: deterministic output"
    printf_output = printf_text.encode("utf-8") + b"\n"
    cases.append(Case(
        case_id="printf-unicode-literal",
        source=f"run printf with message {dsl_quote(printf_text)}\n",
        inputs={},
        output_files={},
        expected_stdout=printf_output,
    ))

    for case in cases:
        fixture_size = sum(map(len, case.inputs.values()))
        if fixture_size > MAX_CASE_INPUT_BYTES:
            raise ValueError(f"case fixture exceeds safety limit: {case.case_id}")
        for relative in (*case.inputs.keys(), *case.output_files.keys()):
            safe_relative_path(relative)
    return cases


def safe_relative_path(value: str) -> PurePosixPath:
    path = PurePosixPath(value)
    if path.is_absolute() or not path.parts or any(part in ("", ".", "..") for part in path.parts):
        raise ValueError(f"fixture path must remain inside its temporary cwd: {value!r}")
    return path


def inspect_manifests(manifest_dir: Path) -> tuple[dict[str, str], dict[str, dict[str, str]]]:
    manifest_hashes = {
        path.name: sha256_file(path)
        for path in sorted(manifest_dir.glob("*.json"))
        if path.is_file()
    }
    applets: dict[str, dict[str, str]] = {}
    for name in TOOLS:
        manifest_path = manifest_dir / f"{name}.json"
        if not manifest_path.is_file():
            raise ValueError(f"manifest directory is missing {manifest_path.name}")
        if manifest_hashes.get(manifest_path.name) != TRUSTED_MANIFEST_SHA256[name]:
            raise ValueError(
                f"{manifest_path.name} differs from the reviewed read-only fixture"
            )
        manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
        configured = manifest.get("binary")
        if not isinstance(configured, str) or Path(configured).name != name:
            raise ValueError(f"{manifest_path.name} must resolve the real {name} binary")
        trusted = {
            Path(candidate).resolve(strict=True)
            for candidate in PREFERRED_BINARIES[name]
            if Path(candidate).is_file() and os.access(candidate, os.X_OK)
        }
        if "/" in configured:
            configured_path = Path(configured)
            if not configured_path.is_absolute():
                raise ValueError(f"{manifest_path.name} must use a system applet path")
            resolved = configured_path.resolve(strict=True)
            if resolved not in trusted:
                raise ValueError(f"{manifest_path.name} is not the expected system {name} binary")
        else:
            resolved = next(iter(sorted(trusted)), None)
        if resolved is None or not resolved.is_file() or not os.access(resolved, os.X_OK):
            raise ValueError(f"could not resolve trusted executable for {name}: {configured!r}")
        applets[name] = {
            "manifest_binary": configured,
            "path": str(resolved),
            "sha256": sha256_file(resolved),
        }
    return manifest_hashes, applets


def controlled_applet_path(root: Path, applets: dict[str, dict[str, str]]) -> Path:
    directory = root / "read-only-applets"
    directory.mkdir(mode=0o700)
    for name in TOOLS:
        (directory / name).symlink_to(applets[name]["path"])
    return directory


def clean_environment(path: Path, cwd: Path) -> dict[str, str]:
    return {
        "PATH": str(path),
        "LC_ALL": "C",
        "LANG": "C",
        "TMPDIR": str(cwd),
        "HOME": str(cwd),
    }


def stop_process_tree(process: subprocess.Popen[bytes]) -> str:
    """Cooperatively stop Execute without signaling cached descendant IDs."""
    sent_term = False
    try:
        if process.poll() is None:
            process.send_signal(signal.SIGTERM)
            sent_term = True
    except ProcessLookupError:
        pass
    try:
        process.wait(timeout=0.5)
        if sent_term and process.returncode != -signal.SIGTERM:
            cleanup_status = (
                "cooperative SIGTERM; Execute exited through runtime-managed child cleanup"
            )
        else:
            cleanup_status = (
                "incomplete: cooperative shutdown was not confirmed; descendant cleanup unverified"
            )
    except subprocess.TimeoutExpired:
        forced = False
        try:
            if process.poll() is None:
                process.kill()
                forced = True
            process.wait(timeout=2.0)
        except subprocess.TimeoutExpired:
            cleanup_status = "incomplete: Execute could not be reaped within 2 seconds"
        except ProcessLookupError:
            try:
                process.wait(timeout=2.0)
                cleanup_status = "cooperative termination; Execute exited before forced kill"
            except subprocess.TimeoutExpired:
                cleanup_status = "incomplete: Execute could not be reaped within 2 seconds"
        else:
            cleanup_status = "incomplete: forced Execute kill; applet cleanup is unverified" if forced else (
                "cooperative termination; Execute exited before forced kill"
            )
    for stream in (process.stdin, process.stdout, process.stderr):
        if stream is not None:
            try:
                stream.close()
            except OSError:
                pass
    return cleanup_status


def bounded_command(
    command: list[str],
    cwd: Path,
    environment: dict[str, str],
    timeout_seconds: float,
    output_limit: int,
) -> tuple[int | None, bytes, bytes, int, str | None, str]:
    def limit_child_file_size() -> None:
        soft_limit, hard_limit = resource.getrlimit(resource.RLIMIT_FSIZE)
        limits = [output_limit]
        if soft_limit != resource.RLIM_INFINITY:
            limits.append(soft_limit)
        if hard_limit != resource.RLIM_INFINITY:
            limits.append(hard_limit)
        child_soft_limit = min(limits)
        resource.setrlimit(resource.RLIMIT_FSIZE, (child_soft_limit, hard_limit))

    started = time.perf_counter_ns()
    process = subprocess.Popen(
        command,
        cwd=cwd,
        env=environment,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        bufsize=0,
        start_new_session=True,
        preexec_fn=limit_child_file_size,
    )
    stdout = bytearray()
    stderr = bytearray()
    selector = selectors.DefaultSelector()
    try:
        assert process.stdout is not None and process.stderr is not None
        selector.register(process.stdout, selectors.EVENT_READ, stdout)
        selector.register(process.stderr, selectors.EVENT_READ, stderr)
    except BaseException as error:
        selector.close()
        cleanup_status = stop_process_tree(process)
        if isinstance(error, KeyboardInterrupt):
            raise RunnerInterrupted(cleanup_status) from None
        raise
    deadline = time.monotonic() + timeout_seconds
    error: str | None = None
    try:
        while selector.get_map():
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                error = f"outer deadline exceeded ({timeout_seconds:g}s)"
                break
            ready = selector.select(min(remaining, 0.25))
            for key, _ in ready:
                target: bytearray = key.data
                remaining_capacity = output_limit - len(stdout) - len(stderr)
                chunk = os.read(key.fd, min(64 * 1024, remaining_capacity + 1))
                if not chunk:
                    selector.unregister(key.fileobj)
                    key.fileobj.close()
                    continue
                target.extend(chunk)
                if len(stdout) + len(stderr) > output_limit:
                    error = f"combined stdout/stderr exceeded {output_limit} bytes"
                    break
            if error:
                break
        if error is None:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                error = f"outer deadline exceeded ({timeout_seconds:g}s)"
            else:
                try:
                    process.wait(timeout=remaining)
                except subprocess.TimeoutExpired:
                    error = f"outer deadline exceeded ({timeout_seconds:g}s)"
    except BaseException as error:
        selector.close()
        cleanup_status = stop_process_tree(process)
        if isinstance(error, KeyboardInterrupt):
            raise RunnerInterrupted(cleanup_status) from None
        raise
    selector.close()
    cleanup_status = "not needed"
    if error is not None:
        cleanup_status = stop_process_tree(process)
    returncode = process.poll()
    elapsed_ns = time.perf_counter_ns() - started
    return returncode, bytes(stdout), bytes(stderr), elapsed_ns, error, cleanup_status


def run_case(
    binary: Path,
    manifest_dir: Path,
    applet_path: Path,
    scratch: Path,
    case: Case,
    args: argparse.Namespace,
    side: str,
) -> Observation:
    with tempfile.TemporaryDirectory(
        prefix=f"{side} {case.case_id} Ω ", dir=scratch
    ) as temporary:
        cwd = Path(temporary)
        program = cwd / f"{case.case_id}.program"
        program.write_text(case.source, encoding="utf-8")
        for name, data in case.inputs.items():
            path = cwd.joinpath(*safe_relative_path(name).parts)
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(data)
        for name in case.output_files:
            path = cwd.joinpath(*safe_relative_path(name).parts)
            path.parent.mkdir(parents=True, exist_ok=True)
        command = [
            str(binary), "run", str(program), "--manifest-dir", str(manifest_dir),
            "--command-timeout-ms", str(args.command_timeout_ms),
            "--max-output-bytes", str(args.max_output_bytes),
        ]
        environment = clean_environment(applet_path, cwd)
        returncode, stdout, stderr, elapsed_ns, error, cleanup_status = bounded_command(
            command, cwd, environment, args.timeout_seconds, args.max_output_bytes
        )
        results: dict[str, bytes | None] = {}
        for name in case.output_files:
            path = cwd.joinpath(*safe_relative_path(name).parts)
            if path.is_file() and not path.is_symlink():
                try:
                    resolved = path.resolve(strict=True)
                    contained = resolved.is_relative_to(cwd.resolve())
                    size = resolved.stat().st_size
                    results[name] = (
                        resolved.read_bytes()
                        if contained and size <= args.max_output_bytes else None
                    )
                except (OSError, RuntimeError):
                    results[name] = None
            else:
                results[name] = None
        return Observation(
            returncode, stdout, stderr, elapsed_ns, error, results, str(cwd), cleanup_status
        )


def check_expected(case: Case, observation: Observation) -> str | None:
    if observation.error:
        return observation.error
    if observation.returncode != 0:
        return f"Execute exited with status {observation.returncode}"
    if case.expected_stdout is not None and observation.stdout != case.expected_stdout:
        return "stdout differs from generated expected bytes"
    if case.expected_wc_lines is not None:
        match = re.fullmatch(rb"\s*(\d+)\s*", observation.stdout)
        if not match or int(match.group(1)) != case.expected_wc_lines:
            return f"wc -l did not report {case.expected_wc_lines} lines"
    for path, expected in case.output_files.items():
        if observation.output_files.get(path) != expected:
            return f"saved output differs or is missing: {path}"
    return None


def failure_observation(observation: Observation) -> dict[str, Any]:
    return {
        "cwd": observation.cwd,
        "returncode": observation.returncode,
        "elapsed_ns": observation.elapsed_ns,
        "error": observation.error,
        "cleanup_status": observation.cleanup_status,
        "stdout": {
            **file_record(observation.stdout),
            "prefix_base64": base64.b64encode(observation.stdout[:CAPTURE_PREFIX_BYTES]).decode("ascii"),
            "prefix_truncated": len(observation.stdout) > CAPTURE_PREFIX_BYTES,
        },
        "stderr": {
            **file_record(observation.stderr),
            "prefix_base64": base64.b64encode(observation.stderr[:CAPTURE_PREFIX_BYTES]).decode("ascii"),
            "prefix_truncated": len(observation.stderr) > CAPTURE_PREFIX_BYTES,
        },
        "output_files": {
            name: None if data is None else {
                **file_record(data),
                "prefix_base64": base64.b64encode(data[:CAPTURE_PREFIX_BYTES]).decode("ascii"),
                "prefix_truncated": len(data) > CAPTURE_PREFIX_BYTES,
            }
            for name, data in sorted(observation.output_files.items())
        },
    }


def failure_artifact(
    round_index: int,
    case: Case,
    reason: str,
    observations: dict[str, Observation],
) -> dict[str, Any]:
    return {
        "round": round_index,
        "case_id": case.case_id,
        "seed_inputs": {
            name: {
                **file_record(data),
                "base64": base64.b64encode(data).decode("ascii"),
            }
            for name, data in sorted(case.inputs.items())
        },
        "program": case.source,
        "reason": reason,
        "observations": {
            side: failure_observation(observation)
            for side, observation in observations.items()
        },
    }


def case_description(case: Case) -> dict[str, Any]:
    return {
        "program": case.source,
        "inputs": {name: file_record(data) for name, data in sorted(case.inputs.items())},
        "expected_stdout": None if case.expected_stdout is None else file_record(case.expected_stdout),
        "expected_wc_lines": case.expected_wc_lines,
        "expected_output_files": {
            name: file_record(data) for name, data in sorted(case.output_files.items())
        },
    }


def compare_observations(left: Observation, right: Observation) -> bool:
    return (
        left.returncode == right.returncode
        and left.stdout == right.stdout
        and left.output_files == right.output_files
    )


def validate_args(parser: argparse.ArgumentParser, args: argparse.Namespace) -> None:
    if args.seed < 0:
        parser.error("--seed must be non-negative")
    if args.duration_seconds is not None and not (0 < args.duration_seconds <= MAX_DURATION_SECONDS):
        parser.error(f"--duration-seconds must be in (0, {MAX_DURATION_SECONDS}]")
    if not 1 <= args.max_rounds <= MAX_ROUNDS:
        parser.error(f"--max-rounds must be in 1..{MAX_ROUNDS}")
    if not 0 <= args.round_pause_seconds <= 60:
        parser.error("--round-pause-seconds must be in 0..60")
    if args.duration_seconds is None and args.round_pause_seconds > 0:
        parser.error("--round-pause-seconds requires --duration-seconds")
    if not 0.1 <= args.timeout_seconds <= MAX_TIMEOUT_SECONDS:
        parser.error(f"--timeout-seconds must be in [0.1, {MAX_TIMEOUT_SECONDS:g}]")
    if not 1 <= args.command_timeout_ms <= MAX_COMMAND_TIMEOUT_MS:
        parser.error(f"--command-timeout-ms must be in 1..{MAX_COMMAND_TIMEOUT_MS}")
    if not MIB + 2 <= args.max_output_bytes <= HARD_OUTPUT_LIMIT:
        parser.error(f"--max-output-bytes must be in {MIB + 2}..{HARD_OUTPUT_LIMIT}")


def write_report(report: dict[str, Any], output: Path | None) -> None:
    text = json.dumps(report, ensure_ascii=False, indent=2, sort_keys=True) + "\n"
    if output is None:
        sys.stdout.write(text)
        return
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(text, encoding="utf-8")


def main() -> int:
    def handle_sigterm(_signum: int, _frame: Any) -> None:
        raise KeyboardInterrupt

    signal.signal(signal.SIGTERM, handle_sigterm)
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--baseline", required=True, type=Path, help="baseline Execute binary")
    parser.add_argument("--candidate", required=True, type=Path, help="candidate Execute binary")
    parser.add_argument(
        "--manifest-dir", required=True, type=Path,
        help="directory containing the exact reviewed project applet manifests",
    )
    parser.add_argument("--seed", type=int, default=20261013)
    parser.add_argument("--duration-seconds", type=float, help="repeat fixed cases until this bounded soak duration")
    parser.add_argument("--max-rounds", type=int, default=MAX_ROUNDS, help="hard round cap for soak mode")
    parser.add_argument(
        "--round-pause-seconds", type=float, default=0.0,
        help="idle between rounds in soak mode (0..60 seconds)",
    )
    parser.add_argument("--timeout-seconds", type=float, default=12.0, help="outer deadline per Execute process")
    parser.add_argument("--command-timeout-ms", type=int, default=8_000, help="inner native pipeline deadline")
    parser.add_argument("--max-output-bytes", type=int, default=DEFAULT_OUTPUT_LIMIT, help="per-process combined stdout/stderr capture cap")
    parser.add_argument("--output", type=Path, help="optional JSON report path; defaults to stdout")
    args = parser.parse_args()
    validate_args(parser, args)

    report: dict[str, Any] = {
        "schema": "runtime-differential-v1",
        "status": "starting",
        "observed_at": dt.datetime.now(dt.timezone.utc).isoformat(),
        "platform": platform.platform(),
        "python": platform.python_version(),
        "seed": args.seed,
        "duration_seconds": args.duration_seconds,
        "max_rounds": args.max_rounds,
        "round_pause_seconds": args.round_pause_seconds,
        "method": {
            "mode": "fixed-case smoke" if args.duration_seconds is None else "bounded seeded soak",
            "pairs": "baseline/candidate order is deterministically shuffled per case and round",
            "timing": "fresh Execute process wall nanoseconds; fixture preparation and correctness checks excluded",
            "correctness": "requires success, generated stdout/result-file expectations, and exact baseline/candidate stdout/result-file equality; stderr equality intentionally excluded",
            "safety": "private Unicode/space temporary cwd; exact reviewed project manifests and system applets; strict child environment; command, output, file-size and fixture caps; cooperative Execute cleanup with forced-kill cases reported as unverified",
        },
        "binaries": {},
        "manifest_dir": None,
        "manifest_sha256": {},
        "applets": {},
        "pipe_buf_bytes": None,
        "cases": {},
        "raw_results": {},
        "completed_rounds": 0,
        "completed_pairs": 0,
        "first_failure": None,
    }

    try:
        binaries = {
            "baseline": args.baseline.resolve(strict=True),
            "candidate": args.candidate.resolve(strict=True),
        }
        for name, path in binaries.items():
            if not path.is_file() or not os.access(path, os.X_OK):
                raise ValueError(f"{name} is not an executable file: {path}")
            report["binaries"][name] = {"path": str(path), "sha256": sha256_file(path)}
        manifest_dir = args.manifest_dir.resolve(strict=True)
        if not manifest_dir.is_dir():
            raise ValueError(f"manifest directory is not a directory: {manifest_dir}")
        report["manifest_dir"] = str(manifest_dir)
        manifest_hashes, applets = inspect_manifests(manifest_dir)
        report["manifest_sha256"] = manifest_hashes
        report["applets"] = applets
        pipe_buf = pipe_buf_bytes()
        report["pipe_buf_bytes"] = pipe_buf
        cases = make_cases(args.seed, pipe_buf)
        report["cases"] = {case.case_id: case_description(case) for case in cases}
        report["harness_sha256"] = sha256_file(Path(__file__))

        rng = random.Random(args.seed ^ 0xD1FF3E71)
        stop_after = 1 if args.duration_seconds is None else args.max_rounds
        soak_deadline = None if args.duration_seconds is None else time.monotonic() + args.duration_seconds
        with tempfile.TemporaryDirectory(prefix="runtime differential Ω ") as temporary:
            scratch = Path(temporary)
            applet_path = controlled_applet_path(scratch, applets)
            failure = False
            for round_index in range(stop_after):
                if soak_deadline is not None and round_index > 0:
                    remaining_soak = soak_deadline - time.monotonic()
                    if remaining_soak <= 0:
                        break
                    if args.round_pause_seconds > 0:
                        time.sleep(min(args.round_pause_seconds, remaining_soak))
                    if time.monotonic() >= soak_deadline:
                        break
                round_complete = True
                for case in cases:
                    if soak_deadline is not None and report["completed_pairs"] > 0 and time.monotonic() >= soak_deadline:
                        round_complete = False
                        break
                    side_order = ["baseline", "candidate"]
                    rng.shuffle(side_order)
                    observations: dict[str, Observation] = {}
                    for side in side_order:
                        observation = run_case(
                            binaries[side], manifest_dir, applet_path, scratch,
                            case, args, side,
                        )
                        observations[side] = observation
                        samples = report["raw_results"].setdefault(
                            case.case_id,
                            {
                                name: {
                                    "rounds": [],
                                    "elapsed_ns": [],
                                    "returncodes": [],
                                    "stdout_sha256": [],
                                    "stderr_sha256": [],
                                    "output_files_sha256": {
                                        output: [] for output in case.output_files
                                    },
                                }
                                for name in ("baseline", "candidate")
                            },
                        )[side]
                        samples["rounds"].append(round_index)
                        samples["elapsed_ns"].append(observation.elapsed_ns)
                        samples["returncodes"].append(observation.returncode)
                        samples["stdout_sha256"].append(sha256_bytes(observation.stdout))
                        samples["stderr_sha256"].append(sha256_bytes(observation.stderr))
                        for output, data in observation.output_files.items():
                            samples["output_files_sha256"][output].append(
                                None if data is None else sha256_bytes(data)
                            )
                        reason = check_expected(case, observation)
                        if reason:
                            report["first_failure"] = failure_artifact(
                                round_index, case, f"{side}: {reason}", observations
                            )
                            failure = True
                            break
                    if failure:
                        break
                    baseline = observations["baseline"]
                    candidate = observations["candidate"]
                    if not compare_observations(baseline, candidate):
                        report["first_failure"] = failure_artifact(
                            round_index,
                            case,
                            "baseline/candidate exit, stdout, or saved result files differ",
                            observations,
                        )
                        failure = True
                        break
                    report["completed_pairs"] += 1
                if failure:
                    break
                if round_complete:
                    report["completed_rounds"] += 1
                if soak_deadline is not None and time.monotonic() >= soak_deadline:
                    break
            report["status"] = "failed" if failure else "passed"
    except RunnerInterrupted as error:
        report["status"] = "interrupted"
        report["first_failure"] = report["first_failure"] or {
            "reason": "keyboard interrupt",
            "cleanup_status": error.cleanup_status,
        }
    except KeyboardInterrupt:
        report["status"] = "interrupted"
        report["first_failure"] = report["first_failure"] or {
            "reason": "keyboard interrupt",
            "cleanup_status": "no active Execute process was being cleaned up",
        }
    except BaseException as error:
        report["status"] = "failed"
        if report["first_failure"] is None:
            report["first_failure"] = {
                "reason": f"harness error: {type(error).__name__}: {error}"
            }
    finally:
        try:
            write_report(report, args.output)
        except OSError as error:
            print(f"could not write differential report: {error}", file=sys.stderr)
            return 2
    return 0 if report["status"] == "passed" else 130 if report["status"] == "interrupted" else 1


if __name__ == "__main__":
    raise SystemExit(main())
