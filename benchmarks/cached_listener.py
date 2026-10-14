#!/usr/bin/env python3
"""Measure cached stdin-listener acknowledgements without trace overhead.

The first record per process warms a private cache entry; ten additional warmups
are excluded. Three paired blocks of identical records follow in a seeded,
shuffled label order. The only executed applet is the reviewed system `tr`.
"""
from __future__ import annotations

import argparse
import base64
from contextlib import ExitStack
import datetime as dt
import hashlib
import json
import math
import os
import platform
import random
import resource
import selectors
import signal
import subprocess
import sys
import tempfile
import time
from pathlib import Path
from typing import Any

MIB = 1024 * 1024
MAX_CACHE_BYTES = 16 * MIB
MAX_STDERR_BYTES = MIB
MAX_OUTPUT_BYTES = 16 * MIB
CAPTURE_PREFIX_BYTES = 16 * 1024
WARMUP_RECORDS = 10
BLOCK_COUNT = 3
RECORDS_PER_BLOCK = 1_000
MAX_PATH_PREFIXES = 64
MAX_TIMEOUT_SECONDS = 3_600.0
MAX_COMMAND_TIMEOUT_MS = 60_000
TR_PATHS = ("/usr/bin/tr", "/bin/tr")

# This is an owned copy of the reviewed 0.4.6 tr manifest. `cacheable` is its
# purity contract: fixed `tr` arguments produce a deterministic result.
TR_MANIFEST = {
    "name": "tr",
    "binary": "tr",
    "accepts_stdin": True,
    "cacheable": True,
    "parameters": {
        "from": {"type": "string", "required": True},
        "to": {"type": "string", "required": True},
    },
    "arguments": [{"parameter": "from"}, {"parameter": "to"}],
}
TR_MANIFEST_SOURCE_SHA256 = "971ed4560d7f290983f444a96a1698b932e051ea2667dceb3c523cccb9b09f7d"
PROGRAM = (
    "listen to stdin\n"
    "whenever a line arrives as record\n"
    '  then tr with from "[:lower:]" and to "[:upper:]" and cache\n'
)


