#!/usr/bin/env python3
"""Compare identical static-check workloads; no applet commands execute."""
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


def workloads():
    literal = json.dumps(["%s", "literal argument " * 4])
    literals = json.dumps(["%s"] + ["literal argument " * 4] * 128)
    mixed = literals[:-1] + ", value]"
    default = 'accept value as text with default "owned value"\n'
    return {
        "literal_no_inputs_control": f'run "printf" with arguments {literal}\n' * 1000,
        "literal_argv_1000": default
        + f'run "printf" with arguments {literal}\n' * 1000,
        "scalar_reference_1000": default
        + 'run "printf" with arguments ["%s", value]\n' * 1000,
        "mixed_argv_128": default + f'run "printf" with arguments {mixed}\n' * 128,
        "whole_list_reference_128": f"accept argv as list of text with default {literals}\n"
        + 'run "printf" with arguments argv\n' * 128,
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--baseline", type=Path, required=True)
    parser.add_argument("--candidate", type=Path, required=True)
    parser.add_argument("--report", type=Path, required=True)
    parser.add_argument("--samples", type=int, default=120)
    args = parser.parse_args()
    if not 20 <= args.samples <= 2000:
        parser.error("--samples must be between 20 and 2000")
    binaries = {
        name: getattr(args, name).resolve() for name in ["baseline", "candidate"]
    }
    rng = random.Random(202610142017)
    results = {}
    with tempfile.TemporaryDirectory(prefix="execute-preflight-bench-") as directory:
        root = Path(directory)
        for name, source in workloads().items():
            file = root / (name + ".program")
            file.write_text(source)
            commands = {
                variant: [
                    str(binary),
                    "check",
                    str(file),
                    "--manifest-dir",
                    str(PROJECT / "manifests"),
                ]
                for variant, binary in binaries.items()
            }
            plans = []
            for binary in binaries.values():
                result = subprocess.run(
                    [
                        str(binary),
                        "explain",
                        str(file),
                        "--manifest-dir",
                        str(PROJECT / "manifests"),
                    ],
                    cwd=root,
                    input=b"",
                    capture_output=True,
                    timeout=20,
                )
                if result.returncode != 0 or result.stderr:
                    raise RuntimeError((name, result.returncode, result.stderr))
                plans.append(result.stdout)
            if plans[0] != plans[1]:
                raise RuntimeError(f"{name}: resolved plans differ")
            raw = {variant: [] for variant in binaries}
            for trial in range(args.samples + 5):
                order = list(binaries)
                rng.shuffle(order)
                for variant in order:
                    start = time.perf_counter_ns()
                    result = subprocess.run(
                        commands[variant],
                        cwd=root,
                        input=b"",
                        capture_output=True,
                        timeout=20,
                    )
                    elapsed = (time.perf_counter_ns() - start) / 1000
                    if result.returncode != 0 or result.stdout or result.stderr:
                        raise RuntimeError(
                            (
                                name,
                                variant,
                                result.returncode,
                                result.stdout,
                                result.stderr,
                            )
                        )
                    if trial >= 5:
                        raw[variant].append(elapsed)
            results[name] = {
                "source_bytes": len(source.encode()),
                "source_sha256": hashlib.sha256(source.encode()).hexdigest(),
                "identical_resolved_plan_sha256": hashlib.sha256(plans[0]).hexdigest(),
                "timings_us": {
                    variant: summary(values) for variant, values in raw.items()
                },
            }
    report = {
        "at_utc": datetime.now(timezone.utc).isoformat(),
        "platform": platform.platform(),
        "binaries": {
            name: {"path": str(path), "sha256": digest(path)}
            for name, path in binaries.items()
        },
        "harness_sha256": digest(Path(__file__)),
        "catalog_sha256": {
            path.name: digest(path)
            for path in sorted((PROJECT / "manifests").glob("*.json"))
        },
        "method": "Same sources, cwd, arguments, inherited environment and catalog across preserved release binaries. Five warmups and shuffled interleaved fresh-process trials; exact successful status and empty stdout/stderr checked. Identical resolved explain plans verified outside timing. No applet executes. Includes CLI startup, parsing, static validation and known-input preflight. Warm filesystem caches, uncontrolled host background activity. Run without concurrent builds/stress; not native command execution, cold-cache, Linux/Windows or universal Bash evidence.",
        "results": results,
    }
    args.report.write_text(json.dumps(report, indent=2) + "\n")
    print(
        json.dumps(
            {
                name: {
                    variant: {
                        key: value for key, value in times.items() if key != "raw_us"
                    }
                    for variant, times in data["timings_us"].items()
                }
                for name, data in results.items()
            },
            indent=2,
        )
    )


if __name__ == "__main__":
    main()
