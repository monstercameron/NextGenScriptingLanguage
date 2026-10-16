"""Offline protocol/oracle checks; these never invoke a benchmark binary."""
from __future__ import annotations

import json
from pathlib import Path
import subprocess
import tempfile
import unittest

import program_module_cache_pair as runner


def valid_report() -> dict:
    return {
        "version": "0.9.0-rc.5",
        "workload": runner.WORKLOAD,
        "measurement": runner.MEASUREMENT_NOTE,
        "module": runner.MODULE,
        "calls_per_run": runner.CALLS_PER_RUN,
        "successful_module_invocations_per_run": runner.CALLS_PER_RUN,
        "per_call_output_retention": runner.RETENTION_NOTE,
        "prepared_program_retention": runner.PROGRAM_RETENTION_NOTE,
        "name_utf8_bytes": runner.NAME_BYTES,
        "warmup_runs": runner.WARMUP_RUNS,
        "measured_runs": runner.INNER_SAMPLES,
        "input_sha256": runner.INPUT_SHA256,
        "caller_source_sha256": runner.sha256_bytes(runner.CALLER_SOURCE.encode("utf-8")),
        "expected_output_bytes": len(runner.EXPECTED_OUTPUT),
        "expected_output_sha256": runner.OUTPUT_SHA256,
        "sample_ns": [100.0, 110.0, 90.0],
    }


def encoded_report(value: dict | None = None) -> bytes:
    return json.dumps(value or valid_report(), allow_nan=False).encode("utf-8")


class ProgramModuleCachePairTests(unittest.TestCase):
    def test_pair_order_is_balanced_with_measured_order_starting_ba(self) -> None:
        order = [
            runner.pair_order(index)
            for index in range(runner.WARMUP_PAIRS + runner.MEASURED_PAIRS)
        ]
        self.assertEqual(order[:3], [("baseline", "candidate"), ("candidate", "baseline"), ("baseline", "candidate")])
        measured = order[runner.WARMUP_PAIRS :]
        self.assertEqual(measured[0], ("candidate", "baseline"))
        self.assertEqual(measured.count(("baseline", "candidate")), 24)
        self.assertEqual(measured.count(("candidate", "baseline")), 24)

    def test_harness_oracle_requires_exact_input_output_calls_and_sample_count(self) -> None:
        parsed = runner.validate_harness_report(encoded_report(), "0.9.0-rc.5")
        self.assertEqual(parsed["median_ns"], 100.0)
        for key, value in (
            ("successful_module_invocations_per_run", 127),
            ("expected_output_sha256", "0" * 64),
            ("input_sha256", "f" * 64),
        ):
            mutated = valid_report()
            mutated[key] = value
            with self.subTest(key=key), self.assertRaises(runner.PairError):
                runner.validate_harness_report(encoded_report(mutated), "0.9.0-rc.5")
        wrong_sample_count = valid_report()
        wrong_sample_count["sample_ns"].pop()
        with self.assertRaisesRegex(runner.PairError, "exactly three"):
            runner.validate_harness_report(encoded_report(wrong_sample_count), "0.9.0-rc.5")

    def test_build_stream_must_bind_one_exact_artifact_and_successful_final_record(self) -> None:
        artifact = {
            "reason": "compiler-artifact",
            "target": {"name": runner.TEST_TARGET},
            "profile": {"test": True},
            "executable": "/tmp/test-binary",
        }
        success = {"reason": "build-finished", "success": True}
        payload = (json.dumps(artifact) + "\n" + json.dumps(success) + "\n").encode()
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "cargo.jsonl"
            path.write_bytes(payload)
            result = runner._load_build_stream(path, artifact, "baseline")
            self.assertEqual(result["artifact_record_index"], 0)
            self.assertEqual(result["build_finished_record_index"], 1)
            self.assertEqual(result["record_count"], 2)

            path.write_bytes((json.dumps(artifact) + "\n" + json.dumps(artifact) + "\n" + json.dumps(success) + "\n").encode())
            with self.assertRaisesRegex(runner.PairError, "exactly the receipted"):
                runner._load_build_stream(path, artifact, "baseline")

            path.write_bytes((json.dumps(artifact) + "\n" + json.dumps({"reason": "build-finished", "success": False}) + "\n").encode())
            with self.assertRaisesRegex(runner.PairError, "successful build-finished"):
                runner._load_build_stream(path, artifact, "baseline")

    def test_build_stream_rejects_duplicate_keys_and_truncated_json(self) -> None:
        artifact = {
            "reason": "compiler-artifact",
            "target": {"name": runner.TEST_TARGET},
            "profile": {"test": True},
        }
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "cargo.jsonl"
            path.write_text('{"reason":"compiler-artifact","reason":"compiler-artifact"}\n')
            with self.assertRaisesRegex(runner.PairError, "invalid"):
                runner._load_build_stream(path, artifact, "candidate")
            path.write_text('{"reason":"compiler-artifact"\n')
            with self.assertRaisesRegex(runner.PairError, "invalid"):
                runner._load_build_stream(path, artifact, "candidate")

    def test_pair_retains_signed_losses_and_failed_attempts(self) -> None:
        oracle = {"input_sha256": runner.INPUT_SHA256, "expected_output_sha256": runner.OUTPUT_SHA256}
        baseline = {"attempt_id": "b", "successful": True, "oracle": oracle, "median_ns": 100.0}
        candidate = {"attempt_id": "c", "successful": True, "oracle": dict(oracle), "median_ns": 125.0}
        pair = runner._pair_result(1, baseline, candidate)
        self.assertTrue(pair["valid_for_timing_comparison"])
        self.assertEqual(pair["measurement"]["candidate_minus_baseline_ns"], 25.0)
        self.assertTrue(pair["measurement"]["candidate_loss"])
        summary = runner._summary([pair])
        self.assertEqual(summary["candidate_losses"], 1)
        self.assertEqual(summary["candidate_improvements"], 0)

        failed = {"attempt_id": "failed", "successful": False, "validation_error": "wrong oracle"}
        invalid = runner._pair_result(2, baseline, failed)
        self.assertFalse(invalid["valid_for_timing_comparison"])
        self.assertIn("wrong oracle", invalid["failure_reasons"]["candidate"])
        self.assertEqual(runner._summary([invalid])["invalid_measured_pairs"], 1)

    def test_source_fingerprint_includes_harness_vendor_build_config_and_runtime_fixture(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            files = {
                "Cargo.toml": b"[package]\nname='execute'\nversion='0.9.0-rc.5'\n",
                "Cargo.lock": b"lock",
                "src/runtime.rs": b"runtime",
                "tests/program_module_cache_bench.rs": b"harness",
                "vendor/example/src/lib.rs": b"vendor",
                ".cargo/config.toml": b"config",
                "examples/modules/records.program": b"fixture",
                "docs/readme.md": b"documentation only",
            }
            for relative, payload in files.items():
                path = root / relative
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(payload)
            subprocess.run(["git", "init", str(root)], check=True, stdout=subprocess.DEVNULL)
            subprocess.run(["git", "-C", str(root), "add", *files], check=True, stdout=subprocess.DEVNULL)
            fingerprint = runner.source_fingerprint(root)
            self.assertEqual(
                set(fingerprint["files"]),
                {
                    "Cargo.toml",
                    "Cargo.lock",
                    "src/runtime.rs",
                    "tests/program_module_cache_bench.rs",
                    "vendor/example/src/lib.rs",
                    ".cargo/config.toml",
                    "examples/modules/records.program",
                },
            )
            self.assertNotIn("docs/readme.md", fingerprint["files"])


if __name__ == "__main__":
    unittest.main()
