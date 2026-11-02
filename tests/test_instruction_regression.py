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
           keys=("builtin_loop", "text_upper", "spawn_loop"), extra=None):
    results = []
    for key in keys:
        results.append({"key": key, "execute": {"median_instructions": instructions.get(key)}})
    metadata = {
        "source_revision": revision,
        "platform": platform,
        "machine": machine,
        "manifest_dir": manifest_dir,
    }
    if extra:
        metadata.update(extra)
    return {"metadata": metadata, "results": results}


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
            row = bash_compare.measure(["true"], None, 20, 0, "label", None,
                                        perf_binary="/usr/bin/perf")
        self.assertEqual(row["median_ms"], statistics.median(samples))
        self.assertLessEqual(row["median_ci_low_ms"], row["median_ms"])
        self.assertGreaterEqual(row["median_ci_high_ms"], row["median_ms"])
        self.assertEqual(row["median_instructions"], 1_000_000)
        self.assertEqual(row["instruction_samples"], [1_000_000, 1_000_002, 999_998])

    def test_measure_uses_cachegrind_when_perf_is_absent(self):
        samples = [2.0 + 0.01 * n for n in range(20)]
        with patch.object(
            bash_compare, "run_once", side_effect=[(value, 0, "", None) for value in samples]
        ), patch.object(bash_compare, "peak_rss_kb", return_value=1234), patch.object(
            bash_compare, "cachegrind_instructions", return_value=[2_000_000, 2_000_002]
        ) as cachegrind, patch.object(
            bash_compare, "perf_instructions",
            side_effect=AssertionError("perf must not run without a perf binary"),
        ):
            row = bash_compare.measure(["true"], None, 20, 0, "label", None,
                                        cachegrind_binary="/usr/bin/valgrind")
        cachegrind.assert_called_once()
        self.assertEqual(row["median_instructions"], 2_000_001)
        self.assertEqual(row["instruction_samples"], [2_000_000, 2_000_002])

    def test_measure_reports_no_instructions_without_a_counter(self):
        samples = [2.0 + 0.01 * n for n in range(20)]
        with patch.object(
            bash_compare, "run_once", side_effect=[(value, 0, "", None) for value in samples]
        ), patch.object(bash_compare, "peak_rss_kb", return_value=None):
            row = bash_compare.measure(["true"], None, 20, 0, "label", None)
        self.assertIsNone(row["median_instructions"])
        self.assertIsNone(row["instruction_samples"])

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


CACHEGRIND_SAMPLE = (
    "==12345== I   refs:      6,081,055\n"
    "==12345== I1  misses:           1,234\n"
    "==12345== LLi misses:             567\n"
)


class CachegrindParsingTests(unittest.TestCase):
    def test_parses_instruction_total_with_thousands_separators(self):
        self.assertEqual(
            bash_compare.parse_cachegrind_instructions(CACHEGRIND_SAMPLE), 6081055
        )

    def test_garbage_output_reads_as_unavailable_not_zero(self):
        self.assertIsNone(bash_compare.parse_cachegrind_instructions("no counters here\n"))
        self.assertIsNone(bash_compare.parse_cachegrind_instructions(""))
        self.assertIsNone(
            bash_compare.parse_cachegrind_instructions("==12345== I1  misses: 12\n")
        )

    def test_cachegrind_instructions_returns_none_without_a_binary(self):
        self.assertIsNone(bash_compare.cachegrind_instructions(["true"], None, None))

    def test_probe_fails_without_valgrind_or_off_linux(self):
        with patch.object(bash_compare.shutil, "which", return_value=None):
            with patch.object(bash_compare.sys, "platform", "linux"):
                self.assertIsNone(bash_compare.cachegrind_stat())
        with patch.object(bash_compare.sys, "platform", "darwin"):
            self.assertIsNone(bash_compare.cachegrind_stat())

    def test_probe_failure_disables_cachegrind_counts(self):
        with patch.object(bash_compare.shutil, "which", return_value="/usr/bin/valgrind"):
            with patch.object(
                bash_compare.subprocess, "run",
                return_value=subprocess.CompletedProcess([], 1, "", "valgrind: bad option"),
            ):
                with patch.object(bash_compare.sys, "platform", "linux"):
                    self.assertIsNone(bash_compare.cachegrind_stat())
        with patch.object(bash_compare.shutil, "which", return_value="/usr/bin/valgrind"):
            with patch.object(
                bash_compare.subprocess, "run",
                return_value=subprocess.CompletedProcess([], 0, "", "no counts here"),
            ):
                with patch.object(bash_compare.sys, "platform", "linux"):
                    self.assertIsNone(bash_compare.cachegrind_stat())

    def test_probe_success_returns_the_binary(self):
        with patch.object(bash_compare.shutil, "which", return_value="/usr/bin/valgrind"):
            with patch.object(
                bash_compare.subprocess, "run",
                return_value=subprocess.CompletedProcess([], 0, "", CACHEGRIND_SAMPLE),
            ):
                with patch.object(bash_compare.sys, "platform", "linux"):
                    self.assertEqual(bash_compare.cachegrind_stat(), "/usr/bin/valgrind")


