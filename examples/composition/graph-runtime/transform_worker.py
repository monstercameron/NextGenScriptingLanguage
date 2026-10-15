#!/usr/bin/env python3
"""Persistent JSON-lines transformer used by the graph composition example."""

import json
import os
import sys
import time


if len(sys.argv) != 4 or sys.argv[1] != "--worker":
    raise SystemExit("expected --worker PID_FILE EFFECTS_FILE")

pid_file, effects_file = sys.argv[2:]
with open(pid_file, "a", encoding="ascii") as marker:
    marker.write(f"{os.getpid()}\n")

sequence = 0
for line in sys.stdin.buffer:
    request = json.loads(line)
    payload = json.loads(bytes(request["input"]))
    sequence += 1
    text = payload["text"]
    with open(effects_file, "a", encoding="utf-8") as effects:
        effects.write(f"{text}\n")

    # Keep processing observably slower than the loopback producer so the
    # capacity-one lossless edge exercises backpressure without dropping data.
    time.sleep(0.15)
    output = json.dumps(
        {"text": f"worker:{text}#{sequence}"}, separators=(",", ":")
    ).encode("utf-8")
    response = {
        "id": request["id"],
        "status": 0,
        "output": list(output),
    }
    sys.stdout.buffer.write(json.dumps(response, separators=(",", ":")).encode())
    sys.stdout.buffer.write(b"\n")
    sys.stdout.buffer.flush()
