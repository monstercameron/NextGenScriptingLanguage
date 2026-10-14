#!/usr/bin/env python3
"""Compare identical typed-input interfaces on preserved native release binaries."""
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

ROOT = Path(__file__).resolve().parents[1]


def digest(data):
    return hashlib.sha256(data).hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--baseline", type=Path, required=True)
    parser.add_argument("--candidate", type=Path, required=True)
    parser.add_argument("--samples", type=int, default=120)
    parser.add_argument("--report", type=Path, required=True)
    args = parser.parse_args()
    if not 20 <= args.samples <= 2000:
        parser.error("--samples must be between 20 and 2000")
    binaries = {
        name: getattr(args, name).resolve() for name in ["baseline", "candidate"]
    }
    rng = random.Random(202610142112)
    results = {}
    with tempfile.TemporaryDirectory(prefix="execute-input-interface-") as directory:
        cwd = Path(directory)
        for count in [0, 1, 8, 64, 1000]:
            values = {f"p_{n:04}": n + 5 for n in range(count)}
            source = "".join(
                f"accept {name} as integer with default 42\n" for name in values
            )
            source += (
                f'then encode {next(reversed(values)) if values else "42"} as json\n'
            )
            file = cwd / "interface.program"
            file.write_text(source)
            supplied = [
                item
                for name, value in values.items()
                for item in ["--input", f"{name}={value}"]
            ]
            common = [str(file), "--manifest-dir", str(ROOT / "manifests"), *supplied]
            inspected = {}
            for name, binary in binaries.items():
                probe = subprocess.run(
                    [str(binary), "inspect", *common],
                    cwd=cwd,
                    input=b"",
                    capture_output=True,
                    timeout=10,
                )
                assert probe.returncode == 0 and not probe.stderr, (name, probe)
                interface = json.loads(probe.stdout)
                assert interface["inputs_complete"] is True
                assert {
                    item["name"]: item["value"] for item in interface["inputs"]
                } == values
                assert all(
                    item["type"] == "integer" and item["default"] == 42
                    for item in interface["inputs"]
                )
                inspected[name] = probe.stdout
                probe = subprocess.run(
                    [str(binary), *common, "--trace"],
                    cwd=cwd,
                    input=b"",
                    capture_output=True,
                    timeout=10,
                )
                assert (
                    probe.returncode == 0
                    and probe.stdout == str(count + 4 if count else 42).encode()
                ), probe
                assert all(
                    json.loads(line).get("event") != "spawn"
                    for line in probe.stderr.splitlines()
                )
            assert inspected["baseline"] == inspected["candidate"]
            for action in ["check", "run", "inspect"]:
                oracle = {
                    "check": b"",
                    "run": str(count + 4 if count else 42).encode(),
                    "inspect": inspected["baseline"],
                }[action]
                commands = {
                    name: [str(binary), action, *common]
                    for name, binary in binaries.items()
                }
                raw = {name: [] for name in binaries}
                for trial in range(args.samples + 5):
                    order = list(binaries)
                    rng.shuffle(order)
                    for name in order:
                        started = time.perf_counter_ns()
                        result = subprocess.run(
                            commands[name],
                            cwd=cwd,
                            input=b"",
                            capture_output=True,
                            timeout=10,
                        )
                        elapsed = (time.perf_counter_ns() - started) / 1000
                        assert (
                            result.returncode == 0
                            and result.stdout == oracle
                            and not result.stderr
                        ), (count, action, name, result)
                        if trial >= 5:
                            raw[name].append(elapsed)
                results[f"{action}_{count:04}"] = {
                    "declarations": count,
                    "source_sha256": digest(source.encode()),
                    "supplied_inputs_sha256": digest(
                        json.dumps(values, sort_keys=True).encode()
                    ),
                    "output_sha256": digest(oracle),
                    "output_bytes": len(oracle),
                    "timings_us": {
                        name: summary(samples) for name, samples in raw.items()
                    },
                }
    report = {
        "recorded_at_utc": datetime.now(timezone.utc).isoformat(),
        "platform": platform.platform(),
        "binary_sha256": {
            name: digest(path.read_bytes()) for name, path in binaries.items()
        },
        "harness_sha256": digest(Path(__file__).read_bytes()),
        "catalog_sha256": {
            p.name: digest(p.read_bytes())
            for p in sorted((ROOT / "manifests").glob("*.json"))
        },
        "method": "Identical source, inputs and catalog on preserved native release binaries. Fresh CLI processes, five excluded warmups per case and randomly interleaved serial samples, warm filesystem cache. Includes process startup, parse, validation, typed resolution, execution/inspection and output transport. Every output/status/stderr checked; full typed interface and zero native launches verified outside timing. No concurrent builds/tests. Large 1000-input schemas are scaling probes, not typical scripts, cold-cache results or a Bash comparison.",
        "results": results,
    }
    args.report.write_text(json.dumps(report, indent=2) + "\n")
    print(
        json.dumps(
            {
                key: {
                    name: {k: v for k, v in val.items() if k != "raw_us"}
                    for name, val in case["timings_us"].items()
                }
                for key, case in results.items()
            },
            indent=2,
        )
    )


if __name__ == "__main__":
    main()
