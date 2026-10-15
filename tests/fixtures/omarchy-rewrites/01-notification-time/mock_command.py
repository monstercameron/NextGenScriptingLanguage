#!/usr/bin/env python3
"""Boundary mocks only: date output and notification request recording."""
import json
import os
from pathlib import Path
import sys

root = Path(os.environ["FIXTURE_ROOT"])
config = json.loads((root / "config.json").read_text())
name = Path(sys.argv[0]).name
with (root / "commands.jsonl").open("a") as log:
    log.write(json.dumps({"command": name, "argv": sys.argv[1:]}, ensure_ascii=False) + "\n")
if name == "date":
    result = config["date"]
elif name == "omarchy-notification-send":
    with (root / "notifications.jsonl").open("a") as log:
        log.write(json.dumps(sys.argv[1:], ensure_ascii=False) + "\n")
    result = config["notification"]
else:
    raise SystemExit("unexpected command: " + name)
sys.stdout.write(result["stdout"])
sys.stderr.write(result["stderr"])
raise SystemExit(result["status"])
