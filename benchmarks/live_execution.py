#!/usr/bin/env python3
"""Paired managed-live execution including RPC and state publication overhead."""
import argparse
from datetime import datetime, timezone
import hashlib
import importlib.util
import json
from pathlib import Path
import platform
import random
import sys
import time

from direct_commands import summary

ROOT = Path(__file__).resolve().parents[1]
saved_args = sys.argv
sys.argv = [str(ROOT / "tests/live_control.py")]
spec = importlib.util.spec_from_file_location(
    "live_control", ROOT / "tests/live_control.py"
)
live = importlib.util.module_from_spec(spec)
spec.loader.exec_module(live)
sys.argv = saved_args


def digest(data):
    return hashlib.sha256(data).hexdigest()


def trial(binary, padding, nodes):
    live.BINARY = binary
    source = "then encode 42 as json\n" * nodes
    with live.Session(source) as app:
        bindings = {
            f"pad_{index}": "x" * min(500_000, padding - start)
            for index, start in enumerate(range(0, padding, 500_000))
        }
        for key, value in bindings.items():
            app.ok("create", path=f"/bindings/{key}", value=value)
        started = time.perf_counter_ns()
        response = app.action("resume")
        deadline = time.monotonic() + 30
        while True:
            response = app.ok(
                "wait", path="/runtime", after=response["revision"], timeout_ms=1000
            )
            runtime = response["result"]["snapshot"]
            if runtime["status"] in ("failed", "completed"):
                break
            if time.monotonic() >= deadline:
                raise RuntimeError("managed execution timed out")
        elapsed = (time.perf_counter_ns() - started) / 1000
        assert runtime["status"] == "completed", runtime
        assert runtime["nodes_completed"] == nodes, runtime
        assert runtime["completed_native_launches"] == 0, runtime
        assert runtime["cache_hits"] == 0, runtime
        assert runtime["last_output"] == {"text": "42"}, runtime
        assert not runtime["effects_may_be_incomplete"], runtime
        assert app.ok("read", path="/bindings")["result"] == bindings
        assert app.ok("read", path="/input")["result"] == {"text": "42"}
        assert app.ok("read", path="/cursor")["result"] == nodes
        return elapsed


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--baseline", type=Path, required=True)
    parser.add_argument("--candidate", type=Path, required=True)
    parser.add_argument("--samples", type=int, default=12)
    parser.add_argument("--nodes", type=int, default=32)
    parser.add_argument("--report", type=Path, required=True)
    args = parser.parse_args()
    if not 5 <= args.samples <= 100 or not 1 <= args.nodes <= 128:
        parser.error("use 5..100 samples and 1..128 nodes")
    binaries = {
        name: getattr(args, name).resolve() for name in ("baseline", "candidate")
    }
    rng = random.Random(202610142153)
    results = {}
    for padding in (0, 64 * 1024, 1024 * 1024, 4 * 1024 * 1024):
        raw = {name: [] for name in binaries}
        orders = []
        for index in range(args.samples + 2):
            order = list(binaries)
            rng.shuffle(order)
            orders.append(order)
            for name in order:
                elapsed = trial(binaries[name], padding, args.nodes)
                if index >= 2:
                    raw[name].append(elapsed)
        results[str(padding)] = {
            "variants": {name: summary(values) for name, values in raw.items()},
            "order_including_two_excluded_warmups": orders,
            "bindings_sha256": digest(
                json.dumps(
                    {
                        f"pad_{n}": "x" * min(500_000, padding - start)
                        for n, start in enumerate(range(0, padding, 500_000))
                    },
                    sort_keys=True,
                ).encode()
            ),
        }
    catalog = [
        {"name": file.name, "sha256": digest(file.read_bytes())}
        for file in sorted((ROOT / "manifests").glob("*.json"))
    ]
    report = {
        "recorded_at_utc": datetime.now(timezone.utc).isoformat(),
        "platform": platform.platform(),
        "source_sha256": digest(("then encode 42 as json\n" * args.nodes).encode()),
        "nodes_per_trial": args.nodes,
        "binaries": {
            name: digest(path.read_bytes()) for name, path in binaries.items()
        },
        "catalog": catalog,
        "harness_sha256": digest(Path(__file__).read_bytes()),
        "helper_sha256": digest((ROOT / "tests/live_control.py").read_bytes()),
        "method": "Fresh paused managed-live server per trial, full identical catalogs. Binding setup and final exact bindings/input/cursor/zero-native-process oracles excluded. Resume through completed wait/RPC response included: socket/Python/JSON transport, node execution, state budget checks/publication and revision waiting. Two excluded warmups per variant/case, seeded shuffled paired order, warm filesystem. No concurrent builds/tests. No provider or child applet calls. Not CLI startup, isolated interpreter latency, allocation or RSS measurement.",
        "results": results,
    }
    args.report.write_text(json.dumps(report, indent=2) + "\n")
    print(
        json.dumps(
            {case: value["variants"] for case, value in results.items()}, indent=2
        )
    )


if __name__ == "__main__":
    main()
