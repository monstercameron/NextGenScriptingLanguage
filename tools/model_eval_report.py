#!/usr/bin/env python3
"""Calibration and threshold report for bounded model decisions (#380).

The module takes labeled cases, each a (confidence, correct) pair, and computes
the numbers a threshold choice needs: accuracy, expected calibration error (ECE)
over equal-width bins, and a coverage-versus-accuracy sweep over confidence
thresholds, with Wilson 95 percent intervals. It makes no model calls. A case
is accepted at threshold t when its confidence is at least t.

Input is a JSON file: a list of cases, or an object with a "cases" list. Each
case has "confidence" (a number in [0, 1]) and either "correct" (a boolean) or
"expected" and "predicted" (correct is derived as predicted == expected).

Gate mode exits 1 when a requirement fails, so a recorded suite can run in CI:
    python tools/model_eval_report.py cases.json --target 0.95 \
        --require accuracy_on_accepted>=0.95 --require coverage>=0.6
The requirements are checked at the lowest threshold that meets --target, or
at --threshold when it is given.
"""

from __future__ import annotations

import argparse
import json
import math
import sys
from pathlib import Path


WILSON_Z = 1.959963984540054
DEFAULT_BINS = 10


def wilson_interval(successes: int, total: int) -> tuple[float, float]:
    """Wilson score interval at 95 percent. Returns (low, high); (0, 1) when empty."""
    if total == 0:
        return (0.0, 1.0)
    p = successes / total
    z2 = WILSON_Z * WILSON_Z
    denom = 1.0 + z2 / total
    center = (p + z2 / (2 * total)) / denom
    half = WILSON_Z * math.sqrt(p * (1 - p) / total + z2 / (4 * total * total)) / denom
    return (max(0.0, center - half), min(1.0, center + half))


def load_cases(data: object) -> list[tuple[float, bool]]:
    """Turn parsed JSON into (confidence, correct) pairs. Raises ValueError on bad input."""
    if isinstance(data, dict):
        data = data.get("cases")
    if not isinstance(data, list):
        raise ValueError("expected a list of cases or an object with a 'cases' list")
    pairs: list[tuple[float, bool]] = []
    for index, case in enumerate(data):
        if not isinstance(case, dict):
            raise ValueError(f"case {index} is not an object")
        confidence = case.get("confidence")
        if isinstance(confidence, bool) or not isinstance(confidence, (int, float)):
            raise ValueError(f"case {index}: confidence must be a number")
        if not 0.0 <= confidence <= 1.0:
            raise ValueError(f"case {index}: confidence {confidence} is outside [0, 1]")
        if "correct" in case:
            correct = case["correct"]
            if not isinstance(correct, bool):
                raise ValueError(f"case {index}: correct must be a boolean")
        elif "expected" in case and "predicted" in case:
            correct = case["expected"] == case["predicted"]
        else:
            raise ValueError(f"case {index}: needs 'correct' or 'expected' and 'predicted'")
        pairs.append((float(confidence), correct))
    return pairs


def accuracy(pairs: list[tuple[float, bool]]) -> float | None:
    if not pairs:
        return None
    return sum(1 for _, ok in pairs if ok) / len(pairs)


def expected_calibration_error(
    pairs: list[tuple[float, bool]], bins: int = DEFAULT_BINS
) -> tuple[float | None, list[dict]]:
    """ECE over equal-width bins. Returns (ece, reliability bins); empty bins are listed with count 0."""
    total = len(pairs)
    buckets: list[list[tuple[float, bool]]] = [[] for _ in range(bins)]
    for confidence, ok in pairs:
        buckets[min(int(confidence * bins), bins - 1)].append((confidence, ok))
    reliability = []
    ece = 0.0 if total else None
    for index, bucket in enumerate(buckets):
        entry = {
            "low": index / bins,
            "high": (index + 1) / bins,
            "count": len(bucket),
            "mean_confidence": None,
            "accuracy": None,
        }
        if bucket:
            mean_conf = sum(c for c, _ in bucket) / len(bucket)
            acc = sum(1 for _, ok in bucket if ok) / len(bucket)
            entry["mean_confidence"] = mean_conf
            entry["accuracy"] = acc
            ece += (len(bucket) / total) * abs(acc - mean_conf)
        reliability.append(entry)
    return ece, reliability


def threshold_sweep(
    pairs: list[tuple[float, bool]], thresholds: list[float]
) -> list[dict]:
    """For each threshold, the accepted count, coverage, accuracy on accepted and its Wilson interval."""
    total = len(pairs)
    rows = []
    for threshold in thresholds:
        accepted = [ok for c, ok in pairs if c >= threshold]
        correct = sum(1 for ok in accepted if ok)
        low, high = wilson_interval(correct, len(accepted))
        rows.append(
            {
                "threshold": threshold,
                "accepted": len(accepted),
                "coverage": (len(accepted) / total) if total else None,
                "accuracy_on_accepted": (correct / len(accepted)) if accepted else None,
                "wilson_low": low if accepted else None,
                "wilson_high": high if accepted else None,
            }
        )
    return rows


