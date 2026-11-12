"""Tests for tools/omarchy_sample.py. Run with: python tools/test_omarchy_sample.py"""

from __future__ import annotations

import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import omarchy_sample as sample  # noqa: E402


class ApportionTests(unittest.TestCase):
    def test_split_sums_to_total_and_is_deterministic(self):
        weights = {"a": 1, "b": 1, "c": 1}
        first = sample.apportion(10, weights)
        self.assertEqual(sum(first.values()), 10)
        self.assertEqual(first, sample.apportion(10, weights))

    def test_remainder_ties_go_to_the_first_key(self):
        self.assertEqual(sample.apportion(1, {"x": 1, "y": 1}), {"x": 1, "y": 0})


class BandTests(unittest.TestCase):
    def test_band_boundaries(self):
        self.assertEqual(sample.band_of(49), "under_50")
        self.assertEqual(sample.band_of(50), "50_to_200")
        self.assertEqual(sample.band_of(200), "50_to_200")
        self.assertEqual(sample.band_of(201), "over_200")

    def test_quota_is_capped_at_population_and_shortfall_moves_to_middle(self):
        population = {"under_50": 283, "50_to_200": 127, "over_200": 34}
        quotas = sample.band_quotas(population, 150)
        self.assertEqual(quotas, {"under_50": 60, "50_to_200": 56, "over_200": 34})
        self.assertEqual(sum(quotas.values()), 150)


class SampleTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.first = sample.build_sample()
        cls.second = sample.build_sample()

    def test_two_runs_with_the_same_seed_are_identical(self):
        self.assertEqual(sample.render(self.first), sample.render(self.second))

    def test_band_quotas_are_met_exactly(self):
        self.assertEqual(self.first["counts"]["bands"], self.first["quotas"]["bands"])
        self.assertEqual(self.first["quotas"]["bands"], {"under_50": 60, "50_to_200": 56, "over_200": 34})

    def test_sample_has_150_distinct_inventory_commands(self):
        commands = self.first["commands"]
        self.assertEqual(len(commands), sample.SAMPLE_SIZE)
        paths = [c["path"] for c in commands]
        self.assertEqual(len(set(paths)), len(paths))

        inventory, _ = sample.load_inventory(sample.INVENTORY)
        by_path = {entry["path"]: entry for entry in inventory["commands"]}
        for command in commands:
            entry = by_path[command["path"]]
            self.assertEqual(command["lines"], entry["lines"])
            self.assertEqual(command["sha256"], entry["sha256"])
            self.assertEqual(command["band"], sample.band_of(entry["lines"]))
            self.assertEqual(command["family"], sample.family_of(command["path"]))

    def test_family_counts_match_the_commands(self):
        counted = {}
        for command in self.first["commands"]:
            counted[command["family"]] = counted.get(command["family"], 0) + 1
        self.assertEqual(counted, self.first["counts"]["families"])

    def test_every_over_200_line_command_is_in_the_sample(self):
        inventory, _ = sample.load_inventory(sample.INVENTORY)
        big = {e["path"] for e in inventory["commands"] if e["lines"] > 200}
        chosen = {c["path"] for c in self.first["commands"] if c["band"] == "over_200"}
        self.assertEqual(chosen, big)

    def test_committed_sample_matches_regeneration(self):
        with open(sample.OUTPUT, "rb") as handle:
            committed = handle.read()
        self.assertEqual(
            committed,
            sample.render(self.first),
            "examples/omarchy-ports/sample-150.json differs from tools/omarchy_sample.py; regenerate it",
        )


if __name__ == "__main__":
    unittest.main()
