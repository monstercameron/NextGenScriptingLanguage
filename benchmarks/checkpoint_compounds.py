#!/usr/bin/env python3
"""Paired completed compound-checkpoint validation, with replay and identity oracles."""
import argparse
from datetime import datetime, timezone
import hashlib
import json
from pathlib import Path
import platform
import random
import shutil
import subprocess
import tempfile
import time

from direct_commands import summary

ROOT = Path(__file__).resolve().parents[1]


def digest(data):
    return hashlib.sha256(data).hexdigest()


def run(command, cwd):
    return subprocess.run(command, cwd=cwd, input=b"", capture_output=True, timeout=20)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--baseline", type=Path, required=True)
    parser.add_argument("--candidate", type=Path, required=True)
    parser.add_argument("--samples", type=int, default=80)
    parser.add_argument("--report", type=Path, required=True)
    args = parser.parse_args()
    if not 20 <= args.samples <= 2000:
        parser.error("use 20..2000 measured samples")
    binaries = {
        name: getattr(args, name).resolve() for name in ("baseline", "candidate")
    }
    native = Path(shutil.which("true")).resolve()
    sources = {
        "json_1": "repeat 0 times\n  then encode 42 as json\n",
        "json_64": "repeat 0 times\n" + "  then encode 42 as json\n" * 64,
        "json_1000": "repeat 0 times\n" + "  then encode 42 as json\n" * 1000,
        "reusable_256": 'define a program named worker\n  accept payload as text with default "'
        + "x" * 1024
        + '"\n  then encode payload as json\nrepeat 0 times\n'
        + "  run program worker\n" * 256,
        "native_256": "repeat 0 times\n" + f"  run {json.dumps(str(native))}\n" * 256,
    }
    rng = random.Random(202610142201)
    results = {}
    with tempfile.TemporaryDirectory(
        prefix="execute-compound-checkpoint-"
    ) as directory:
        cwd = Path(directory)
        for name, body in sources.items():
            source = (
                body + 'then encode "stable output" as json\nthen save to "effect"\n'
            )
            file = cwd / (name + ".program")
            file.write_text(source)
            checkpoint = cwd / name
            common = [
                str(file),
                "--checkpoint",
                str(checkpoint),
                "--manifest-dir",
                str(ROOT / "manifests"),
            ]
            setup = run([str(binaries["baseline"]), *common], cwd)
            assert (
                setup.returncode == 0
                and setup.stdout == b'"stable output"'
                and not setup.stderr
            ), setup
            effect = cwd / "effect"
            assert effect.read_bytes() == b'"stable output"'
            effect.unlink()
            before = (checkpoint / "journal.json").read_bytes()
            raw = {name: [] for name in binaries}
            orders = []
            for index in range(args.samples + 5):
                order = list(binaries)
                rng.shuffle(order)
                orders.append(order)
                for variant in order:
                    started = time.perf_counter_ns()
                    result = run([str(binaries[variant]), "resume", *common], cwd)
                    elapsed = (time.perf_counter_ns() - started) / 1000
                    assert (
                        result.returncode == 0
                        and result.stdout == b'"stable output"'
                        and not result.stderr
                    ), result
                    assert not effect.exists(), "completed save was replayed"
                    assert (
                        checkpoint / "journal.json"
                    ).read_bytes() == before, "completed journal changed"
                    if index >= 5:
                        raw[variant].append(elapsed)
            for variant, binary in binaries.items():
                probe = run([str(binary), "resume", *common, "--trace"], cwd)
                assert (
                    probe.returncode == 0 and probe.stdout == b'"stable output"'
                ), probe
                events = [json.loads(line) for line in probe.stderr.splitlines()]
                assert not any(event["event"] == "spawn" for event in events)
                assert any(
                    event["event"] == "checkpoint_resume"
                    and event["restored_nodes"] == 3
                    for event in events
                )
                changed = source.replace(
                    'then encode "stable output"', 'then encode "changed output"'
                )
                file.write_text(changed)
                rejected = run([str(binary), "resume", *common], cwd)
                assert rejected.returncode != 0 and not rejected.stdout
                assert b"completed checkpoint node changed" in rejected.stderr, rejected
                assert not effect.exists()
                assert (checkpoint / "journal.json").read_bytes() == before
                file.write_text(source)
            results[name] = {
                "source_sha256": digest(source.encode()),
                "checkpoint_sha256": digest(before),
                "variants": {key: summary(values) for key, values in raw.items()},
                "order_including_five_excluded_warmups": orders,
                "oracles": "baseline checkpoint resumes on both binaries; exact restored output, 3 restored nodes, no native spawn, no save replay, unchanged journal bytes; changed completed node rejected without mutation",
            }
    report = {
        "recorded_at_utc": datetime.now(timezone.utc).isoformat(),
        "platform": platform.platform(),
        "binaries": {
            name: digest(binary.read_bytes()) for name, binary in binaries.items()
        },
        "harness_sha256": digest(Path(__file__).read_bytes()),
        "native_fingerprinted_only": {
            "path": str(native),
            "sha256": digest(native.read_bytes()),
        },
        "catalog": [
            {"name": path.name, "sha256": digest(path.read_bytes())}
            for path in sorted((ROOT / "manifests").glob("*.json"))
        ],
        "method": "Same baseline-created completed compound checkpoints, identical source/cwd/catalog/options and process environment for each variant. Fresh CLI resume timing includes Python spawn/wait/capture, loading, parsing, validation, semantic/executable fingerprints and state restore. Setup, filesystem oracle reads and separate trace/mutation checks excluded. Five warmups, shuffled paired order, warm filesystem. No concurrent builds/tests; no provider calls. No durability/fsync change, first-run timing, peak/RSS or other-platform/Bash claim.",
        "results": results,
    }
    args.report.write_text(json.dumps(report, indent=2) + "\n")
    print(
        json.dumps(
            {name: value["variants"] for name, value in results.items()}, indent=2
        )
    )


if __name__ == "__main__":
    main()
