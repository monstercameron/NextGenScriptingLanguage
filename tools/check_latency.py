#!/usr/bin/env python3
"""Time the startup floor, a one-line program, a ten-step program, and a hot loop.

Each program runs through `execute run` with the binary's default manifests,
except `--version`, which measures the process floor alone. Passing
`--manifest-dir` adds a second set of rows for that explicit directory. The script
prints median and 95th-percentile wall time and, on Linux with GNU time, each
child's peak resident set size. Python 3.9+, standard library only.
"""
import argparse
import functools
import json
import shutil
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


@functools.lru_cache(maxsize=1)
def gnu_time():
    """Return GNU time when available; its child process reports per-run RSS."""
    if sys.platform != "linux":
        return None
    candidate = shutil.which("/usr/bin/time") or shutil.which("time")
    if not candidate:
        return None
    try:
        version = subprocess.run(
            [candidate, "--version"], capture_output=True, text=True, check=False
        )
    except OSError:
        return None
    version_text = f"{version.stdout}\n{version.stderr}".casefold()
    return candidate if version.returncode == 0 and "gnu time" in version_text else None


def run_once(command, rss_command=None):
    """Run one child and return (elapsed_ms, peak_rss_kb or None).

    Wall time uses direct subprocess execution, preserving the original timing
    method. A second, untimed GNU time invocation measures per-child RSS without
    inheriting Python's Linux high-water mark. Callers with side effects can pass
    a fresh `rss_command` so both invocations use independent state.
    """
    time_binary = gnu_time()
    with tempfile.TemporaryFile() as stderr:
        started = time.perf_counter()
        result = subprocess.run(command, stdout=subprocess.DEVNULL, stderr=stderr, check=False)
        elapsed = (time.perf_counter() - started) * 1000.0
        rss = None
        if time_binary:
            rss_path = None
            try:
                with tempfile.NamedTemporaryFile(prefix="execute-rss-", delete=False) as output:
                    rss_path = output.name
                measured = subprocess.run(
                    [time_binary, "-f", "%M", "-o", rss_path, *(rss_command or command)],
                    stdout=subprocess.DEVNULL,
                    stderr=subprocess.DEVNULL,
                    check=False,
                )
                if measured.returncode == 0:
                    rss = float(
                        Path(rss_path).read_text(encoding="utf-8").strip().splitlines()[-1]
                    )
            except (OSError, ValueError, IndexError):
                rss = None
            finally:
                if rss_path:
                    Path(rss_path).unlink(missing_ok=True)
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


def budget_limit(budget, key, metric):
    """Use a row-specific limit, or reuse the base limit for manifest-dir rows."""
    limit = budget.get(f"{key}_{metric}")
    if limit is None and key.endswith("_manifest_dir"):
        limit = budget.get(f"{key.removesuffix('_manifest_dir')}_{metric}")
    return limit


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("binary", type=Path)
    parser.add_argument("budget", type=Path)
    parser.add_argument("--manifest-dir", type=Path, help="also select an explicit manifest directory")
    parser.add_argument(
        "--require-rss",
        action="store_true",
        help="fail unless Linux GNU time reports per-child RSS for every workload",
    )
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
            plans[key] = ["run", str(path)]
            if args.manifest_dir:
                plans[f"{key}_manifest_dir"] = [
                    "run", str(path), "--manifest-dir", str(args.manifest_dir)
                ]
        for key, arguments in plans.items():
            runs = max(3, args.runs // 4) if key.startswith("hot_loop") else args.runs
            samples, peak = time_program(args.binary, arguments, runs, args.warmup, key)
            results[key] = {
                "median_ms": statistics.median(samples),
                "p95_ms": percentile(samples, 0.95),
                "runs": len(samples),
                "max_rss_kb": peak,
            }

    failures = []
    if args.require_rss:
        missing = [key for key, measured in results.items() if measured["max_rss_kb"] is None]
        if missing:
            failures.append(
                "per-child RSS was not measured for: " + ", ".join(missing)
                + " (GNU time is required on Linux)"
            )
        else:
            version_rss = results["version"]["max_rss_kb"]
            hot_loop_rss = results["hot_loop"]["max_rss_kb"]
            relationship = "differs from" if version_rss != hot_loop_rss else "matches"
            print(f"process-floor RSS {relationship} hot-loop RSS: {version_rss:.0f} vs {hot_loop_rss:.0f} KiB")
    labels = dict(LABELS)
    for key in results:
        if key.endswith("_manifest_dir"):
            base_key = key.removesuffix("_manifest_dir")
            base_label = dict(LABELS)[base_key]
            labels[key] = f"{base_label} (--manifest-dir)"
    for key, label in labels.items():
        measured = results[key]
        print(
            f"{label}: median {measured['median_ms']:.1f} ms, "
            f"p95 {measured['p95_ms']:.1f} ms over {measured['runs']} runs"
        )
        if measured["max_rss_kb"] is None:
            print("  peak RSS: not measured on this platform")
        else:
            print(f"  peak RSS {measured['max_rss_kb']:.0f} KiB")
        limit = budget_limit(budget, key, "median_ms")
        if limit is None:
            print(f"  no {key}_median_ms budget recorded; reported, not enforced")
        elif measured["median_ms"] > limit:
            failures.append(
                f"{label} median {measured['median_ms']:.1f} ms exceeds the {limit} ms budget"
            )
        else:
            print(f"  within budget: {measured['median_ms']:.1f} <= {limit} ms")
        rss_limit = budget_limit(budget, key, "max_rss_kb")
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