class BackendResolutionTests(unittest.TestCase):
    def test_auto_prefers_perf_over_cachegrind(self):
        self.assertEqual(
            bash_compare.resolve_instruction_backend(
                "auto", perf_binary="/usr/bin/perf", cachegrind_binary="/usr/bin/valgrind"),
            ("perf", "/usr/bin/perf"),
        )

    def test_auto_falls_back_to_cachegrind_without_perf(self):
        self.assertEqual(
            bash_compare.resolve_instruction_backend(
                "auto", perf_binary="", cachegrind_binary="/usr/bin/valgrind"),
            ("cachegrind", "/usr/bin/valgrind"),
        )

    def test_auto_reports_no_counters_without_a_backend(self):
        self.assertEqual(
            bash_compare.resolve_instruction_backend(
                "auto", perf_binary="", cachegrind_binary=""),
            (None, None),
        )

    def test_explicit_backend_never_falls_through(self):
        self.assertEqual(
            bash_compare.resolve_instruction_backend(
                "perf", perf_binary="", cachegrind_binary="/usr/bin/valgrind"),
            ("perf", None),
        )
        self.assertEqual(
            bash_compare.resolve_instruction_backend(
                "cachegrind", perf_binary="/usr/bin/perf", cachegrind_binary=""),
            ("cachegrind", None),
        )
        self.assertEqual(
            bash_compare.resolve_instruction_backend(
                "none", perf_binary="/usr/bin/perf",
                cachegrind_binary="/usr/bin/valgrind"),
            (None, None),
        )

    def test_rejects_unknown_backend(self):
        with self.assertRaises(ValueError):
            bash_compare.resolve_instruction_backend("callgrind")


