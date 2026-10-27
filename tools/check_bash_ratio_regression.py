#!/usr/bin/env python3
"""Fail when Execute/Bash ratios regress over a stored benchmark report.

Both inputs are JSON reports produced by tools/bash_compare.py. The baseline
report must come from the same OS family, machine architecture, and manifest
selection as the candidate. A baseline is intentionally required: this tool
never invents one from a current run.
"""
import argparse
import json
import math
import sys
from pathlib import Path


MAX_REGRESSION = 0.10


def report_ratios(report, label):
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
    ratios = {}
    for row in rows:
        key = row.get("key") if isinstance(row, dict) else None
        if not isinstance(key, str) or key in ratios:
            raise ValueError(f"{label} report has a missing or duplicate scenario key")
        try:
            bash_ms = float(row["bash"]["median_ms"])
            execute_ms = float(row["execute"]["median_ms"])
        except (KeyError, TypeError, ValueError) as error:
            raise ValueError(f"{label} report has invalid medians for {key!r}") from error
        if not math.isfinite(bash_ms) or not math.isfinite(execute_ms) or bash_ms <= 0 or execute_ms <= 0:
            raise ValueError(f"{label} report has nonpositive or non-finite medians for {key!r}")
        ratios[key] = execute_ms / bash_ms
    if not ratios:
        raise ValueError(f"{label} report contains no scenario results")
    return metadata, ratios


def evaluate(baseline, candidate, max_regression=MAX_REGRESSION):
    if not 0 <= max_regression < 1:
        raise ValueError("max regression must be between 0 and 1")
    baseline_meta, baseline_ratios = report_ratios(baseline, "baseline")
    candidate_meta, candidate_ratios = report_ratios(candidate, "candidate")
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

    missing = sorted(set(candidate_ratios) - set(baseline_ratios))
    removed = sorted(set(baseline_ratios) - set(candidate_ratios))
    if missing or removed:
        parts = []
        if missing:
            parts.append("scenarios without a stored baseline: " + ", ".join(missing))
        if removed:
            parts.append("stored baseline scenarios missing from candidate: " + ", ".join(removed))
        raise ValueError("; ".join(parts))

    failures = []
    comparisons = []
    for key in sorted(baseline_ratios):
        before, after = baseline_ratios[key], candidate_ratios[key]
        regression = after / before - 1.0
        comparisons.append((key, before, after, regression))
        if regression > max_regression + 1e-12:
            failures.append(key)
    return comparisons, failures


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("baseline", type=Path, help="retained baseline bash_compare JSON report")
    parser.add_argument("candidate", type=Path, help="current bash_compare JSON report")
    parser.add_argument(
        "--max-regression-percent",
        type=float,
        default=MAX_REGRESSION * 100,
        help="maximum allowed ratio regression (default: 10)",
    )
    args = parser.parse_args(argv)
    if not args.baseline.is_file() or not args.candidate.is_file():
        print("error: baseline and candidate reports must both exist", file=sys.stderr)
        return 2
    try:
        baseline = json.loads(args.baseline.read_text(encoding="utf-8"))
        candidate = json.loads(args.candidate.read_text(encoding="utf-8"))
        comparisons, failures = evaluate(baseline, candidate, args.max_regression_percent / 100)
    except (json.JSONDecodeError, ValueError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 2
    for key, before, after, regression in comparisons:
        print(f"{key}: Execute/Bash ratio {before:.3f}x -> {after:.3f}x ({regression:+.1%})")
    if failures:
        print(
            f"error: Execute/Bash ratio regressed by more than {args.max_regression_percent:g}% in: "
            + ", ".join(failures),
            file=sys.stderr,
        )
        return 1
    print(f"all Execute/Bash ratios remain within {args.max_regression_percent:g}% of the stored baseline")
    return 0


if __name__ == "__main__":
    sys.exit(main())
