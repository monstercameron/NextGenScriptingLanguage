#!/usr/bin/env python3
"""Time the startup floor, a one-line program, a ten-step program, and a hot loop.

Each program runs through `execute run` with the shipped manifests, except
`--version`, which measures the process floor alone. The script prints the median
and 95th-percentile wall time of each over several runs, and on POSIX the peak
resident set size of each child. The budget file may hold `<key>_median_ms` and
`<key>_max_rss_kb` for the keys below; a null or missing budget reports the value
without enforcing it. Python 3.9+, standard library only.
"""
import argparse
import json
import os
import statistics
import subprocess
import sys
import tempfile
import time
from pathlib import Path

ONE_LINE = 'run printf with message "one"\n'
TEN_STEPS = "".join(f'run printf with message "step {n}"\n' for n in range(1, 11))
# One loop of 100000 in-process applet calls, the largest count the loop budget allows in one run.
HOT_LOOP = "repeat 100000 times\n  run number.to_text with value 7\n"

LABELS = (
    ("version", "process floor (--version)"),
    ("one_line", "one-line program"),
    ("ten_step", "ten-step program"),
    ("hot_loop", "hot loop (100000 applet calls)"),
)


def percentile(values, fraction):
    ordered = sorted(values)
    index = min(len(ordered) - 1, max(0, round(fraction * (len(ordered) - 1))))
    return ordered[index]


def run_once(command):
    """Run one child and return (elapsed_ms, peak_rss_kb or None).

    On POSIX, os.wait4 returns the child's own rusage, so the peak is per child
    and not the largest of every child so far. On Windows there is no wait4, so
    the peak is not reported.
    """
    with tempfile.TemporaryFile() as stderr:
        started = time.perf_counter()
        if hasattr(os, "wait4"):
            process = subprocess.Popen(command, stdout=subprocess.DEVNULL, stderr=stderr)
            _, status, usage = os.wait4(process.pid, 0)
            elapsed = (time.perf_counter() - started) * 1000.0
            process.returncode = os.waitstatus_to_exitcode(status)
            # ru_maxrss is bytes on macOS and kilobytes elsewhere.
            rss = usage.ru_maxrss / 1024 if sys.platform == "darwin" else usage.ru_maxrss
            returncode = process.returncode
        else:
            result = subprocess.run(command, stdout=subprocess.DEVNULL, stderr=stderr, check=False)
            elapsed = (time.perf_counter() - started) * 1000.0
            rss = None
            returncode = result.returncode
        stderr.seek(0)
        message = stderr.read().decode("utf-8", "replace").strip()
    return elapsed, rss, returncode, message


def time_program(binary, arguments, runs, warmup, label):
    command = [str(binary), *arguments]
    for _ in range(warmup):
        run_once(command)
    samples, rss_values = [], []
    for _ in range(runs):
        elapsed, rss, returncode, message = run_once(command)
        if returncode != 0:
            raise RuntimeError(f"{label} failed with status {returncode}: {message}")
        samples.append(elapsed)
        if rss is not None:
            rss_values.append(rss)
    return samples, (max(rss_values) if rss_values else None)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("binary", type=Path)
    parser.add_argument("budget", type=Path)
    parser.add_argument("--manifest-dir", type=Path, default=Path("manifests"))
    parser.add_argument("--runs", type=int, default=20)
    parser.add_argument("--warmup", type=int, default=2)
    args = parser.parse_args(argv)

    if not args.binary.is_file():
        print(f"error: {args.binary} is not a built executable", file=sys.stderr)
        return 1
    if args.runs < 1:
        print("error: --runs must be at least 1", file=sys.stderr)
        return 1
    budget = json.loads(args.budget.read_text(encoding="utf-8"))

    results = {}
    with tempfile.TemporaryDirectory(prefix="execute-latency-") as directory:
        programs = {
            "one_line": (Path(directory) / "one_line.program", ONE_LINE),
            "ten_step": (Path(directory) / "ten_step.program", TEN_STEPS),
            "hot_loop": (Path(directory) / "hot_loop.program", HOT_LOOP),
        }
        for path, source in programs.values():
            path.write_text(source, encoding="utf-8")
        plans = {"version": ["--version"]}
        for key, (path, _) in programs.items():
            plans[key] = [
                "run",
                str(path),
                "--manifest-dir",
                str(args.manifest_dir),
            ]
        for key, arguments in plans.items():
            runs = args.runs if key != "hot_loop" else max(3, args.runs // 4)
            samples, peak = time_program(args.binary, arguments, runs, args.warmup, key)
            results[key] = {
                "median_ms": statistics.median(samples),
                "p95_ms": percentile(samples, 0.95),
                "runs": len(samples),
                "max_rss_kb": peak,
            }

    failures = []
    for key, label in LABELS:
        measured = results[key]
        print(
            f"{label}: median {measured['median_ms']:.1f} ms, "
            f"p95 {measured['p95_ms']:.1f} ms over {measured['runs']} runs"
        )
        if measured["max_rss_kb"] is None:
            print("  peak RSS: not measured on this platform")
        else:
            print(f"  peak RSS {measured['max_rss_kb']:.0f} KiB")
        limit = budget.get(f"{key}_median_ms")
        if limit is None:
            print(f"  no {key}_median_ms budget recorded; reported, not enforced")
        elif measured["median_ms"] > limit:
            failures.append(
                f"{label} median {measured['median_ms']:.1f} ms exceeds the {limit} ms budget"
            )
        else:
            print(f"  within budget: {measured['median_ms']:.1f} <= {limit} ms")
        rss_limit = budget.get(f"{key}_max_rss_kb")
        if measured["max_rss_kb"] is not None:
            if rss_limit is None:
                print(f"  no {key}_max_rss_kb budget recorded; reported, not enforced")
            elif measured["max_rss_kb"] > rss_limit:
                failures.append(
                    f"{label} peak RSS {measured['max_rss_kb']:.0f} KiB exceeds the {rss_limit} KiB budget"
                )
            else:
                print(f"  RSS within budget: {measured['max_rss_kb']:.0f} <= {rss_limit} KiB")
    for failure in failures:
        print(f"error: {failure}", file=sys.stderr)
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
