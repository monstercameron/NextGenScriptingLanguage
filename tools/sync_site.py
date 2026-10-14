#!/usr/bin/env python3
"""Refresh the curated site's script copies and exact benchmark receipts."""
import hashlib
import json
from pathlib import Path
import shutil

ROOT = Path(__file__).resolve().parents[1]
ASSETS = ROOT / "docs/assets"
path = ASSETS / "data/examples.json"
entries = json.loads(path.read_text())
for entry in entries:
    source = ROOT / entry["file"]
    if not source.resolve().is_relative_to(ROOT / "examples"):
        raise ValueError("Site examples must come from examples/")
    entry["source"] = source.read_text()
    entry["sha256"] = hashlib.sha256(source.read_bytes()).hexdigest()
    shutil.copyfile(source, ROOT / "docs" / entry["download"])
path.write_text(json.dumps(entries, indent=2) + "\n")
for filename in [
    "process-inputs-v0.9.0-dev-macos-arm64.json",
    "process-inputs-v0.9.0-dev-macos-arm64-repeat.json",
]:
    shutil.copyfile(ROOT / "benchmarks" / filename, ASSETS / "data" / filename)
print(
    f"Refreshed {len(entries)} canonical script copies and two exact benchmark receipts."
)
