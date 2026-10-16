"""Focused offline contract tests for payload_size_pair.py; no benchmark binary runs."""
from __future__ import annotations

import hashlib
import importlib.util
import io
import json
import copy
from pathlib import Path
import tempfile
import unittest
from contextlib import redirect_stderr
from unittest import mock


RUNNER_PATH = Path(__file__).with_name("payload_size_pair.py")
SPEC = importlib.util.spec_from_file_location("payload_size_pair", RUNNER_PATH)
assert SPEC is not None and SPEC.loader is not None
runner = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(runner)


def valid_report() -> dict:
    cases = []
    for index, name in enumerate(runner.CASE_NAMES, start=1):
        size = index * 17
        cases.append(
            {
                "name": name,
                "payload_kind": "typed_value",
                "input_payload_sha256": hashlib.sha256(f"case-{index}".encode()).hexdigest(),
                "serialized_bytes": size,
                "calls_per_sample": runner.CALLS_PER_SAMPLE,
                "warmup_calls": runner.WARMUP_CALLS_PER_CASE,
                "samples_in_execution_order": [
                    {
                        "sample_index": sample_index,
                        "duration_ns": 100 + sample_index,
                        "calls": runner.CALLS_PER_SAMPLE,
                        "checksum": size * runner.CALLS_PER_SAMPLE,
                    }
                    for sample_index in range(runner.RAW_SAMPLES)
                ],
            }
        )
    return {
        "format": runner.HARNESS_FORMAT,
        "method": runner.HARNESS_METHOD,
        "calls_per_sample": runner.CALLS_PER_SAMPLE,
        "raw_samples": runner.RAW_SAMPLES,
        "warmup_calls_per_case": runner.WARMUP_CALLS_PER_CASE,
        "cases": cases,
    }


def completed_attempt(parsed: dict, side: str) -> dict:
    return {
        "side": side,
        "successful": True,
        "oracle": copy.deepcopy(parsed["oracle"]),
        "median_duration_ns": copy.deepcopy(parsed["median_duration_ns"]),
        "raw_duration_ns": copy.deepcopy(parsed["raw_duration_ns"]),
        "validation_error": None,
        "execution_error": None,
    }


