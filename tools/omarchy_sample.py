"""Seeded, quota-stratified sample of Omarchy commands for the 150-command port gate (issue #397).

The sample is drawn from the pinned inventory docs/omarchy-inventory.json before any new port
is written. Band quotas are exact. Family counts are apportioned inside each size band, so
the family of every command follows its band share and no family is left out by rounding.

Run from the repository root:
    python tools/omarchy_sample.py          write examples/omarchy-ports/sample-150.json
    python tools/omarchy_sample.py --check  exit 1 when the committed file differs
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import random
import sys

SEED = 397
SAMPLE_SIZE = 150

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
INVENTORY = os.path.join(ROOT, "docs", "omarchy-inventory.json")
OUTPUT = os.path.join(ROOT, "examples", "omarchy-ports", "sample-150.json")

BANDS = ("under_50", "50_to_200", "over_200")
MIDDLE_BAND = "50_to_200"
# Requested band shares from the issue. A share is a target, not a promise: the inventory
# holds fewer commands over 200 lines than the requested share, so the quota is capped.
REQUESTED_BAND_SHARE = {"under_50": 0.40, "50_to_200": 0.35, "over_200": 0.25}


def band_of(lines: int) -> str:
    if lines < 50:
        return "under_50"
    if lines <= 200:
        return MIDDLE_BAND
    return "over_200"


def family_of(path: str) -> str:
    name = os.path.basename(path)
    if name == "omarchy":
        return "omarchy"
    parts = name.split("-")
    return parts[1] if len(parts) > 1 else name


def apportion(total: int, weights: dict) -> dict:
    """Largest-remainder integer split of total in proportion to weights.

    Ties on the remainder go to the key that sorts first, so the result is deterministic.
    """
    keys = sorted(weights)
    weight_sum = sum(weights[k] for k in keys)
    exact = {k: total * weights[k] / weight_sum for k in keys}
    result = {k: int(exact[k]) for k in keys}
    left = total - sum(result.values())
    for k in sorted(keys, key=lambda k: (-(exact[k] - result[k]), k))[:left]:
        result[k] += 1
    return result


def band_quotas(band_population: dict, size: int) -> dict:
    quotas = apportion(size, REQUESTED_BAND_SHARE)
    shortfall = 0
    for band in BANDS:
        if quotas[band] > band_population[band]:
            shortfall += quotas[band] - band_population[band]
            quotas[band] = band_population[band]
    quotas[MIDDLE_BAND] += shortfall
    for band in BANDS:
        if quotas[band] > band_population[band]:
            raise ValueError(f"band {band} needs {quotas[band]} but has {band_population[band]}")
    return quotas


def load_inventory(path: str):
    with open(path, "rb") as handle:
        raw = handle.read()
    return json.loads(raw.decode("utf-8")), hashlib.sha256(raw).hexdigest()


def build_sample(inventory_path: str = INVENTORY, seed: int = SEED, size: int = SAMPLE_SIZE) -> dict:
    inventory, inventory_sha256 = load_inventory(inventory_path)
    rows = []
    seen = set()
    for entry in inventory["commands"]:
        path = entry["path"]
        if path in seen:
            raise ValueError(f"duplicate inventory path {path}")
        seen.add(path)
        rows.append({
            "path": path,
            "family": family_of(path),
            "band": band_of(entry["lines"]),
            "lines": entry["lines"],
            "sha256": entry["sha256"],
        })
    rows.sort(key=lambda row: row["path"])

    band_population = {band: 0 for band in BANDS}
    for row in rows:
        band_population[row["band"]] += 1
    quotas = band_quotas(band_population, size)

    chosen = []
    for band in BANDS:
        in_band = [row for row in rows if row["band"] == band]
        family_population = {}
        for row in in_band:
            family_population[row["family"]] = family_population.get(row["family"], 0) + 1
        family_quota = apportion(quotas[band], family_population)
        for family in sorted(family_quota):
            cell = sorted((row for row in in_band if row["family"] == family), key=lambda r: r["path"])
            # One independent generator per cell keeps each cell's draw stable if others change.
            rng = random.Random(f"{seed}:{band}:{family}")
            chosen.extend(rng.sample(cell, family_quota[family]))
    chosen.sort(key=lambda row: row["path"])

    counts_band = {band: 0 for band in BANDS}
    counts_family = {}
    for row in chosen:
        counts_band[row["band"]] += 1
        counts_family[row["family"]] = counts_family.get(row["family"], 0) + 1

    return {
        "schema": 1,
        "issue": 397,
        "seed": seed,
        "sample_size": size,
        "inventory": {
            "path": "docs/omarchy-inventory.json",
            "sha256": inventory_sha256,
            "release_tag": inventory["release_tag"],
            "commit": inventory["commit"],
            "command_count": len(rows),
        },
        "method": (
            "Band quotas from the requested 40/35/25 percent shares, capped at the band population "
            "and with any shortfall moved to the middle band. Family counts are apportioned "
            "inside each band in proportion to the band population of each family. Each band and "
            "family cell is drawn with random.Random seeded by seed, band and family name."
        ),
        "bands": {
            "under_50": "lines < 50",
            "50_to_200": "50 <= lines <= 200",
            "over_200": "lines > 200",
        },
        "population": {
            "bands": band_population,
        },
        "quotas": {
            "bands": quotas,
            "requested_band_share": REQUESTED_BAND_SHARE,
        },
        "counts": {
            "bands": counts_band,
            "families": dict(sorted(counts_family.items())),
        },
        "commands": chosen,
    }


def render(sample: dict) -> bytes:
    return (json.dumps(sample, indent=2, sort_keys=True, ensure_ascii=False) + "\n").encode("utf-8")


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("--check", action="store_true", help="fail when the committed sample differs")
    args = parser.parse_args(argv)
    expected = render(build_sample())
    if args.check:
        try:
            with open(OUTPUT, "rb") as handle:
                committed = handle.read()
        except FileNotFoundError:
            print(f"missing {OUTPUT}", file=sys.stderr)
            return 1
        if committed != expected:
            print(f"{OUTPUT} differs from the seeded sample; regenerate it", file=sys.stderr)
            return 1
        print("sample matches")
        return 0
    with open(OUTPUT, "wb") as handle:
        handle.write(expected)
    print(f"wrote {OUTPUT}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
