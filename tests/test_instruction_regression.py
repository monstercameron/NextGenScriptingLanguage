#!/usr/bin/env python3
"""Contract tests for bootstrap wall-time intervals, perf instruction counts,
and the stored CPU-bound instruction regression gate (issue #571)."""
import importlib.util
import json
import statistics
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
COMPARE_SPEC = importlib.util.spec_from_file_location(
    "bash_compare_issue571", ROOT / "tools" / "bash_compare.py"
)
bash_compare = importlib.util.module_from_spec(COMPARE_SPEC)
COMPARE_SPEC.loader.exec_module(bash_compare)
GATE_SPEC = importlib.util.spec_from_file_location(
    "check_instruction_regression", ROOT / "tools" / "check_instruction_regression.py"
)
gate = importlib.util.module_from_spec(GATE_SPEC)
GATE_SPEC.loader.exec_module(gate)


def report(revision, instructions, *, machine="x86_64", platform="Linux-6.x", manifest_dir="manifests",
           keys=("builtin_loop", "text_upper", "spawn_loop")):
    results = []
    for key in keys:
        results.append({"key": key, "execute": {"median_instructions": instructions.get(key)}})
    return {
        "metadata": {
            "source_revision": revision,
            "platform": platform,
            "machine": machine,
            "manifest_dir": manifest_dir,
        },
        "results": results,
    }


def full_report(revision, instructions, keys):
    return report(revision, instructions, keys=keys)


class BootstrapIntervalTests(unittest.TestCase):
    def test_constant_samples_give_a_point_interval(self):
        self.assertEqual(bash_compare.median_confidence_interval([3.0] * 20), (3.0, 3.0))

    def test_interval_brackets_the_median_and_is_deterministic(self):
        samples = [float(n) for n in range(1, 21)]
        first = bash_compare.median_confidence_interval(samples)
        second = bash_compare.median_confidence_interval(samples)
        self.assertEqual(first, second)
        low, high = first
        median = statistics.median(samples)
        self.assertLessEqual(low, median)
        self.assertGreaterEqual(high, median)
        self.assertLess(low, high)

    def test_rejects_empty_samples_and_nonpositive_resamples(self):
        with self.assertRaises(ValueError):
            bash_compare.median_confidence_interval([])
        with self.assertRaises(ValueError):
            bash_compare.median_confidence_interval([1.0], resamples=0)


class PerfParsingTests(unittest.TestCase):
    def test_parses_machine_readable_perf_output(self):
        self.assertEqual(
            bash_compare.parse_perf_instructions("1234567,,instructions:u,1000,100.00,,\n"),
            1234567,
        )

    def test_restricted_counters_read_as_unavailable_not_zero(self):
        self.assertIsNone(
            bash_compare.parse_perf_instructions("<not counted>,,instructions:u,,,,\n")
        )
        self.assertIsNone(
            bash_compare.parse_perf_instructions("<not supported>,,instructions:u,,,,\n")
        )

    def test_garbage_output_reads_as_unavailable(self):
        self.assertIsNone(bash_compare.parse_perf_instructions("no counters here\n"))
        self.assertIsNone(bash_compare.parse_perf_instructions(""))

    def test_probe_failure_disables_instruction_counts(self):
        with patch.object(bash_compare.shutil, "which", return_value=None):
            with patch.object(bash_compare.sys, "platform", "linux"):
                self.assertIsNone(bash_compare.perf_stat())
        with patch.object(bash_compare.sys, "platform", "darwin"):
            self.assertIsNone(bash_compare.perf_stat())
        with patch.object(bash_compare.shutil, "which", return_value="/usr/bin/perf"):
            with patch.object(
                bash_compare.subprocess, "run",
                return_value=subprocess.CompletedProcess([], 255, "", "permission denied"),
            ):
                with patch.object(bash_compare.sys, "platform", "linux"):
                    self.assertIsNone(bash_compare.perf_stat())

    def test_perf_instructions_returns_none_without_a_counter(self):
        self.assertIsNone(bash_compare.perf_instructions(["true"], None, None))


class MeasureEnrichmentTests(unittest.TestCase):
    def test_measure_reports_confidence_interval_and_instructions(self):
        samples = [2.0 + 0.01 * n for n in range(20)]
        with patch.object(
            bash_compare, "run_once", side_effect=[(value, 0, "", None) for value in samples]
        ), patch.object(bash_compare, "peak_rss_kb", return_value=1234), patch.object(
            bash_compare, "perf_instructions", return_value=[1_000_000, 1_000_002, 999_998]
        ):
            row = bash_compare.measure(["true"], None, 20, 0, "label", None)
        self.assertEqual(row["median_ms"], statistics.median(samples))
        self.assertLessEqual(row["median_ci_low_ms"], row["median_ms"])
        self.assertGreaterEqual(row["median_ci_high_ms"], row["median_ms"])
        self.assertEqual(row["median_instructions"], 1_000_000)
        self.assertEqual(row["instruction_samples"], [1_000_000, 1_000_002, 999_998])

    def test_render_shows_instruction_and_interval_columns(self):
        row = {
            "key": "builtin_loop",
            "label": "builtin loop",
            "group": "processes",
            "bash": {
                "median_ms": 4.0, "median_instructions": 500_000,
                "median_ci_low_ms": 3.9, "median_ci_high_ms": 4.1, "max_rss_kb": 3000,
            },
            "execute": {
                "median_ms": 2.0, "median_instructions": 250_000,
                "median_ci_low_ms": 1.9, "median_ci_high_ms": 2.1, "max_rss_kb": 6000,
            },
        }
        table = bash_compare.render([row], {})
        self.assertIn("execute instr", table)
        self.assertIn("250,000", table)
        self.assertIn("500,000", table)
        self.assertIn("95% CI", table)
        missing = dict(row)
        missing["execute"] = dict(row["execute"], median_instructions=None,
                                  median_ci_low_ms=None, median_ci_high_ms=None)
        missing["bash"] = dict(row["bash"], median_instructions=None)
        self.assertIn("-", bash_compare.render([missing], {}))


