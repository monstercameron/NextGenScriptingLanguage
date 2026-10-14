#!/usr/bin/env python3
"""Paired fresh CLI native cat streams with exact binary output oracles."""
import argparse
from datetime import datetime, timezone
import hashlib
import json
from pathlib import Path
import platform
import random
import subprocess
import tempfile
import time

from direct_commands import summary

PROJECT = Path(__file__).resolve().parents[1]


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--baseline", type=Path, required=True)
    parser.add_argument("--candidate", type=Path, required=True)
    parser.add_argument("--report", type=Path, required=True)
    parser.add_argument("--samples", type=int, default=20)
    args = parser.parse_args()
    if not 10 <= args.samples <= 200:
        parser.error("samples must be between 10 and 200")
    binaries = {
        name: getattr(args, name).resolve() for name in ["baseline", "candidate"]
    }
    results = {}
    rng = random.Random(202610142332)
    with tempfile.TemporaryDirectory(prefix="execute-stream-framing-") as temporary:
        root = Path(temporary)
        source = 'accept path as text\nstream from "cat" with arguments ["--", path]\nshow each line\n'
        program = root / "forward.program"
        program.write_text(source)
        for record_size, size in [
            (8, 2 * 1024**2),
            (1024, 16 * 1024**2),
            (8192, 16 * 1024**2),
            (65536, 16 * 1024**2),
        ]:
            record = (b"x\x00\xff\r" * ((record_size - 1 + 3) // 4))[
                : record_size - 1
            ] + b"\n"
            content = record * (size // record_size) + b"unterminated\x00\xff"
            fixture = root / "input.bin"
            fixture.write_bytes(content)
            commands = {
                name: [
                    str(binary),
                    str(program),
                    "--input",
                    f"path={fixture}",
                    "--manifest-dir",
                    str(PROJECT / "manifests"),
                    "--max-record-bytes",
                    "65536",
                    "--command-timeout-ms",
                    "20000",
                ]
                for name, binary in binaries.items()
            }
            oracle = {}
            for name, command in commands.items():
                captured = subprocess.run(
                    command + ["--trace"],
                    cwd=root,
                    input=b"",
                    capture_output=True,
                    timeout=25,
                )
                if captured.returncode != 0 or captured.stdout != content:
                    raise RuntimeError(
                        (name, record_size, captured.returncode, captured.stderr)
                    )
                trace = captured.stderr.decode()
                if "cat" not in trace:
                    raise RuntimeError("native cat trace missing")
                oracle[name] = {
                    "stdout_sha256": hashlib.sha256(captured.stdout).hexdigest(),
                    "status": captured.returncode,
                    "trace": trace,
                }
            raw = {name: [] for name in binaries}
            orders = []
            with open("/dev/null", "wb") as discard:
                for trial in range(args.samples + 3):
                    order = list(binaries)
                    rng.shuffle(order)
                    if trial >= 3:
                        orders.append(order)
                    for name in order:
                        start = time.perf_counter_ns()
                        completed = subprocess.run(
                            commands[name],
                            cwd=root,
                            stdin=subprocess.DEVNULL,
                            stdout=discard,
                            stderr=subprocess.PIPE,
                            timeout=25,
                        )
                        elapsed = time.perf_counter_ns() - start
                        if completed.returncode != 0 or completed.stderr:
                            raise RuntimeError(
                                (
                                    name,
                                    record_size,
                                    completed.returncode,
                                    completed.stderr,
                                )
                            )
                        if trial >= 3:
                            raw[name].append(elapsed / 1e3)
            stats = {name: summary(values) for name, values in raw.items()}
            results[str(record_size)] = {
                "input_bytes": len(content),
                "records": size // record_size + 1,
                "input_sha256": digest(fixture),
                "oracle": oracle,
                "raw_us": raw,
                "summary_us": stats,
                "order": orders,
            }
        report = {
            "recorded_at_utc": datetime.now(timezone.utc).isoformat(),
            "platform": platform.platform(),
            "binary_sha256": {name: digest(path) for name, path in binaries.items()},
            "harness_sha256": digest(Path(__file__)),
            "source": source,
            "source_sha256": hashlib.sha256(source.encode()).hexdigest(),
            "catalog_sha256": hashlib.sha256(
                b"".join(
                    path.read_bytes()
                    for path in sorted((PROJECT / "manifests").glob("*.json"))
                )
            ).hexdigest(),
            "method": "Serial randomized paired fresh CLI runs; three warmups, identical inherited environment/catalog/input/cwd. Actual native cat, runtime record framing, cancellation/deadline checks and OS writes to /dev/null included. Whole binary stdout compared exactly to independent binary fixture and native trace retained outside timing. Warm filesystem, uncontrolled background activity. Not per-record latency, cold IO, allocation, RSS or cross-platform measurement.",
            "results": results,
        }
        args.report.parent.mkdir(parents=True, exist_ok=True)
        args.report.write_text(json.dumps(report, indent=2) + "\n")
        print(
            json.dumps(
                {name: result["summary_us"] for name, result in results.items()},
                indent=2,
            )
        )


if __name__ == "__main__":
    main()
