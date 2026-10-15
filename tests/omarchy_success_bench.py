#!/usr/bin/env python3
"""Non-measuring unit checks for the paired Omarchy success wrapper."""
from pathlib import Path
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))

from benchmarks import omarchy_success as harness


class OmarchySuccessHarnessTests(unittest.TestCase):
    def test_summary_uses_median_and_nearest_rank_p95(self):
        self.assertEqual(
            harness.summarize([7, 1, 5, 3, 9]),
            {
                "sample_count": 5,
                "median_ns": 5,
                "p95_nearest_rank_ns": 9,
                "minimum_ns": 1,
                "maximum_ns": 9,
            },
        )

    def test_interleaving_alternates_the_first_side(self):
        self.assertEqual(harness.interleaved_order(0), ("baseline", "candidate"))
        self.assertEqual(harness.interleaved_order(1), ("candidate", "baseline"))
        self.assertEqual(harness.interleaved_order(2), ("baseline", "candidate"))

    def test_report_path_is_fresh_and_scoped_to_benchmarks(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            directory = root / "benchmarks"
            directory.mkdir()
            expected = directory / "omarchy-success-final.json"
            self.assertEqual(harness.validate_report_path(expected, directory), expected)
            with self.assertRaises(ValueError):
                harness.validate_report_path(root / "omarchy-success-outside.json", directory)
            with self.assertRaises(ValueError):
                harness.validate_report_path(directory / "result.json", directory)
            expected.write_text("existing")
            with self.assertRaises(FileExistsError):
                harness.validate_report_path(expected, directory)

    def test_parity_requires_success_and_exact_encoded_streams(self):
        equal = {
            "status": 0,
            "stdout_b64": "b2sK",
            "stderr_b64": "",
        }
        harness.assert_success_parity(equal, dict(equal), "fixture")
        with self.assertRaisesRegex(AssertionError, "did not exit successfully"):
            harness.assert_success_parity(dict(equal, status=1), equal, "fixture")
        with self.assertRaisesRegex(AssertionError, "differ in stdout_b64"):
            harness.assert_success_parity(equal, dict(equal, stdout_b64="bm8K"), "fixture")
        with self.assertRaisesRegex(AssertionError, "differ in stderr_b64"):
            harness.assert_success_parity(equal, dict(equal, stderr_b64="ZXJyb3I="), "fixture")

    def test_baseline_inventory_is_explicitly_ten_programs(self):
        self.assertEqual(len(harness.SCENARIOS), 10)
        self.assertEqual(
            {scenario["id"] for scenario in harness.SCENARIOS},
            {f"{index:02d}-" + suffix for index, suffix in (
                (1, "notification-time"),
                (2, "theme-current"),
                (3, "update-analyze-logs"),
                (4, "power-present"),
                (5, "theme-set-browser"),
                (6, "default-browser"),
                (7, "workspace-layout-toggle"),
                (8, "webapp-remove"),
                (9, "audio-output-volume"),
                (10, "theme-set-vscode"),
            )},
        )

    def test_legacy_baseline_catalog_is_inert_and_reproducible(self):
        with tempfile.TemporaryDirectory() as temporary:
            manifest_dir, digest = harness.make_legacy_catalog(Path(temporary))
            manifest_path = manifest_dir / "legacy-catalog.json"
            document = harness.json.loads(manifest_path.read_text(encoding="utf-8"))
            self.assertEqual(document, harness.LEGACY_CATALOG_MANIFEST)
            self.assertNotIn("contract", document)
            self.assertEqual(digest, harness.legacy_catalog_sha256())


if __name__ == "__main__":
    unittest.main()
