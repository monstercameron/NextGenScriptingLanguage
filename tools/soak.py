#!/usr/bin/env python3
"""Soak Execute and assert that its resources stay flat. Linux only (samples /proc).

Three scenarios:

- ``service``: one long-running service (a body every second) sampled for the whole
  duration. The process must stay alive, and after the warm-up its resident size,
  descriptor count and thread count must stay flat.
- ``live``: one ``execute live serve`` process, driven by client calls for the whole
  duration (reads, and pause and resume cycles). Every client call must succeed, and the
  server's resources must stay flat as for ``service``.
- ``graph``: a graph with a named input, run repeatedly with a new input each time. Each
  run is sampled while it runs. The peaks of the last third of the runs must stay within
  the allowed growth of the first third, and every run must succeed.

Exit status is 0 when every assertion holds, 1 when one fails, and 2 when the process a
scenario depends on ends early.
"""
import argparse
import json
import os
import signal
import subprocess
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
MANIFESTS = ROOT / "manifests"
SERVICE_PROGRAM = 'every 1 seconds\n  run printf with message "tick"\n'
GRAPH_PROGRAM = (
    'accept greeting as text with default "soak"\n'
    "graph soak\n"
    '  run text.concat with parts [greeting, "graph"] and separator " " as joined\n'
    "  run text.trim_final_lf as cleaned\n"
    "  bind joined.value to cleaned.text with capacity 1 lossless\n"
    "end graph\n"
)


def parse_status(text):
    """Resident kilobytes and thread count from a /proc/<pid>/status body."""
    rss_kb = None
    threads = None
    for line in text.splitlines():
        if line.startswith("VmRSS:"):
            rss_kb = int(line.split()[1])
        elif line.startswith("Threads:"):
            threads = int(line.split()[1])
    if rss_kb is None or threads is None:
        raise ValueError("status has no VmRSS or Threads field")
    return rss_kb, threads


# Graph RSS growth is judged against the larger of the percentage and this floor, so a few
# megabytes of allocator noise on a small process cannot fail the run on its own.
MIN_RSS_GROWTH_KB = 2048


def fd_targets(pid):
    """What each open descriptor of a live process points at, as sorted ``fd -> target``."""
    targets = {}
    for name in os.listdir(f"/proc/{pid}/fd"):
        try:
            targets[int(name)] = os.readlink(f"/proc/{pid}/fd/{name}")
        except (FileNotFoundError, ProcessLookupError, PermissionError):
            # The descriptor closed between listing and reading it.
            continue
    return {str(fd): targets[fd] for fd in sorted(targets)}


def sample(pid):
    """One sample of a live process: (rss_kb, threads, open_descriptors)."""
    status = Path(f"/proc/{pid}/status").read_text()
    rss_kb, threads = parse_status(status)
    descriptors = len(os.listdir(f"/proc/{pid}/fd"))
    return rss_kb, threads, descriptors


def violations(samples, warmup_index, limits):
    """Flatness after warm-up for a sampled process. Returns a list of failures."""
    if len(samples) <= warmup_index:
        return ["too few samples after warm-up"]
    baseline = samples[warmup_index]
    later = samples[warmup_index + 1 :]
    failures = []
    if later:
        peak_rss = max(s[0] for s in later)
        growth = (peak_rss - baseline[0]) * 100.0 / max(baseline[0], 1)
        if growth > limits["rss_growth_pct"]:
            failures.append(
                f"RSS grew {growth:.1f}% after warm-up ({baseline[0]} kB to {peak_rss} kB)"
            )
        peak_fds = max(s[2] for s in later)
        if peak_fds > baseline[2] + limits["fd_slack"]:
            failures.append(f"descriptors rose from {baseline[2]} to {peak_fds}")
        peak_threads = max(s[1] for s in later)
        if peak_threads > baseline[1] + limits["thread_slack"]:
            failures.append(f"threads rose from {baseline[1]} to {peak_threads}")
    return failures


