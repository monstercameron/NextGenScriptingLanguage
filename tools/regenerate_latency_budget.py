#!/usr/bin/env python3
"""Regenerate Linux latency budgets from retained CI samples.

Each sample file is JSON emitted by `tools/check_latency.py --json-out` with
`metadata` (run id, source revision, binary digest, platform, machine) and
`results` mapping each workload key to its `median_ms`, `p95_ms`, `runs`, and
`max_rss_kb`. All samples must come from the same OS family and machine
architecture with the same manifest configuration, must cover the same
workload keys, and must come from distinct runs; at least three retained
samples are required so one outlier cannot set a ceiling.

Ceilings follow the #518 proposal: time is the median of the retained medians
plus `max(15%, 0.3 ms)`; RSS is the retained peak plus 10%. Times round up to
one decimal, RSS rounds up to whole KiB. The recorded note keeps the run ids,
architecture, formula, and sample count, so a re-budget always says where it
came from. `--check` compares ceilings only, so identical ceilings regenerated
from newer runs still match. Python 3.9+, standard library only.
"""
import argparse
import json
import math
import re
import statistics
import sys
from pathlib import Path

TIME_HEADROOM_RATIO = 0.15
TIME_HEADROOM_FLOOR_MS = 0.3
RSS_HEADROOM_RATIO = 0.10

MIN_SAMPLES = 3

_SHA256_RE = re.compile(r"[0-9a-f]{64}")


def os_family(platform_name):
    """Return the lowercase OS family from a `platform.platform()` string.

    Only the leading alphabetic token is significant, so `Linux-6.11.0-...`,
    `macOS-15.0`, and `Windows-10-...` map to `linux`, `macos`, and `windows`
    regardless of kernel or build suffixes.
    """
    match = re.match(r"[A-Za-z]+", platform_name)
    if not match:
        raise ValueError(f"sample metadata has an unrecognised platform {platform_name!r}")
    return match.group(0).lower()


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
    for field in ("platform", "machine", "run_id", "source_revision", "binary_sha256", "python"):
        value = metadata.get(field)
        if not isinstance(value, str) or not value:
            raise ValueError(f"sample {path} metadata is missing {field!r}")
    if not _SHA256_RE.fullmatch(metadata["binary_sha256"]):
        raise ValueError(f"sample {path} metadata has an invalid binary_sha256 digest")
    manifest_dir = metadata.get("manifest_dir")
    if manifest_dir is not None and not isinstance(manifest_dir, str):
        raise ValueError(f"sample {path} metadata has an invalid manifest_dir")
    parsed = {}
    for key, measured in results.items():
        if not isinstance(measured, dict):
            raise ValueError(f"sample {path} has invalid results for {key!r}")
        try:
            median_ms = float(measured["median_ms"])
        except (KeyError, TypeError, ValueError) as error:
            raise ValueError(f"sample {path} has no numeric median_ms for {key!r}") from error
        if not math.isfinite(median_ms) or median_ms <= 0:
            raise ValueError(f"sample {path} has a non-positive median_ms for {key!r}")
        try:
            runs = measured["runs"]
        except KeyError as error:
            raise ValueError(f"sample {path} has no runs count for {key!r}") from error
        if isinstance(runs, bool) or not isinstance(runs, int) or runs < 1:
            raise ValueError(f"sample {path} has an invalid runs count for {key!r}")
        p95 = measured.get("p95_ms")
        if p95 is not None:
            try:
                p95 = float(p95)
            except (TypeError, ValueError) as error:
                raise ValueError(f"sample {path} has non-numeric p95_ms for {key!r}") from error
            if not math.isfinite(p95) or p95 < median_ms:
                raise ValueError(f"sample {path} has an inconsistent p95_ms for {key!r}")
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
    if len(samples) < MIN_SAMPLES:
        raise ValueError(f"at least {MIN_SAMPLES} retained samples are required, got {len(samples)}")
    run_ids = [metadata.get("run_id") for metadata, _ in samples]
    if any(not isinstance(run_id, str) or not run_id for run_id in run_ids):
        raise ValueError("every sample must record a nonempty run_id")
    if len(set(run_ids)) != len(run_ids):
        raise ValueError(f"samples must come from distinct runs, got run ids: {sorted(map(str, run_ids))}")
    families = {os_family(str(metadata.get("platform", ""))) for metadata, _ in samples}
    machines = {metadata.get("machine") for metadata, _ in samples}
    manifests = {metadata.get("manifest_dir") for metadata, _ in samples}
    if len(families) != 1:
        raise ValueError(f"samples span OS families: {sorted(families)}")
    if len(machines) != 1:
        raise ValueError(f"samples span architectures: {sorted(machines)}")
    if len(manifests) != 1:
        raise ValueError("samples span manifest configurations")
    key_sets = [set(parsed) for _, parsed in samples]
    if any(keys != key_sets[0] for keys in key_sets[1:]):
        raise ValueError("samples must cover the same workload keys")
    budget = {}
    for key in sorted(key_sets[0]):
        medians = [parsed[key][0] for _, parsed in samples]
        if any(not math.isfinite(median) or median <= 0 for median in medians):
            raise ValueError(f"samples have a non-positive median_ms for {key!r}")
        baseline = statistics.median(medians)
        budget[f"{key}_median_ms"] = ceiling_ms(baseline)
        peaks = [parsed[key][1] for _, parsed in samples if parsed[key][1] is not None]
        if peaks:
            budget[f"{key}_max_rss_kb"] = ceiling_kb(max(peaks))
    family = next(iter(families))
    machine = next(iter(machines))
    budget["note"] = (
        f"Regenerated from {len(samples)} retained {family}/{machine} sample(s) measured "
        f"with tools/check_latency.py --json-out (runs: {', '.join(map(str, sorted(run_ids)))}): "
        "time ceilings are the median of retained medians plus max(15%, 0.3 ms), "
        "RSS ceilings are the retained peak plus 10%."
    )
    return budget


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("samples", nargs="+", type=Path, help="retained check_latency --json-out files")
    parser.add_argument("--output", type=Path, help="write the regenerated budget here")
    parser.add_argument("--check", type=Path, help="fail unless this budget file holds matching ceilings")
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
        if not isinstance(current, dict):
            print(f"error: {args.check} must be a JSON object", file=sys.stderr)
            return 2
        current_ceilings = {key: value for key, value in current.items() if key != "note"}
        regenerated_ceilings = {key: value for key, value in budget.items() if key != "note"}
        if current_ceilings != regenerated_ceilings:
            print(f"error: {args.check} does not match ceilings regenerated from retained samples", file=sys.stderr)
            return 1
        print(f"{args.check} matches regenerated ceilings")
    return 0


if __name__ == "__main__":
    sys.exit(main())
