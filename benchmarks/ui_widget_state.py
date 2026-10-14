#!/usr/bin/env python3
"""Paired real widget edits with growing table state, using owned headless servers."""
import argparse
from datetime import datetime, timezone
import hashlib
import json
from pathlib import Path
import platform
import random
import time

from direct_commands import summary
from live_state_scaling import live

PROJECT = Path(__file__).resolve().parents[1]
SOURCE = (PROJECT / "examples/live/widget-console.program").read_text()
ROWS = "/ui/programs/dashboard/widgets/metrics/rows"
MESSAGE = "/ui/programs/console/widgets/message/text"


def digest(value):
    return hashlib.sha256(value).hexdigest()


def applied(app, revision):
    deadline = time.monotonic() + 10
    while True:
        response = app.ok("read", path="/runtime/ui_applied_revision")
        if response["result"] >= revision:
            return
        app.ok(
            "wait",
            path="/runtime/ui_applied_revision",
            after=response["revision"],
            timeout_ms=100,
        )
        assert time.monotonic() < deadline, "renderer did not apply widget edit"


def trial(binary, rows):
    live.BINARY = binary
    with live.Session(SOURCE, headless=True) as app:
        app.until(lambda state: "ready" in state["runtime"]["frame"])
        # Bounded transport chunks; table construction and loading are untimed.
        for start in range(0, len(rows), 64):
            response = app.ok(
                "patch",
                operations=[
                    {"op": "add", "path": ROWS + "/-", "value": row}
                    for row in rows[start : start + 64]
                ],
            )
            applied(app, response["revision"])
        totals = {"ack": 0, "applied": 0}
        frames = []
        for index in range(8):
            text = f"edit{index:04}"
            started = time.perf_counter_ns()
            response = app.ok("update", path=MESSAGE, value=text)
            totals["ack"] += (time.perf_counter_ns() - started) / 1000
            applied(app, response["revision"])
            totals["applied"] += (time.perf_counter_ns() - started) / 1000
            frame = app.ok("read", path="/runtime/frame")["result"]
            assert text in frame
            frames.append(digest(frame.encode()))
        assert app.ok("read", path=ROWS)["result"] == rows
        assert app.ok("read", path=MESSAGE)["result"] == "edit0007"
        assert app.ok("read", path="/runtime/ui_errors")["result"] == {}
        assert app.ok("read", path="/runtime/in_flight")["result"] is False
    assert app.child.returncode == 0, "owned renderer did not exit cleanly"
    return totals, frames


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--baseline", type=Path, required=True)
    parser.add_argument("--candidate", type=Path, required=True)
    parser.add_argument("--samples", type=int, default=12)
    parser.add_argument("--report", type=Path, required=True)
    args = parser.parse_args()
    if not 5 <= args.samples <= 100:
        parser.error("use 5..100 samples")
    binaries = {
        name: getattr(args, name).resolve() for name in ("baseline", "candidate")
    }
    results = {}
    rng = random.Random(202610142247)
    for count in (0, 16, 1024):
        rows = [
            {"name": f"row {index}", "value": index, "padding": "x" * 4096}
            for index in range(count)
        ]
        raw = {name: {"ack": [], "applied": []} for name in binaries}
        order = []
        expected_frames = None
        for index in range(args.samples + 2):
            labels = list(binaries)
            rng.shuffle(labels)
            order.append(labels)
            for name in labels:
                totals, frames = trial(binaries[name], rows)
                if expected_frames is None:
                    expected_frames = frames
                assert frames == expected_frames, "complete renderer frames differ"
                if index >= 2:
                    for metric, value in totals.items():
                        raw[name][metric].append(value)
        results[str(count)] = {
            "rows_sha256": digest(json.dumps(rows, sort_keys=True).encode()),
            "serialized_rows_bytes": len(json.dumps(rows).encode()),
            "per_edit_full_frame_sha256": expected_frames,
            "order_including_two_excluded_warmups": order,
            "variants": {
                name: {metric: summary(values) for metric, values in metrics.items()}
                for name, metrics in raw.items()
            },
        }
    report = {
        "recorded_at_utc": datetime.now(timezone.utc).isoformat(),
        "platform": platform.platform(),
        "source_sha256": digest(SOURCE.encode()),
        "binaries": {
            name: digest(path.read_bytes()) for name, path in binaries.items()
        },
        "harness_sha256": digest(Path(__file__).read_bytes()),
        "helper_sha256": digest((PROJECT / "tests/live_control.py").read_bytes()),
        "catalog_sha256": {
            path.name: digest(path.read_bytes())
            for path in sorted((PROJECT / "manifests").glob("*.json"))
        },
        "method": "Owned paused headless real renderers, no native jobs or providers. Startup and table loading excluded. Each trial sums eight edit-to-ack and edit-to-applied segments. Applied observation includes projected RPC/long-poll/Python/socket overhead. Every complete frame hash matches across variants/trials; independent edited text, full retained rows, last text and empty UI errors checked outside timing. Two excluded warmup trials, seeded shuffled pairs, warm filesystem, serial measurement without builds/stress. Not event-to-photon, cold start, RSS or Linux timing.",
        "edits_per_trial": 8,
        "cleanup": "all owned servers stopped, reaped with status zero and temporary workspaces removed",
        "results": results,
    }
    args.report.write_text(json.dumps(report, indent=2) + "\n")
    print("Widget state latency and output oracles passed.")


if __name__ == "__main__":
    main()
