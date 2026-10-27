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
import os
import signal
import statistics
import subprocess
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from check_latency import percentile, run_once  # noqa: E402

PIPE_1MB_BYTES = 1048576
PARALLEL_ITEMS = 32
PARALLEL_LIMIT = 8
PARALLEL_EACH_ITEMS = 10000
PARALLEL_EACH_LIMIT = 64
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


def parallel_each_10000():
    items = ", ".join(f'"{n}"' for n in range(PARALLEL_EACH_ITEMS))
    body = (
        f"parallel for each item in [{items}] at most {PARALLEL_EACH_LIMIT} as results\n"
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
    ("parallel_each_10000", parallel_each_10000, False, "10,000 in-process parallel elements at limit 64"),
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


def peak_linux_threads(binary, manifests, source, directory, name, runs, warmup):
    """Sample a child process's live Linux thread count during this workload."""
    program = Path(directory) / f"{name}-threads.program"
    program.write_text(source, encoding="utf-8")
    command = [str(binary), "run", str(program), "--manifest-dir", str(manifests)]
    peak = 0
    for _ in range(warmup + runs):
        with tempfile.TemporaryFile() as stderr:
            process = subprocess.Popen(command, stdout=subprocess.DEVNULL, stderr=stderr)
            task_dir = Path(f"/proc/{process.pid}/task")
            while process.poll() is None:
                try:
                    peak = max(peak, len(os.listdir(task_dir)))
                except FileNotFoundError:
                    pass
                time.sleep(0.001)
            returncode = process.wait()
            if returncode != 0:
                stderr.seek(0)
                raise RuntimeError(
                    f"thread-count run exited {returncode}: "
                    f"{stderr.read().decode('utf-8', 'replace')[:200]}"
                )
    return peak


def voluntary_context_switches(pid):
    """Sum Linux voluntary context switches across every thread in a process."""
    total = 0
    for task in Path(f"/proc/{pid}/task").iterdir():
        try:
            status = (task / "status").read_text(encoding="ascii")
        except FileNotFoundError:
            continue
        for line in status.splitlines():
            if line.startswith("voluntary_ctxt_switches:"):
                total += int(line.split(":", 1)[1].strip())
                break
    return total


def measure_parallel_native_wake_rate(binary, manifests, directory):
    """Measure idle wakeups across one 64-element slow-native group on Linux."""
    worker = Path(directory) / "parallel-native-sleeper.sh"
    worker.write_text("#!/bin/sh\n: > \"$1\"\nexec sleep 30\n", encoding="utf-8")
    worker.chmod(0o755)
    markers = [Path(directory) / f"native-started-{index}" for index in range(64)]
    items = ", ".join(json.dumps(str(marker)) for marker in markers)
    source = (
        f"parallel for each marker in [{items}] at most 64 as results\n"
        f"  run {json.dumps(str(worker))} with arguments [marker]\n"
    )
    program = Path(directory) / "parallel-native-idle.program"
    program.write_text(source, encoding="utf-8")
    command = [str(binary), "run", str(program), "--manifest-dir", str(manifests)]
    with tempfile.TemporaryFile() as stderr:
        process = subprocess.Popen(command, stdout=subprocess.DEVNULL, stderr=stderr)
        try:
            startup_deadline = time.monotonic() + 15
            while not all(marker.exists() for marker in markers):
                if process.poll() is not None:
                    stderr.seek(0)
                    raise RuntimeError(
                        "native idle group exited before every element started: "
                        + stderr.read().decode("utf-8", "replace")[:200]
                    )
                if time.monotonic() >= startup_deadline:
                    raise RuntimeError("native idle group did not start all 64 elements")
                time.sleep(0.005)
            # A child writes its marker before the parent worker finishes
            # registering its observer and entering the steady-state poll.
            time.sleep(1.0)
            before = voluntary_context_switches(process.pid)
            started = time.perf_counter()
            sample_seconds = 10.0
            time.sleep(sample_seconds)
            elapsed = time.perf_counter() - started
            after = voluntary_context_switches(process.pid)
            process.send_signal(signal.SIGINT)
            returncode = process.wait(timeout=5)
            if returncode != 128 + signal.SIGINT:
                stderr.seek(0)
                raise RuntimeError(
                    f"native idle group cleanup exited {returncode}: "
                    f"{stderr.read().decode('utf-8', 'replace')[:200]}"
                )
            return (after - before) / elapsed, after - before, elapsed
        finally:
            if process.poll() is None:
                process.send_signal(signal.SIGINT)
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("binary", type=Path)
    parser.add_argument("budget", type=Path)
    parser.add_argument("--manifest-dir", type=Path, default=Path("manifests"))
    parser.add_argument("--baseline-binary", type=Path)
    parser.add_argument("--runs", type=int, default=10)
    parser.add_argument("--warmup", type=int, default=1)
    args = parser.parse_args(argv)
    if not args.binary.is_file():
        print(f"error: {args.binary} is not a built executable", file=sys.stderr)
        return 1
    if args.baseline_binary is not None and not args.baseline_binary.is_file():
        print(f"error: {args.baseline_binary} is not a built baseline executable", file=sys.stderr)
        return 1
    budget = json.loads(args.budget.read_text(encoding="utf-8"))

    failures = []
    parallel_candidate = None
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
            if name == "parallel_each_10000" and sys.platform.startswith("linux"):
                try:
                    result["max_threads"] = peak_linux_threads(
                        args.binary,
                        args.manifest_dir,
                        source,
                        directory,
                        name,
                        args.runs,
                        args.warmup,
                    )
                except RuntimeError as error:
                    print(f"{name}: thread measurement failed: {error}")
                    failures.append(f"{name} thread measurement failed")
                else:
                    print(f"  peak live process threads: {result['max_threads']}")
                    thread_limit = budget.get(f"{name}_max_threads")
                    if thread_limit is not None and result["max_threads"] > thread_limit:
                        failures.append(
                            f"{name} peak threads {result['max_threads']} exceeds {thread_limit}"
                        )
                    elif thread_limit is not None:
                        print(f"  thread count within budget: {result['max_threads']} <= {thread_limit}")
            if name == "parallel_each_10000":
                parallel_candidate = result
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
        if args.baseline_binary is not None:
            if parallel_candidate is None:
                failures.append("parallel_each_10000 candidate result is missing")
            else:
                source, checkpointed = parallel_each_10000()
                try:
                    baseline = measure(
                        args.baseline_binary,
                        args.manifest_dir,
                        source,
                        checkpointed,
                        args.runs,
                        args.warmup,
                        directory,
                        "parallel_each_10000_baseline",
                    )
                except RuntimeError as error:
                    print(f"parallel_each_10000 baseline failed: {error}")
                    failures.append("parallel_each_10000 baseline failed to run")
                else:
                    candidate_ms = parallel_candidate["median_ms"]
                    baseline_ms = baseline["median_ms"]
                    print(
                        f"parallel_each_10000 paired comparison: baseline {baseline_ms:.1f} ms, "
                        f"candidate {candidate_ms:.1f} ms"
                    )
                    if candidate_ms * 2 > baseline_ms:
                        failures.append(
                            "parallel_each_10000 candidate did not improve at least 2x "
                            f"({baseline_ms:.1f} ms baseline, {candidate_ms:.1f} ms candidate)"
                        )
                    else:
                        print("  candidate is at least 2x faster than the baseline")
        if sys.platform.startswith("linux"):
            try:
                wake_rate, wake_count, wake_elapsed = measure_parallel_native_wake_rate(
                    args.binary,
                    args.manifest_dir,
                    directory,
                )
            except RuntimeError as error:
                print(f"parallel native wake-rate measurement failed: {error}")
                failures.append("parallel native wake-rate measurement failed")
            else:
                print(
                    "parallel native idle group: "
                    f"{wake_count} voluntary process context switches over {wake_elapsed:.1f} s "
                    f"({wake_rate:.1f}/s) across 64 slow native elements"
                )
                wake_limit = budget.get("parallel_native_idle_group_max_wakeups_per_sec")
                # The two /proc snapshots bound the sample, so a scheduler
                # timeout can land at either edge. Allow one boundary switch;
                # sustained rates above the budget still fail.
                boundary_tolerance = 1.0 / wake_elapsed
                if wake_limit is not None and wake_rate > wake_limit + boundary_tolerance:
                    failures.append(
                        f"parallel native idle group wake rate {wake_rate:.1f}/s "
                        f"exceeds {wake_limit}/s beyond one boundary switch"
                    )
                elif wake_limit is not None:
                    print(
                        f"  group wake rate within budget: {wake_rate:.1f}/s "
                        f"(limit {wake_limit}/s plus one boundary switch)"
                    )
    for failure in failures:
        print(f"error: {failure}", file=sys.stderr)
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