class DiagnosticsAndCoverageTests(unittest.TestCase):
    def test_diagnostics_names_backend_and_probe_outcomes(self):
        text = bash_compare.counter_diagnostics(
            backend="perf", perf_binary="/usr/bin/perf",
            cachegrind_binary=None, cachegrind_attempted=True)
        self.assertIn("backend=perf", text)
        self.assertIn("perf=/usr/bin/perf(probe ok)", text)
        self.assertIn("perf_event_paranoid=", text)

    def test_diagnostics_explains_missing_counters(self):
        text = bash_compare.counter_diagnostics(
            backend=None, perf_binary=None,
            cachegrind_binary=None, cachegrind_attempted=True)
        self.assertIn("backend=none", text)
        self.assertNotIn("probe ok", text)

    def test_coverage_counts_execute_arms_with_data(self):
        results = [
            {"key": "a", "execute": {"median_instructions": 10}},
            {"key": "b", "execute": {"median_instructions": None}},
            {"key": "c", "execute": {}},
        ]
        self.assertEqual(bash_compare.instruction_coverage(results), (1, 3))
        self.assertEqual(bash_compare.instruction_coverage([]), (0, 0))


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

    def test_fail_closed_missing_gated_data_fails(self):
        _, failures, skipped = gate.evaluate(
            full_report("base", {"builtin_loop": 1_000_000, "text_upper": None},
                        ("builtin_loop", "text_upper")),
            full_report("head", {"builtin_loop": 1_000_000, "text_upper": None},
                        ("builtin_loop", "text_upper")),
            fail_closed=True,
        )
        self.assertEqual(failures, ["text_upper"])
        self.assertEqual(skipped, ["text_upper"])

    def test_fail_closed_missing_non_gated_data_stays_skipped(self):
        _, failures, skipped = gate.evaluate(
            full_report("base", {"builtin_loop": 1_000_000, "spawn_loop": None},
                        ("builtin_loop", "spawn_loop")),
            full_report("head", {"builtin_loop": 1_000_000, "spawn_loop": None},
                        ("builtin_loop", "spawn_loop")),
            fail_closed=True,
        )
        self.assertEqual(failures, [])
        self.assertEqual(skipped, ["spawn_loop"])

    def test_rejects_runner_and_repeat_mismatch(self):
        baseline = full_report("base", {"builtin_loop": 1}, ("builtin_loop",))
        baseline["metadata"]["runner_sha256"] = "runner-a"
        candidate = full_report("head", {"builtin_loop": 1}, ("builtin_loop",))
        candidate["metadata"]["runner_sha256"] = "runner-b"
        with self.assertRaisesRegex(ValueError, "runner_sha256"):
            gate.evaluate(baseline, candidate)
        baseline = full_report("base", {"builtin_loop": 1}, ("builtin_loop",))
        baseline["metadata"]["instruction_repeats"] = 3
        candidate = full_report("head", {"builtin_loop": 1}, ("builtin_loop",))
        candidate["metadata"]["instruction_repeats"] = 5
        with self.assertRaisesRegex(ValueError, "instruction_repeats"):
            gate.evaluate(baseline, candidate)

    def test_rejects_instruction_backend_mismatch(self):
        baseline = full_report("base", {"builtin_loop": 1}, ("builtin_loop",))
        baseline["metadata"]["instructions_backend"] = "perf"
        candidate = full_report("head", {"builtin_loop": 1}, ("builtin_loop",))
        candidate["metadata"]["instructions_backend"] = "cachegrind"
        with self.assertRaisesRegex(ValueError, "instructions_backend"):
            gate.evaluate(baseline, candidate)

    def test_matching_backend_compares_cleanly(self):
        backend = {"instructions_backend": "cachegrind"}
        baseline = report("base", {"builtin_loop": 1_000_000},
                          keys=("builtin_loop",), extra=backend)
        candidate = report("head", {"builtin_loop": 1_010_000},
                           keys=("builtin_loop",), extra=backend)
        _, failures, _ = gate.evaluate(baseline, candidate)
        self.assertEqual(failures, [])

    def test_missing_backend_is_rejected_as_unknown_units(self):
        baseline = full_report("base", {"builtin_loop": 1_000_000}, ("builtin_loop",))
        candidate = report("head", {"builtin_loop": 1_010_000},
                           keys=("builtin_loop",),
                           extra={"instructions_backend": "perf"})
        with self.assertRaisesRegex(ValueError, "instructions_backend"):
            gate.evaluate(baseline, candidate)

    def test_explicit_none_backend_rejected_against_a_counting_backend(self):
        baseline = report("base", {"builtin_loop": 1_000_000},
                          keys=("builtin_loop",),
                          extra={"instructions_backend": None})
        candidate = report("head", {"builtin_loop": 1_010_000},
                           keys=("builtin_loop",),
                           extra={"instructions_backend": "cachegrind"})
        with self.assertRaisesRegex(ValueError, "instructions_backend"):
            gate.evaluate(baseline, candidate)

    def test_matching_none_backends_without_data_stay_comparable(self):
        baseline = report("base", {"builtin_loop": None},
                          keys=("builtin_loop",),
                          extra={"instructions_backend": None})
        candidate = report("head", {"builtin_loop": None},
                           keys=("builtin_loop",),
                           extra={"instructions_backend": None})
        _, failures, skipped = gate.evaluate(baseline, candidate)
        self.assertEqual(failures, [])
        self.assertEqual(skipped, ["builtin_loop"])
        _, failures, _ = gate.evaluate(baseline, candidate, fail_closed=True)
        self.assertEqual(failures, ["builtin_loop"])

    def test_require_provenance_needs_recorded_hashes(self):
        provenance = {"execute_sha256": "exe", "runner_sha256": "runner", "instruction_repeats": 3}
        baseline = report("base", {"builtin_loop": 1}, keys=("builtin_loop",), extra=provenance)
        candidate = report("head", {"builtin_loop": 1}, keys=("builtin_loop",), extra=provenance)
        _, failures, _ = gate.evaluate(baseline, candidate, require_provenance=True)
        self.assertEqual(failures, [])
        with self.assertRaisesRegex(ValueError, "verified provenance"):
            gate.evaluate(
                full_report("base", {"builtin_loop": 1}, ("builtin_loop",)),
                full_report("head", {"builtin_loop": 1}, ("builtin_loop",)),
                require_provenance=True,
            )

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

    def test_cli_fail_closed_reports_missing_gated_data_as_incomplete(self):
        baseline = full_report("base", {"builtin_loop": None}, ("builtin_loop",))
        candidate = full_report("head", {"builtin_loop": None}, ("builtin_loop",))
        with tempfile.TemporaryDirectory(prefix="instruction-gate-") as tmp:
            baseline_path = Path(tmp) / "baseline.json"
            candidate_path = Path(tmp) / "candidate.json"
            baseline_path.write_text(json.dumps(baseline), encoding="utf-8")
            candidate_path.write_text(json.dumps(candidate), encoding="utf-8")
            self.assertEqual(gate.main([str(baseline_path), str(candidate_path)]), 0)
            self.assertEqual(
                gate.main([str(baseline_path), str(candidate_path), "--fail-closed"]), 1)


