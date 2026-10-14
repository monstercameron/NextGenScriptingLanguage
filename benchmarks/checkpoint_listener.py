#!/usr/bin/env python3
"""Matched checkpointed-listener latency benchmark for two explicit binaries.

Each paired record is sent to both binaries in randomized sequential order. The
listener echoes the exact record through a per-handler save, while checkpoint
journals and output sinks live in owned temporary directories. Per-record timing
ends when the expected stdout bytes arrive, before stdin EOF and without treating
that response as a durable-finish acknowledgement.
"""

import argparse
import datetime
import hashlib
import json
import math
import os
import platform
import random
import selectors
import statistics
import subprocess
import tempfile
import time
from pathlib import Path

WARMUPS_DEFAULT = 10
BYTE_BUDGET = 64 * 1024 * 1024
STDERR_CAPTURE_LIMIT = 256 * 1024
EXTRA_STDOUT_LIMIT = 1024 * 1024
STARTUP_HOOKS = {
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


def child_environment():
    environment = {
        key: value
        for key, value in os.environ.items()
        if key not in STARTUP_HOOKS
        and not key.startswith("BASH_FUNC_")
        and not value.lstrip().startswith("() {")
    }
    environment["LC_ALL"] = "C"
    return environment


def file_sha256(path):
    digest = hashlib.sha256()
    with Path(path).open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def tree_sha256(directory):
    """Hash regular files in a manifest tree without following symlinks."""
    directory = Path(directory)
    digest = hashlib.sha256()
    for root, dirs, files in os.walk(directory, followlinks=False):
        root_path = Path(root)
        dirs[:] = sorted(name for name in dirs if not (root_path / name).is_symlink())
        for name in sorted(files):
            path = root_path / name
            if path.is_symlink() or not path.is_file():
                continue
            relative = path.relative_to(directory).as_posix().encode("utf-8")
            digest.update(len(relative).to_bytes(8, "big"))
            digest.update(relative)
            digest.update(bytes.fromhex(file_sha256(path)))
    return digest.hexdigest()


def distribution(values):
    ordered = sorted(values)
    if not ordered:
        raise ValueError("cannot summarize an empty sample")
    return {
        "samples": len(values),
        "p50": statistics.median(ordered),
        "p95": ordered[math.ceil(len(ordered) * 0.95) - 1],
        "p99": ordered[math.ceil(len(ordered) * 0.99) - 1],
        "raw": list(values),
    }


def version_of(binary):
    result = subprocess.run(
        [str(binary), "--version"],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        timeout=10,
        check=False,
        env=child_environment(),
    )
    if result.returncode != 0:
        raise RuntimeError(
            f"{binary} --version failed ({result.returncode}): "
            f"{result.stderr.decode(errors='replace')}"
        )
    return result.stdout.decode(errors="replace").strip()


def record_for(seed, nonce, record_bytes):
    """Build an ASCII record of exact size, unique by a visible 48-bit nonce."""
    if nonce >= 1 << 48:
        raise ValueError("record nonce exceeded its unique 48-bit field")
    body_bytes = record_bytes - 1
    token = f"{nonce:012x}".encode("ascii")
    if body_bytes < len(token):
        raise ValueError("record size is too small for the unique nonce")
    body = bytearray(token)
    counter = 0
    while len(body) < body_bytes:
        block = hashlib.sha256(f"{seed}:{nonce}:{counter}".encode("ascii"))
        body.extend(block.hexdigest().encode("ascii"))
        counter += 1
    return bytes(body[:body_bytes]) + b"\n"


def projected_budget(record_sizes, samples, blocks, warmups):
    pair_count = blocks * (samples + warmups)
    # Count input records sent to both binaries plus an upper bound on the
    # output blob and JSON-encoded string-binding blob in both journals. The
    # records are printable ASCII except for a final newline, encoded as "\\n".
    input_bytes = 2 * pair_count * sum(record_sizes)
    journal_blob_bytes = pair_count * sum(4 * size + 4 for size in record_sizes)
    return {
        "limit_bytes": BYTE_BUDGET,
        "input_record_bytes_both_binaries": input_bytes,
        "estimated_unique_journal_blob_payload_bytes": journal_blob_bytes,
        "accounted_bytes": input_bytes + journal_blob_bytes,
        "accounting": (
            "Includes submitted record bytes plus unique output and JSON string-binding blob payloads "
            "for both binaries, including warmups. Excludes stdout acknowledgements, sink rewrites, "
            "filesystem metadata, and transient atomic-write files."
        ),
    }


def append_capped(buffer, data, limit):
    remaining = limit - len(buffer)
    if remaining > 0:
        buffer.extend(data[:remaining])


class ListenerSession:
    def __init__(self, label, process, run_dir, checkpoint_dir, sink_path, timeout_seconds):
        self.label = label
        self.process = process
        self.run_dir = run_dir
        self.checkpoint_dir = checkpoint_dir
        self.sink_path = sink_path
        self.timeout_seconds = timeout_seconds
        self.selector = selectors.DefaultSelector()
        self.stdout = process.stdout
        self.stderr = process.stderr
        self.stdin = process.stdin
        self.stdout_eof = False
        self.stderr_eof = False
        self.stderr_bytes = bytearray()
        self.stderr_truncated = False
        self.extra_stdout = bytearray()
        self.extra_stdout_truncated = False
        self.closed = False
        for stream in (self.stdout, self.stderr, self.stdin):
            os.set_blocking(stream.fileno(), False)
        self.selector.register(self.stdout, selectors.EVENT_READ, "stdout")
        self.selector.register(self.stderr, selectors.EVENT_READ, "stderr")

    def diagnostic(self):
        text = self.stderr_bytes.decode(errors="replace")
        if self.stderr_truncated:
            text += "\n[stderr capture truncated]"
        return text

    def _unregister(self, stream):
        try:
            self.selector.unregister(stream)
        except (KeyError, ValueError):
            pass

    def _read_stderr(self):
        try:
            chunk = os.read(self.stderr.fileno(), 65536)
        except BlockingIOError:
            return
        if not chunk:
            self.stderr_eof = True
            self._unregister(self.stderr)
            return
        before = len(self.stderr_bytes)
        append_capped(self.stderr_bytes, chunk, STDERR_CAPTURE_LIMIT)
        self.stderr_truncated |= before + len(chunk) > STDERR_CAPTURE_LIMIT

    def _read_stdout(self, destination, limit):
        try:
            chunk = os.read(self.stdout.fileno(), 65536)
        except BlockingIOError:
            return
        if not chunk:
            self.stdout_eof = True
            self._unregister(self.stdout)
            return
        if destination is None:
            before = len(self.extra_stdout)
            append_capped(self.extra_stdout, chunk, limit)
            self.extra_stdout_truncated |= before + len(chunk) > limit
        else:
            destination.extend(chunk)

    def exchange(self, record, pair_id, warmup):
        """Write one record and receive exactly its echo within a bounded time."""
        write_offset = 0
        response = bytearray()
        deadline = time.monotonic() + self.timeout_seconds
        started_ns = time.perf_counter_ns()
        registered_stdin = False
        while len(response) < len(record):
            if time.monotonic() >= deadline:
                raise TimeoutError(
                    f"{self.label} timed out on {pair_id}; "
                    f"wrote {write_offset}/{len(record)}, read {len(response)}/{len(record)} bytes; "
                    f"{self.diagnostic()}"
                )
            if self.process.poll() is not None and self.stdout_eof:
                raise RuntimeError(
                    f"{self.label} exited before acknowledging {pair_id} "
                    f"({self.process.returncode}): {self.diagnostic()}"
                )
            if write_offset < len(record) and not registered_stdin:
                self.selector.register(self.stdin, selectors.EVENT_WRITE, "stdin")
                registered_stdin = True
            events = self.selector.select(max(0, deadline - time.monotonic()))
            if not events and time.monotonic() >= deadline:
                continue
            for key, _ in events:
                kind = key.data
                if kind == "stdin":
                    try:
                        written = os.write(
                            self.stdin.fileno(), memoryview(record)[write_offset:]
                        )
                    except BlockingIOError:
                        continue
                    except BrokenPipeError as error:
                        raise RuntimeError(
                            f"{self.label} closed stdin during {pair_id}: {self.diagnostic()}"
                        ) from error
                    if written <= 0:
                        raise RuntimeError(f"{self.label} made no stdin write progress")
                    write_offset += written
                    if write_offset == len(record):
                        self._unregister(self.stdin)
                        registered_stdin = False
                elif kind == "stdout":
                    remaining = len(record) - len(response)
                    try:
                        chunk = os.read(self.stdout.fileno(), remaining)
                    except BlockingIOError:
                        continue
                    if not chunk:
                        self.stdout_eof = True
                        self._unregister(self.stdout)
                        raise RuntimeError(
                            f"{self.label} closed stdout before acknowledging {pair_id}: "
                            f"{self.diagnostic()}"
                        )
                    response.extend(chunk)
                else:
                    self._read_stderr()
        elapsed_ns = time.perf_counter_ns() - started_ns
        if response != record:
            raise AssertionError(
                f"{self.label} returned incorrect bytes for {pair_id}: "
                f"expected sha256={hashlib.sha256(record).hexdigest()}, "
                f"actual sha256={hashlib.sha256(response).hexdigest()}"
            )
        return {
            "pair_id": pair_id,
            "warmup": warmup,
            "latency_ns": elapsed_ns,
            "record_bytes": len(record),
            "record_sha256": hashlib.sha256(record).hexdigest(),
            "ack_sha256": hashlib.sha256(response).hexdigest(),
        }

    def finish(self):
        """Send EOF, drain output streams, and wait only for this child."""
        try:
            self.stdin.close()
        except BrokenPipeError:
            pass
        self.stdin = None
        deadline = time.monotonic() + self.timeout_seconds
        while True:
            if self.stdout_eof and self.stderr_eof and self.process.poll() is not None:
                break
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError(f"{self.label} did not exit after stdin EOF")
            events = self.selector.select(min(remaining, 0.1))
            for key, _ in events:
                if key.data == "stdout":
                    self._read_stdout(None, EXTRA_STDOUT_LIMIT)
                else:
                    self._read_stderr()
            if not events and self.process.poll() is None:
                time.sleep(min(0.01, remaining))
        return_code = self.process.wait(timeout=max(0.01, deadline - time.monotonic()))
        if return_code != 0:
            raise RuntimeError(
                f"{self.label} exited with status {return_code}: {self.diagnostic()}"
            )
        if self.extra_stdout or self.extra_stdout_truncated:
            raise AssertionError(
                f"{self.label} emitted unexpected stdout after the final record "
                f"(bytes={len(self.extra_stdout)}, truncated={self.extra_stdout_truncated})"
            )
        journal_path = self.checkpoint_dir / "journal.json"
        envelope = json.loads(journal_path.read_text(encoding="utf-8"))
        status = envelope["data"]["status"]
        if status != "idle":
            raise AssertionError(f"{self.label} journal status is {status!r}, expected 'idle'")
        expected_sink = getattr(self, "last_record", None)
        if expected_sink is None:
            raise AssertionError(f"{self.label} did not process any records")
        sink_bytes = self.sink_path.read_bytes()
        if sink_bytes != expected_sink:
            raise AssertionError(
                f"{self.label} sink did not match the last record: "
                f"expected sha256={hashlib.sha256(expected_sink).hexdigest()}, "
                f"actual sha256={hashlib.sha256(sink_bytes).hexdigest()}"
            )
        return {
            "return_code": return_code,
            "journal_status": status,
            "journal_sha256": file_sha256(journal_path),
            "sink_sha256": hashlib.sha256(sink_bytes).hexdigest(),
            "sink_bytes": len(sink_bytes),
        }

    def close(self):
        if self.closed:
            return
        self.closed = True
        if self.process.poll() is None:
            try:
                self.process.terminate()
            except ProcessLookupError:
                pass
            try:
                self.process.wait(timeout=2)
            except subprocess.TimeoutExpired:
                try:
                    self.process.kill()
                except ProcessLookupError:
                    pass
                try:
                    self.process.wait(timeout=2)
                except subprocess.TimeoutExpired:
                    pass
        for stream in (self.stdin, self.stdout, self.stderr):
            if stream is not None:
                try:
                    stream.close()
                except OSError:
                    pass
        self.selector.close()


def start_listener(label, binary, manifest_dir, run_dir, record_bytes, timeout_seconds):
    run_dir.mkdir(parents=True, exist_ok=False)
    workflow = run_dir / "listener.program"
    workflow.write_text(
        'listen to stdin\n'
        'whenever a line arrives as record\n'
        '  then save to file "sink.bin"\n',
        encoding="utf-8",
    )
    checkpoint_dir = run_dir / "checkpoint"
    sink_path = run_dir / "sink.bin"
    command = [
        str(binary),
        str(workflow),
        "--manifest-dir",
        str(manifest_dir),
        "--checkpoint",
        str(checkpoint_dir),
        "--max-record-bytes",
        str(record_bytes),
        "--max-output-bytes",
        str(record_bytes),
    ]
    process = subprocess.Popen(
        command,
        cwd=run_dir,
        env=child_environment(),
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        bufsize=0,
        start_new_session=True,
    )
    return ListenerSession(
        label, process, run_dir, checkpoint_dir, sink_path, timeout_seconds
    )


def summarize(rows, record_sizes):
    summary = {}
    for record_bytes in record_sizes:
        selected = [
            row
            for row in rows
            if row["record_bytes"] == record_bytes and not row["warmup"]
        ]
        by_pair = {}
        for row in selected:
            by_pair.setdefault(row["pair_id"], {})[row["variant"]] = row["latency_ns"]
        pairs = [
            pair
            for pair in by_pair.values()
            if "baseline" in pair and "candidate" in pair
        ]
        ratios = [pair["candidate"] / pair["baseline"] for pair in pairs]
        summary[str(record_bytes)] = {
            "paired_samples": len(pairs),
            "baseline_latency_ns": distribution(
                [pair["baseline"] for pair in pairs]
            ),
            "candidate_latency_ns": distribution(
                [pair["candidate"] for pair in pairs]
            ),
            "candidate_over_baseline_ratio": distribution(ratios),
        }
    return summary


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--baseline-binary", type=Path, required=True)
    parser.add_argument("--candidate-binary", type=Path, required=True)
    parser.add_argument("--manifest-dir", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--samples", type=int, default=30)
    parser.add_argument("--blocks", type=int, default=3)
    parser.add_argument("--warmups", type=int, default=WARMUPS_DEFAULT)
    parser.add_argument("--record-bytes", type=int, nargs="+", default=[16, 8192, 65536])
    parser.add_argument("--seed", type=int, default=20261014)
    parser.add_argument("--timeout-seconds", type=float, default=30.0)
    args = parser.parse_args()
    if args.samples < 1 or args.blocks < 1 or args.warmups < 0:
        parser.error("samples and blocks must be positive; warmups cannot be negative")
    if args.timeout_seconds <= 0:
        parser.error("timeout-seconds must be positive")
    if not args.record_bytes or any(size < 16 for size in args.record_bytes):
        parser.error("each record size must be at least 16 bytes, including newline")
    if len(set(args.record_bytes)) != len(args.record_bytes):
        parser.error("record sizes must be unique")
    if args.seed < 0:
        parser.error("seed must be nonnegative")

    baseline = args.baseline_binary.resolve(strict=True)
    candidate = args.candidate_binary.resolve(strict=True)
    manifest_dir = args.manifest_dir.resolve(strict=True)
    if not baseline.is_file() or not candidate.is_file():
        parser.error("both binary paths must be files")
    if not os.access(baseline, os.X_OK) or not os.access(candidate, os.X_OK):
        parser.error("both binary paths must be executable")
    if not manifest_dir.is_dir():
        parser.error("manifest-dir must be a directory")

    budget = projected_budget(
        args.record_bytes, args.samples, args.blocks, args.warmups
    )
    if budget["accounted_bytes"] > BYTE_BUDGET:
        parser.error(
            "projected benchmark data exceeds the 64 MiB budget: "
            f"{budget['accounted_bytes']} bytes"
        )

    output_path = args.output.expanduser().resolve()
    report = {
        "schema": "flow.checkpoint-listener-benchmark.v1",
        "status": "running",
        "observed_at_utc": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        "platform": {
            "system": platform.system(),
            "release": platform.release(),
            "architecture": platform.machine(),
            "python": platform.python_version(),
        },
        "binaries": {
            "baseline": {
                "path": str(baseline),
                "sha256": file_sha256(baseline),
                "version": version_of(baseline),
            },
            "candidate": {
                "path": str(candidate),
                "sha256": file_sha256(candidate),
                "version": version_of(candidate),
            },
        },
        "manifest_dir": {
            "path": str(manifest_dir),
            "tree_sha256": tree_sha256(manifest_dir),
        },
        "method": {
            "samples_per_case_per_block": args.samples,
            "blocks": args.blocks,
            "warmups_per_case_per_block": args.warmups,
            "record_bytes_including_newline": args.record_bytes,
            "seed": args.seed,
            "timer": (
                "time.perf_counter_ns from before the first input-byte write through receipt "
                "of the exact expected stdout record; process startup and post-run validation excluded"
            ),
            "ack_semantics": (
                "The service emits handler output before Journal::finish. A received stdout record "
                "measures output delivery and is not a durable-finish acknowledgement; the read can "
                "race with subsequent finish work."
            ),
            "ordering": (
                "Sequential paired blocks; record sizes are shuffled per block and each matched "
                "baseline/candidate record pair is run in randomized order without concurrent benchmark work."
            ),
            "fixture": (
                "ASCII record payload with a unique 48-bit pair nonce and exact requested byte length, "
                "including terminating newline; both binaries receive byte-identical paired records."
            ),
            "listener": (
                "One fresh checkpointed stdin listener per binary/case/block; 10 warmups by default, "
                "then measured records; startup excluded. Handler saves each record to an owned sink file."
            ),
            "verification": (
                "Exact stdout echo checked before stdin EOF for every record; after EOF each child must "
                "exit zero, journal data.status must be idle, and sink bytes must equal its last record."
            ),
            "budget": budget,
            "safety": (
                "Owned temporary working directories, journals, sinks, and child processes; bounded "
                "select-based pipe I/O and process waits; no API, Git, network, or system-setting effects."
            ),
            "metrics": "Response latency only; no RSS or CPU measurement is claimed.",
            "trace": "disabled to keep diagnostic serialization out of the timed path",
        },
        "events": [],
        "completed_blocks": [],
    }

    def write_report():
        output_path.parent.mkdir(parents=True, exist_ok=True)
        output_path.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")

    rng = random.Random(args.seed)
    nonce = 0
    total_runs = 0
    try:
        with tempfile.TemporaryDirectory(prefix="flow-checkpoint-listener-") as temp_dir:
            temp_root = Path(temp_dir)
            for block in range(args.blocks):
                case_order = list(args.record_bytes)
                rng.shuffle(case_order)
                for record_bytes in case_order:
                    case_root = temp_root / f"block-{block:03d}-bytes-{record_bytes}"
                    labels = ["baseline", "candidate"]
                    rng.shuffle(labels)
                    sessions = {}
                    try:
                        for label in labels:
                            binary = baseline if label == "baseline" else candidate
                            sessions[label] = start_listener(
                                label,
                                binary,
                                manifest_dir,
                                case_root / label,
                                record_bytes,
                                args.timeout_seconds,
                            )
                        for index in range(args.warmups + args.samples):
                            pair_id = f"b{block:03d}-n{record_bytes}-i{index:04d}"
                            record = record_for(args.seed, nonce, record_bytes)
                            pair_sha = hashlib.sha256(record).hexdigest()
                            nonce += 1
                            labels = ["baseline", "candidate"]
                            rng.shuffle(labels)
                            for label in labels:
                                event = sessions[label].exchange(
                                    record,
                                    pair_id,
                                    warmup=index < args.warmups,
                                )
                                event.update(
                                    {
                                        "block": block,
                                        "record_bytes": record_bytes,
                                        "variant": label,
                                        "record_nonce": nonce - 1,
                                        "record_sha256": pair_sha,
                                    }
                                )
                                report["events"].append(event)
                                sessions[label].last_record = record
                                total_runs += 1
                        validation = {}
                        for label in labels:
                            validation[label] = sessions[label].finish()
                        report["completed_blocks"].append(
                            {
                                "block": block,
                                "record_bytes": record_bytes,
                                "pair_count": args.warmups + args.samples,
                                "variants": validation,
                            }
                        )
                    finally:
                        for session in sessions.values():
                            session.close()
                report["status"] = "running"
                report["progress"] = {
                    "completed_blocks": len(report["completed_blocks"]),
                    "expected_blocks": args.blocks * len(args.record_bytes),
                    "records_verified": total_runs,
                }
                write_report()

        report["status"] = "completed"
        report["summary"] = summarize(report["events"], args.record_bytes)
        report.pop("progress", None)
        write_report()
    except BaseException as error:
        report["status"] = "failed"
        report["error"] = f"{type(error).__name__}: {error}"
        report["summary"] = summarize(report["events"], args.record_bytes) if report["events"] else {}
        try:
            write_report()
        except OSError:
            pass
        raise
    print(json.dumps({"status": report["status"], "output": str(output_path), "events": total_runs}, indent=2))


if __name__ == "__main__":
    main()
