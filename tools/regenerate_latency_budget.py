#!/usr/bin/env python3
"""Regenerate Linux latency budgets from retained CI samples.

Each sample file is JSON emitted by `tools/check_latency.py --json-out` with
`metadata` (run id, platform, machine) and `results` mapping each workload key
to its `median_ms` and `max_rss_kb`. All samples must come from the same OS
family and machine architecture, and must cover the same workload keys.

Ceilings follow the #518 proposal: time is the median of the retained medians
plus `max(15%, 0.3 ms)`; RSS is the retained peak plus 10%. Times round up to
one decimal, RSS rounds up to whole KiB. The recorded note keeps the run ids,
architecture, formula, and sample count, so a re-budget always says where it
came from. Python 3.9+, standard library only.
"""
import argparse
import json
import math
import statistics
import sys
from pathlib import Path

TIME_HEADROOM_RATIO = 0.15
TIME_HEADROOM_FLOOR_MS = 0.3
RSS_HEADROOM_RATIO = 0.10


def load_sample(path):
    try:
        document = json.loads(Path(path).read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise ValueError(f"sample {path} is not readable JSON: {error}") from error
    if not isinstance(document, dict):
        raise ValueError(f"sample {path} must be a JSON object")
    metadata = document.get("metadata")
    results = document.get("results")
    if not isinstance(metadata, dict) or not isinstance(results, dict) or not results:
        raise ValueError(f"sample {path} must contain metadata and nonempty results")
    for field in ("platform", "machine"):
        value = metadata.get(field)
        if not isinstance(value, str) or not value:
            raise ValueError(f"sample {path} metadata is missing {field!r}")
    parsed = {}
    for key, measured in results.items():
        if not isinstance(measured, dict):
            raise ValueError(f"sample {path} has invalid results for {key!r}")
        try:
            median_ms = float(measured["median_ms"])
        except (KeyError, TypeError, ValueError) as error:
            raise ValueError(f"sample {path} has no numeric median_ms for {key!r}") from error
        if not math.isfinite(median_ms) or median_ms < 0:
            raise ValueError(f"sample {path} has non-finite median_ms for {key!r}")
        rss = measured.get("max_rss_kb")
        if rss is not None:
            try:
                rss = float(rss)
            except (TypeError, ValueError) as error:
                raise ValueError(f"sample {path} has non-numeric max_rss_kb for {key!r}") from error
            if not math.isfinite(rss) or rss < 0:
                raise ValueError(f"sample {path} has non-finite max_rss_kb for {key!r}")
        parsed[key] = (median_ms, rss)
    return metadata, parsed


def ceiling_ms(baseline):
    return math.ceil((baseline + max(TIME_HEADROOM_RATIO * baseline, TIME_HEADROOM_FLOOR_MS)) * 10) / 10


def ceiling_kb(peak):
    return int(math.ceil(peak * (1 + RSS_HEADROOM_RATIO)))


def regenerate(samples):
    """Return a budget dict computed from loaded samples."""
    if not samples:
        raise ValueError("at least one retained sample is required")
    families = {str(metadata.get("platform", "")).split("-", 1)[0] for metadata, _ in samples}
    machines = {metadata.get("machine") for metadata, _ in samples}
    if len(families) != 1:
        raise ValueError(f"samples span OS families: {sorted(families)}")
    if len(machines) != 1:
        raise ValueError(f"samples span architectures: {sorted(machines)}")
    key_sets = [set(parsed) for _, parsed in samples]
    if any(keys != key_sets[0] for keys in key_sets[1:]):
        raise ValueError("samples must cover the same workload keys")
    run_ids = []
    for metadata, _ in samples:
        run_id = metadata.get("run_id", "local")
        if run_id not in run_ids:
            run_ids.append(run_id)
    budget = {}
    for key in sorted(key_sets[0]):
        medians = [parsed[key][0] for _, parsed in samples]
        baseline = statistics.median(medians)
        budget[f"{key}_median_ms"] = ceiling_ms(baseline)
        peaks = [parsed[key][1] for _, parsed in samples if parsed[key][1] is not None]
        if peaks:
            budget[f"{key}_max_rss_kb"] = ceiling_kb(max(peaks))
    family = next(iter(families))
    machine = next(iter(machines))
    budget["note"] = (
        f"Regenerated from {len(samples)} retained {family}/{machine} sample(s) measured "
        f"with tools/check_latency.py --json-out (runs: {', '.join(map(str, run_ids))}): "
        "time ceilings are the median of retained medians plus max(15%, 0.3 ms), "
        "RSS ceilings are the retained peak plus 10%."
    )
    return budget


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("samples", nargs="+", type=Path, help="retained check_latency --json-out files")
    parser.add_argument("--output", type=Path, help="write the regenerated budget here")
    parser.add_argument("--check", type=Path, help="fail unless this budget file already matches")
    args = parser.parse_args(argv)
    if args.output is None and args.check is None:
        parser.error("one of --output or --check is required")
    try:
        budget = regenerate([load_sample(path) for path in args.samples])
    except ValueError as error:
        print(f"error: {error}", file=sys.stderr)
        return 2
    text = json.dumps(budget, indent=2, sort_keys=True) + "\n"
    if args.output is not None:
        args.output.write_text(text, encoding="utf-8")
        print(f"wrote {len(budget) - 1} ceilings to {args.output}")
    if args.check is not None:
        try:
            current = json.loads(args.check.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError) as error:
            print(f"error: cannot read {args.check}: {error}", file=sys.stderr)
            return 2
        if current != budget:
            print(f"error: {args.check} does not match ceilings regenerated from retained samples", file=sys.stderr)
            return 1
        print(f"{args.check} matches regenerated ceilings")
    return 0


if __name__ == "__main__":
    sys.exit(main())
