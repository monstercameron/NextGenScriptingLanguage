#!/usr/bin/env python3
"""Regression checks for the calibration and threshold report (#380).

The fixture is ten synthetic cases with known outcomes, built so the accuracy,
the expected calibration error and the threshold sweep can be worked out by hand:

    5 cases at confidence 0.95, 4 correct  -> bin 9: gap 0.15, weight 0.5
    3 cases at confidence 0.75, 3 correct  -> bin 7: gap 0.25, weight 0.3
    2 cases at confidence 0.25, 1 correct  -> bin 2: gap 0.25, weight 0.2

Accuracy is 8/10 = 0.8. ECE is 0.5*0.15 + 0.3*0.25 + 0.2*0.25 = 0.2.
At threshold 0.8 the accepted cases are the five at 0.95 (coverage 0.5, accuracy 0.8).
At threshold 0.75 eight cases are accepted with seven correct (accuracy 0.875).
"""

from __future__ import annotations

import contextlib
import io
import json
import sys
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "tools"))
import model_eval_report as report_tool


FIXTURE = (
    [{"id": f"hi-{i}", "confidence": 0.95, "correct": i < 4} for i in range(5)]
    + [{"id": f"mid-{i}", "confidence": 0.75, "correct": True} for i in range(3)]
    + [
        {"id": "lo-0", "confidence": 0.25, "correct": True},
        {"id": "lo-1", "confidence": 0.25, "correct": False},
    ]
)


def fixture_pairs() -> list[tuple[float, bool]]:
    return report_tool.load_cases(FIXTURE)


class AccuracyAndCalibrationTest(unittest.TestCase):
    def test_accuracy_is_exact(self) -> None:
        self.assertAlmostEqual(report_tool.accuracy(fixture_pairs()), 0.8, places=12)

    def test_ece_matches_hand_computed_value(self) -> None:
        ece, reliability = report_tool.expected_calibration_error(fixture_pairs(), bins=10)
        self.assertAlmostEqual(ece, 0.2, places=12)
        occupied = [b for b in reliability if b["count"]]
        self.assertEqual([b["count"] for b in occupied], [2, 3, 5])

    def test_expected_and_predicted_form_matches_correct_flag(self) -> None:
        cases = [
            {"confidence": 0.95, "expected": "A", "predicted": "A"},
            {"confidence": 0.95, "expected": "B", "predicted": "B"},
            {"confidence": 0.95, "expected": "A", "predicted": "B"},
            {"confidence": 0.5, "expected": "A", "predicted": "A"},
        ]
        pairs = report_tool.load_cases(cases)
        self.assertEqual([ok for _, ok in pairs], [True, True, False, True])

    def test_empty_input_has_no_accuracy(self) -> None:
        self.assertIsNone(report_tool.accuracy([]))
        ece, _ = report_tool.expected_calibration_error([], bins=10)
        self.assertIsNone(ece)


class ThresholdSweepTest(unittest.TestCase):
    def test_sweep_at_0_8_keeps_exactly_the_five_high_cases(self) -> None:
        rows = report_tool.threshold_sweep(fixture_pairs(), [0.8])
        self.assertEqual(rows[0]["accepted"], 5)
        self.assertAlmostEqual(rows[0]["coverage"], 0.5, places=12)
        self.assertAlmostEqual(rows[0]["accuracy_on_accepted"], 0.8, places=12)

    def test_sweep_at_0_75_accepts_eight_with_seven_correct(self) -> None:
        rows = report_tool.threshold_sweep(fixture_pairs(), [0.75])
        self.assertEqual(rows[0]["accepted"], 8)
        self.assertAlmostEqual(rows[0]["accuracy_on_accepted"], 0.875, places=12)

    def test_recommended_threshold_is_lowest_meeting_target(self) -> None:
        sweep = report_tool.threshold_sweep(fixture_pairs(), [0.25, 0.75, 0.95])
        self.assertEqual(report_tool.recommended_threshold(sweep, 0.85)["threshold"], 0.75)
        self.assertEqual(report_tool.recommended_threshold(sweep, 0.8)["threshold"], 0.25)
        self.assertIsNone(report_tool.recommended_threshold(sweep, 0.95))

    def test_wilson_interval_bounds(self) -> None:
        low, high = report_tool.wilson_interval(4, 5)
        self.assertTrue(0.3 < low < 0.45, low)
        self.assertTrue(0.95 < high <= 1.0, high)
        self.assertEqual(report_tool.wilson_interval(0, 0), (0.0, 1.0))
        self.assertEqual(report_tool.wilson_interval(0, 10)[0], 0.0)


class ReportAndGateTest(unittest.TestCase):
    def test_build_report_carries_the_fixture_numbers(self) -> None:
        report = report_tool.build_report(fixture_pairs(), target=0.85)
        self.assertEqual(report["cases"], 10)
        self.assertAlmostEqual(report["accuracy"], 0.8, places=12)
        self.assertAlmostEqual(report["ece"], 0.2, places=12)
        self.assertAlmostEqual(report["recommended"]["threshold"], 0.75, places=12)

    def test_invalid_confidence_is_rejected(self) -> None:
        with self.assertRaises(ValueError):
            report_tool.load_cases([{"confidence": 1.5, "correct": True}])
        with self.assertRaises(ValueError):
            report_tool.load_cases([{"confidence": True, "correct": True}])

    def _run_gate(self, extra: list[str]) -> tuple[int, str]:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "cases.json"
            path.write_text(json.dumps({"cases": FIXTURE}), encoding="utf-8")
            err = io.StringIO()
            with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(err):
                code = report_tool.main([str(path), *extra])
        return code, err.getvalue()

    def test_gate_passes_when_requirement_holds(self) -> None:
        code, err = self._run_gate(["--threshold", "0.8", "--require", "accuracy_on_accepted>=0.8"])
        self.assertEqual(code, 0, err)

    def test_gate_exits_nonzero_below_requirement(self) -> None:
        code, err = self._run_gate(["--threshold", "0.8", "--require", "accuracy_on_accepted>=0.95"])
        self.assertEqual(code, 1)
        self.assertIn("gate failed", err)

    def test_gate_fails_on_coverage_requirement(self) -> None:
        code, _ = self._run_gate(["--threshold", "0.8", "--require", "coverage>=0.6"])
        self.assertEqual(code, 1)

    def test_bad_input_exits_with_usage_code(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "bad.json"
            path.write_text(json.dumps([{"confidence": 2, "correct": True}]), encoding="utf-8")
            err = io.StringIO()
            with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(err):
                code = report_tool.main([str(path)])
        self.assertEqual(code, 2)


if __name__ == "__main__":
    unittest.main()
