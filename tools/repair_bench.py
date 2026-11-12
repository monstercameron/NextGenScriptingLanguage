#!/usr/bin/env python3
"""Model-free repair benchmark (issue #502).

Mutates the checking example programs with mistakes a model is likely to make, runs
`execute check --json` on each mutated copy, and scores how well the diagnostics
drive repair:

  detection       the mutated program fails check
  localisation    the first diagnostic's span covers the mutated token
  specificity     the first diagnostic has a specific code (not check_failed or parse.syntax)
  cascade         diagnostics per mutation (lower is better)

Fix rate (applying the top fix and comparing `explain --json`) is not measured yet; it
needs `check --fix` (issue #473).

The mutation taxonomy is versioned (TAXONOMY_VERSION). Mutations are chosen with a
fixed seed, so a run is reproducible. Only the standard library is used.

Usage:
  python tools/repair_bench.py --execute PATH/TO/execute [--seed 502] [--max-mutations 3]
      [--corpus examples] [--report OUT.json]
      [--baseline BASE.json] [--write-baseline BASE.json]

Exit status: 0 when every metric holds its baseline (or no baseline is given),
1 when a metric regressed against the baseline, 2 on usage or I/O errors.
"""

from __future__ import annotations

import argparse
import json
import random
import re
import shutil
import subprocess
import sys
import tempfile
from dataclasses import dataclass
from pathlib import Path
from typing import Callable

SCHEMA = "execute.repair_bench.v1"
TAXONOMY_VERSION = 1
DEFAULT_SEED = 502
DEFAULT_MAX_MUTATIONS = 3
DEFAULT_TIMEOUT = 60
GENERIC_CODES = frozenset({"check_failed", "parse.syntax"})
# Metrics where a larger number is better. diagnostics_per_mutation is a cascade count.
HIGHER_IS_BETTER = ("detection_rate", "localisation_rate", "specificity_rate")
LOWER_IS_BETTER = ("diagnostics_per_mutation",)

ROOT = Path(__file__).resolve().parent.parent
DEFAULT_CORPUS = ROOT / "examples"


@dataclass(frozen=True)
class Mutation:
    """One entry of the taxonomy.

    find(text) returns sites as (window_start, cut_start, cut_end) character offsets in
    the original text: the window starts at window_start, the text in [cut_start,
    cut_end) is removed, and the character at cut_start after the removal is the one
    the diagnostic should point at.
    """

    kind: str
    description: str
    find: Callable[[str], list[tuple[int, int, int]]]


# `then save to "PATH"`: the model drops the `to`. Matches the statement on any line,
# with any indentation, and only when a quoted path follows the `to`.
_DROP_SAVE_TO = re.compile(r'\bthen save to (?=")')


def _find_drop_save_to(text: str) -> list[tuple[int, int, int]]:
    sites = []
    for match in _DROP_SAVE_TO.finditer(text):
        window_start = match.start() + len("then ")
        cut_start = match.start() + len("then save ")
        cut_end = match.end()  # removes "to " including the space
        sites.append((window_start, cut_start, cut_end))
    return sites


TAXONOMY: dict[str, Mutation] = {
    "drop_save_to": Mutation(
        kind="drop_save_to",
        description='drop the `to` in `then save to "PATH"`',
        find=_find_drop_save_to,
    ),
}


def apply_sites(text: str, sites: list[tuple[int, int, int]]) -> tuple[str, list[tuple[int, int]]]:
    """Applies the chosen sites and returns the new text and each site's window in it.

    Sites are applied in ascending order; `removed` tracks how much text earlier cuts
    took out, so later offsets map into the new text.
    """
    pieces: list[str] = []
    windows: list[tuple[int, int]] = []
    cursor = 0
    removed = 0
    for window_start, cut_start, cut_end in sorted(sites, key=lambda s: s[1]):
        pieces.append(text[cursor:cut_start])
        windows.append((window_start - removed, cut_start - removed + 1))
        cursor = cut_end
        removed += cut_end - cut_start
    pieces.append(text[cursor:])
    return "".join(pieces), windows


