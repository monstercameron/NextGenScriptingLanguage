#!/usr/bin/env python3
"""Paired process-input measurements with literal output and native spawn oracles."""
import argparse
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path
import platform
import random
import shlex
import shutil
import statistics
import subprocess
import tempfile
import time

ROOT = Path(__file__).resolve().parents[1]


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def system_tool(name):
    for parent in ("/usr/bin", "/bin"):
        path = Path(parent) / name
        if path.is_file():
            return str(path)
    raise RuntimeError("Required system tool unavailable: " + name)


def summary(raw):
    return {
        "p50_us": statistics.median(raw),
        "p95_us": sorted(raw)[(95 * len(raw) + 99) // 100 - 1],
        "raw_us": raw,
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--baseline", required=True, type=Path)
    parser.add_argument("--candidate", required=True, type=Path)
    parser.add_argument("--report", required=True, type=Path)
    parser.add_argument("--samples", type=int, default=80)
    parser.add_argument("--seed", type=int, default=9092026)
    args = parser.parse_args()
    if not 20 <= args.samples <= 1000:
        parser.error("--samples must be between 20 and 1000")
    binaries = {
        "baseline": args.baseline.resolve(),
        "candidate": args.candidate.resolve(),
        "bash": Path(system_tool("bash")),
    }
    printf, printenv = system_tool("printf"), system_tool("printenv")
    jq = shutil.which("jq")
    require(jq is not None, "jq is required for the pre-feature helper pipeline")
    jq = str(Path(jq).resolve())
    value = "folder with spaces/雪\nnext line\n"
    expected = value.encode()
    literal = (
        "run "
        + json.dumps(printf)
        + ' with arguments ["%s", '
        + json.dumps(value, ensure_ascii=False)
        + "]\n"
    )
    sources = {
        "environment": {
            "baseline": "run "
            + json.dumps(printenv)
            + ' with arguments ["EXECUTE_BENCH_ROOT"]\nthen '
            + json.dumps(jq)
            + ' with arguments ["-Rs", '
            + json.dumps('rtrimstr("\\n")')
            + ']\nthen parse json as value\ntake text at "" from value as root\nrun '
            + json.dumps(printf)
            + ' with arguments ["%s", root]\n',
            "candidate": 'accept root as text from environment "EXECUTE_BENCH_ROOT"\nrun '
            + json.dumps(printf)
            + ' with arguments ["%s", root]\n',
            "bash": "exec " + shlex.quote(printf) + ' "%s" "$EXECUTE_BENCH_ROOT"\n',
        },
        "literal_control": {
            "baseline": literal,
            "candidate": literal,
            "bash": "exec "
            + shlex.quote(printf)
            + " '%s' "
            + shlex.quote(value)
            + "\n",
        },
    }
    env = {"PATH": "/usr/bin:/bin", "LC_ALL": "C", "EXECUTE_BENCH_ROOT": value}
    results, spawns = {}, {}
    rng = random.Random(args.seed)
    with tempfile.TemporaryDirectory(prefix="execute-process-inputs-") as directory:
        folder = Path(directory)
        for case, variants in sources.items():
            commands, raw = {}, {name: [] for name in variants}
            for name, source in variants.items():
                path = folder / (
                    case + "-" + name + (".sh" if name == "bash" else ".program")
                )
                path.write_text(source)
                commands[name] = [
                    str(binaries[name]),
                    *(["--noprofile", "--norc", "-p"] if name == "bash" else []),
                    str(path),
                ]
                if name != "bash":
                    commands[name] += ["--manifest-dir", str(ROOT / "manifests")]
            for trial in range(args.samples + 10):
                order = list(commands)
                rng.shuffle(order)
                for name in order:
                    start = time.perf_counter_ns()
                    result = subprocess.run(
                        commands[name],
                        cwd=folder,
                        env=env,
                        capture_output=True,
                        timeout=5,
                    )
                    elapsed = (time.perf_counter_ns() - start) / 1000
                    require(
                        (result.returncode, result.stdout, result.stderr)
                        == (0, expected, b""),
                        f"{case}/{name}: output/status oracle failed: {result.stderr!r}",
                    )
                    if trial >= 10:
                        raw[name].append(elapsed)
            results[case] = {name: summary(values) for name, values in raw.items()}
            spawns[case] = {}
            for name in ("baseline", "candidate"):
                result = subprocess.run(
                    commands[name] + ["--trace"],
                    cwd=folder,
                    env=env,
                    capture_output=True,
                    timeout=5,
                )
                require(
                    result.returncode == 0 and result.stdout == expected,
                    "Trace outcome differs",
                )
                events = [json.loads(line) for line in result.stderr.splitlines()]
                observed = [
                    event["binary"] for event in events if event.get("event") == "spawn"
                ]
                wanted = (
                    [printenv, jq, printf]
                    if case == "environment" and name == "baseline"
                    else [printf]
                )
                require(
                    observed == wanted,
                    f"{case}/{name}: native spawn oracle failed: {observed}",
                )
                spawns[case][name] = observed
    report = {
        "version": 1,
        "status": "passed",
        "recorded_at_utc": datetime.now(timezone.utc).isoformat(),
        "platform": platform.platform(),
        "machine": platform.machine(),
        "samples_per_variant": args.samples,
        "excluded_warmups_per_variant": 10,
        "seed": args.seed,
        "method": "Seeded randomized paired rounds; fresh process; warm filesystem caches. Python launch/capture and OS scheduling included. Exact stdout, stderr and exit-code checks every invocation; spawn traces outside timing. No concurrent test/build work requested. Background host activity uncontrolled. Environment workload removes two native helpers; literal control retains equivalent commands. Bash executes the same external printf. No inference, desktop, cold-start, memory, Linux or general interpreter speed claim.",
        "binaries": {
            name: {
                "sha256": digest(path),
                "version": subprocess.check_output([str(path), "--version"], env=env)
                .decode()
                .splitlines()[0],
            }
            for name, path in binaries.items()
        },
        "native_tools": {
            name: {"path": path, "sha256": digest(Path(path))}
            for name, path in {"printf": printf, "printenv": printenv, "jq": jq}.items()
        },
        "harness_sha256": digest(Path(__file__)),
        "sources": sources,
        "native_spawns": spawns,
        "expected_stdout_sha256": hashlib.sha256(expected).hexdigest(),
        "results": results,
    }
    args.report.parent.mkdir(parents=True, exist_ok=True)
    args.report.write_text(json.dumps(report, indent=2, ensure_ascii=False) + "\n")
    print(
        json.dumps(
            {
                case: {
                    name: {k: v for k, v in values.items() if k != "raw_us"}
                    for name, values in variants.items()
                }
                for case, variants in results.items()
            },
            indent=2,
        )
    )


if __name__ == "__main__":
    main()
