#!/usr/bin/env python3
"""Response-only audio endpoints. No system commands or audio APIs are called."""
import json
import os
from pathlib import Path
import sys

root = Path(os.environ["FIXTURE_ROOT"])
config = json.loads((root / "config.json").read_text())
name = Path(sys.argv[0]).name
argv = sys.argv[1:]
log = root / "commands.jsonl"
prior = [json.loads(line) for line in log.read_text().splitlines()] if log.exists() else []
runtime = root / "runtime with spaces"
files = {str(p.relative_to(runtime)): p.read_text() for p in sorted(runtime.rglob("*")) if p.is_file()}
with log.open("a") as stream:
    stream.write(json.dumps({"command": name, "argv": argv, "runtime_files": files}) + "\n")
if name == "pactl":
    key = argv[0]
elif name == "omarchy-audio-output-sink":
    key = "sink"
elif name == "date":
    assert argv == ["+%s%3N"]
    key = "date"
else:
    assert name == "omarchy-osd"
    key = "osd"
responses = config.get(key, [{}])
if isinstance(responses, dict):
    responses = [responses]
count = sum(item["command"] == name and (name != "pactl" or item["argv"][0] == key) for item in prior)
response = responses[min(count, len(responses) - 1)]
sys.stdout.write(response.get("stdout", ""))
sys.stderr.write(response.get("stderr", ""))
sys.exit(response.get("status", 0))
