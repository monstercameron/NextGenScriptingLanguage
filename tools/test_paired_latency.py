#!/usr/bin/env python3
"""Focused tests for interleaved latency comparison behavior."""
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from compare_latency import paired_samples, summarize_pairs  # noqa: E402


class PairedLatencyTests(unittest.TestCase):
    def test_workflow_pairs_only_failed_main_pushes_with_current_manifests(self) -> None:
        root = Path(__file__).resolve().parents[1]
        workflow = (root / ".github/workflows/ci.yml").read_text(encoding="utf-8")
        self.assertIn("id: linux_latency", workflow)
        self.assertIn(
            "if: failure() && steps.linux_latency.outcome == 'failure' && github.event_name == 'push'",
            workflow,
        )
        self.assertIn("BASE_SHA: ${{ github.event.before }}", workflow)
        self.assertIn('--manifest-dir "$GITHUB_WORKSPACE/manifests"', workflow)
        self.assertIn("continue-on-error: true", workflow)
        self.assertIn("id: paired_latency", workflow)
        self.assertIn("path: target/paired-latency.json", workflow)

    def test_warmups_and_samples_alternate_balanced_order(self) -> None:
        calls = []
        elapsed = iter(range(1, 11))

        def runner(command):
            name = Path(command[0]).name
            calls.append(name)
            return next(elapsed), 100 if name == "baseline" else 120, 0, ""

        samples, rss = paired_samples(
            Path("/bin/baseline"), Path("/bin/candidate"), ["--version"], 3, 2, runner
        )

        self.assertEqual(
            calls,
            [
                "baseline", "candidate",
                "candidate", "baseline",
                "baseline", "candidate",
                "candidate", "baseline",
                "baseline", "candidate",
            ],
        )
        self.assertEqual(samples, {"baseline": [5, 8, 9], "candidate": [6, 7, 10]})
        self.assertEqual(rss, {"baseline": [100, 100, 100], "candidate": [120, 120, 120]})

    def test_reports_independent_medians_and_paired_deltas(self) -> None:
        result = summarize_pairs(
            {"baseline": [4.0, 6.0], "candidate": [6.0, 6.0]},
            {"baseline": [100, 110], "candidate": [120, 130]},
        )

        self.assertEqual(result["baseline_median_ms"], 5.0)
        self.assertEqual(result["candidate_median_ms"], 6.0)
        self.assertEqual(result["paired_delta_pct"], [50.0, 0.0])
        self.assertEqual(result["paired_delta_pct_median"], 25.0)
        self.assertEqual(result["baseline_max_rss_kb"], 110)
        self.assertEqual(result["candidate_max_rss_kb"], 130)

    def test_nonzero_child_status_names_the_binary_that_failed(self) -> None:
        def runner(command):
            return 1.0, None, 2, "bad run"

        with self.assertRaisesRegex(RuntimeError, "baseline failed with status 2: bad run"):
            paired_samples(
                Path("/bin/baseline"), Path("/bin/candidate"), ["--version"], 1, 0, runner
            )


if __name__ == "__main__":
    unittest.main()
