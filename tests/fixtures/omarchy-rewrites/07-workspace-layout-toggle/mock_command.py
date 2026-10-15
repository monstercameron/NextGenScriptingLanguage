#!/usr/bin/env python3
"""Owned fixture endpoints only; never calls a desktop service."""
import json
import os
from pathlib import Path
import sys

root = Path(os.environ["FIXTURE_ROOT"])
config = json.loads((root / "config.json").read_text())
name = Path(sys.argv[0]).name
argv = sys.argv[1:]
log = root / "commands.jsonl"
previous = [json.loads(line) for line in log.read_text().splitlines()] if log.exists() else []
home = Path(os.environ["HOME"])
files = {str(path.relative_to(home)): path.read_bytes().decode("utf-8") for path in sorted(home.rglob("*")) if path.is_file()}
with log.open("a") as out:
    out.write(json.dumps({"command": name, "argv": argv, "home_files": files}, ensure_ascii=False) + "\n")
if name == "hyprctl":
    if argv == ["activeworkspace", "-j"]:
        count = sum(item["command"] == name and item["argv"] == argv for item in previous)
        response = config["workspaces"][min(count, len(config["workspaces"]) - 1)]
    elif argv and argv[0] == "eval":
        response = config.get("eval", {})
    elif argv[:2] == ["keyword", "workspace"]:
        response = config.get("keyword", {})
    else:
        raise RuntimeError("unexpected hyprctl fixture invocation")
else:
    assert name == "omarchy-notification-send"
    response = config.get("notification", {})
sys.stdout.write(response.get("stdout", ""))
sys.stderr.write(response.get("stderr", ""))
sys.exit(response.get("status", 0))
