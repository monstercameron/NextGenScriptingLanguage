#!/usr/bin/env python3
"""Reference validation of explain schemas; requires `pip install jsonschema`.

Run after building execute: python3 tests/explain_schema_reference.py [path/to/execute].
Unsupported screen, stream and graph programs and the separately installed module
example do not emit explain JSON and are checked as expected CLI failures.
"""
import json
import pathlib
import subprocess
import sys

from jsonschema import Draft202012Validator

ROOT = pathlib.Path(__file__).resolve().parents[1]
BINARY = pathlib.Path(sys.argv[1]).resolve() if len(sys.argv) > 1 else ROOT / "target/debug/execute"
MANIFESTS = ROOT / "manifests"
validators = {}
for version in (1, 2):
    schema = json.loads((ROOT / f"docs/schemas/explain.v{version}.schema.json").read_text())
    Draft202012Validator.check_schema(schema)
    validators[version] = Draft202012Validator(schema)

counts = {1: 0, 2: 0}
for source in sorted((ROOT / "examples").rglob("*.program")):
    for version in (1, 2):
        result = subprocess.run([str(BINARY), "explain", str(source), "--json",
                                 "--explain-version", str(version), "--manifest-dir", str(MANIFESTS)],
                                cwd=ROOT, capture_output=True, text=True)
        if result.returncode:
            unsupported = "explain --json applies only to finite programs" in result.stderr
            external = source == ROOT / "examples/modules/caller.program" and 'unknown applet "examples.records.report"' in result.stderr
            if not (unsupported or external):
                raise AssertionError(f"{source}: {result.stderr}")
            continue
        plan = json.loads(result.stdout)
        errors = list(validators[version].iter_errors(plan))
        if errors:
            raise AssertionError(f"{source} v{version}: {errors[0]}")
        if version == 2:
            invocation = next((row for row in plan["steps"] if row["kind"] == "invoke"), None)
            if invocation:
                original = invocation["argv"]
                invocation["argv"] = None
                if validators[2].is_valid(plan):
                    raise AssertionError(f"{source}: null invoke argv accepted")
                invocation["argv"] = [{"ref": 42}]
                if validators[2].is_valid(plan):
                    raise AssertionError(f"{source}: malformed reference accepted")
                invocation["argv"] = original
        counts[version] += 1
assert min(counts.values()) >= 100, counts
print(f"Validated example plans with jsonschema Draft 2020-12: {counts}")