class CanaryTests(unittest.TestCase):
    def test_canary_catches_a_three_percent_regression(self):
        candidate = full_report(
            "head", {"builtin_loop": 1_030_000, "text_upper": 515_000},
            ("builtin_loop", "text_upper", "spawn_loop"),
        )
        # The non-gated row carries a large jump the gate must ignore.
        candidate["results"][2]["execute"]["median_instructions"] = 2_200_000
        with tempfile.TemporaryDirectory(prefix="instruction-canary-") as tmp:
            path = Path(tmp) / "candidate.json"
            path.write_text(json.dumps(candidate), encoding="utf-8")
            self.assertEqual(gate.main(["--canary", str(path)]), 0)

    def test_canary_scales_only_gated_rows(self):
        candidate = full_report(
            "head", {"builtin_loop": 1_030_000, "spawn_loop": 2_000_000},
            ("builtin_loop", "spawn_loop"),
        )
        baseline, expected = gate.synthesize_canary_baseline(candidate, 3.0)
        self.assertEqual(expected, ["builtin_loop"])
        self.assertEqual(
            baseline["metadata"]["source_revision"], "head-canary-baseline")
        by_key = {row["key"]: row for row in baseline["results"]}
        self.assertAlmostEqual(
            by_key["builtin_loop"]["execute"]["median_instructions"],
            1_030_000 / 1.03,
        )
        self.assertEqual(
            by_key["spawn_loop"]["execute"]["median_instructions"], 2_000_000)

    def test_canary_is_inconclusive_without_gated_data(self):
        candidate = full_report(
            "head", {"builtin_loop": None, "text_upper": None},
            ("builtin_loop", "text_upper"))
        with tempfile.TemporaryDirectory(prefix="instruction-canary-") as tmp:
            path = Path(tmp) / "candidate.json"
            path.write_text(json.dumps(candidate), encoding="utf-8")
            self.assertEqual(gate.main(["--canary", str(path)]), 2)

    def test_canary_is_inconclusive_without_a_revision(self):
        candidate = full_report("head", {"builtin_loop": 1_030_000}, ("builtin_loop",))
        candidate["metadata"]["source_revision"] = ""
        with tempfile.TemporaryDirectory(prefix="instruction-canary-") as tmp:
            path = Path(tmp) / "candidate.json"
            path.write_text(json.dumps(candidate), encoding="utf-8")
            self.assertEqual(gate.main(["--canary", str(path)]), 2)

    def test_canary_rejects_a_percent_at_or_below_the_threshold(self):
        candidate = full_report("head", {"builtin_loop": 1_030_000}, ("builtin_loop",))
        with tempfile.TemporaryDirectory(prefix="instruction-canary-") as tmp:
            path = Path(tmp) / "candidate.json"
            path.write_text(json.dumps(candidate), encoding="utf-8")
            self.assertEqual(
                gate.main(["--canary", str(path), "--canary-percent", "2"]), 2)
            self.assertEqual(
                gate.main(["--canary", str(path), "--canary-percent", "0"]), 2)

    def test_canary_fails_the_job_when_the_gate_misses(self):
        candidate = full_report("head", {"builtin_loop": 1_030_000}, ("builtin_loop",))
        with tempfile.TemporaryDirectory(prefix="instruction-canary-") as tmp:
            path = Path(tmp) / "candidate.json"
            path.write_text(json.dumps(candidate), encoding="utf-8")
            with patch.object(gate, "evaluate", return_value=([], [], [])):
                self.assertEqual(gate.main(["--canary", str(path)]), 1)

    def test_canary_reports_a_missing_report_as_incomplete(self):
        self.assertEqual(gate.main(["--canary", "/nonexistent/candidate.json"]), 2)

    def test_canary_baseline_keeps_backend_and_provenance(self):
        candidate = full_report(
            "head", {"builtin_loop": 1_030_000}, ("builtin_loop",))
        candidate["metadata"].update({
            "instructions_backend": "cachegrind",
            "execute_sha256": "exe",
            "runner_sha256": "runner",
            "instruction_repeats": 2,
        })
        baseline, expected = gate.synthesize_canary_baseline(candidate, 3.0)
        self.assertEqual(expected, ["builtin_loop"])
        for field in ("instructions_backend", "execute_sha256",
                      "runner_sha256", "instruction_repeats"):
            self.assertEqual(baseline["metadata"][field],
                             candidate["metadata"][field])
        _, failures, _ = gate.evaluate(baseline, candidate)
        self.assertEqual(failures, ["builtin_loop"])

    def test_canary_flags_failures_outside_the_synthetic_set(self):
        candidate = full_report(
            "head", {"builtin_loop": 1_030_000, "spawn_loop": 2_000_000},
            ("builtin_loop", "spawn_loop"),
        )
        with patch.object(gate, "evaluate",
                          return_value=([], ["builtin_loop", "spawn_loop"], [])):
            code, detail = gate.run_canary(candidate, 3.0)
        self.assertEqual(code, 1)
        self.assertIn("spawn_loop", detail)


