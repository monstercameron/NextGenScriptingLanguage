"""Budget lookup contracts for default and explicit manifest rows."""
import importlib.util
import json
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location("check_latency", ROOT / "tools" / "check_latency.py")
check_latency = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(check_latency)


class LatencyBudgetLimitTests(unittest.TestCase):
    def test_explicit_manifest_row_inherits_existing_limit(self):
        budget = {"one_line_median_ms": 5.0, "one_line_max_rss_kb": 20000}
        self.assertEqual(check_latency.budget_limit(budget, "one_line_manifest_dir", "median_ms"), 5.0)
        self.assertEqual(check_latency.budget_limit(budget, "one_line_manifest_dir", "max_rss_kb"), 20000)

    def test_row_specific_limit_takes_precedence(self):
        budget = {"one_line_median_ms": 5.0, "one_line_manifest_dir_median_ms": 4.0}
        self.assertEqual(check_latency.budget_limit(budget, "one_line_manifest_dir", "median_ms"), 4.0)

    def test_existing_budget_file_is_unchanged(self):
        budget = json.loads(
            (ROOT / "tools/linux-latency-budget.json").read_text(encoding="utf-8")
        )
        self.assertEqual(budget["version_median_ms"], 3.0)
        self.assertEqual(budget["one_line_median_ms"], 5.0)
        self.assertEqual(budget["ten_step_median_ms"], 8.0)
        self.assertEqual(budget["hot_loop_median_ms"], 500.0)
        self.assertEqual(budget["version_max_rss_kb"], 16000)
        self.assertEqual(budget["one_line_max_rss_kb"], 20000)
        self.assertEqual(budget["ten_step_max_rss_kb"], 24000)
        self.assertEqual(budget["hot_loop_max_rss_kb"], 32000)


if __name__ == "__main__":
    unittest.main()