def run_peak_violations(peaks, limits):
    """Per-run peaks must not drift upward over the run.

    RSS growth fails only when it is above max(rss_growth_pct, MIN_RSS_GROWTH_KB) of the
    first third's peak AND it keeps rising: the middle third and the last third are both
    above that limit, and the last third reaches at least the middle third's peak. A single
    high run at the end is not a trend and passes. Descriptor and thread drift use their
    slack directly.
    """
    if len(peaks) < 6:
        return ["too few runs to compare thirds"]
    third = len(peaks) // 3
    first, middle, last = peaks[:third], peaks[third : 2 * third], peaks[2 * third :]
    failures = []
    first_rss = max(p[0] for p in first)
    middle_rss = max(p[0] for p in middle)
    last_rss = max(p[0] for p in last)
    limit = max(limits["rss_growth_pct"] * first_rss / 100.0, MIN_RSS_GROWTH_KB)
    if (
        middle_rss - first_rss > limit
        and last_rss - first_rss > limit
        and last_rss >= middle_rss
    ):
        failures.append(
            f"run peak RSS rose above {first_rss + limit:.0f} kB and kept rising: "
            f"first third {first_rss} kB, middle {middle_rss} kB, last {last_rss} kB"
        )
    if max(p[2] for p in last) > max(p[2] for p in first) + limits["fd_slack"]:
        failures.append("run peak descriptors drifted upward")
    if max(p[1] for p in last) > max(p[1] for p in first) + limits["thread_slack"]:
        failures.append("run peak threads drifted upward")
    return failures


def wait_for_socket(path, timeout):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if path.exists():
            return True
        time.sleep(0.1)
    return False


def soak_service(binary, duration, warmup, interval, limits):
    with tempfile.TemporaryDirectory(prefix="execute-soak-") as temp:
        root = Path(temp)
        program = root / "service.program"
        program.write_text(SERVICE_PROGRAM)
        with (root / "service.log").open("wb") as log:
            process = subprocess.Popen(
                [str(binary), "run", str(program), "--manifest-dir", str(MANIFESTS)],
                stdout=log,
                stderr=subprocess.STDOUT,
                cwd=root,
            )
            samples = []
            early = None
            targets = None
            started = time.monotonic()
            try:
                while time.monotonic() - started < duration:
                    if process.poll() is not None:
                        early = process.returncode
                        break
                    samples.append(sample(process.pid))
                    time.sleep(interval)
            finally:
                if process.poll() is None:
                    targets = fd_targets(process.pid)
                stop(process)
        result = finish("service", samples, early, process.returncode, warmup, interval, limits)
        result["fd_targets"] = targets
        return result


def soak_live(binary, duration, warmup, interval, limits):
    with tempfile.TemporaryDirectory(prefix="execute-soak-") as temp:
        root = Path(temp)
        program = root / "service.program"
        program.write_text(SERVICE_PROGRAM)
        socket = root / "live.sock"
        with (root / "server.log").open("wb") as log:
            server = subprocess.Popen(
                [
                    str(binary),
                    "live",
                    "serve",
                    str(program),
                    "--socket",
                    str(socket),
                    "--manifest-dir",
                    str(MANIFESTS),
                ],
                stdout=log,
                stderr=subprocess.STDOUT,
                cwd=root,
            )
            if not wait_for_socket(socket, 60):
                stop(server)
                return {"ok": False, "failures": ["the live socket never appeared"]}
            samples = []
            failed_calls = []
            early = None
            targets = None
            started = time.monotonic()
            calls = 0
            try:
                while time.monotonic() - started < duration:
                    if server.poll() is not None:
                        early = server.returncode
                        break
                    calls += 1
                    arguments = ["live", "read", "--socket", str(socket)]
                    if calls % 25 == 0:
                        # Exercise the control path as well as reads.
                        arguments = ["live", "control", "pause", "--socket", str(socket)]
                    elif calls % 25 == 1:
                        arguments = ["live", "control", "resume", "--socket", str(socket)]
                    result = subprocess.run(
                        [str(binary), *arguments],
                        stdout=subprocess.DEVNULL,
                        stderr=subprocess.PIPE,
                        timeout=30,
                    )
                    if result.returncode != 0:
                        failed_calls.append(
                            f"call {calls} exited {result.returncode}: "
                            f"{result.stderr.decode(errors='replace').strip()}"
                        )
                    samples.append(sample(server.pid))
                    time.sleep(interval)
            finally:
                if server.poll() is None:
                    targets = fd_targets(server.pid)
                subprocess.run(
                    [str(binary), "live", "control", "stop", "--socket", str(socket)],
                    stdout=subprocess.DEVNULL,
                    stderr=subprocess.DEVNULL,
                    timeout=60,
                )
                stop(server)
        result = finish("live", samples, early, server.returncode, warmup, interval, limits)
        result["calls"] = calls
        result["fd_targets"] = targets
        result["failed_calls"] = failed_calls[:10]
        if failed_calls:
            result["ok"] = False
            result["failures"].append(f"{len(failed_calls)} client calls failed")
        return result