class InstructionGateTests(unittest.TestCase):
    def test_allows_regression_at_or_below_two_percent(self):
        comparisons, failures, skipped = gate.evaluate(
            full_report("base", {"builtin_loop": 1_000_000, "text_upper": 500_000},
                        ("builtin_loop", "text_upper")),
            full_report("head", {"builtin_loop": 1_020_000, "text_upper": 510_000},
                        ("builtin_loop", "text_upper")),
        )
        self.assertEqual(failures, [])
        self.assertEqual(skipped, [])
        self.assertEqual(len(comparisons), 2)
        self.assertTrue(all(gated for _, _, _, _, gated in comparisons))

    def test_synthetic_three_percent_regression_fails_the_gate(self):
        _, failures, _ = gate.evaluate(
            full_report("base", {"builtin_loop": 1_000_000}, ("builtin_loop",)),
            full_report("head", {"builtin_loop": 1_030_000}, ("builtin_loop",)),
        )
        self.assertEqual(failures, ["builtin_loop"])

    def test_non_cpu_bound_regressions_are_reported_not_gated(self):
        comparisons, failures, skipped = gate.evaluate(
            full_report("base", {"builtin_loop": 1_000_000, "spawn_loop": 2_000_000},
                        ("builtin_loop", "spawn_loop")),
            full_report("head", {"builtin_loop": 1_000_000, "spawn_loop": 2_200_000},
                        ("builtin_loop", "spawn_loop")),
        )
        self.assertEqual(failures, [])
        self.assertEqual(skipped, [])
        by_key = {key: gated for key, _, _, _, gated in comparisons}
        self.assertTrue(by_key["builtin_loop"])
        self.assertFalse(by_key["spawn_loop"])

    def test_missing_instruction_data_is_skipped_not_failed(self):
        _, failures, skipped = gate.evaluate(
            full_report("base", {"builtin_loop": 1_000_000, "text_upper": None},
                        ("builtin_loop", "text_upper")),
            full_report("head", {"builtin_loop": 1_000_000, "text_upper": None},
                        ("builtin_loop", "text_upper")),
        )
        self.assertEqual(failures, [])
        self.assertEqual(skipped, ["text_upper"])

    def test_requires_exact_scenario_coverage(self):
        with self.assertRaisesRegex(ValueError, "without a stored baseline"):
            gate.evaluate(
                full_report("base", {"builtin_loop": 1}, ("builtin_loop",)),
                full_report("head", {"builtin_loop": 1, "text_upper": 2},
                            ("builtin_loop", "text_upper")),
            )

    def test_rejects_cross_platform_architecture_or_manifest_baselines(self):
        for field, metadata in (
            ("platform", {"platform": "macOS-15.0-arm64"}),
            ("machine", {"machine": "aarch64"}),
            ("manifest_dir", {"manifest_dir": None}),
        ):
            baseline = full_report("base", {"builtin_loop": 1}, ("builtin_loop",))
            baseline["metadata"].update(metadata)
            with self.subTest(field=field), self.assertRaisesRegex(ValueError, field):
                gate.evaluate(
                    baseline, full_report("head", {"builtin_loop": 1}, ("builtin_loop",)))

    def test_requires_distinct_recorded_revisions(self):
        with self.assertRaisesRegex(ValueError, "distinct source revisions"):
            gate.evaluate(
                full_report("same", {"builtin_loop": 1}, ("builtin_loop",)),
                full_report("same", {"builtin_loop": 1}, ("builtin_loop",)))

    def test_cli_rejects_a_three_percent_regression(self):
        baseline = full_report("base", {"builtin_loop": 1_000_000}, ("builtin_loop",))
        candidate = full_report("head", {"builtin_loop": 1_030_000}, ("builtin_loop",))
        with tempfile.TemporaryDirectory(prefix="instruction-gate-") as tmp:
            baseline_path = Path(tmp) / "baseline.json"
            candidate_path = Path(tmp) / "candidate.json"
            baseline_path.write_text(json.dumps(baseline), encoding="utf-8")
            candidate_path.write_text(json.dumps(candidate), encoding="utf-8")
            self.assertEqual(
                gate.main([str(baseline_path), str(candidate_path)]), 1)
            steady = full_report("head2", {"builtin_loop": 1_010_000}, ("builtin_loop",))
            steady_path = Path(tmp) / "steady.json"
            steady_path.write_text(json.dumps(steady), encoding="utf-8")
            self.assertEqual(gate.main([str(baseline_path), str(steady_path)]), 0)


if __name__ == "__main__":
    unittest.main()
