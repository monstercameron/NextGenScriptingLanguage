"""Offline contract tests; never build or execute the Rust product."""
from __future__ import annotations

import importlib.util
import json
import os
from pathlib import Path
import tempfile
import unittest

RUNNER_PATH = Path(__file__).with_name("text_json_decoder_pair.py")
SPEC = importlib.util.spec_from_file_location("text_json_decoder_pair", RUNNER_PATH)
assert SPEC is not None and SPEC.loader is not None
pair = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(pair)

DRIVER_PATH = Path(__file__).resolve().parents[2] / "work/build_text_json_decoder_pair.py"
DRIVER_SPEC = importlib.util.spec_from_file_location("build_text_json_decoder_pair", DRIVER_PATH)
assert DRIVER_SPEC is not None and DRIVER_SPEC.loader is not None
driver = importlib.util.module_from_spec(DRIVER_SPEC)
DRIVER_SPEC.loader.exec_module(driver)


def report_fixture():
    workload = pair.workload_inputs()
    oracle = {
        "decoded_values": 10_000,
        "exact_value_matches": 10_000,
        "errors": 0,
        "decoded_string_bytes": 4096,
        "decoded_string_sha256": workload["decoded_string_sha256"],
    }
    return {
        "schema": "output_schema_text_json_decode_bench/v1",
        "method": pair.EXPECTED_METHOD,
        "warmup_runs": 0,
        "measured_samples": 3,
        "workload": workload,
        "results": {
            pair.CASE: {
                "p50_ns": 20,
                "p95_ns": 30,
                "raw_sample_ns": [10, 20, 30],
                "semantic_oracles": [dict(oracle) for _ in range(3)],
            }
        },
    }


class TextJsonDecoderPairTests(unittest.TestCase):
    def test_build_driver_rejects_optimized_python(self):
        with self.assertRaisesRegex(RuntimeError, "optimization disabled"):
            driver.require_unoptimized_python(1)
        driver.require_unoptimized_python(0)

    def test_deployed_root_full_source_bindings_and_one_file_delta(self):
        self.assertEqual(pair.C, driver.C)
        binding = pair.validate_source_bindings()
        driver.validate_source_bindings(binding)
        self.assertEqual(binding["arms"]["baseline"]["tree"]["file_count"], 5917)
        self.assertEqual(binding["arms"]["candidate"]["tree"]["file_count"], 5917)
        self.assertEqual(binding["arms"]["candidate"]["tracked_diff_sha256"], pair.PRODUCT_PATCH_SHA256)

    def test_builder_filters_environment_and_pins_campaign_target(self):
        with tempfile.TemporaryDirectory() as directory:
            host = {
                "PATH": "/host/bin:/usr/bin", "HOME": directory,
                "LANG": "C.UTF-8", "RUSTFLAGS": "-C opt-level=0",
                "CARGO_HOME": "/untrusted/cargo", "UNLISTED_OVERRIDE": "drop",
            }
            env = driver.setup_environment(host)
        allowed = (set(driver.INHERITED_BUILD_ENV_KEYS) & set(host)) | set(driver.FIXED_BUILD_ENVIRONMENT)
        self.assertEqual(set(env), allowed)
        self.assertTrue(env["PATH"].startswith(str(driver.PINNED_CARGO_BIN) + os.pathsep))
        self.assertEqual(env["CARGO_TARGET_DIR"], str(pair.TARGET.resolve()))
        self.assertNotIn("RUSTFLAGS", env)
        self.assertNotIn("UNLISTED_OVERRIDE", env)

    def test_workload_is_a_fixed_4k_ascii_json_string(self):
        workload = pair.workload_inputs()
        payload = json.dumps("x" * 4096, ensure_ascii=False, separators=(",", ":")).encode()
        self.assertEqual(workload["api"], "flow::contracts::OutputSchema::decode")
        self.assertEqual(workload["iterations_per_sample"], 10_000)
        self.assertEqual(workload["decoded_string_bytes"], 4096)
        self.assertEqual(workload["json_payload_bytes"], 4098)
        self.assertEqual(workload["json_payload_sha256"], pair.sha_bytes(payload))
        self.assertEqual(workload["decoded_string_sha256"], pair.sha_bytes(b"x" * 4096))

    def test_protocol_has_three_warmup_and_fixed_balanced_48_pairs(self):
        orders = [pair.interleaved_order(i) for i in range(pair.WARMUP_PAIRS + pair.MEASURED_PAIRS)]
        self.assertEqual(len(orders), 51)
        self.assertEqual(orders[:3], [("baseline", "candidate"), ("candidate", "baseline"), ("baseline", "candidate")])
        measured = orders[3:]
        self.assertEqual(measured[0], ("candidate", "baseline"))
        self.assertEqual(measured.count(("baseline", "candidate")), 24)
        self.assertEqual(measured.count(("candidate", "baseline")), 24)
        self.assertEqual((pair.INNER_WARMUPS, pair.INNER_SAMPLES), (0, 3))
        self.assertEqual(pair.INVOCATION_TIMEOUT_SECONDS, 120)

    def test_raw_report_accepts_exact_decoder_oracles(self):
        parsed = pair.validate_raw_report(json.dumps(report_fixture()).encode())
        self.assertEqual(parsed["median_duration_ns"], 20)
        self.assertEqual(parsed["raw_sample_ns"], [10, 20, 30])
        self.assertEqual(parsed["workload"], pair.workload_inputs())

    def test_raw_report_rejects_fixture_oracle_and_percentile_drift(self):
        mutations = (
            lambda r: r["workload"].update(json_payload_sha256="0" * 64),
            lambda r: r["workload"].update(decoded_string_bytes=4095),
            lambda r: r["results"][pair.CASE]["semantic_oracles"][0].update(exact_value_matches=9999),
            lambda r: r["results"][pair.CASE]["semantic_oracles"][1].update(errors=1),
            lambda r: r["results"][pair.CASE].update(raw_sample_ns=[10, 20]),
            lambda r: r["results"][pair.CASE].update(p50_ns=21),
            lambda r: r["results"][pair.CASE].update(p95_ns=29),
            lambda r: r.update(unexpected=True),
        )
        for mutate in mutations:
            with self.subTest(mutate=mutate):
                value = report_fixture()
                mutate(value)
                with self.assertRaises(pair.PairError):
                    pair.validate_raw_report(json.dumps(value).encode())

    def test_strict_json_rejects_duplicate_keys_nonfinite_and_invalid_utf8(self):
        for payload in (b'{"x":1,"x":2}', b'{"x":NaN}', b'{"x":Infinity}', b"\xff"):
            with self.subTest(payload=payload):
                with self.assertRaises(pair.PairError):
                    pair.strict_json_bytes(payload, "test JSON")

    def test_actual_build_provenance_when_root_has_prepared_it(self):
        if not (pair.BUILD_RUN / "build-pair-receipt.json").is_file():
            self.skipTest("root has not completed source-bound clean release builds")
        metadata, arms = pair.load_build_provenance()
        self.assertEqual(metadata["build_driver_sha256"], pair.BUILD_DRIVER_SHA256)
        self.assertEqual(set(arms), {"baseline", "candidate"})
        self.assertNotEqual(arms["baseline"]["test_binary"]["binary_sha256"], arms["candidate"]["test_binary"]["binary_sha256"])


if __name__ == "__main__":
    unittest.main()