def soak_graph(binary, duration, limits):
    with tempfile.TemporaryDirectory(prefix="execute-soak-") as temp:
        root = Path(temp)
        program = root / "graph.program"
        program.write_text(GRAPH_PROGRAM)
        peaks = []
        failed_runs = []
        runs = 0
        started = time.monotonic()
        while time.monotonic() - started < duration:
            runs += 1
            process = subprocess.Popen(
                [
                    str(binary),
                    "run",
                    str(program),
                    "--input",
                    f"greeting=run-{runs}",
                    "--manifest-dir",
                    str(MANIFESTS),
                ],
                stdin=subprocess.DEVNULL,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.PIPE,
                cwd=root,
            )
            peak = (0, 0, 0)
            while process.poll() is None:
                try:
                    current = sample(process.pid)
                except (FileNotFoundError, ProcessLookupError):
                    break
                peak = tuple(max(a, b) for a, b in zip(peak, current))
                time.sleep(0.02)
            _, stderr = process.communicate()
            if process.returncode != 0:
                failed_runs.append(
                    f"run {runs} exited {process.returncode}: "
                    f"{stderr.decode(errors='replace').strip()[:200]}"
                )
            peaks.append(peak)
        failures = run_peak_violations(peaks, limits)
        if failed_runs:
            failures.append(f"{len(failed_runs)} graph runs failed")
        return {
            "scenario": "graph",
            "ok": not failures,
            "runs": runs,
            "failed_runs": failed_runs[:10],
            "failures": failures,
            "first_peak": peaks[0] if peaks else None,
            "last_peak": peaks[-1] if peaks else None,
        }


def stop(process):
    if process.poll() is None:
        process.send_signal(signal.SIGTERM)
        try:
            process.wait(timeout=30)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()


def finish(name, samples, early, status, warmup, interval, limits):
    if early is not None:
        return {
            "scenario": name,
            "ok": False,
            "exited_early": True,
            "exit_status": early,
            "failures": [f"{name} process exited before the soak ended"],
            "samples": len(samples),
        }
    warmup_index = max(0, int(warmup / interval))
    failures = violations(samples, warmup_index, limits)
    return {
        "scenario": name,
        "ok": not failures,
        "exited_early": False,
        "exit_status": status,
        "failures": failures,
        "samples": len(samples),
        "baseline": samples[warmup_index] if len(samples) > warmup_index else None,
        "last": samples[-1] if samples else None,
        "timeline": [
            [round(index * interval, 1), rss_kb, threads, fds]
            for index, (rss_kb, threads, fds) in enumerate(samples)
        ],
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--scenario", choices=["service", "live", "graph"], default="service")
    parser.add_argument("--duration", type=float, default=3600.0, help="seconds to run")
    parser.add_argument("--warmup", type=float, default=300.0, help="seconds before the baseline")
    parser.add_argument("--interval", type=float, default=5.0, help="seconds between samples")
    parser.add_argument("--rss-growth-pct", type=float, default=10.0)
    parser.add_argument("--fd-slack", type=int, default=2)
    parser.add_argument("--thread-slack", type=int, default=1)
    parser.add_argument("--out", type=Path, help="write the result as JSON")
    args = parser.parse_args()
    if not sys.platform.startswith("linux"):
        raise SystemExit("soak samples /proc and runs on Linux only")
    limits = {
        "rss_growth_pct": args.rss_growth_pct,
        "fd_slack": args.fd_slack,
        "thread_slack": args.thread_slack,
    }
    binary = args.binary.resolve()
    if args.scenario == "service":
        result = soak_service(binary, args.duration, args.warmup, args.interval, limits)
    elif args.scenario == "live":
        result = soak_live(binary, args.duration, args.warmup, args.interval, limits)
    else:
        result = soak_graph(binary, args.duration, limits)
    result["scenario"] = args.scenario
    if args.out:
        args.out.write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps(result, indent=2))
    if result.get("exited_early"):
        raise SystemExit(2)
    if not result["ok"]:
        raise SystemExit(1)


if __name__ == "__main__":
    main()
