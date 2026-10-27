#!/usr/bin/env python3
"""Contract tests for same-runner binary-size comparison."""
import io
import json
import sys
import tempfile
import unittest
from contextlib import redirect_stdout
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from compare_binary_size import main, measure, summarize  # noqa: E402


def write_bytes(root, name, size):
    path = root / name
    path.write_bytes(b"x" * size)
    return path


class SummarizeTests(unittest.TestCase):
    def test_equal_binaries_report_zero_delta(self):
        self.assertEqual(
            summarize(8_000_000, 8_000_000),
            {
                "baseline_bytes": 8_000_000,
                "candidate_bytes": 8_000_000,
                "delta_bytes": 0,
                "delta_pct": 0.0,
            },
        )

    def test_growth_reports_signed_delta(self):
        result = summarize(8_200_000, 8_228_112)
        self.assertEqual(result["delta_bytes"], 28_112)
        self.assertAlmostEqual(result["delta_pct"], 0.3428, places=3)

    def test_shrinkage_reports_negative_delta(self):
        result = summarize(8_200_000, 8_100_000)
        self.assertEqual(result["delta_bytes"], -100_000)
        self.assertLess(result["delta_pct"], 0.0)

    def test_nonpositive_baseline_is_rejected(self):
        with self.assertRaisesRegex(ValueError, "positive"):
            summarize(0, 8_000_000)

    def test_missing_binary_names_the_path(self):
        with self.assertRaisesRegex(ValueError, "not a built executable"):
            measure(Path("/does/not/exist-execute"))


class CompareMainTests(unittest.TestCase):
    def run_main(self, argv):
        stdout = io.StringIO()
        with redirect_stdout(stdout):
            status = main(argv)
        return status, stdout.getvalue()

    def test_identical_binaries_pass_without_thresholds(self):
        with tempfile.TemporaryDirectory(prefix="execute-size-") as directory:
            root = Path(directory)
            first = write_bytes(root, "baseline", 1000)
            second = write_bytes(root, "candidate", 1000)
            status, output = self.run_main(
                ["--baseline", str(first), "--candidate", str(second)]
            )
            self.assertEqual(status, 0)
            self.assertIn("delta: +0 bytes", output)
            self.assertIn("budgets are unchanged", output)

    def test_growth_is_diagnostic_without_thresholds(self):
        with tempfile.TemporaryDirectory(prefix="execute-size-") as directory:
            root = Path(directory)
            first = write_bytes(root, "baseline", 1000)
            second = write_bytes(root, "candidate", 1100)
            status, output = self.run_main(
                [
                    "--baseline", str(first),
                    "--candidate", str(second),
                    "--baseline-label", "abc123",
                    "--candidate-label", "def456",
                ]
            )
            self.assertEqual(status, 0)
            self.assertIn("abc123: 1000 bytes; def456: 1100 bytes", output)
            self.assertIn("delta: +100 bytes (+10.00%)", output)

    def test_bytes_threshold_fails_only_on_growth_past_limit(self):
        with tempfile.TemporaryDirectory(prefix="execute-size-") as directory:
            root = Path(directory)
            first = write_bytes(root, "baseline", 1000)
            bigger = write_bytes(root, "bigger", 1100)
            smaller = write_bytes(root, "smaller", 900)
            self.assertEqual(
                self.run_main(
                    ["--baseline", str(first), "--candidate", str(bigger),
                     "--max-delta-bytes", "99"]
                )[0],
                1,
            )
            self.assertEqual(
                self.run_main(
                    ["--baseline", str(first), "--candidate", str(bigger),
                     "--max-delta-bytes", "100"]
                )[0],
                0,
            )
            self.assertEqual(
                self.run_main(
                    ["--baseline", str(first), "--candidate", str(smaller),
                     "--max-delta-bytes", "0"]
                )[0],
                0,
            )

    def test_percent_threshold_rejects_excess_growth(self):
        with tempfile.TemporaryDirectory(prefix="execute-size-") as directory:
            root = Path(directory)
            first = write_bytes(root, "baseline", 1000)
            second = write_bytes(root, "candidate", 1100)
            self.assertEqual(
                self.run_main(
                    ["--baseline", str(first), "--candidate", str(second),
                     "--max-delta-pct", "9.9"]
                )[0],
                1,
            )
            self.assertEqual(
                self.run_main(
                    ["--baseline", str(first), "--candidate", str(second),
                     "--max-delta-pct", "10.0"]
                )[0],
                0,
            )

    def test_negative_bytes_threshold_is_rejected(self):
        with tempfile.TemporaryDirectory(prefix="execute-size-") as directory:
            root = Path(directory)
            first = write_bytes(root, "baseline", 1000)
            status, _ = self.run_main(
                ["--baseline", str(first), "--candidate", str(first),
                 "--max-delta-bytes", "-1"]
            )
            self.assertEqual(status, 1)

    def test_missing_candidate_reports_an_error(self):
        with tempfile.TemporaryDirectory(prefix="execute-size-") as directory:
            root = Path(directory)
            first = write_bytes(root, "baseline", 1000)
            self.assertEqual(
                main(["--baseline", str(first), "--candidate", str(root / "absent")]),
                1,
            )

    def test_json_out_retains_both_sizes_and_delta(self):
        with tempfile.TemporaryDirectory(prefix="execute-size-") as directory:
            root = Path(directory)
            first = write_bytes(root, "baseline", 1000)
            second = write_bytes(root, "candidate", 1100)
            document_path = root / "paired.json"
            status, _ = self.run_main(
                ["--baseline", str(first), "--candidate", str(second),
                 "--json-out", str(document_path)]
            )
            self.assertEqual(status, 0)
            document = json.loads(document_path.read_text(encoding="utf-8"))
            self.assertEqual(document["results"]["delta_bytes"], 100)
            self.assertEqual(document["results"]["baseline_bytes"], 1000)
            self.assertEqual(document["results"]["candidate_bytes"], 1100)
            self.assertIn("baseline", document["metadata"])
            self.assertIn("candidate", document["metadata"])


class WorkflowWiringTests(unittest.TestCase):
    def test_size_gate_failure_triggers_same_runner_diagnosis(self) -> None:
        root = Path(__file__).resolve().parents[1]
        workflow = (root / ".github/workflows/ci.yml").read_text(encoding="utf-8")
        self.assertIn("id: linux_binary_size", workflow)
        self.assertIn(
            "if: failure() && steps.linux_binary_size.outcome == 'failure'"
            " && github.event_name == 'push'",
            workflow,
        )
        self.assertIn("BASE_SHA: ${{ github.event.before }}", workflow)
        self.assertIn("binary-size-parent-src", workflow)
        self.assertIn("tools/compare_binary_size.py", workflow)
        self.assertIn("--baseline \"$baseline_bin\"", workflow)
        self.assertIn("--candidate \"$candidate_bin\"", workflow)
        self.assertIn("--json-out target/paired-binary-size.json", workflow)
        self.assertIn("continue-on-error: true", workflow)
        self.assertIn("id: paired_binary_size", workflow)
        self.assertIn("path: target/paired-binary-size.json", workflow)
        self.assertIn("python3 tools/test_compare_binary_size.py", workflow)


if __name__ == "__main__":
    unittest.main()
