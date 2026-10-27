#!/usr/bin/env python3
"""Measure parallel-for-each result RSS against issue #548's 64 MiB goal.

Example: python3 benchmarks/parallel_each_rss.py target/release/execute
The workload uses in-process applets, so the measured child is the entire
workload rather than a parent whose external children are omitted from RSS.
"""
import argparse
import platform
import resource
import subprocess
import sys
import tempfile
import time
from pathlib import Path


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    args = parser.parse_args()
    root = Path(__file__).resolve().parent.parent
    binary = args.binary.resolve(strict=True)
    if platform.system() not in ("Darwin", "Linux"):
        parser.error("RSS accounting requires macOS or Linux")
    with tempfile.TemporaryDirectory() as temporary:
        program = Path(temporary) / "case.program"
        items = ", ".join('"x"' for _ in range(10_000))
        program.write_text(
            f"parallel for each n in [{items}] at most 8 as results\n"
            '  run text.repeat with text n and count 1024\n'
            'use value "done"\n',
            encoding="utf-8",
        )
        start = time.monotonic()
        result = subprocess.run(
            [str(binary), "run", str(program), "--manifest-dir", str(root / "manifests"),
             "--max-output-bytes", str(128 * 1024 * 1024)],
            cwd=temporary, stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=False,
        )
        elapsed = time.monotonic() - start
        rss = resource.getrusage(resource.RUSAGE_CHILDREN).ru_maxrss
        if platform.system() == "Linux":
            rss *= 1024
        if result.returncode or result.stdout != b"done":
            sys.stderr.write(result.stderr.decode("utf-8", errors="replace"))
            print(f"workload failed: exit={result.returncode}, stdout={result.stdout!r}")
            return 1
        limit = 64 * 1024 * 1024
        print(f"peak_rss_bytes={rss} limit_bytes={limit} elapsed_seconds={elapsed:.3f}")
        return int(rss >= limit)


if __name__ == "__main__":
    raise SystemExit(main())
