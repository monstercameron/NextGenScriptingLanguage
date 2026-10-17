#!/usr/bin/env python3
"""Benchmark the hot paths of `execute run` and check them against per-benchmark budgets.

Each benchmark writes its own program into a temporary directory and runs it through
`execute run`. The script reports the median and 95th-percentile wall time and, on
POSIX, the peak resident set size of each run. A checkpointed benchmark gets a fresh
checkpoint directory on every run, so no run resumes another run's journal.

The budget file holds `<name>_median_ms` and `<name>_max_rss_kb`. A null or missing
budget reports the value without enforcing it. Benchmarks that need a POSIX tool are
marked as such and skipped elsewhere. Python 3.9+, standard library only.
"""
import argparse
import json
import statistics
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from check_latency import percentile, run_once  # noqa: E402

PIPE_1MB_BYTES = 1048576
PARALLEL_ITEMS = 32
PARALLEL_LIMIT = 8
FOR_EACH_ITEMS = 10000


def ten_step_pipeline():
    stages = ['run "printf" with arguments ["x"]'] + ['pipe to "cat"'] * 9
    return "\n".join(stages) + "\n", False


def for_each_10000(checkpointed):
    items = ", ".join(f'"{n}"' for n in range(FOR_EACH_ITEMS))
    body = f"for each item in [{items}]\n  run text.upper with text item\n"
    return body, checkpointed


def applet_100():
    return 'repeat 100 times\n  run text.trim_final_lf with text "x"\n', False


def pipe_1mb():
    source = (
        f'run "head" with arguments ["-c", "{PIPE_1MB_BYTES}", "/dev/zero"]\n'
        'pipe to "cat"\n'
    )
    return source, False


def parallel_limit_8():
    items = ", ".join(f'"{n}"' for n in range(PARALLEL_ITEMS))
    body = (
        f"parallel for each item in [{items}] at most {PARALLEL_LIMIT} as results\n"
        "  run text.trim_final_lf with text item\n"
    )
    return body, False


# name, program builder, needs a POSIX tool, description
BENCHMARKS = [
    ("ten_step_pipeline", ten_step_pipeline, True, "one pipeline of ten native stages"),
    ("for_each_10000", lambda: for_each_10000(False), False, "10,000-element for each, no checkpoint"),
    ("for_each_10000_checkpoint", lambda: for_each_10000(True), False, "10,000-element for each, with checkpoints"),
    ("applet_100", applet_100, False, "100 in-process applet calls"),
    ("pipe_1mb", pipe_1mb, True, "1 MiB through one pipe"),
    ("parallel_limit_8", parallel_limit_8, False, "32 parallel elements at limit 8"),
]


def measure(binary, manifests, source, checkpointed, runs, warmup, directory, name):
    program = Path(directory) / f"{name}.program"
    program.write_text(source, encoding="utf-8")
    checkpoints = []

    def arguments():
        base = [str(binary), "run", str(program), "--manifest-dir", str(manifests)]
        if not checkpointed:
            return base
        fresh = Path(directory) / f"ck-{len(checkpoints)}"
        checkpoints.append(fresh)
        return base + ["--checkpoint", str(fresh)]

    for _ in range(warmup):
        run_once(arguments())
    samples, rss_values = [], []
    for _ in range(runs):
        elapsed, rss, returncode, message = run_once(arguments())
        if returncode != 0:
            raise RuntimeError(f"exit {returncode}: {message[:200]}")
        samples.append(elapsed)
        if rss is not None:
            rss_values.append(rss)
    return {
        "median_ms": statistics.median(samples),
        "p95_ms": percentile(samples, 0.95),
        "runs": len(samples),
        "max_rss_kb": max(rss_values) if rss_values else None,
    }


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("binary", type=Path)
    parser.add_argument("budget", type=Path)
    parser.add_argument("--manifest-dir", type=Path, default=Path("manifests"))
    parser.add_argument("--runs", type=int, default=10)
    parser.add_argument("--warmup", type=int, default=1)
    args = parser.parse_args(argv)
    if not args.binary.is_file():
        print(f"error: {args.binary} is not a built executable", file=sys.stderr)
        return 1
    budget = json.loads(args.budget.read_text(encoding="utf-8"))

    failures = []
    with tempfile.TemporaryDirectory(prefix="execute-bench-") as directory:
        for name, build, posix_only, description in BENCHMARKS:
            source, checkpointed = build()
            if posix_only and sys.platform == "win32":
                print(f"{name}: {description}: skipped, needs a POSIX tool")
                continue
            try:
                result = measure(
                    args.binary,
                    args.manifest_dir,
                    source,
                    checkpointed,
                    args.runs,
                    args.warmup,
                    directory,
                    name,
                )
            except RuntimeError as error:
                print(f"{name}: {description}: failed: {error}")
                failures.append(f"{name} failed to run")
                continue
            print(
                f"{name}: {description}: median {result['median_ms']:.1f} ms, "
                f"p95 {result['p95_ms']:.1f} ms over {result['runs']} runs"
            )
            if result["max_rss_kb"] is None:
                print("  peak RSS: not measured on this platform")
            else:
                print(f"  peak RSS {result['max_rss_kb']:.0f} KiB")
            limit = budget.get(f"{name}_median_ms")
            if limit is None:
                print(f"  no {name}_median_ms budget recorded; reported, not enforced")
            elif result["median_ms"] > limit:
                failures.append(f"{name} median {result['median_ms']:.1f} ms exceeds {limit} ms")
            else:
                print(f"  within budget: {result['median_ms']:.1f} <= {limit} ms")
            rss_limit = budget.get(f"{name}_max_rss_kb")
            if result["max_rss_kb"] is not None:
                if rss_limit is None:
                    print(f"  no {name}_max_rss_kb budget recorded; reported, not enforced")
                elif result["max_rss_kb"] > rss_limit:
                    failures.append(f"{name} peak RSS {result['max_rss_kb']:.0f} KiB exceeds {rss_limit} KiB")
                else:
                    print(f"  RSS within budget: {result['max_rss_kb']:.0f} <= {rss_limit} KiB")
    for failure in failures:
        print(f"error: {failure}", file=sys.stderr)
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
