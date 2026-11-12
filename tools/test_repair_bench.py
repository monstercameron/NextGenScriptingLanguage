"""Tests for tools/repair_bench.py. Run with: python tools/test_repair_bench.py

The scoring and mutation tests use fixed text and fake diagnostics, so they need no
execute binary. The end-to-end test runs only when EXECUTE_BIN names a built binary.
"""

from __future__ import annotations

import json
import os
import random
import shutil
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import repair_bench as bench  # noqa: E402

SAMPLE = (
    'then save to "a.json"\n'
    'then save "b"\n'
    '  then save to "c.json"\n'
)


def diagnostic(start: int, end: int, code: str = "parse.foreign.pipe") -> dict:
    return {
        "severity": "error",
        "code": code,
        "message": "fixture",
        "span": {"start_byte": start, "end_byte": end, "line": 1, "column": 1, "end_line": 1, "end_column": 1, "precision": "exact"},
    }


class MutationTests(unittest.TestCase):
    def test_finds_only_save_to_statements_with_a_quoted_path(self):
        sites = bench.TAXONOMY["drop_save_to"].find(SAMPLE)
        self.assertEqual(len(sites), 2)

    def test_dropping_to_removes_exactly_the_word_and_space(self):
        sites = bench.TAXONOMY["drop_save_to"].find(SAMPLE)
        mutated, windows = bench.apply_sites(SAMPLE, sites)
        self.assertEqual(
            mutated,
            'then save "a.json"\nthen save "b"\n  then save "c.json"\n',
        )

    def test_each_window_covers_save_through_the_quote(self):
        sites = bench.TAXONOMY["drop_save_to"].find(SAMPLE)
        mutated, windows = bench.apply_sites(SAMPLE, sites)
        self.assertEqual(len(windows), 2)
        for start, end in windows:
            self.assertEqual(mutated[start:end], 'save "')

    def test_fixed_seed_gives_the_same_mutation(self):
        first = bench.choose_mutations(SAMPLE, random.Random(502), 3, ["drop_save_to"])
        second = bench.choose_mutations(SAMPLE, random.Random(502), 3, ["drop_save_to"])
        self.assertEqual(first, second)

    def test_mutation_count_is_within_the_requested_range(self):
        for seed in range(20):
            _, labels, _ = bench.choose_mutations(SAMPLE, random.Random(seed), 3, ["drop_save_to"])
            self.assertGreaterEqual(len(labels), 1)
            self.assertLessEqual(len(labels), 2)  # the sample has only two sites

    def test_text_without_sites_is_left_alone(self):
        text = 'then save "only.json"\n'
        mutated, labels, windows = bench.choose_mutations(text, random.Random(1), 3, ["drop_save_to"])
        self.assertEqual(mutated, text)
        self.assertEqual(labels, [])
        self.assertEqual(windows, [])


class ScoringTests(unittest.TestCase):
    def mutated_one(self):
        sites = bench.TAXONOMY["drop_save_to"].find(SAMPLE)[:1]
        mutated, windows = bench.apply_sites(SAMPLE, sites)
        return mutated, windows

    def test_diagnostic_covering_the_dropped_token_localises(self):
        mutated, windows = self.mutated_one()
        # The first window is "save \"" at character 5 to 11 of the mutated text.
        start, end = windows[0]
        diagnostics = [diagnostic(start, end)]
        self.assertTrue(bench.localised(diagnostics, mutated, windows))

    def test_diagnostic_elsewhere_does_not_localise(self):
        mutated, windows = self.mutated_one()
        far = len(mutated.encode("utf-8")) - 2
        self.assertFalse(bench.localised([diagnostic(far, far + 1)], mutated, windows))

    def test_zero_length_span_at_the_window_localises(self):
        mutated, windows = self.mutated_one()
        start, _ = bench.byte_window(mutated, windows[0])
        self.assertTrue(bench.localised([diagnostic(start, start)], mutated, windows))

    def test_missing_span_or_no_diagnostics_does_not_localise(self):
        mutated, windows = self.mutated_one()
        self.assertFalse(bench.localised([], mutated, windows))
        self.assertFalse(bench.localised([{"code": "parse.syntax", "severity": "error"}], mutated, windows))

    def test_only_the_first_diagnostic_counts_for_localisation(self):
        mutated, windows = self.mutated_one()
        start, end = windows[0]
        far = len(mutated.encode("utf-8")) - 2
        self.assertFalse(bench.localised([diagnostic(far, far + 1), diagnostic(start, end)], mutated, windows))

    def test_generic_codes_are_not_specific(self):
        self.assertFalse(bench.is_specific([]))
        self.assertFalse(bench.is_specific([diagnostic(0, 1, "check_failed")]))
        self.assertFalse(bench.is_specific([diagnostic(0, 1, "parse.syntax")]))
        self.assertTrue(bench.is_specific([diagnostic(0, 1, "parse.foreign.pipe")]))

    def test_report_metrics_follow_the_case_counts(self):
        cases = [
            {"mutations": [{"kind": "drop_save_to"}], "detected": True, "localised": True, "specific": True, "diagnostics": 1},
            {"mutations": [{"kind": "drop_save_to"}], "detected": True, "localised": False, "specific": False, "diagnostics": 3},
            {"mutations": [{"kind": "drop_save_to"}], "detected": False, "localised": False, "specific": False, "diagnostics": 0},
            {"mutations": [{"kind": "drop_save_to"}], "detected": False, "localised": False, "specific": False, "diagnostics": 0},
        ]
        report = bench.build_report("fake", 502, 3, ["drop_save_to"], 4, 0, cases)
        metrics = report["metrics"]
        self.assertEqual(metrics["detection_rate"], 0.5)
        self.assertEqual(metrics["localisation_rate"], 0.25)
        self.assertEqual(metrics["specificity_rate"], 0.5)
        self.assertEqual(metrics["diagnostics_per_mutation"], 1.0)
        self.assertEqual(report["corpus"]["mutated_programs"], 4)
        self.assertEqual(report["schema"], bench.SCHEMA)