def recommended_threshold(sweep: list[dict], target: float) -> dict | None:
    """Lowest threshold whose accuracy on accepted cases reaches the target (maximises coverage)."""
    for row in sorted(sweep, key=lambda r: r["threshold"]):
        if row["accuracy_on_accepted"] is not None and row["accuracy_on_accepted"] >= target:
            return row
    return None


def build_report(
    pairs: list[tuple[float, bool]],
    target: float | None = None,
    thresholds: list[float] | None = None,
    bins: int = DEFAULT_BINS,
) -> dict:
    if thresholds is None:
        thresholds = sorted({c for c, _ in pairs})
    total = len(pairs)
    correct = sum(1 for _, ok in pairs if ok)
    ece, reliability = expected_calibration_error(pairs, bins)
    sweep = threshold_sweep(pairs, thresholds)
    low, high = wilson_interval(correct, total)
    report = {
        "cases": total,
        "accuracy": accuracy(pairs),
        "accuracy_wilson": [low, high],
        "ece": ece,
        "bins": bins,
        "reliability": reliability,
        "sweep": sweep,
        "target": target,
        "recommended": None,
    }
    if target is not None:
        report["recommended"] = recommended_threshold(sweep, target)
    return report


def parse_requirement(text: str) -> tuple[str, float]:
    """Parse 'name>=value'. Only the >= form is supported."""
    name, sep, value = text.partition(">=")
    if not sep or not name.strip():
        raise ValueError(f"requirement must look like name>=value: {text!r}")
    return name.strip(), float(value)


def check_requirements(report: dict, requirements: list[str], threshold: float | None) -> list[str]:
    """Return the failed requirements. Names refer to the row used for the gate."""
    row = None
    if threshold is not None:
        matches = [r for r in report["sweep"] if r["threshold"] == threshold]
        row = matches[0] if matches else None
    elif report["recommended"] is not None:
        row = report["recommended"]
    failures = []
    for text in requirements:
        name, minimum = parse_requirement(text)
        if name == "accuracy":
            value = report["accuracy"]
        elif row is None:
            value = None
        else:
            value = row.get(name)
        if value is None or value < minimum:
            shown = "n/a" if value is None else f"{value:.4f}"
            failures.append(f"{text} (got {shown})")
    return failures


def format_text(report: dict) -> str:
    lines = [f"cases: {report['cases']}"]
    if report["accuracy"] is not None:
        low, high = report["accuracy_wilson"]
        lines.append(f"accuracy: {report['accuracy']:.4f} (95% Wilson {low:.4f} to {high:.4f})")
        lines.append(f"ECE ({report['bins']} bins): {report['ece']:.4f}")
    lines.append("threshold  accepted  coverage  acc_on_accepted")
    for row in report["sweep"]:
        cov = "n/a" if row["coverage"] is None else f"{row['coverage']:.4f}"
        acc = "n/a" if row["accuracy_on_accepted"] is None else f"{row['accuracy_on_accepted']:.4f}"
        lines.append(f"{row['threshold']:<10.4f} {row['accepted']:<9} {cov:<9} {acc}")
    if report["target"] is not None:
        rec = report["recommended"]
        if rec is None:
            lines.append(f"no threshold reaches target {report['target']}")
        else:
            lines.append(f"recommended minimum for target {report['target']}: {rec['threshold']}")
    return "\n".join(lines)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("cases", type=Path, help="JSON file of labeled cases")
    parser.add_argument("--target", type=float, help="accuracy on accepted cases to aim for")
    parser.add_argument("--threshold", type=float, help="gate at this threshold instead of the recommended one")
    parser.add_argument("--bins", type=int, default=DEFAULT_BINS)
    parser.add_argument("--require", action="append", default=[], help="gate requirement, e.g. coverage>=0.6")
    parser.add_argument("--json", action="store_true", help="print the report as JSON")
    args = parser.parse_args(argv)

    try:
        pairs = load_cases(json.loads(args.cases.read_text(encoding="utf-8")))
        if args.bins < 1:
            raise ValueError("--bins must be at least 1")
        thresholds = sorted({c for c, _ in pairs} | ({args.threshold} if args.threshold is not None else set()))
        report = build_report(pairs, target=args.target, thresholds=thresholds, bins=args.bins)
        failures = check_requirements(report, args.require, args.threshold) if args.require else []
    except (OSError, ValueError, json.JSONDecodeError) as error:
        print(f"model_eval_report: {error}", file=sys.stderr)
        return 2

    print(json.dumps(report, indent=2) if args.json else format_text(report))
    if failures:
        for failure in failures:
            print(f"gate failed: {failure}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
