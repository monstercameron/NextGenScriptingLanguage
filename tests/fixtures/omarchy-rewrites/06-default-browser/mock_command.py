#!/usr/bin/env python3
"""Private external boundary mock; all browser mapping remains in the program."""
import json
import os
from pathlib import Path
import sys

root = Path(os.environ["FIXTURE_ROOT"])
name = Path(sys.argv[0]).name
argv = sys.argv[1:]
record = {"command": name, "argv": argv, "browser_present": "BROWSER" in os.environ}
with (root / "commands.jsonl").open("a") as stream:
    stream.write(json.dumps(record, ensure_ascii=False) + "\n")
config = json.loads((root / "config.json").read_text())
if name == "xdg-settings":
    if argv == ["get", "default-web-browser"]:
        count_file = root / "get-count"
        count = int(count_file.read_text()) if count_file.exists() else 0
        count_file.write_text(str(count + 1))
        responses = config["get"]
        response = responses[min(count, len(responses) - 1)]
    elif len(argv) == 3 and argv[:2] == ["set", "default-web-browser"]:
        response = config["set"]
        if response.get("status", 0) == 0:
            (root / "browser-state").write_text(argv[2])
    else:
        raise AssertionError(argv)
else:
    assert name == "omarchy-notification-send", name
    response = config["notification"]
    with (root / "notifications.jsonl").open("a") as stream:
        stream.write(json.dumps(argv, ensure_ascii=False) + "\n")
sys.stdout.write(response.get("stdout", ""))
sys.stderr.write(response.get("stderr", ""))
sys.exit(response.get("status", 0))
