#!/usr/bin/env python3
"""Time a one-line program and a ten-step program, and check them against a budget.

Each program runs through `execute run` with the shipped manifests. The script
prints the median and 95th-percentile wall time of each over several runs. The
budget file holds {"one_line_median_ms": N, "ten_step_median_ms": N}; a null
budget reports the time without enforcing it. Python 3.9+, standard library only.
"""
import argparse
import json
import statistics
import subprocess
import sys
import tempfile
import time
from pathlib import Path

ONE_LINE = 'run printf with message "one"\n'
TEN_STEPS = "".join(f'run printf with message "step {n}"\n' for n in range(1, 11))


def percentile(values, fraction):
    ordered = sorted(values)
    index = min(len(ordered) - 1, max(0, round(fraction * (len(ordered) - 1))))
    return ordered[index]


def time_program(binary, program, manifests, runs, warmup):
    command = [str(binary), "run", str(program), "--manifest-dir", str(manifests)]
    for _ in range(warmup):
        subprocess.run(command, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, check=False)
    samples = []
    for _ in range(runs):
        started = time.perf_counter()
        result = subprocess.run(
            command, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, check=False
        )
        elapsed = (time.perf_counter() - started) * 1000.0
        if result.returncode != 0:
            raise RuntimeError(
                f"{program.name} failed with status {result.returncode}: "
                f"{result.stderr.decode('utf-8', 'replace').strip()}"
            )
        samples.append(elapsed)
    return samples


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
        }
        for key, (path, source) in programs.items():
            path.write_text(source, encoding="utf-8")
            samples = time_program(
                args.binary, path, args.manifest_dir, args.runs, args.warmup
            )
            results[key] = {
                "median_ms": statistics.median(samples),
                "p95_ms": percentile(samples, 0.95),
                "runs": len(samples),
            }

    failures = []
    for key, label in (("one_line", "one-line program"), ("ten_step", "ten-step program")):
        measured = results[key]
        print(
            f"{label}: median {measured['median_ms']:.1f} ms, "
            f"p95 {measured['p95_ms']:.1f} ms over {measured['runs']} runs"
        )
        limit = budget.get(f"{key}_median_ms")
        if limit is None:
            print(f"  no {key}_median_ms budget recorded; reported, not enforced")
        elif measured["median_ms"] > limit:
            failures.append(
                f"{label} median {measured['median_ms']:.1f} ms exceeds the {limit} ms budget"
            )
        else:
            print(f"  within budget: {measured['median_ms']:.1f} <= {limit} ms")
    for failure in failures:
        print(f"error: {failure}", file=sys.stderr)
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