class SameRunnerPairTests(unittest.TestCase):
    """The CI gate pair: two focused runs of one binary on one runner."""

    GATED_KEYS = ("builtin_loop", "text_upper", "json_get", "csv_parse",
                  "read_10k_lines", "suffix_strip", "here_string",
                  "text_substitution")
    COUNTS = {"builtin_loop": 1_000_000, "text_upper": 500_000,
              "json_get": 750_000, "csv_parse": 620_000,
              "read_10k_lines": 410_000, "suffix_strip": 90_000,
              "here_string": 95_000, "text_substitution": 88_000}

    def focused_pair(self, scale=1.0):
        provenance = {"execute_sha256": "exe", "runner_sha256": "runner",
                      "instructions_backend": "cachegrind",
                      "instruction_repeats": 2}
        before = report("abc123-run-a",
                        {key: self.COUNTS[key] for key in self.GATED_KEYS},
                        keys=self.GATED_KEYS, extra=dict(provenance))
        after = report("abc123-run-b",
                       {key: self.COUNTS[key] * scale for key in self.GATED_KEYS},
                       keys=self.GATED_KEYS, extra=dict(provenance))
        return before, after

    def test_stable_pair_passes_fail_closed_with_provenance(self):
        before, after = self.focused_pair()
        comparisons, failures, skipped = gate.evaluate(
            before, after, fail_closed=True, require_provenance=True)
        self.assertEqual(failures, [])
        self.assertEqual(skipped, [])
        self.assertEqual(len(comparisons), len(self.GATED_KEYS))
        self.assertTrue(all(gated for _, _, _, _, gated in comparisons))

    def test_three_percent_pair_fails_on_every_gated_row(self):
        before, after = self.focused_pair(scale=1.03)
        _, failures, _ = gate.evaluate(
            before, after, fail_closed=True, require_provenance=True)
        self.assertEqual(sorted(failures), sorted(self.GATED_KEYS))

    def test_pair_needs_distinct_revisions(self):
        before, _ = self.focused_pair()
        twin = report("abc123-run-a",
                      {key: self.COUNTS[key] for key in self.GATED_KEYS},
                      keys=self.GATED_KEYS,
                      extra={"instructions_backend": "cachegrind"})
        with self.assertRaisesRegex(ValueError, "distinct source revisions"):
            gate.evaluate(before, twin)

    def test_pair_rejects_a_flapped_backend(self):
        before, after = self.focused_pair()
        after["metadata"]["instructions_backend"] = "perf"
        with self.assertRaisesRegex(ValueError, "instructions_backend"):
            gate.evaluate(before, after, fail_closed=True)

    def test_cross_commit_pair_allows_distinct_builds(self):
        provenance = {"instructions_backend": "cachegrind",
                      "instruction_repeats": 2}
        baseline = report("a" * 40,
                          {key: self.COUNTS[key] for key in self.GATED_KEYS},
                          keys=self.GATED_KEYS,
                          extra=dict(provenance, execute_sha256="exe-base",
                                     runner_sha256="runner"))
        candidate = report("b" * 40,
                           {key: self.COUNTS[key] for key in self.GATED_KEYS},
                           keys=self.GATED_KEYS,
                           extra=dict(provenance, execute_sha256="exe-head",
                                      runner_sha256="runner"))
        _, failures, skipped = gate.evaluate(
            baseline, candidate, fail_closed=True, require_provenance=True)
        self.assertEqual(failures, [])
        self.assertEqual(skipped, [])

    def test_cross_commit_pair_regressions_fail(self):
        provenance = {"instructions_backend": "cachegrind",
                      "instruction_repeats": 2}
        baseline = report("a" * 40,
                          {key: self.COUNTS[key] for key in self.GATED_KEYS},
                          keys=self.GATED_KEYS,
                          extra=dict(provenance, execute_sha256="exe-base",
                                     runner_sha256="runner"))
        candidate = report("b" * 40,
                           {key: int(self.COUNTS[key] * 1.03)
                            for key in self.GATED_KEYS},
                           keys=self.GATED_KEYS,
                           extra=dict(provenance, execute_sha256="exe-head",
                                      runner_sha256="runner"))
        _, failures, _ = gate.evaluate(
            baseline, candidate, fail_closed=True, require_provenance=True)
        self.assertEqual(sorted(failures), sorted(self.GATED_KEYS))


