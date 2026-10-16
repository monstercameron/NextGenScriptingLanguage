"""Contract tests for the literal-argument paired runner; no product runs."""
import json
import importlib.util
from pathlib import Path
import unittest

_RUNNER_PATH = Path(__file__).with_name("resolved_json_arguments_pair.py")
_SPEC = importlib.util.spec_from_file_location("resolved_json_arguments_pair", _RUNNER_PATH)
assert _SPEC is not None and _SPEC.loader is not None
pair = importlib.util.module_from_spec(_SPEC)
_SPEC.loader.exec_module(pair)


def raw_report():
    sample_oracle = {
        "output_sha256": pair.EXPECTED_OUTPUT_SHA256,
        "output_matches": True,
        "completed_call_count": 1000,
        "call_count_matches": True,
        "child_launches": 0,
        "no_child_launches": True,
        "valid": True,
    }
    return {
        "schema": "execute.runtime-json-arguments.raw/1",
        "profile": "release",
        "warmups_configured": 0,
        "samples_configured": 3,
        "workload": {
            "path": pair.WORKLOAD_RELATIVE,
            "source_sha256": pair.EXPECTED_WORKLOAD_SHA256,
            "input_sha256": pair.EXPECTED_INPUT_SHA256,
            "expected_output_sha256": pair.EXPECTED_OUTPUT_SHA256,
        },
        "warmups": [],
        "samples": [
            {"index": index, "elapsed_ns": (index + 1) * 10, **sample_oracle}
            for index in range(3)
        ],
        "all_runs_valid": True,
    }


class LiteralArgumentsPairTests(unittest.TestCase):
    def test_schedule_is_fixed_alternating_and_balanced(self):
        orders = [pair.interleaved_order(index) for index in range(pair.WARMUP_PAIRS + pair.MEASURED_PAIRS)]
        self.assertEqual(orders[:3], [("baseline", "candidate"), ("candidate", "baseline"), ("baseline", "candidate")])
        measured = orders[pair.WARMUP_PAIRS :]
        self.assertEqual(measured[0], ("candidate", "baseline"))
        self.assertEqual(measured.count(("baseline", "candidate")), 24)
        self.assertEqual(measured.count(("candidate", "baseline")), 24)

    def test_raw_report_accepts_exact_public_runtime_oracle(self):
        result = pair.validate_raw_report(json.dumps(raw_report()).encode())
        self.assertEqual(result["median_duration_ns"], 20)
        self.assertEqual([sample["elapsed_ns"] for sample in result["samples"]], [10, 20, 30])

    def test_raw_report_rejects_wrong_count_output_and_child_launches(self):
        for field, value in (
            ("completed_call_count", 999),
            ("output_matches", False),
            ("child_launches", 1),
            ("output_sha256", "0" * 64),
        ):
            with self.subTest(field=field):
                report = raw_report()
                report["samples"][1][field] = value
                with self.assertRaises(pair.PairError):
                    pair.validate_raw_report(json.dumps(report).encode())

    def test_raw_report_rejects_missing_or_extra_samples(self):
        report = raw_report()
        report["samples"].pop()
        with self.assertRaises(pair.PairError):
            pair.validate_raw_report(json.dumps(report).encode())

    def test_raw_report_rejects_boolean_warmup_count(self):
        report = raw_report()
        report["warmups_configured"] = False
        with self.assertRaises(pair.PairError):
            pair.validate_raw_report(json.dumps(report).encode())


if __name__ == "__main__":
    unittest.main()
