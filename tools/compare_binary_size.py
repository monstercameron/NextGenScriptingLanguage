#!/usr/bin/env python3
"""Compare two Linux release binaries by file size on one runner.

This is a diagnostic companion to check_binary_size.py. It stats a baseline
binary and a candidate binary built on the same runner, then prints both
sizes with the signed delta in bytes and percent. Absolute budgets still
live in tools/linux-binary-budget.json and are enforced only by
check_binary_size.py; this tool never reads or changes them.

With --max-delta-bytes or --max-delta-pct the tool additionally fails when
the candidate grows past the baseline by more than the threshold, so a
future gate can enforce growth limits without moving the absolute
ceiling. Shrinkage always passes. Python 3.9+, standard library only.
"""
import argparse
import json
import os
import platform
import subprocess
import sys
from pathlib import Path


def measure(binary):
    """Return the size of a built binary, raising ValueError when missing."""
    path = Path(binary)
    if not path.is_file():
        raise ValueError(f"binary {path} is not a built executable")
    return path.stat().st_size


def summarize(baseline_bytes, candidate_bytes):
    """Return the signed size delta between two measured binaries."""
    if baseline_bytes <= 0 or candidate_bytes < 0:
        raise ValueError("binary sizes must be positive")
    delta = candidate_bytes - baseline_bytes
    return {
        "baseline_bytes": baseline_bytes,
        "candidate_bytes": candidate_bytes,
        "delta_bytes": delta,
        "delta_pct": delta / baseline_bytes * 100.0,
    }


def sample_metadata(binary):
    """Return provenance metadata for a retained size sample."""
    source = subprocess.run(
        ["git", "rev-parse", "HEAD"],
        cwd=Path.cwd(),
        capture_output=True,
        text=True,
        check=False,
    )
    return {
        "source_revision": source.stdout.strip() if source.returncode == 0 else None,
        "run_id": os.environ.get("GITHUB_RUN_ID", "local"),
        "platform": platform.platform(),
        "machine": platform.machine(),
        "binary": str(binary),
    }


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--baseline", required=True, type=Path)
    parser.add_argument("--candidate", required=True, type=Path)
    parser.add_argument("--baseline-label", default="baseline")
    parser.add_argument("--candidate-label", default="candidate")
    parser.add_argument(
        "--max-delta-bytes",
        type=int,
        default=None,
        help="fail when the candidate grows past the baseline by more than this",
    )
    parser.add_argument(
        "--max-delta-pct",
        type=float,
        default=None,
        help="fail when the candidate grows past the baseline by more than this percent",
    )
    parser.add_argument("--json-out", type=Path, help="retain the paired sizes for budget review")
    args = parser.parse_args(argv)

    try:
        baseline_bytes = measure(args.baseline)
        candidate_bytes = measure(args.candidate)
        result = summarize(baseline_bytes, candidate_bytes)
    except ValueError as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    if args.max_delta_bytes is not None and args.max_delta_bytes < 0:
        print("error: --max-delta-bytes cannot be negative", file=sys.stderr)
        return 1

    print(
        f"{args.baseline_label}: {result['baseline_bytes']} bytes; "
        f"{args.candidate_label}: {result['candidate_bytes']} bytes; "
        f"delta: {result['delta_bytes']:+d} bytes ({result['delta_pct']:+.2f}%)"
    )
    print("Diagnostic comparison only; binary budgets are unchanged and not enforced here.")

    if args.json_out is not None:
        args.json_out.parent.mkdir(parents=True, exist_ok=True)
        args.json_out.write_text(
            json.dumps(
                {
                    "metadata": {
                        "baseline": sample_metadata(args.baseline),
                        "candidate": sample_metadata(args.candidate),
                    },
                    "results": result,
                },
                indent=2,
                sort_keys=True,
            )
            + "\n",
            encoding="utf-8",
        )

    failures = []
    if args.max_delta_bytes is not None and result["delta_bytes"] > args.max_delta_bytes:
        failures.append(
            f"candidate grew by {result['delta_bytes']} bytes, "
            f"past the {args.max_delta_bytes} byte growth limit"
        )
    if args.max_delta_pct is not None and result["delta_pct"] > args.max_delta_pct:
        failures.append(
            f"candidate grew by {result['delta_pct']:.2f}%, "
            f"past the {args.max_delta_pct:.2f}% growth limit"
        )
    for failure in failures:
        print(f"error: {failure}", file=sys.stderr)
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
