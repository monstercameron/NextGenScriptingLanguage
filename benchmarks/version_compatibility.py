#!/usr/bin/env python3
"""Bounded local cache/checkpoint compatibility smoke test for two Execute builds.

Only fixed workflows, private temporary directories, and the four absolute
system applets below are used. This is a targeted compatibility check, not a
general cross-version support claim.
"""
from __future__ import annotations

import argparse
import base64
import hashlib
import json
import os
import platform
import selectors
import signal
import subprocess
import sys
import tempfile
import time
from pathlib import Path
from typing import Any

APPLET_PATHS = {
    "printf": Path("/usr/bin/printf"),
    "tr": Path("/usr/bin/tr"),
    "false": Path("/usr/bin/false"),
    "true": Path("/usr/bin/true"),
}
MAX_CAPTURE = 128 * 1024
MAX_FILE = 1024 * 1024
MAX_FILES = 32
MAX_BINARY = 256 * 1024 * 1024
MAX_TIMEOUT = 30.0


def digest(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def file_record(data: bytes) -> dict[str, Any]:
    return {"bytes": len(data), "sha256": digest(data),
            "base64": base64.b64encode(data).decode("ascii")}


def sha256_file(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(32768), b""):
            h.update(chunk)
    return h.hexdigest()


def close_streams(process: subprocess.Popen[bytes]) -> None:
    for stream in (process.stdin, process.stdout, process.stderr):
        if stream is not None:
            try:
                stream.close()
            except OSError:
                pass


def child_file_limit() -> None:
    import resource
    soft, hard = resource.getrlimit(resource.RLIMIT_FSIZE)
    limits = [MAX_FILE]
    if soft != resource.RLIM_INFINITY:
        limits.append(soft)
    if hard != resource.RLIM_INFINITY:
        limits.append(hard)
    limit = min(limits)
    resource.setrlimit(resource.RLIMIT_FSIZE, (limit, hard))


def stop_runner(process: subprocess.Popen[bytes]) -> str:
    """Signal only the still-owned runner PID; never scan or signal reused PIDs."""
    if process.poll() is not None:
        return "runner already exited; descendant cleanup unverified"
    try:
        process.send_signal(signal.SIGTERM)
    except ProcessLookupError:
        pass
    try:
        process.wait(timeout=0.75)
        return "cooperative SIGTERM; runtime-managed descendant cleanup not independently verified"
    except subprocess.TimeoutExpired:
        pass
    if process.poll() is None:
        try:
            process.kill()
        except ProcessLookupError:
            pass
    try:
        process.wait(timeout=1.5)
        return "forced runner kill; applet/descendant cleanup unverified"
    except subprocess.TimeoutExpired:
        return "runner still unreaped after bounded kill wait; descendant cleanup unverified"


def run_bounded(argv: list[str], cwd: Path, env: dict[str, str], timeout: float) -> dict[str, Any]:
    process = subprocess.Popen(
        argv, cwd=cwd, env=env, stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, bufsize=0,
        start_new_session=True, preexec_fn=child_file_limit,
    )
    out, err = bytearray(), bytearray()
    selector = selectors.DefaultSelector()
    deadline = time.monotonic() + timeout
    error: str | None = None
    try:
        assert process.stdout is not None and process.stderr is not None
        selector.register(process.stdout, selectors.EVENT_READ, out)
        selector.register(process.stderr, selectors.EVENT_READ, err)
        while selector.get_map():
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                error = f"outer deadline exceeded ({timeout:g}s)"
                break
            for key, _ in selector.select(min(remaining, 0.2)):
                remaining_capacity = MAX_CAPTURE - len(out) - len(err)
                chunk = os.read(key.fd, min(32768, remaining_capacity + 1))
                if not chunk:
                    selector.unregister(key.fileobj)
                    key.fileobj.close()
                    continue
                key.data.extend(chunk)
                if len(out) + len(err) > MAX_CAPTURE:
                    error = f"combined stdout/stderr exceeded {MAX_CAPTURE} bytes"
                    break
            if error:
                break
        if error is None:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                error = f"outer deadline exceeded ({timeout:g}s)"
            else:
                try:
                    process.wait(timeout=remaining)
                except subprocess.TimeoutExpired:
                    error = f"outer deadline exceeded ({timeout:g}s)"
    except BaseException:
        stop_runner(process)
        close_streams(process)
        raise
    finally:
        selector.close()
    cleanup = "not needed" if error is None else stop_runner(process)
    close_streams(process)
    stderr_text = bytes(err).decode("utf-8", errors="replace")
    events = []
    for line in stderr_text.splitlines():
        try:
            value = json.loads(line)
        except json.JSONDecodeError:
            continue
        if isinstance(value, dict) and isinstance(value.get("event"), str):
            events.append(value)
    return {
        "argv": argv,
        "returncode": process.poll(),
        "error": error,
        "cleanup": cleanup,
        "stdout": file_record(bytes(out)),
        "stderr": file_record(bytes(err)),
        "trace_events": events,
    }


def artifact_hashes(directory: Path) -> dict[str, dict[str, Any]]:
    found: dict[str, dict[str, Any]] = {}
    for current, dirs, files in os.walk(directory, followlinks=False):
        base = Path(current)
        for name in dirs:
            path = base / name
            if path.is_symlink() or not path.is_dir():
                raise RuntimeError(f"unexpected symlink/non-directory artifact: {path}")
        for name in files:
            path = base / name
            if path.is_symlink() or not path.is_file():
                raise RuntimeError(f"unexpected symlink/non-file artifact: {path}")
            size = path.stat().st_size
            relative = str(path.relative_to(directory))
            if size > MAX_FILE or len(found) >= MAX_FILES:
                raise RuntimeError(f"artifact limit exceeded at {relative}")
            found[relative] = {"bytes": size, "sha256": sha256_file(path)}
    return dict(sorted(found.items()))


def read_output(path: Path) -> bytes:
    if path.is_symlink() or not path.is_file():
        raise RuntimeError(f"expected regular result file is missing: {path.name}")
    if path.stat().st_size > MAX_FILE:
        raise RuntimeError(f"result file exceeds {MAX_FILE} bytes: {path.name}")
    return path.read_bytes()


def make_catalog(directory: Path) -> dict[str, dict[str, str]]:
    directory.mkdir(mode=0o700)
    specifications = {
        "printf": {"name": "printf", "parameters": {"message": {"type": "string", "required": True}},
                   "arguments": [{"literal": "%s\\n"}, {"parameter": "message"}]},
        "tr": {"name": "tr", "accepts_stdin": True, "cacheable": True,
               "parameters": {"from": {"type": "string", "required": True},
                              "to": {"type": "string", "required": True}},
               "arguments": [{"parameter": "from"}, {"parameter": "to"}]},
        "false": {"name": "false", "arguments": []},
        "true": {"name": "true", "arguments": []},
    }
    result = {}
    for name, path in APPLET_PATHS.items():
        if not path.is_absolute() or path.is_symlink() or not path.is_file() or not os.access(path, os.X_OK):
            raise RuntimeError(f"required absolute system applet unavailable: {path}")
        spec = {"binary": str(path), **specifications[name]}
        encoded = (json.dumps(spec, sort_keys=True, separators=(",", ":")) + "\n").encode()
        target = directory / f"{name}.json"
        target.write_bytes(encoded)
        result[name] = {"path": str(path), "sha256": sha256_file(path),
                        "manifest_sha256": digest(encoded)}
    return result


def cli(binary: Path, command: str, program: Path, catalog: Path,
        env: dict[str, str], timeout: float, *options: str) -> dict[str, Any]:
    argv = [str(binary), command, str(program), "--manifest-dir", str(catalog),
            "--command-timeout-ms", "2000", "--max-output-bytes", "65536",
            "--trace", *options]
    return run_bounded(argv, program.parent, env, timeout)


def require_success(result: dict[str, Any], expected: bytes, label: str) -> None:
    actual = base64.b64decode(result["stdout"]["base64"])
    if result["error"] or result["returncode"] != 0 or actual != expected:
        raise RuntimeError(f"{label}: expected success and exact stdout; got {result}")


def require_known_false(result: dict[str, Any], label: str) -> None:
    events = result["trace_events"]
    committed = any(e.get("event") == "checkpoint_commit" and e.get("completed_nodes") == 1 for e in events)
    false_failed = any(e.get("event") == "exit" and e.get("applet") == "false"
                       and e.get("status") == 1 and e.get("line") == 2 for e in events)
    if result["error"] or result["returncode"] in (None, 0) or not committed or not false_failed:
        raise RuntimeError(f"{label}: did not record one committed prefix and known false failure: {result}")


def require_resumed(result: dict[str, Any], expected: bytes, label: str) -> None:
    events = result["trace_events"]
    resumed = any(e.get("event") == "checkpoint_resume" and e.get("restored_nodes") == 1
                  and e.get("next_step") == 1 for e in events)
    suffix_spawn = any(e.get("event") == "spawn" and e.get("applet") == "printf"
                       and e.get("line") == 2 for e in events)
    prefix_ran = any(e.get("event") in ("spawn", "exit") and e.get("line") == 1 for e in events)
    actual = base64.b64decode(result["stdout"]["base64"])
    if (result["error"] or result["returncode"] != 0 or actual != expected
            or not resumed or not suffix_spawn or prefix_ran):
        raise RuntimeError(f"{label}: repaired suffix/binding resume mismatch: {result}")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--baseline", required=True, type=Path, help="explicit original 0.4.6 Execute binary")
    parser.add_argument("--candidate", required=True, type=Path, help="explicit candidate Execute binary")
    parser.add_argument("--timeout-seconds", type=float, default=8.0)
    args = parser.parse_args()
    if not 1.0 <= args.timeout_seconds <= MAX_TIMEOUT:
        parser.error(f"--timeout-seconds must be in [1, {MAX_TIMEOUT:g}]")

    report: dict[str, Any] = {
        "schema": "version-compatibility-v1", "status": "starting",
        "platform": platform.platform(), "python": platform.python_version(),
        "method": "one explicit baseline/candidate pair; private temp cwd; fixed absolute applets; no timing claims",
        "limits": {"outer_seconds_per_process": args.timeout_seconds,
                   "native_command_ms": 2000, "combined_output_bytes": MAX_CAPTURE,
                   "regular_file_bytes": MAX_FILE, "artifact_file_count": MAX_FILES,
                   "binary_bytes": MAX_BINARY,
                   "timeout_cleanup": "SIGTERM runner, bounded wait, then kill runner only; descendant cleanup is reported unverified"},
        "binaries": {}, "applets": {}, "environment": {}, "cases": {},
        "harness_sha256": sha256_file(Path(__file__)),
    }
    try:
        binaries = {"baseline": args.baseline.resolve(strict=True),
                    "candidate": args.candidate.resolve(strict=True)}
        for name, path in binaries.items():
            if (not path.is_file() or path.stat().st_size > MAX_BINARY
                    or not os.access(path, os.X_OK)):
                raise RuntimeError(f"{name} binary is not executable: {path}")
            report["binaries"][name] = {"path": str(path), "sha256": sha256_file(path)}
        if binaries["baseline"] == binaries["candidate"]:
            raise RuntimeError("baseline and candidate resolve to the same executable path")
        if report["binaries"]["baseline"]["sha256"] == report["binaries"]["candidate"]["sha256"]:
            raise RuntimeError("baseline and candidate hashes are identical; cross-version check would be inconclusive")
        with tempfile.TemporaryDirectory(prefix="flow-version-compat-") as temporary:
            root = Path(temporary)
            catalog = root / "catalog"
            report["applets"] = make_catalog(catalog)

            cache_cwd = root / "cache-case"
            cache_cwd.mkdir()
            empty_path = root / "empty-path"
            empty_path.mkdir()
            env = {"PATH": str(empty_path), "LC_ALL": "C", "LANG": "C",
                   "HOME": str(cache_cwd), "TMPDIR": str(cache_cwd)}
            # Eager CoreFoundation adds this variable on macOS; lazy loading
            # does not. Supply it explicitly so both cache keys see identical
            # effective environments, rather than weakening runtime identity.
            if sys.platform == "darwin":
                env["__CF_USER_TEXT_ENCODING"] = f"0x{os.getuid():X}:0x0:0x0"
            report["environment"] = env.copy()
            cache_source = ('read "cache-input.txt"\n'
                            'then tr with from "[:lower:]" and to "[:upper:]" and cache\n'
                            'then save to file "cache-result.txt"\n')
            cache_program = cache_cwd / "cache.program"
            cache_input = b"small-cache seed\n"
            cache_expected = b"SMALL-CACHE SEED\n"
            cache_program.write_text(cache_source, encoding="utf-8")
            (cache_cwd / "cache-input.txt").write_bytes(cache_input)
            (cache_cwd / "cache").mkdir()
            cache_args = ("--cache-dir", str(cache_cwd / "cache"))
            cache_case = report["cases"].setdefault("baseline-cache-to-candidate", {
                "program": cache_source, "input": file_record(cache_input),
                "environment": env.copy(),
                "expected_stdout_and_saved_file": file_record(cache_expected),
            })
            first = cli(binaries["baseline"], "run", cache_program, catalog, env,
                        args.timeout_seconds, *cache_args)
            cache_case["baseline"] = first
            require_success(first, cache_expected, "baseline cold cache")
            first["saved_file"] = file_record(read_output(cache_cwd / "cache-result.txt"))
            if first["saved_file"]["sha256"] != digest(cache_expected):
                raise RuntimeError("baseline cold cache saved unexpected bytes")
            first_cache = artifact_hashes(cache_cwd / "cache")
            if not any(e.get("event") == "cache_store" and e.get("applet") == "tr"
                       for e in first["trace_events"]):
                raise RuntimeError("baseline did not store the expected pure tr result")

            (cache_cwd / "cache-result.txt").unlink()
            second = cli(binaries["candidate"], "run", cache_program, catalog, env,
                         args.timeout_seconds, *cache_args)
            cache_case["candidate"] = second
            require_success(second, cache_expected, "candidate warm cache")
            second["saved_file"] = file_record(read_output(cache_cwd / "cache-result.txt"))
            second_cache = artifact_hashes(cache_cwd / "cache")
            if (not any(e.get("event") == "cache_hit" and e.get("applet") == "tr"
                        for e in second["trace_events"])
                    or any(e.get("event") == "spawn" and e.get("applet") == "tr"
                           for e in second["trace_events"])
                    or first_cache != second_cache
                    or second["saved_file"]["sha256"] != digest(cache_expected)):
                raise RuntimeError("candidate did not consume the baseline cache identically")
            cache_case["cache_files_after_baseline"] = first_cache
            cache_case["cache_files_after_candidate"] = second_cache

            prefix = 'run printf with message "kept prefix" as remembered\n'
            failure = prefix + "then false\n"
            repaired = prefix + "then printf with message remembered\nthen save to file \"resumed output.txt\"\n"
            resume_expected = b"kept prefix\n\n"
            for label, producer, consumer in (
                ("baseline-checkpoint-to-candidate", binaries["baseline"], binaries["candidate"]),
                ("candidate-checkpoint-to-baseline", binaries["candidate"], binaries["baseline"]),
            ):
                case_cwd = root / label
                case_cwd.mkdir()
                case_env = {"PATH": str(empty_path), "LC_ALL": "C", "LANG": "C",
                            "HOME": str(case_cwd), "TMPDIR": str(case_cwd)}
                if sys.platform == "darwin":
                    case_env["__CF_USER_TEXT_ENCODING"] = f"0x{os.getuid():X}:0x0:0x0"
                program = case_cwd / "resume.program"
                checkpoint = case_cwd / "checkpoint"
                program.write_text(failure, encoding="utf-8")
                resume_case = report["cases"].setdefault(label, {
                    "failed_program": failure, "repaired_program": repaired,
                    "environment": case_env.copy(),
                    "expected_resumed_stdout_and_file": file_record(resume_expected),
                    "retry_uncertain_passed": False,
                })
                initial = cli(producer, "run", program, catalog, case_env,
                              args.timeout_seconds, "--checkpoint", str(checkpoint))
                resume_case["producer"] = initial
                require_known_false(initial, f"{label} producer")
                journal_after_failure = artifact_hashes(checkpoint)
                resume_case["checkpoint_files_after_failure"] = journal_after_failure
                program.write_text(repaired, encoding="utf-8")
                resumed = cli(consumer, "resume", program, catalog, case_env,
                              args.timeout_seconds, "--checkpoint", str(checkpoint))
                resume_case["consumer"] = resumed
                require_resumed(resumed, resume_expected, f"{label} consumer")
                resumed["saved_file"] = file_record(read_output(case_cwd / "resumed output.txt"))
                if resumed["saved_file"]["sha256"] != digest(resume_expected):
                    raise RuntimeError(f"{label}: resumed file differs from expected bytes")
                resume_case["checkpoint_files_after_resume"] = artifact_hashes(checkpoint)
        report["status"] = "passed"
    except BaseException as error:
        report["status"] = "failed"
        report["failure"] = f"{type(error).__name__}: {error}"
        if isinstance(error, KeyboardInterrupt):
            report["interrupted"] = True
    sys.stdout.write(json.dumps(report, ensure_ascii=False, indent=2, sort_keys=True) + "\n")
    return 0 if report["status"] == "passed" else 1


if __name__ == "__main__":
    raise SystemExit(main())
