#!/usr/bin/env python3
"""Check the stored Execute/Bash ratio gate in check_latency.py."""
import json
import tempfile
import unittest
from pathlib import Path

import sys

# The tool sits beside this file; make the import work under unittest discovery as well.
sys.path.insert(0, str(Path(__file__).resolve().parent))

from check_latency import main, run_ratio_gate  # noqa: E402


def report(revision, rows):
    """Build a bash_compare-shaped report from {key: (bash_ms, execute_ms)}."""
    return {
        "metadata": {
            "source_revision": revision,
            "platform": "Linux-6.8",
            "machine": "x86_64",
            "manifest_dir": "manifests",
        },
        "results": [
            {"key": key, "bash": {"median_ms": bash}, "execute": {"median_ms": execute}}
            for key, (bash, execute) in rows.items()
        ],
    }


class RatioGateTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="execute-ratio-gate-")
        self.root = Path(self.directory.name)

    def tearDown(self):
        self.directory.cleanup()

    def write(self, name, payload):
        path = self.root / name
        path.write_text(json.dumps(payload), encoding="utf-8")
        return path

    def test_twenty_percent_worse_ratio_fails(self):
        baseline = self.write("baseline.json", report("aaa111", {"startup": (10.0, 20.0)}))
        candidate = self.write("candidate.json", report("bbb222", {"startup": (10.0, 24.0)}))
        self.assertEqual(run_ratio_gate(baseline, candidate), 1)

    def test_unchanged_ratio_passes_even_when_absolute_times_move(self):
        baseline = self.write("baseline.json", report("aaa111", {"startup": (10.0, 20.0)}))
        candidate = self.write("candidate.json", report("bbb222", {"startup": (12.0, 24.0)}))
        self.assertEqual(run_ratio_gate(baseline, candidate), 0)

    def test_regression_within_ten_percent_passes(self):
        baseline = self.write("baseline.json", report("aaa111", {"startup": (10.0, 20.0)}))
        candidate = self.write("candidate.json", report("bbb222", {"startup": (10.0, 21.8)}))
        self.assertEqual(run_ratio_gate(baseline, candidate), 0)

    def test_one_regressed_scenario_fails_the_gate(self):
        baseline = self.write(
            "baseline.json", report("aaa111", {"startup": (10.0, 20.0), "pipeline": (5.0, 10.0)})
        )
        candidate = self.write(
            "candidate.json", report("bbb222", {"startup": (10.0, 20.0), "pipeline": (5.0, 12.5)})
        )
        self.assertEqual(run_ratio_gate(baseline, candidate), 1)

    def test_missing_report_is_an_error_not_a_pass(self):
        baseline = self.write("baseline.json", report("aaa111", {"startup": (10.0, 20.0)}))
        self.assertEqual(run_ratio_gate(baseline, self.root / "absent.json"), 2)

    def test_identical_revisions_are_refused(self):
        baseline = self.write("baseline.json", report("aaa111", {"startup": (10.0, 20.0)}))
        candidate = self.write("candidate.json", report("aaa111", {"startup": (10.0, 24.0)}))
        self.assertEqual(run_ratio_gate(baseline, candidate), 2)

    def test_main_requires_both_ratio_reports(self):
        baseline = self.write("baseline.json", report("aaa111", {"startup": (10.0, 20.0)}))
        self.assertEqual(
            main([str(self.root / "execute"), str(self.root / "budget.json"), "--ratio-baseline", str(baseline)]),
            2,
        )

    def test_main_ratio_mode_exits_without_timing(self):
        baseline = self.write("baseline.json", report("aaa111", {"startup": (10.0, 20.0)}))
        candidate = self.write("candidate.json", report("bbb222", {"startup": (10.0, 24.0)}))
        status = main(
            [
                str(self.root / "execute"),
                str(self.root / "budget.json"),
                "--ratio-baseline",
                str(baseline),
                "--ratio-candidate",
                str(candidate),
            ]
        )
        self.assertEqual(status, 1)


if __name__ == "__main__":
    unittest.main()
