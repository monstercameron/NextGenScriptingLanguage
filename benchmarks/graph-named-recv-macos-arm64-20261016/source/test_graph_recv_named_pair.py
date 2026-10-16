import json
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

import graph_recv_named_pair as runner


def report_payload() -> bytes:
    cases = []
    for case_index, (method, source_count, input_count, case_name) in enumerate(runner.CASE_SPECS):
        durations = [1_000_000 + case_index * 100 + sample_index * 10 for sample_index in range(3)]
        samples = [
            {
                "method": method,
                "case": case_name,
                "source_count": source_count,
                "input_count": input_count,
                "messages_per_source": runner.EVENTS_PER_CASE // source_count,
                "sample_index": sample_index,
                "events": runner.EVENTS_PER_CASE,
                "duration_ns": duration,
                "ns_per_event": duration / runner.EVENTS_PER_CASE,
                "checksum": runner.EXPECTED_CHECKSUM,
                "order_oracle": runner.ORDER_ORACLE,
            }
            for sample_index, duration in enumerate(durations)
        ]
        cases.append(
            {
                "method": method,
                "case": case_name,
                "p50_ns_per_event": durations[1] / runner.EVENTS_PER_CASE,
                "samples": samples,
            }
        )
    return json.dumps(
        {"format_version": 1, "harness": runner.BENCHMARK_NAME, "cases": cases},
        allow_nan=False,
    ).encode()