def sha256_bytes(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def trusted_tr() -> Path:
    for candidate in TR_PATHS:
        path = Path(candidate)
        if path.is_file() and os.access(path, os.X_OK):
            return path.resolve(strict=True)
    raise RuntimeError("could not resolve an executable system tr at /usr/bin/tr or /bin/tr")


def strict_environment(path: str, cwd: Path) -> dict[str, str]:
    return {
        "PATH": path,
        "LC_ALL": "C",
        "LANG": "C",
        "TMPDIR": str(cwd),
        "HOME": str(cwd),
    }


def set_file_size_limit(limit: int) -> None:
    old_soft, old_hard = resource.getrlimit(resource.RLIMIT_FSIZE)
    bounds = [limit]
    if old_soft != resource.RLIM_INFINITY:
        bounds.append(old_soft)
    if old_hard != resource.RLIM_INFINITY:
        bounds.append(old_hard)
    resource.setrlimit(resource.RLIMIT_FSIZE, (min(bounds), old_hard))


def stop_owned_process(process: subprocess.Popen[bytes]) -> str:
    """Stop only the Popen-owned runner; forced termination leaves child cleanup unknown."""
    sent_term = False
    try:
        if process.poll() is None:
            process.send_signal(signal.SIGTERM)
            sent_term = True
    except ProcessLookupError:
        pass
    try:
        process.wait(timeout=0.5)
    except subprocess.TimeoutExpired:
        forced = False
        try:
            if process.poll() is None:
                process.kill()
                forced = True
            process.wait(timeout=2.0)
        except subprocess.TimeoutExpired:
            return "incomplete: runner could not be reaped within 2 seconds"
        except ProcessLookupError:
            try:
                process.wait(timeout=2.0)
            except subprocess.TimeoutExpired:
                return "incomplete: runner could not be reaped within 2 seconds"
        else:
            if forced:
                return "incomplete: runner required SIGKILL; applet cleanup is unverified"
            return "runner exited before forced kill; applet cleanup is not independently verified"
    else:
        if sent_term and process.returncode != -signal.SIGTERM:
            return "cooperative SIGTERM; runtime-managed child cleanup completed"
        return "incomplete: cooperative shutdown was not confirmed; applet cleanup is unverified"
    return "incomplete: runner cleanup state is unknown"


class Listener:
    def __init__(
        self,
        label: str,
        binary: Path,
        program: Path,
        manifest_dir: Path,
        cache_dir: Path,
        cwd: Path,
        environment: dict[str, str],
        deadline: float,
        max_record_bytes: int,
        max_runs: int,
        command_timeout_ms: int,
        traced: bool = False,
    ) -> None:
        self.label = label
        self.deadline = deadline
        self.stdout_buffer = bytearray()
        self.stderr = bytearray()
        self.total_stdout_bytes = 0
        self.total_stdout_limit = max_runs * max_record_bytes + CAPTURE_PREFIX_BYTES
        self.stdout_eof = False
        self.stderr_eof = False
        self.input_registered = False
        self.closed = False
        self.finished = False
        command = [
            str(binary), "run", str(program),
            "--manifest-dir", str(manifest_dir),
            "--cache-dir", str(cache_dir),
            "--max-record-bytes", str(max_record_bytes),
            "--max-runs", str(max_runs),
            "--command-timeout-ms", str(command_timeout_ms),
            "--max-output-bytes", str(MAX_OUTPUT_BYTES),
        ]
        if traced:
            command.append("--trace")
        self.process = subprocess.Popen(
            command,
            cwd=cwd,
            env=environment,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            bufsize=0,
            start_new_session=True,
            preexec_fn=lambda: set_file_size_limit(MAX_CACHE_BYTES),
        )
        self.selector = selectors.DefaultSelector()
        try:
            if self.process.stdin is None or self.process.stdout is None or self.process.stderr is None:
                raise RuntimeError("listener subprocess pipes were not created")
            for stream, name in (
                (self.process.stdout, "stdout"),
                (self.process.stderr, "stderr"),
            ):
                os.set_blocking(stream.fileno(), False)
                self.selector.register(stream, selectors.EVENT_READ, name)
            os.set_blocking(self.process.stdin.fileno(), False)
        except BaseException as error:
            cleanup_status = stop_owned_process(self.process)
            self.close_handles()
            if isinstance(error, KeyboardInterrupt):
                raise
            raise RuntimeError(
                f"{label}: could not initialize bounded pipes ({cleanup_status}): {error}"
            ) from error

    def _set_input_interest(self, enabled: bool) -> None:
        assert self.process.stdin is not None
        if enabled and not self.input_registered:
            self.selector.register(self.process.stdin, selectors.EVENT_WRITE, "stdin")
            self.input_registered = True
        elif not enabled and self.input_registered:
            self.selector.unregister(self.process.stdin)
            self.input_registered = False

    def _read_ready(self, key: selectors.SelectorKey) -> None:
        chunk = os.read(key.fd, 64 * 1024)
        if not chunk:
            self.selector.unregister(key.fileobj)
            key.fileobj.close()
            if key.data == "stdout":
                self.stdout_eof = True
            else:
                self.stderr_eof = True
            return
        if key.data == "stdout":
            self.total_stdout_bytes += len(chunk)
            if self.total_stdout_bytes > self.total_stdout_limit:
                raise RuntimeError(f"{self.label}: stdout exceeded its bounded session size")
            self.stdout_buffer.extend(chunk)
        else:
            if len(self.stderr) + len(chunk) > MAX_STDERR_BYTES:
                raise RuntimeError(f"{self.label}: stderr exceeded {MAX_STDERR_BYTES} bytes")
            self.stderr.extend(chunk)

    def exchange(self, record: bytes) -> int:
        """Write one line and return its exact-output acknowledgement latency."""
        wire = record + b"\n"
        expected = record.upper() + b"\n"
        write_offset = 0
        started = time.perf_counter_ns()
        while True:
            if time.monotonic() >= self.deadline:
                raise TimeoutError(f"{self.label}: overall deadline exceeded")
            if len(self.stdout_buffer) > len(expected):
                raise RuntimeError(f"{self.label}: received extra or misframed stdout bytes")
            if write_offset == len(wire) and len(self.stdout_buffer) == len(expected):
                actual = bytes(self.stdout_buffer)
                self.stdout_buffer.clear()
                if actual != expected:
                    raise RuntimeError(f"{self.label}: response did not match uppercase input")
                return time.perf_counter_ns() - started
            if self.stdout_eof:
                raise RuntimeError(f"{self.label}: stdout closed before the record acknowledgement")
            self._set_input_interest(write_offset < len(wire))
            remaining = self.deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError(f"{self.label}: overall deadline exceeded")
            ready = self.selector.select(min(remaining, 0.1))
            for key, _ in ready:
                if key.data == "stdin":
                    try:
                        written = os.write(key.fd, wire[write_offset:])
                    except BlockingIOError:
                        continue
                    if written <= 0:
                        raise BrokenPipeError(f"{self.label}: listener stdin closed")
                    write_offset += written
                else:
                    self._read_ready(key)
            if self.process.poll() is not None and self.stdout_eof and len(self.stdout_buffer) < len(expected):
                raise RuntimeError(f"{self.label}: listener exited before the record acknowledgement")

    def finish(self) -> None:
        assert self.process.stdin is not None
        self._set_input_interest(False)
        self.process.stdin.close()
        unexpected = bytearray()
        while self.selector.get_map() or self.process.poll() is None:
            remaining = self.deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError(f"{self.label}: overall deadline exceeded during EOF shutdown")
            ready = self.selector.select(min(remaining, 0.1)) if self.selector.get_map() else []
            for key, _ in ready:
                before = len(self.stdout_buffer)
                self._read_ready(key)
                if key.data == "stdout" and len(self.stdout_buffer) > before:
                    unexpected.extend(self.stdout_buffer[before:])
                    if len(unexpected) > CAPTURE_PREFIX_BYTES:
                        raise RuntimeError(f"{self.label}: unexpected trailing stdout exceeded limit")
            if not self.selector.get_map() and self.process.poll() is None:
                time.sleep(min(0.05, remaining))
        remaining = self.deadline - time.monotonic()
        if remaining <= 0:
            raise TimeoutError(f"{self.label}: overall deadline exceeded while reaping runner")
        status = self.process.wait(timeout=remaining)
        self.finished = True
        self.close_handles()
        if status != 0:
            raise RuntimeError(f"{self.label}: listener exited with status {status}")
        if unexpected:
            raise RuntimeError(f"{self.label}: listener emitted trailing stdout after all records")

    def cleanup(self) -> str:
        if self.closed:
            return "already closed"
        if self.process.poll() is None:
            status = stop_owned_process(self.process)
        else:
            status = "runner already exited; descendant cleanup unverified"
        self.close_handles()
        return status

    def close_handles(self) -> None:
        if self.closed:
            return
        self.closed = True
        try:
            self.selector.close()
        except AttributeError:
            pass
        for stream in (self.process.stdin, self.process.stdout, self.process.stderr):
            if stream is not None:
                try:
                    stream.close()
                except OSError:
                    pass


def build_record(size: int) -> bytes:
    return b"alpha" + b"a" * (size - len(b"alpha"))


def cache_snapshot(directory: Path) -> dict[str, Any]:
    files = []
    total = 0
    for path in sorted(directory.iterdir()):
        if not path.is_file() or path.is_symlink():
            raise RuntimeError(f"cache contains an unexpected non-file entry: {path.name}")
        size = path.stat().st_size
        total += size
        if total > MAX_CACHE_BYTES:
            raise RuntimeError(f"cache exceeded {MAX_CACHE_BYTES} bytes")
        files.append({"name": path.name, "bytes": size, "sha256": sha256_file(path)})
    if not files:
        raise RuntimeError(f"cache has no stored result: {directory}")
    if len(files) != 1:
        raise RuntimeError(f"expected one stable cache entry, found {len(files)}")
    return {"total_bytes": total, "files": files}


def percentile(values: list[int], p: float) -> int:
    ordered = sorted(values)
    return ordered[max(0, math.ceil(p * len(ordered)) - 1)]


def summary(values: list[int]) -> dict[str, int]:
    return {
        "samples": len(values),
        "p50_ns": percentile(values, 0.50),
        "p95_ns": percentile(values, 0.95),
        "p99_ns": percentile(values, 0.99),
        "min_ns": min(values),
        "max_ns": max(values),
    }


def write_trace_fixture(path: Path) -> bytes:
    encoded = (json.dumps(TR_MANIFEST, ensure_ascii=False, indent=2) + "\n").encode("utf-8")
    path.mkdir(parents=True)
    (path / "tr.json").write_bytes(encoded)
    return encoded


def parse_trace(stderr: bytes) -> list[dict[str, Any]]:
    events = []
    for line in stderr.splitlines():
        if not line:
            continue
        event = json.loads(line.decode("utf-8"))
        if not isinstance(event, dict) or not isinstance(event.get("event"), str):
            raise RuntimeError("trace line did not contain a JSON event")
        events.append(event)
    return events


def handler_event_groups(events: list[dict[str, Any]]) -> list[list[str]]:
    groups: list[list[str]] = []
    current: list[str] | None = None
    for event in events:
        name = event["event"]
        if name == "handler_start":
            if current is not None:
                raise RuntimeError("trace nested handler_start events")
            current = []
        elif name == "handler_end":
            if current is None:
                raise RuntimeError("trace handler_end had no handler_start")
            groups.append(current)
            current = None
        elif current is not None:
            current.append(name)
    if current is not None:
        raise RuntimeError("trace ended before handler_end")
    return groups


def validate_traces(stderr: bytes) -> dict[str, Any]:
    events = parse_trace(stderr)
    groups = handler_event_groups(events)
    if len(groups) != 3:
        raise RuntimeError(f"expected three traced handlers, found {len(groups)}")
    if "cache_store" not in groups[0] or "spawn" not in groups[0]:
        raise RuntimeError("first traced handler did not store the cold cache result")
    for index, group in enumerate(groups[1:], start=1):
        if "cache_hit" not in group or "spawn" in group:
            raise RuntimeError(f"traced warm handler {index} did not hit cache without spawning")
    return {
        "handler_event_names": groups,
        "cache_hits_after_cold_record": 2,
        "native_spawns": [group.count("spawn") for group in groups],
        "trace_sha256": sha256_bytes(stderr),
    }


def validate_args(parser: argparse.ArgumentParser, args: argparse.Namespace) -> None:
    if args.seed < 0:
        parser.error("--seed must be non-negative")
    if args.path_prefix_count < 0 or args.path_prefix_count > MAX_PATH_PREFIXES:
        parser.error(f"--path-prefix-count must be in 0..{MAX_PATH_PREFIXES}")
    if not 0 < args.timeout_seconds <= MAX_TIMEOUT_SECONDS:
        parser.error(f"--timeout-seconds must be in (0, {MAX_TIMEOUT_SECONDS:g}]")
    if not 1 <= args.command_timeout_ms <= MAX_COMMAND_TIMEOUT_MS:
        parser.error(f"--command-timeout-ms must be in 1..{MAX_COMMAND_TIMEOUT_MS}")


def cleanup_listeners(listeners: dict[str, Listener], report: dict[str, Any]) -> None:
    for label, listener in listeners.items():
        if listener.finished:
            report["cleanup"].setdefault(label, "runner completed and was reaped")
        else:
            try:
                report["cleanup"][label] = listener.cleanup()
            except Exception as error:
                report["cleanup"][label] = f"incomplete: cleanup error: {type(error).__name__}: {error}"


def main() -> int:
    def handle_sigterm(_signum: int, _frame: Any) -> None:
        raise KeyboardInterrupt

    signal.signal(signal.SIGTERM, handle_sigterm)
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--baseline", type=Path, required=True, help="baseline Execute executable")
    parser.add_argument("--candidate", type=Path, required=True, help="candidate Execute executable")
    parser.add_argument("--seed", type=int, default=20261013)
    parser.add_argument("--record-size", type=int, choices=(8, 8192), default=8)
    parser.add_argument("--path-prefix-count", type=int, default=0)
    parser.add_argument("--timeout-seconds", type=float, default=180.0)
    parser.add_argument("--command-timeout-ms", type=int, default=10_000)
    args = parser.parse_args()
    validate_args(parser, args)

    report: dict[str, Any] = {
        "schema": "cached-listener-ack-v1",
        "status": "starting",
        "observed_at": dt.datetime.now(dt.timezone.utc).isoformat(),
        "platform": platform.platform(),
        "python": platform.python_version(),
        "seed": args.seed,
        "record_size_bytes_excluding_lf": args.record_size,
        "warmup": {"cold_records_excluded": 1, "additional_records_excluded": WARMUP_RECORDS},
        "blocks": BLOCK_COUNT,
        "records_per_block": RECORDS_PER_BLOCK,
        "path_prefix_count": args.path_prefix_count,
        "timeout_seconds": args.timeout_seconds,
        "command_timeout_ms": args.command_timeout_ms,
        "method": {
            "timing": "per-record acknowledgement: first nonblocking input write through exact uppercase response bytes read",
            "sampling": "all 3000 acknowledgements per label; p50/p95/p99 use nearest-rank ceil(p*n)-1",
            "cache": "one private cache and one persistent listener process per label; cache is preserved across blocks",
            "tracing": "disabled during timing; separate cold-plus-two-hit trace validation follows measurement",
            "scope": "interpreter/listener/cache hot path; native tr spawn occurs only for the excluded cold cache miss",
            "safety": "owned temporary files/cache; exact embedded tr manifest; trusted real system tr; strict child environment; bounded records, cache files, pipes, deadlines and runner cleanup",
        },
        "tr_manifest": {},
        "binaries": {},
        "record_sha256": None,
        "expected_output_sha256": None,
        "warmup_order": [],
        "block_order": [],
        "results": {},
        "trace_validation": {},
        "first_failure": None,
        "cleanup": {},
    }
    listeners: dict[str, Listener] = {}
    active_stage = "setup"
    rng = random.Random(args.seed)

    try:
        binary_paths = {
            "baseline": args.baseline.resolve(strict=True),
            "candidate": args.candidate.resolve(strict=True),
        }
        for label, binary in binary_paths.items():
            if not binary.is_file() or not os.access(binary, os.X_OK):
                raise ValueError(f"{label} is not an executable file: {binary}")
            report["binaries"][label] = {"path": str(binary), "sha256": sha256_file(binary)}
        tr_path = trusted_tr()
        report["tr_manifest"] = {
            "cacheable": TR_MANIFEST["cacheable"],
            "source_fixture_sha256": TR_MANIFEST_SOURCE_SHA256,
            "binary_path": str(tr_path),
            "binary_sha256": sha256_file(tr_path),
        }
        record = build_record(args.record_size)
        expected = record.upper() + b"\n"
        report["record_sha256"] = sha256_bytes(record)
        report["record_base64"] = base64.b64encode(record).decode("ascii")
        report["expected_output_sha256"] = sha256_bytes(expected)
        deadline = time.monotonic() + args.timeout_seconds
        total_records = 1 + WARMUP_RECORDS + BLOCK_COUNT * RECORDS_PER_BLOCK

        with tempfile.TemporaryDirectory(prefix="cached listener Ω ") as temporary, ExitStack() as cleanup_stack:
            cleanup_stack.callback(cleanup_listeners, listeners, report)
            root = Path(temporary)
            manifest_bytes = write_trace_fixture(root / "manifests")
            manifest_dir = root / "manifests"
            report["tr_manifest"]["owned_copy_sha256"] = sha256_bytes(manifest_bytes)
            tr_bin_dir = root / "trusted-applet-bin"
            tr_bin_dir.mkdir()
            (tr_bin_dir / "tr").symlink_to(tr_path)
            prefixes = []
            for index in range(args.path_prefix_count):
                prefix = root / f"empty PATH prefix {index:02d} Ω"
                prefix.mkdir()
                prefixes.append(str(prefix))
            child_path = os.pathsep.join((*prefixes, str(tr_bin_dir)))
            program = root / "stdin listener Ω.program"
            program.write_text(PROGRAM, encoding="utf-8")
            for label, binary in binary_paths.items():
                cwd = root / f"{label} work Ω"
                cwd.mkdir()
                cache = root / f"{label} private cache Ω"
                cache.mkdir()
                listeners[label] = Listener(
                    label,
                    binary,
                    program,
                    manifest_dir,
                    cache,
                    cwd,
                    strict_environment(child_path, cwd),
                    deadline,
                    args.record_size + 1,
                    total_records,
                    args.command_timeout_ms,
                )

            warm_order = ["baseline", "candidate"]
            rng.shuffle(warm_order)
            report["warmup_order"] = warm_order
            before_blocks = {}
            for label in warm_order:
                active_stage = f"{label}: excluded cold record"
                cold_latency = listeners[label].exchange(record)
                warmup_latency = []
                for index in range(WARMUP_RECORDS):
                    active_stage = f"{label}: excluded warmup {index + 1}/{WARMUP_RECORDS}"
                    warmup_latency.append(listeners[label].exchange(record))
                cache_dir = root / f"{label} private cache Ω"
                before_blocks[label] = cache_snapshot(cache_dir)
                if before_blocks[label]["total_bytes"] > MAX_CACHE_BYTES:
                    raise RuntimeError(f"{label}: cache exceeds the 16 MiB budget")
                report["results"][label] = {
                    "cold_miss_ack_ns_excluded": cold_latency,
                    "warmup_ack_ns_excluded": warmup_latency,
                    "cache_after_warmup": before_blocks[label],
                    "blocks": [],
                    "ack_latency_ns_by_block": [],
                }

            for block_index in range(BLOCK_COUNT):
                order = ["baseline", "candidate"]
                rng.shuffle(order)
                report["block_order"].append(order)
                for label in order:
                    active_stage = f"{label}: measured block {block_index + 1}/{BLOCK_COUNT}"
                    latencies = [listeners[label].exchange(record) for _ in range(RECORDS_PER_BLOCK)]
                    result = report["results"][label]
                    result["blocks"].append(summary(latencies))
                    result["ack_latency_ns_by_block"].append(latencies)
                    cache_dir = root / f"{label} private cache Ω"
                    after_block = cache_snapshot(cache_dir)
                    if after_block != before_blocks[label]:
                        raise RuntimeError(f"{label}: cache changed after warmup during measured hits")
                for label in order:
                    report["results"][label]["cache_stable_across_blocks"] = True

            for label in ("baseline", "candidate"):
                active_stage = f"{label}: graceful EOF shutdown"
                listeners[label].finish()
                raw_samples = [
                    value
                    for block in report["results"][label]["ack_latency_ns_by_block"]
                    for value in block
                ]
                report["results"][label]["summary"] = summary(
                    raw_samples
                )
                report["cleanup"][label] = "normal service exit; runner reaped"

            trace_order = ["baseline", "candidate"]
            rng.shuffle(trace_order)
            report["trace_order"] = trace_order
            trace_records = [record, record, record]
            for label in trace_order:
                active_stage = f"{label}: separate traced cold/hit validation"
                trace_cwd = root / f"{label} trace work Ω"
                trace_cwd.mkdir()
                trace_cache = root / f"{label} trace cache Ω"
                trace_cache.mkdir()
                trace_listener = Listener(
                    label,
                    binary_paths[label],
                    program,
                    manifest_dir,
                    trace_cache,
                    trace_cwd,
                    strict_environment(child_path, trace_cwd),
                    deadline,
                    args.record_size + 1,
                    len(trace_records),
                    args.command_timeout_ms,
                    traced=True,
                )
                listeners[f"{label}-trace"] = trace_listener
                for _ in trace_records:
                    trace_listener.exchange(record)
                trace_listener.finish()
                report["trace_validation"][label] = validate_traces(bytes(trace_listener.stderr))

        report["status"] = "passed"
    except KeyboardInterrupt:
        report["status"] = "interrupted"
        report["first_failure"] = {
            "stage": active_stage,
            "reason": "keyboard interrupt",
        }
    except BaseException as error:
        report["status"] = "failed"
        if report["first_failure"] is None:
            report["first_failure"] = {
                "stage": active_stage,
                "reason": f"{type(error).__name__}: {error}",
            }
    finally:
        json.dump(report, sys.stdout, ensure_ascii=False, indent=2, sort_keys=True)
        sys.stdout.write("\n")
    return 0 if report["status"] == "passed" else 130 if report["status"] == "interrupted" else 1


if __name__ == "__main__":
    raise SystemExit(main())