def choose_mutations(
    text: str, rng: random.Random, max_mutations: int, kinds: list[str]
) -> tuple[str, list[tuple[str, int, int]], list[tuple[int, int]]]:
    """Picks 1..max_mutations sites over the taxonomy kinds, applies them, and returns
    (mutated text, [(kind, window_start, window_end)] in character offsets, windows)."""
    candidates: list[tuple[str, tuple[int, int, int]]] = []
    for kind in kinds:
        for site in TAXONOMY[kind].find(text):
            candidates.append((kind, site))
    if not candidates:
        return text, [], []
    count = min(rng.randint(1, max_mutations), len(candidates))
    chosen = rng.sample(candidates, count)
    mutated, windows = apply_sites(text, [site for _, site in chosen])
    labelled = [(kind, w[0], w[1]) for (kind, _), w in zip(sorted(chosen, key=lambda c: c[1][1]), windows)]
    return mutated, labelled, windows


def byte_window(text: str, window: tuple[int, int]) -> tuple[int, int]:
    start, end = window
    return len(text[:start].encode("utf-8")), len(text[:end].encode("utf-8"))


def span_bytes(diagnostic: dict) -> tuple[int, int] | None:
    span = diagnostic.get("span")
    if not isinstance(span, dict):
        return None
    start = span.get("start_byte")
    end = span.get("end_byte")
    if not isinstance(start, int) or not isinstance(end, int):
        return None
    return start, max(end, start + 1)


def localised(diagnostics: list[dict], mutated: str, windows: list[tuple[int, int]]) -> bool:
    """True when the first diagnostic's span overlaps a mutated window."""
    if not diagnostics or not windows:
        return False
    first = span_bytes(diagnostics[0])
    if first is None:
        return False
    for window in windows:
        w_start, w_end = byte_window(mutated, window)
        if first[0] < w_end and w_start < first[1]:
            return True
    return False


def is_specific(diagnostics: list[dict]) -> bool:
    return bool(diagnostics) and diagnostics[0].get("code") not in GENERIC_CODES


def run_check(execute: str, path: Path, timeout: int) -> tuple[bool, list[dict]]:
    """Runs `execute check --json` and returns (failed, diagnostics)."""
    try:
        proc = subprocess.run(
            [execute, "check", "--json", str(path)],
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            timeout=timeout,
        )
    except subprocess.TimeoutExpired:
        return True, [{"severity": "error", "code": "check_failed", "message": "check timed out"}]
    try:
        report = json.loads(proc.stdout)
    except (json.JSONDecodeError, TypeError):
        report = None
    if not isinstance(report, dict):
        return True, [{"severity": "error", "code": "check_failed", "message": "unreadable check output"}]
    diagnostics = [d for d in report.get("diagnostics", []) if isinstance(d, dict)]
    failed = proc.returncode != 0 or any(d.get("severity") == "error" for d in diagnostics)
    return failed, diagnostics


def corpus_files(corpus: Path) -> list[Path]:
    return sorted(p for p in corpus.rglob("*.program") if p.is_file())


def relative(path: Path) -> str:
    try:
        return path.resolve().relative_to(ROOT).as_posix()
    except ValueError:
        return path.as_posix()


def run_benchmark(
    execute: str,
    corpus: Path,
    seed: int,
    max_mutations: int,
    timeout: int,
    kinds: list[str] | None = None,
) -> dict:
    kinds = kinds or sorted(TAXONOMY)
    rng = random.Random(seed)
    cases: list[dict] = []
    clean = 0
    skipped = 0
    with tempfile.TemporaryDirectory(prefix="repair-bench-") as scratch:
        scratch_dir = Path(scratch)
        for index, path in enumerate(corpus_files(corpus)):
            original_failed, _ = run_check(execute, path, timeout)
            if original_failed:
                skipped += 1
                continue
            clean += 1
            text = path.read_text(encoding="utf-8")
            mutated, labels, windows = choose_mutations(text, rng, max_mutations, kinds)
            if not labels:
                continue
            target = scratch_dir / f"{index}-{path.name}"
            target.write_text(mutated, encoding="utf-8", newline="\n")
            failed, diagnostics = run_check(execute, target, timeout)
            cases.append(
                {
                    "program": relative(path),
                    "mutations": [{"kind": k, "window": [s, e]} for k, s, e in labels],
                    "detected": failed,
                    "localised": failed and localised(diagnostics, mutated, windows),
                    "specific": failed and is_specific(diagnostics),
                    "first_code": diagnostics[0].get("code") if diagnostics else None,
                    "diagnostics": len(diagnostics),
                }
            )
    return build_report(execute, seed, max_mutations, kinds, clean, skipped, cases)


def ratio(numerator: int, denominator: int) -> float:
    return round(numerator / denominator, 4) if denominator else 0.0


