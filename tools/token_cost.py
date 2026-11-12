#!/usr/bin/env python3
"""Estimate the token cost of Omarchy ports relative to their Bash originals.

Each directory under examples/omarchy-ports that holds both original.sh and
port.program is one pair. Full-line comments and blank lines are removed from both
files, and two deterministic proxies are measured on what remains:

  bytes   UTF-8 bytes of the stripped text
  tokens  word runs ([A-Za-z0-9_]+) plus each other non-space character

These proxies are an offline approximation of a model tokenizer, not a count from
one. Ratios are port / original. The median of the per-port token ratios is the
ratchet metric. Standard library only.

Usage:
  python tools/token_cost.py [--root DIR] [--json PATH] [--baseline PATH]

--root      directory of port pairs (default: examples/omarchy-ports)
--json      write the per-port rows and the summary as JSON
--baseline  JSON file with "median_token_ratio"; exit 1 when the current median
            token ratio is larger than that value by more than 0.005
"""

from __future__ import annotations

import argparse
import json
import os
import re
import statistics
import sys
from pathlib import Path

ROOT = Path(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
DEFAULT_ROOT = ROOT / "examples" / "omarchy-ports"
RATCHET_SLACK = 0.005
WORD_OR_CHAR = re.compile(r"[A-Za-z0-9_]+|[^\sA-Za-z0-9_]")


def strip_text(text: str) -> str:
    """Drop blank lines and full-line comments (including the shebang)."""
    kept = [
        line
        for line in text.splitlines()
        if line.strip() and not line.lstrip().startswith("#")
    ]
    return "\n".join(kept) + ("\n" if kept else "")


def measure(text: str) -> tuple[int, int]:
    """Return (bytes, tokens) for the stripped text."""
    stripped = strip_text(text)
    return len(stripped.encode("utf-8")), len(WORD_OR_CHAR.findall(stripped))


def find_pairs(root: Path) -> list[tuple[str, Path, Path]]:
    pairs = []
    for child in sorted(root.iterdir()):
        original = child / "original.sh"
        port = child / "port.program"
        if child.is_dir() and original.is_file() and port.is_file():
            pairs.append((child.name, original, port))
    return pairs


def ratio(numerator: int, denominator: int, name: str) -> float:
    if denominator == 0:
        raise ValueError(f"{name}: the original has no code after stripping comments")
    return numerator / denominator


def report(root: Path) -> list[dict]:
    rows = []
    for name, original, port in find_pairs(root):
        orig_bytes, orig_tokens = measure(original.read_bytes().decode("utf-8"))
        port_bytes, port_tokens = measure(port.read_bytes().decode("utf-8"))
        rows.append(
            {
                "name": name,
                "original_bytes": orig_bytes,
                "port_bytes": port_bytes,
                "byte_ratio": ratio(port_bytes, orig_bytes, name),
                "original_tokens": orig_tokens,
                "port_tokens": port_tokens,
                "token_ratio": ratio(port_tokens, orig_tokens, name),
            }
        )
    return rows


def summarize(rows: list[dict]) -> dict:
    if not rows:
        raise ValueError("no port pairs found")
    return {
        "scripts": len(rows),
        "median_byte_ratio": statistics.median(r["byte_ratio"] for r in rows),
        "median_token_ratio": statistics.median(r["token_ratio"] for r in rows),
        "aggregate_token_ratio": ratio(
            sum(r["port_tokens"] for r in rows),
            sum(r["original_tokens"] for r in rows),
            "aggregate",
        ),
    }


def check_baseline(summary: dict, baseline_path: Path) -> str | None:
    """Return a failure message when the median token ratio grew past the baseline."""
    baseline = json.loads(baseline_path.read_text(encoding="utf-8"))["median_token_ratio"]
    current = summary["median_token_ratio"]
    if current > baseline + RATCHET_SLACK:
        return (
            f"median token ratio {current:.4f} exceeds baseline {baseline:.4f} "
            f"(allowed slack {RATCHET_SLACK})"
        )
    return None


def print_table(rows: list[dict], summary: dict) -> None:
    print(
        f"{'script':<40} {'orig B':>7} {'port B':>7} {'byte x':>7} "
        f"{'orig tok':>9} {'port tok':>9} {'tok x':>7}"
    )
    for r in rows:
        print(
            f"{r['name']:<40} {r['original_bytes']:>7} {r['port_bytes']:>7} "
            f"{r['byte_ratio']:>7.2f} {r['original_tokens']:>9} "
            f"{r['port_tokens']:>9} {r['token_ratio']:>7.2f}"
        )
    print(
        f"scripts {summary['scripts']}  "
        f"median byte ratio {summary['median_byte_ratio']:.2f}  "
        f"median token ratio {summary['median_token_ratio']:.2f}  "
        f"aggregate token ratio {summary['aggregate_token_ratio']:.2f}"
    )


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--root", type=Path, default=DEFAULT_ROOT)
    parser.add_argument("--json", type=Path)
    parser.add_argument("--baseline", type=Path)
    args = parser.parse_args(argv)

    try:
        rows = report(args.root)
        summary = summarize(rows)
    except (OSError, ValueError) as exc:
        print(f"token_cost: {exc}", file=sys.stderr)
        return 2

    print_table(rows, summary)
    if args.json:
        payload = {
            "summary": {k: (round(v, 4) if isinstance(v, float) else v) for k, v in summary.items()},
            "rows": [
                {k: (round(v, 4) if isinstance(v, float) else v) for k, v in r.items()}
                for r in rows
            ],
        }
        args.json.write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")
    if args.baseline:
        failure = check_baseline(summary, args.baseline)
        if failure:
            print(f"token_cost: RATCHET FAILED: {failure}", file=sys.stderr)
            return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