class PayloadSizePairContractTests(unittest.TestCase):
    def test_full_oracle_and_raw_samples_validate(self) -> None:
        report = valid_report()
        parsed = runner.validate_harness_report(
            json.dumps(report, separators=(",", ":")).encode("utf-8")
        )
        self.assertEqual(parsed["raw_duration_ns"][runner.CASE_NAMES[0]], [100, 101, 102])
        first = report["cases"][0]
        self.assertEqual(
            parsed["oracle"][runner.CASE_NAMES[0]]["input_payload_sha256"],
            first["input_payload_sha256"],
        )
        self.assertEqual(
            parsed["oracle"][runner.CASE_NAMES[0]]["serialized_bytes"],
            first["serialized_bytes"],
        )
        self.assertEqual(
            parsed["oracle"][runner.CASE_NAMES[0]]["sample_checksums"],
            [first["serialized_bytes"] * runner.CALLS_PER_SAMPLE] * runner.RAW_SAMPLES,
        )

    def test_bad_case_order_checksum_and_json_are_rejected(self) -> None:
        bad_order = valid_report()
        bad_order["cases"][0], bad_order["cases"][1] = bad_order["cases"][1], bad_order["cases"][0]
        with self.assertRaises(runner.PairError):
            runner.validate_harness_report(json.dumps(bad_order).encode())

        bad_checksum = valid_report()
        bad_checksum["cases"][0]["samples_in_execution_order"][1]["checksum"] += 1
        with self.assertRaises(runner.PairError):
            runner.validate_harness_report(json.dumps(bad_checksum).encode())

        with self.assertRaises(runner.PairError):
            runner.parse_json(b'{"same":1,"same":2}', "duplicate-key fixture")
        with self.assertRaises(runner.PairError):
            runner.parse_json(b'{"value":NaN}', "non-finite fixture")

    def test_pair_order_alternates_and_first_measured_pair_is_candidate_first(self) -> None:
        self.assertEqual(runner.pair_order(0), ("baseline", "candidate"))
        self.assertEqual(runner.pair_order(1), ("candidate", "baseline"))
        self.assertEqual(runner.pair_order(runner.WARMUP_PAIRS - 1), ("baseline", "candidate"))
        self.assertEqual(runner.pair_order(runner.WARMUP_PAIRS), ("candidate", "baseline"))
        self.assertEqual(
            runner.pair_order(runner.WARMUP_PAIRS + runner.MEASURED_PAIRS - 1),
            ("baseline", "candidate"),
        )

    def test_input_hash_mismatch_invalidates_pair_even_when_timing_is_present(self) -> None:
        parsed = runner.validate_harness_report(json.dumps(valid_report()).encode())
        baseline = completed_attempt(parsed, "baseline")
        candidate = completed_attempt(parsed, "candidate")
        case = runner.CASE_NAMES[0]
        candidate["oracle"][case]["input_payload_sha256"] = "f" * 64

        pair = runner._pair_result(1, baseline, candidate)
        self.assertFalse(pair["valid_for_timing_comparison"])
        self.assertTrue(pair["failure_reasons"]["oracle_mismatch"])
        self.assertEqual(pair["cases"], {})

    def test_equal_oracle_pair_preserves_raw_values_and_delta(self) -> None:
        parsed = runner.validate_harness_report(json.dumps(valid_report()).encode())
        baseline = completed_attempt(parsed, "baseline")
        candidate = completed_attempt(parsed, "candidate")
        candidate["median_duration_ns"][runner.CASE_NAMES[0]] += 9

        pair = runner._pair_result(7, baseline, candidate)
        self.assertTrue(pair["valid_for_timing_comparison"])
        first_case = pair["cases"][runner.CASE_NAMES[0]]
        self.assertEqual(first_case["candidate_minus_baseline_ns"], 9)
        self.assertEqual(first_case["baseline_raw_duration_ns"], [100, 101, 102])
        self.assertEqual(first_case["candidate_raw_duration_ns"], [100, 101, 102])

    def test_failed_invocation_retains_logs_and_report_failure(self) -> None:
        with tempfile.TemporaryDirectory(prefix="payload-size-runner-test-") as temporary:
            root = Path(temporary)
            binary = root / "unused-test-binary"
            binary.write_bytes(b"synthetic executable identity; never executed")
            transport = root / "src" / "graph" / "transport.rs"
            transport.parent.mkdir(parents=True)
            transport.write_text("synthetic frozen source", encoding="utf-8")
            identity = {
                "binary": str(binary),
                "binary_sha256": runner.sha256_file(binary),
                "root": str(root),
                "transport_sha256": runner.sha256_file(transport),
            }
            attempt_dir = root / "attempts" / "synthetic-failure"
            metadata = {"attempt_id": "synthetic-failure", "side": "baseline"}
            with mock.patch.object(
                runner.subprocess,
                "run",
                return_value=mock.Mock(returncode=17, stdout=b"synthetic stdout", stderr=b"synthetic failure"),
            ):
                result = runner.run_attempt(identity, attempt_dir, metadata)

            self.assertFalse(result["successful"])
            self.assertEqual(result["returncode"], 17)
            self.assertEqual(result["validation_error"], "harness did not create a raw report")
            self.assertEqual((attempt_dir / "stdout.bin").read_bytes(), b"synthetic stdout")
            self.assertEqual((attempt_dir / "stderr.bin").read_bytes(), b"synthetic failure")
            self.assertTrue((attempt_dir / "started.json").is_file())
            self.assertTrue((attempt_dir / "finished.json").is_file())

    def test_main_propagates_failed_run_and_converts_operational_errors(self) -> None:
        argv = ["run", "--preregistration", "prereg.json", "--report-dir", "report-dir"]
        with mock.patch.object(runner, "run", return_value=1):
            self.assertEqual(runner.main(argv), 1)

        for error in (OSError("synthetic I/O failure"), ValueError("synthetic value failure")):
            with self.subTest(error=type(error).__name__):
                stderr = io.StringIO()
                with mock.patch.object(runner, "run", side_effect=error), redirect_stderr(stderr):
                    self.assertEqual(runner.main(argv), 2)
                self.assertIn(str(error), stderr.getvalue())


if __name__ == "__main__":
    unittest.main()