class ToolComparabilityTests(unittest.TestCase):
    """A tool-only change must read as incomparable, never as green or red.

    Both gate reports are measured with the same head-revision tool, so a
    matching tool hash and valgrind version pass while any drift -- including
    a pre-tool baseline that records neither -- fails the job instead of
    passing silently or reporting a phantom regression.
    """

    GATED_KEYS = SameRunnerPairTests.GATED_KEYS
    COUNTS = SameRunnerPairTests.COUNTS

    def extras(self, **overrides):
        extra = {
            "instructions_backend": "cachegrind",
            "instruction_repeats": 2,
            "execute_sha256": "exe",
            "runner_sha256": "tool-v1",
            "tool_sha256": "tool-v1",
            "valgrind_version": "valgrind-3.24.0",
        }
        extra.update(overrides)
        return extra

    def pair(self, base_extra, head_extra):
        baseline = report("a" * 40,
                          {key: self.COUNTS[key] for key in self.GATED_KEYS},
                          keys=self.GATED_KEYS, extra=base_extra)
        candidate = report("b" * 40,
                           {key: self.COUNTS[key] for key in self.GATED_KEYS},
                           keys=self.GATED_KEYS, extra=head_extra)
        return baseline, candidate

    def test_matching_tool_and_valgrind_pass(self):
        baseline, candidate = self.pair(self.extras(), self.extras())
        _, failures, skipped = gate.evaluate(
            baseline, candidate, fail_closed=True, require_provenance=True)
        self.assertEqual(failures, [])
        self.assertEqual(skipped, [])

    def test_runner_hash_mismatch_is_incomparable(self):
        baseline, candidate = self.pair(
            self.extras(), self.extras(runner_sha256="tool-v2"))
        with self.assertRaisesRegex(ValueError, "runner_sha256"):
            gate.evaluate(baseline, candidate)

    def test_tool_hash_mismatch_is_incomparable(self):
        baseline, candidate = self.pair(
            self.extras(), self.extras(tool_sha256="tool-v2"))
        with self.assertRaisesRegex(ValueError, "tool_sha256"):
            gate.evaluate(baseline, candidate)

    def test_valgrind_mismatch_is_incomparable(self):
        baseline, candidate = self.pair(
            self.extras(), self.extras(valgrind_version="valgrind-3.25.0"))
        with self.assertRaisesRegex(ValueError, "valgrind_version"):
            gate.evaluate(baseline, candidate)

    def test_pre_tool_baseline_without_valgrind_is_incomparable(self):
        base = self.extras()
        del base["valgrind_version"]
        baseline, candidate = self.pair(base, self.extras())
        with self.assertRaisesRegex(ValueError, "valgrind_version"):
            gate.evaluate(baseline, candidate)

    def test_pre_tool_baseline_without_backend_is_incomparable(self):
        base = self.extras()
        del base["instructions_backend"]
        baseline, candidate = self.pair(base, self.extras())
        with self.assertRaisesRegex(ValueError, "instructions_backend"):
            gate.evaluate(baseline, candidate)

    def test_pre_tool_baseline_without_provenance_fails_closed(self):
        baseline = full_report(
            "a" * 40, {"builtin_loop": 1_000_000}, ("builtin_loop",))
        candidate = full_report(
            "b" * 40, {"builtin_loop": 1_000_000}, ("builtin_loop",))
        with self.assertRaisesRegex(ValueError, "verified provenance"):
            gate.evaluate(baseline, candidate, require_provenance=True)