class RatchetTests(unittest.TestCase):
    base = {"metrics": {"detection_rate": 0.9, "localisation_rate": 0.5, "specificity_rate": 0.8, "diagnostics_per_mutation": 2.0}}

    def test_equal_metrics_pass(self):
        self.assertEqual(bench.compare_to_baseline(dict(self.base["metrics"]), self.base), [])

    def test_better_metrics_pass(self):
        better = {"detection_rate": 1.0, "localisation_rate": 0.6, "specificity_rate": 0.9, "diagnostics_per_mutation": 1.0}
        self.assertEqual(bench.compare_to_baseline(better, self.base), [])

    def test_a_dropped_rate_fails(self):
        worse = dict(self.base["metrics"], localisation_rate=0.4)
        problems = bench.compare_to_baseline(worse, self.base)
        self.assertEqual(len(problems), 1)
        self.assertIn("localisation_rate dropped", problems[0])

    def test_a_cascade_increase_fails(self):
        worse = dict(self.base["metrics"], diagnostics_per_mutation=2.5)
        problems = bench.compare_to_baseline(worse, self.base)
        self.assertEqual(len(problems), 1)
        self.assertIn("diagnostics_per_mutation rose", problems[0])


class MainTests(unittest.TestCase):
    def test_missing_corpus_is_a_usage_error(self):
        code = bench.main(["--execute", "unused", "--corpus", str(Path(tempfile.gettempdir()) / "no-such-corpus-502")])
        self.assertEqual(code, 2)

    def test_bad_baseline_is_a_usage_error(self):
        scratch = tempfile.mkdtemp(prefix="repair-bench-test-")
        try:
            bad = Path(scratch) / "bad.json"
            bad.write_text("{not json", encoding="utf-8")
            code = bench.main(["--execute", "unused", "--corpus", scratch, "--baseline", str(bad)])
            self.assertEqual(code, 2)
        finally:
            shutil.rmtree(scratch, ignore_errors=True)


EXECUTE_BIN = os.environ.get("EXECUTE_BIN", "")


@unittest.skipUnless(EXECUTE_BIN and Path(EXECUTE_BIN).is_file(), "set EXECUTE_BIN to a built execute binary")
class EndToEndTests(unittest.TestCase):
    def test_dropped_to_is_detected_and_localised_on_one_program(self):
        source = ROOT_EXAMPLE
        scratch = tempfile.mkdtemp(prefix="repair-bench-e2e-")
        try:
            corpus = Path(scratch) / "corpus"
            corpus.mkdir()
            shutil.copy(source, corpus / source.name)
            report = bench.run_benchmark(EXECUTE_BIN, corpus, seed=502, max_mutations=1, timeout=120)
            self.assertEqual(report["corpus"]["clean"], 1)
            self.assertEqual(report["corpus"]["mutated_programs"], 1)
            self.assertEqual(report["metrics"]["detection_rate"], 1.0)
            self.assertEqual(report["metrics"]["localisation_rate"], 1.0)
        finally:
            shutil.rmtree(scratch, ignore_errors=True)


ROOT_EXAMPLE = bench.ROOT / "examples" / "ai-gauntlet" / "001-log-errors.program"


if __name__ == "__main__":
    unittest.main()
