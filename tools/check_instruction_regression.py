#!/usr/bin/env python3
"""Fail when CPU-bound Execute instruction counts regress over a stored report.

Both inputs are JSON reports produced by tools/bash_compare.py. The baseline
report must come from the same OS family, machine architecture, and manifest
selection as the candidate. A baseline is intentionally required: this tool
never invents one from a current run.

Only CPU-bound scenarios are gated: rows where the Execute arm is dominated by
interpreter work rather than child processes or filesystem traffic, so the
`instructions:u` count is a stable signal for the optimization backlog (#305).
Every other scenario's instruction counts are reported for stability but never
fail the gate. Scenarios without instruction data in either report (perf was
unavailable or forbidden from counting) are listed as skipped, not failed.
"""
import argparse
import json
import math
import sys
from pathlib import Path


MAX_REGRESSION = 0.02

# Execute arms dominated by interpreter work: in-process applets and string,
# JSON, CSV, or line handling with no spawned children. Child-heavy rows
# (spawn_loop, pipelines, find/xargs, file writes, checkpoint journaling) mix
# child execution into the counter and are reported but not gated.
CPU_BOUND_SCENARIOS = frozenset({
    "builtin_loop",
    "text_upper",
    "json_get",
    "csv_parse",
    "read_10k_lines",
    "suffix_strip",
    "here_string",
    "text_substitution",
})


def report_instructions(report, label):
    if not isinstance(report, dict):
        raise ValueError(f"{label} report must be a JSON object")
    metadata = report.get("metadata")
    rows = report.get("results")
    if not isinstance(metadata, dict) or not isinstance(rows, list):
        raise ValueError(f"{label} report must contain metadata and results")
    required_metadata = ("source_revision", "platform", "machine", "manifest_dir")
    missing_metadata = [field for field in required_metadata if field not in metadata]
    if missing_metadata:
        raise ValueError(f"{label} report metadata is missing: " + ", ".join(missing_metadata))
    if any(not isinstance(metadata[field], str) or not metadata[field] for field in required_metadata[:3]):
        raise ValueError(f"{label} report must record source revision, platform, and machine")
    instructions = {}
    for row in rows:
        key = row.get("key") if isinstance(row, dict) else None
        if not isinstance(key, str) or key in instructions:
            raise ValueError(f"{label} report has a missing or duplicate scenario key")
        try:
            value = row["execute"]["median_instructions"]
        except (KeyError, TypeError) as error:
            raise ValueError(f"{label} report has no Execute instruction count for {key!r}") from error
        if value is None:
            continue
        try:
            number = float(value)
        except (TypeError, ValueError) as error:
            raise ValueError(f"{label} report has an invalid instruction count for {key!r}") from error
        if not math.isfinite(number) or number <= 0:
            raise ValueError(f"{label} report has a nonpositive or non-finite instruction count for {key!r}")
        instructions[key] = number
    if not rows:
        raise ValueError(f"{label} report contains no scenario results")
    return metadata, instructions


def evaluate(baseline, candidate, max_regression=MAX_REGRESSION):
    if not 0 <= max_regression < 1:
        raise ValueError("max regression must be between 0 and 1")
    baseline_meta, baseline_instructions = report_instructions(baseline, "baseline")
    candidate_meta, candidate_instructions = report_instructions(candidate, "candidate")
    if str(baseline_meta.get("platform", "")).split("-", 1)[0] != str(
        candidate_meta.get("platform", "")
    ).split("-", 1)[0]:
        raise ValueError(
            f"baseline/candidate platform differ: "
            f"{baseline_meta.get('platform')!r} != {candidate_meta.get('platform')!r}"
        )
    for field in ("machine", "manifest_dir"):
        if baseline_meta.get(field) != candidate_meta.get(field):
            raise ValueError(
                f"baseline/candidate {field} differ: "
                f"{baseline_meta.get(field)!r} != {candidate_meta.get(field)!r}"
            )
    baseline_revision = baseline_meta.get("source_revision")
    candidate_revision = candidate_meta.get("source_revision")
    if not baseline_revision or not candidate_revision or baseline_revision == candidate_revision:
        raise ValueError("baseline and candidate must name distinct source revisions")

    baseline_keys = {row["key"] for row in baseline["results"] if isinstance(row, dict)}
    candidate_keys = {row["key"] for row in candidate["results"] if isinstance(row, dict)}
    missing = sorted(candidate_keys - baseline_keys)
    removed = sorted(baseline_keys - candidate_keys)
    if missing or removed:
        parts = []
        if missing:
            parts.append("scenarios without a stored baseline: " + ", ".join(missing))
        if removed:
            parts.append("stored baseline scenarios missing from candidate: " + ", ".join(removed))
        raise ValueError("; ".join(parts))

    failures = []
    comparisons = []
    skipped = []
    for key in sorted(baseline_keys):
        before = baseline_instructions.get(key)
        after = candidate_instructions.get(key)
        if before is None or after is None:
            skipped.append(key)
            continue
        regression = after / before - 1.0
        gated = key in CPU_BOUND_SCENARIOS
        comparisons.append((key, before, after, regression, gated))
        if gated and regression > max_regression + 1e-12:
            failures.append(key)
    return comparisons, failures, skipped


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("baseline", type=Path, help="retained baseline bash_compare JSON report")
    parser.add_argument("candidate", type=Path, help="current bash_compare JSON report")
    parser.add_argument(
        "--max-regression-percent",
        type=float,
        default=MAX_REGRESSION * 100,
        help="maximum allowed instruction regression on CPU-bound rows (default: 2)",
    )
    args = parser.parse_args(argv)
    if not args.baseline.is_file() or not args.candidate.is_file():
        print("error: baseline and candidate reports must both exist", file=sys.stderr)
        return 2
    try:
        baseline = json.loads(args.baseline.read_text(encoding="utf-8"))
        candidate = json.loads(args.candidate.read_text(encoding="utf-8"))
        comparisons, failures, skipped = evaluate(baseline, candidate, args.max_regression_percent / 100)
    except (json.JSONDecodeError, ValueError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 2
    for key, before, after, regression, gated in comparisons:
        tag = "gated" if gated else "reported"
        print(f"{key}: Execute instructions {before:,.0f} -> {after:,.0f} ({regression:+.1%}) [{tag}]")
    if skipped:
        print("skipped (no instruction data in one or both reports): " + ", ".join(skipped))
    if failures:
        print(
            f"error: CPU-bound instruction count regressed by more than {args.max_regression_percent:g}% in: "
            + ", ".join(failures),
            file=sys.stderr,
        )
        return 1
    print(f"all gated CPU-bound instruction counts remain within {args.max_regression_percent:g}% of the stored baseline")
    return 0


if __name__ == "__main__":
    sys.exit(main())
