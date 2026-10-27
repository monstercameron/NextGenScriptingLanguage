#!/usr/bin/env python3
"""Contract tests for the stored Execute/Bash ratio gate."""
import importlib.util
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location(
    "check_bash_ratio_regression", ROOT / "tools" / "check_bash_ratio_regression.py"
)
gate = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(gate)


def report(revision, ratios, *, machine="x86_64", platform="Linux-6.x", manifest_dir="manifests"):
    return {
        "metadata": {
            "source_revision": revision,
            "platform": platform,
            "machine": machine,
            "manifest_dir": manifest_dir,
        },
        "results": [
            {"key": key, "bash": {"median_ms": 1.0}, "execute": {"median_ms": ratio}}
            for key, ratio in ratios.items()
        ],
    }


class BashRatioRegressionTests(unittest.TestCase):
    def test_allows_regression_at_or_below_ten_percent(self):
        comparisons, failures = gate.evaluate(
            report("base", {"startup": 1.0, "pipeline": 0.5}),
            report("head", {"startup": 1.1, "pipeline": 0.55}),
        )
        self.assertEqual(failures, [])
        self.assertEqual(len(comparisons), 2)

    def test_allows_linux_kernel_patch_changes_for_same_platform_and_architecture(self):
        _, failures = gate.evaluate(
            report("base", {"startup": 1.0}, platform="Linux-6.11.0-1018-azure"),
            report("head", {"startup": 1.05}, platform="Linux-6.11.0-1021-azure"),
        )
        self.assertEqual(failures, [])

    def test_rejects_more_than_ten_percent_regression_per_scenario(self):
        _, failures = gate.evaluate(
            report("base", {"startup": 1.0, "pipeline": 0.5}),
            report("head", {"startup": 1.2, "pipeline": 0.5}),
        )
        self.assertEqual(failures, ["startup"])

    def test_requires_exact_scenario_coverage(self):
        with self.assertRaisesRegex(ValueError, "without a stored baseline"):
            gate.evaluate(report("base", {"startup": 1.0}), report("head", {"startup": 1.0, "new": 2.0}))

    def test_rejects_cross_platform_architecture_or_manifest_baselines(self):
        for field, metadata in (
            ("platform", {"platform": "macOS-15.0-arm64"}),
            ("machine", {"machine": "aarch64"}),
            ("manifest_dir", {"manifest_dir": None}),
        ):
            baseline = report("base", {"startup": 1.0})
            baseline["metadata"].update(metadata)
            with self.subTest(field=field), self.assertRaisesRegex(ValueError, field):
                gate.evaluate(baseline, report("head", {"startup": 1.0}))

    def test_requires_distinct_recorded_revisions(self):
        with self.assertRaisesRegex(ValueError, "distinct source revisions"):
            gate.evaluate(report("same", {"startup": 1.0}), report("same", {"startup": 1.0}))

    def test_rejects_incomplete_baseline_report(self):
        baseline = report("base", {"startup": 1.0})
        del baseline["metadata"]["machine"]
        with self.assertRaisesRegex(ValueError, "missing: machine"):
            gate.evaluate(baseline, report("head", {"startup": 1.0}))


if __name__ == "__main__":
    unittest.main()