class NamedReceivePairedRunnerTests(unittest.TestCase):
    def test_valid_report_checks_all_twelve_cases_and_raw_samples(self):
        result = runner.validate_benchmark_json(report_payload())
        self.assertEqual(len(result["metrics"]), 12)
        self.assertEqual(len(result["oracle_sha256"]), 64)

    def test_method_mismatch_in_raw_sample_is_rejected(self):
        report = json.loads(report_payload())
        report["cases"][0]["samples"][0]["method"] = "recv_timeout_zero"
        with self.assertRaisesRegex(runner.BenchmarkError, "method"):
            runner.validate_benchmark_json(json.dumps(report).encode())

    def test_missing_raw_sample_is_rejected(self):
        report = json.loads(report_payload())
        report["cases"][0]["samples"].pop()
        with self.assertRaisesRegex(runner.BenchmarkError, "exactly three raw samples"):
            runner.validate_benchmark_json(json.dumps(report).encode())

    def test_wrong_count_checksum_or_order_is_rejected(self):
        for field, bad_value in (
            ("events", runner.EVENTS_PER_CASE - 1),
            ("events", float(runner.EVENTS_PER_CASE)),
            ("checksum", runner.EXPECTED_CHECKSUM + 1),
            ("order_oracle", "different ordering"),
        ):
            with self.subTest(field=field):
                report = json.loads(report_payload())
                report["cases"][0]["samples"][0][field] = bad_value
                with self.assertRaises(runner.BenchmarkError):
                    runner.validate_benchmark_json(json.dumps(report).encode())

    def test_cross_arm_oracle_mismatch_invalidates_pair(self):
        valid = runner.validate_benchmark_json(report_payload())
        baseline = {
            "attempt_id": "baseline-1",
            "successful": True,
            "oracle_sha256": valid["oracle_sha256"],
            "case_p50_ns_per_event": valid["metrics"],
        }
        candidate = {
            "attempt_id": "candidate-1",
            "successful": True,
            "oracle_sha256": "0" * 64,
            "case_p50_ns_per_event": valid["metrics"],
        }
        pair = runner._comparison(1, baseline, candidate)
        self.assertFalse(pair["valid"])
        self.assertFalse(pair["cross_arm_oracle_match"])
        self.assertEqual(pair["failure_reasons"]["cross_arm_oracle"], "baseline/candidate sample metadata differ")

    def test_nonzero_exit_preserves_report_and_failure_without_retry(self):
        payload = report_payload()
        with tempfile.TemporaryDirectory() as directory:
            attempt = Path(directory) / "attempt"

            def failed_process(command, **kwargs):
                Path(kwargs["env"]["FLOW_RECV_NAMED_BENCH_JSON"]).write_bytes(payload)
                return runner.subprocess.CompletedProcess(command, 7, b"partial stdout", b"failure stderr")

            with patch.object(runner.subprocess, "run", side_effect=failed_process) as mocked:
                result = runner.run_attempt(["frozen-test-binary"], attempt, {"attempt_id": "failed-1"}, 120)

            self.assertEqual(mocked.call_count, 1)
            self.assertFalse(result["successful"])
            self.assertEqual(result["returncode"], 7)
            self.assertTrue(result["raw_json_present"])
            self.assertEqual((attempt / "raw.json").read_bytes(), payload)
            self.assertEqual((attempt / "stdout.bin").read_bytes(), b"partial stdout")
            self.assertEqual((attempt / "stderr.bin").read_bytes(), b"failure stderr")
            self.assertTrue((attempt / "finished.json").is_file())

    def test_timeout_preserves_partial_report_and_timeout_status(self):
        payload = report_payload()
        with tempfile.TemporaryDirectory() as directory:
            attempt = Path(directory) / "attempt"

            def timed_out_process(command, **kwargs):
                Path(kwargs["env"]["FLOW_RECV_NAMED_BENCH_JSON"]).write_bytes(payload)
                raise runner.subprocess.TimeoutExpired(
                    command, kwargs["timeout"], output=b"partial stdout", stderr=b"partial stderr"
                )

            with patch.object(runner.subprocess, "run", side_effect=timed_out_process) as mocked:
                result = runner.run_attempt(["frozen-test-binary"], attempt, {"attempt_id": "timeout-1"}, 120)

            self.assertEqual(mocked.call_count, 1)
            self.assertTrue(result["timed_out"])
            self.assertFalse(result["successful"])
            self.assertTrue(result["raw_json_present"])
            self.assertEqual((attempt / "raw.json").read_bytes(), payload)
            self.assertEqual((attempt / "stdout.bin").read_bytes(), b"partial stdout")
            self.assertEqual((attempt / "stderr.bin").read_bytes(), b"partial stderr")

    def test_pair_schedule_is_fixed_and_restarts_ab_ba_order_for_measurement(self):
        expected = [
            ("baseline", "candidate"),
            ("candidate", "baseline"),
            ("baseline", "candidate"),
        ]
        self.assertEqual([runner.interleaved_order(index) for index in range(3)], expected)
        measurement_orders = [runner.interleaved_order(pair_index) for pair_index in range(48)]
        self.assertEqual(measurement_orders[:3], expected)
        self.assertEqual(measurement_orders[-1], ("candidate", "baseline"))
        self.assertEqual(runner.WARMUP_PAIRS, 3)
        self.assertEqual(runner.MEASURED_PAIRS, 48)
        self.assertEqual(runner.INVOCATION_TIMEOUT_SECONDS, 120)
        self.assertLessEqual(runner.MAX_TOTAL_BUDGET_SECONDS, 3 * 60 * 60)

    def test_budget_exhaustion_slots_preserve_pair_order(self):
        first_measurement_pair = runner._planned_slots_after(3, 1)
        self.assertEqual(
            first_measurement_pair,
            [
                {
                    "phase": "measurement",
                    "pair_index": 1,
                    "position_in_pair": 2,
                    "side": "candidate",
                    "status": "not_run_total_budget_exhausted",
                }
            ],
        )
        second_measurement_pair = runner._planned_slots_after(4, 1)
        self.assertEqual(second_measurement_pair[0]["side"], "baseline")

    def test_canonical_receipts_bind_sequential_fresh_builds_to_distinct_saved_binaries(self):
        campaign = Path(__file__).resolve().parents[2]
        evidence = campaign / "evidence/graph-named-recv"
        freeze_hash = runner.sha256_file(evidence / "source-freeze.json")
        baseline = runner._verify_build_receipt(
            "baseline",
            evidence / "baseline-binary-receipt.json",
            campaign / "work/graph-named-recv-baseline-c6",
            runner.SOURCE_FREEZE_BASE_COMMIT,
            runner.EXPECTED_PATCH_SHA256_BY_SIDE["baseline"],
            freeze_hash,
        )
        candidate = runner._verify_build_receipt(
            "candidate",
            evidence / "candidate-binary-receipt.json",
            campaign / "work/graph-named-recv-worktree",
            runner.SOURCE_FREEZE_BASE_COMMIT,
            runner.EXPECTED_PATCH_SHA256_BY_SIDE["candidate"],
            freeze_hash,
        )
        self.assertEqual(baseline["shared_target_dir"], candidate["shared_target_dir"])
        self.assertNotEqual(baseline["binary"], candidate["binary"])
        self.assertNotEqual(baseline["binary_sha256"], candidate["binary_sha256"])
        self.assertNotEqual(baseline["source_sha256"], candidate["source_sha256"])
        self.assertFalse(baseline["compiler_artifact_fresh"])
        self.assertFalse(candidate["compiler_artifact_fresh"])


if __name__ == "__main__":
    unittest.main()