class WorkflowPinTests(unittest.TestCase):
    """Action pins stay full-length SHAs; unrelated pin churn is a risk."""

    WORKFLOW = ROOT / ".github" / "workflows" / "bash-compare.yml"

    def test_all_action_pins_are_full_length_hex(self):
        import re

        text = self.WORKFLOW.read_text(encoding="utf-8")
        pins = re.findall(r"@([0-9a-f]+)", text)
        self.assertTrue(pins, "expected pinned actions in bash-compare.yml")
        for pin in pins:
            self.assertEqual(len(pin), 40, f"truncated action pin: {pin!r}")

    def test_restored_pins_match_the_reviewed_revisions(self):
        text = self.WORKFLOW.read_text(encoding="utf-8")
        self.assertIn(
            "dtolnay/rust-toolchain@686976e191b89faba57d3206551f0f330d8cb249",
            text,
        )
        self.assertIn(
            "Swatinem/rust-cache@d1527c8f67ef9af357290475d4af4b0fab8f12e0",
            text,
        )


class MeasurementToolTests(unittest.TestCase):
    def test_valgrind_version_returns_none_without_valgrind(self):
        with patch.object(bash_compare.shutil, "which", return_value=None):
            self.assertIsNone(bash_compare.valgrind_version())

    def test_valgrind_version_reports_the_provider_verbatim(self):
        with patch.object(bash_compare.shutil, "which",
                          return_value="/usr/bin/valgrind"):
            with patch.object(
                bash_compare.subprocess, "run",
                return_value=subprocess.CompletedProcess(
                    [], 0, "valgrind-3.24.0\n", ""),
            ):
                self.assertEqual(
                    bash_compare.valgrind_version(), "valgrind-3.24.0")

    def test_valgrind_version_returns_none_on_probe_failure(self):
        with patch.object(bash_compare.shutil, "which",
                          return_value="/usr/bin/valgrind"):
            with patch.object(
                bash_compare.subprocess, "run",
                return_value=subprocess.CompletedProcess([], 1, "", ""),
            ):
                self.assertIsNone(bash_compare.valgrind_version())

    def test_source_revision_flag_exists_for_cross_checkout_measurement(self):
        import contextlib
        import io

        stdout = io.StringIO()
        with self.assertRaises(SystemExit):
            with contextlib.redirect_stdout(stdout):
                bash_compare.main(["--help"])
        self.assertIn("--source-revision", stdout.getvalue())


if __name__ == "__main__":
    unittest.main()