def build_report(
    execute: str,
    seed: int,
    max_mutations: int,
    kinds: list[str],
    clean: int,
    skipped: int,
    cases: list[dict],
) -> dict:
    mutated = len(cases)
    detected = sum(1 for c in cases if c["detected"])
    localised_count = sum(1 for c in cases if c["localised"])
    specific = sum(1 for c in cases if c["specific"])
    diagnostics = sum(c["diagnostics"] for c in cases)
    mutations = sum(len(c["mutations"]) for c in cases)
    metrics = {
        "detection_rate": ratio(detected, mutated),
        "localisation_rate": ratio(localised_count, mutated),
        "specificity_rate": ratio(specific, detected),
        "diagnostics_per_mutation": round(diagnostics / mutations, 4) if mutations else 0.0,
    }
    return {
        "schema": SCHEMA,
        "taxonomy_version": TAXONOMY_VERSION,
        "seed": seed,
        "max_mutations": max_mutations,
        "kinds": kinds,
        "corpus": {"clean": clean, "skipped_not_clean": skipped, "mutated_programs": mutated},
        "mutations": mutations,
        "metrics": metrics,
        "cases": cases,
    }


def compare_to_baseline(metrics: dict, baseline: dict) -> list[str]:
    """Returns one message per metric that is worse than the baseline."""
    problems = []
    base_metrics = baseline.get("metrics", {})
    for name in HIGHER_IS_BETTER:
        if name in base_metrics and metrics.get(name, 0.0) < base_metrics[name] - 1e-9:
            problems.append(f"{name} dropped: {metrics.get(name, 0.0)} < baseline {base_metrics[name]}")
    for name in LOWER_IS_BETTER:
        if name in base_metrics and metrics.get(name, 0.0) > base_metrics[name] + 1e-9:
            problems.append(f"{name} rose: {metrics.get(name, 0.0)} > baseline {base_metrics[name]}")
    return problems


def summary_lines(report: dict) -> list[str]:
    corpus = report["corpus"]
    lines = [
        f"corpus: {corpus['clean']} clean programs, {corpus['skipped_not_clean']} skipped, "
        f"{corpus['mutated_programs']} mutated, {report['mutations']} mutations (seed {report['seed']})",
    ]
    for name, value in report["metrics"].items():
        lines.append(f"  {name:<26} {value}")
    return lines


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--execute", required=True, help="path to the execute binary")
    parser.add_argument("--corpus", type=Path, default=DEFAULT_CORPUS)
    parser.add_argument("--seed", type=int, default=DEFAULT_SEED)
    parser.add_argument("--max-mutations", type=int, default=DEFAULT_MAX_MUTATIONS)
    parser.add_argument("--timeout", type=int, default=DEFAULT_TIMEOUT)
    parser.add_argument("--report", type=Path, help="write the full JSON report here")
    parser.add_argument("--baseline", type=Path, help="fail when a metric is worse than this file")
    parser.add_argument("--write-baseline", type=Path, help="write the metrics as a baseline")
    args = parser.parse_args(argv)

    if args.max_mutations < 1:
        print("repair_bench: --max-mutations must be at least 1", file=sys.stderr)
        return 2
    if shutil.which(args.execute) is None and not Path(args.execute).is_file():
        print(f"repair_bench: execute binary not found: {args.execute}", file=sys.stderr)
        return 2
    if not args.corpus.is_dir():
        print(f"repair_bench: corpus folder not found: {args.corpus}", file=sys.stderr)
        return 2
    baseline = None
    if args.baseline is not None:
        try:
            baseline = json.loads(args.baseline.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError) as error:
            print(f"repair_bench: cannot read baseline {args.baseline}: {error}", file=sys.stderr)
            return 2

    report = run_benchmark(args.execute, args.corpus, args.seed, args.max_mutations, args.timeout)
    for line in summary_lines(report):
        print(line)

    if args.report is not None:
        args.report.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
    if args.write_baseline is not None:
        baseline_out = {
            "schema": SCHEMA,
            "taxonomy_version": TAXONOMY_VERSION,
            "seed": args.seed,
            "metrics": report["metrics"],
        }
        args.write_baseline.write_text(json.dumps(baseline_out, indent=2) + "\n", encoding="utf-8")
    if baseline is not None:
        problems = compare_to_baseline(report["metrics"], baseline)
        for problem in problems:
            print(f"repair_bench: REGRESSION {problem}", file=sys.stderr)
        if problems:
            return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
