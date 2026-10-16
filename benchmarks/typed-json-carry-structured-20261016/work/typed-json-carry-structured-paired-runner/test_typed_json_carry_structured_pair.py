"""Offline contract tests; never build or execute the Rust product."""
from __future__ import annotations

import importlib.util
import json
import os
from pathlib import Path
import tempfile
import unittest

RUNNER_PATH = Path(__file__).with_name("typed_json_carry_structured_pair.py")
SPEC = importlib.util.spec_from_file_location("typed_json_carry_structured_pair", RUNNER_PATH)
assert SPEC is not None and SPEC.loader is not None
pair = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(pair)

DRIVER_PATH = Path(__file__).resolve().parents[2] / "work/build_typed_json_carry_structured_pair.py"
DRIVER_SPEC = importlib.util.spec_from_file_location("build_typed_json_carry_structured_pair", DRIVER_PATH)
assert DRIVER_SPEC is not None and DRIVER_SPEC.loader is not None
driver = importlib.util.module_from_spec(DRIVER_SPEC)
DRIVER_SPEC.loader.exec_module(driver)


def raw_report():
    workload = pair.workload_inputs()
    workload["catalog_sha256"] = "a" * 64
    oracle = {
        "output_sha256": workload["expected_output_sha256"],
        "output_bytes": workload["expected_output_bytes"],
        "launched_processes": 0,
        "cache_hits": 0,
        "retained_execution_records": 256,
        "retained_all_success_status_0": True,
        "retained_stdout_bytes_per_call": workload["expected_per_call_stdout_bytes_including_lf"],
        "dropped_execution_records": 744,
        "dropped_stages": 744,
    }
    return {
        "schema": "typed_json_carry_public_runtime_structured_bench/v1",
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


class TypedJsonCarryStructuredPairTests(unittest.TestCase):
    def test_build_driver_rejects_optimized_python_explicitly(self):
        with self.assertRaisesRegex(RuntimeError, "optimization disabled"):
            driver.require_unoptimized_python(1)
        driver.require_unoptimized_python(0)

    def test_deployed_campaign_root_and_full_source_bindings(self):
        self.assertEqual(pair.C, driver.C)
        self.assertEqual(pair.CAMPAIGN, driver.C)
        binding = pair.validate_source_bindings()
        self.assertEqual(binding["arms"]["baseline"]["tree"]["file_count"], 2197)
        self.assertEqual(binding["arms"]["candidate"]["tree"]["file_count"], 2197)
        self.assertEqual(binding["arms"]["candidate"]["tracked_diff_sha256"], pair.PRODUCT_PATCH_SHA256)

    def test_builder_filters_environment_and_pins_shared_target(self):
        with tempfile.TemporaryDirectory() as directory:
            host = {
                "PATH": "/untrusted/tool/bin:/usr/bin", "HOME": directory,
                "LANG": "C.UTF-8", "RUSTFLAGS": "-C opt-level=0",
                "CARGO_HOME": "/untrusted/cargo", "EXTRA_OVERRIDE": "drop-me",
            }
            environment = driver.setup_environment(host)
        expected = (set(driver.INHERITED_BUILD_ENV_KEYS) & set(host)) | set(driver.FIXED_BUILD_ENVIRONMENT)
        self.assertEqual(set(environment), expected)
        self.assertTrue(environment["PATH"].startswith(str(driver.PINNED_CARGO_BIN) + os.pathsep))
        self.assertEqual(environment["CARGO_TARGET_DIR"], str(pair.TARGET.resolve()))
        self.assertNotIn("RUSTFLAGS", environment)
        self.assertNotIn("EXTRA_OVERRIDE", environment)

    def test_runtime_child_environment_is_exact_and_hashed(self):
        raw_path = Path("/tmp/structured-bench/raw.json")
        environment, digest = pair.invocation_environment(raw_path)
        self.assertEqual(environment, {
            "LC_ALL": "C", "TZ": "UTC", "OPT_BENCH_REPORT": str(raw_path),
        })
        self.assertEqual(digest, pair.canonical_json_sha256(environment))
        contract = pair.runtime_environment_contract()
        self.assertEqual(set(contract), {"base", "base_sha256", "invocation_template", "invocation_template_sha256", "policy"})
        self.assertEqual(contract["base_sha256"], pair.canonical_json_sha256({"LC_ALL": "C", "TZ": "UTC"}))
        self.assertEqual(contract["invocation_template"], {
            "LC_ALL": "C", "TZ": "UTC", "OPT_BENCH_REPORT": "<fresh-absolute-path>",
        })
        self.assertEqual(contract["invocation_template_sha256"], pair.canonical_json_sha256(contract["invocation_template"]))
        with self.assertRaises(pair.PairError):
            pair.invocation_environment(Path("relative/raw.json"))

    def test_cargo_log_has_one_exact_receipt_artifact_row(self):
        root = pair.BASELINE_ROOT
        binary = pair.TARGET / "release" / "deps" / "typed_json_carry_structured-abc"
        row = {
            "reason": "compiler-artifact", "fresh": False,
            "target": {
                "name": pair.TARGET_NAME, "kind": ["test"], "test": True,
                "src_path": str(root / pair.HARNESS_RELATIVE),
            },
            "profile": {"test": True, "opt_level": "3", "debug_assertions": False},
            "manifest_path": str(root / "Cargo.toml"),
            "package_id": f"path+{root.as_uri()}#execute@0.1.0",
            "executable": str(binary), "filenames": [str(binary)],
        }
        unrelated = {"reason": "compiler-artifact", "target": {"name": "other-test"}}
        payload = (json.dumps(unrelated) + "\n" + json.dumps(row) + "\n").encode()
        self.assertEqual(pair.validate_build_log_artifact(payload, side="baseline", root=root, receipt_row=row), row)
        with self.assertRaisesRegex(pair.PairError, "exactly one"):
            pair.validate_build_log_artifact(payload + json.dumps(row).encode() + b"\n", side="baseline", root=root, receipt_row=row)
        changed_receipt = {**row, "future_receipt_field": True}
        with self.assertRaisesRegex(pair.PairError, "differs from the hashed build log"):
            pair.validate_build_log_artifact(payload, side="baseline", root=root, receipt_row=changed_receipt)

    def test_fixed_workload_is_exactly_1000_unique_result_names_and_64x4_input(self):
        workload = pair.workload_inputs()
        self.assertEqual(workload["calls"], 1000)
        self.assertEqual(workload["applet"], "csv.parse")
        self.assertEqual(workload["binding_pattern"], "table0000 through table0999 (distinct bindings required by the DSL)")
        self.assertEqual(workload["retained_parsed_tables"], 1000)
        self.assertEqual((workload["rows"], workload["columns"]), (64, 4))
        self.assertIn("expected_headers", workload)
        self.assertEqual(len(workload["expected_rows"]), 64)
        self.assertTrue(3000 <= workload["input_bytes"] <= 5000)
        self.assertEqual(len(workload["expected_output_hex"]) // 2, workload["expected_output_bytes"])
        self.assertEqual(len(workload["expected_output_sha256"]), 64)
        self.assertEqual(workload["expected_per_call_stdout_bytes_including_lf"], workload["expected_output_bytes"] + 1)

    def test_protocol_has_three_warmups_and_balanced_fixed_48_pairs(self):
        orders = [pair.interleaved_order(i) for i in range(pair.WARMUP_PAIRS + pair.MEASURED_PAIRS)]
        self.assertEqual(orders[:3], [
            ("baseline", "candidate"),
            ("candidate", "baseline"),
            ("baseline", "candidate"),
        ])
        measured = orders[3:]
        self.assertEqual(len(measured), 48)
        self.assertEqual(measured[0], ("candidate", "baseline"))
        self.assertEqual(measured.count(("baseline", "candidate")), 24)
        self.assertEqual(measured.count(("candidate", "baseline")), 24)
        self.assertEqual(pair.INVOCATION_TIMEOUT_SECONDS, 120)
        self.assertEqual(pair.INNER_WARMUPS, 0)
        self.assertEqual(pair.INNER_SAMPLES, 3)

    def test_raw_report_accepts_exact_table_bytes_and_semantics(self):
        report = raw_report()
        validated = pair.validate_raw_report(json.dumps(report).encode())
        self.assertEqual(validated["median_duration_ns"], 20)
        self.assertEqual(validated["raw_sample_ns"], [10, 20, 30])
        self.assertEqual(validated["workload"]["expected_headers"], ["identifier", "label", "category", "note"])
        self.assertEqual(validated["workload"]["expected_output_sha256"], report["workload"]["expected_output_sha256"])

    def test_raw_report_rejects_altered_fixture_output_and_catalog_identity(self):
        mutations = (
            lambda report: report["workload"].update(input_sha256="0" * 64),
            lambda report: report["workload"].update(expected_rows=[]),
            lambda report: report["workload"].update(expected_output_hex="00"),
            lambda report: report["workload"].update(expected_output_bytes=1),
            lambda report: report["workload"].update(catalog_sha256="z" * 64),
        )
        for mutate in mutations:
            with self.subTest(mutate=mutate):
                report = raw_report()
                mutate(report)
                with self.assertRaises(pair.PairError):
                    pair.validate_raw_report(json.dumps(report).encode())

    def test_raw_report_rejects_changed_per_sample_execution_oracles(self):
        mutations = (
            ("output_sha256", "0" * 64),
            ("output_bytes", 1),
            ("launched_processes", 1),
            ("cache_hits", 1),
            ("retained_execution_records", 255),
            ("retained_all_success_status_0", False),
            ("retained_stdout_bytes_per_call", 0),
            ("dropped_execution_records", 743),
            ("dropped_stages", 743),
        )
        for field, value in mutations:
            with self.subTest(field=field):
                report = raw_report()
                report["results"][pair.CASE]["semantic_oracles"][1][field] = value
                with self.assertRaises(pair.PairError):
                    pair.validate_raw_report(json.dumps(report).encode())

    def test_raw_report_rejects_wrong_samples_percentiles_and_extra_fields(self):
        mutations = (
            lambda report: report["results"][pair.CASE].update(raw_sample_ns=[10, 20]),
            lambda report: report["results"][pair.CASE].update(raw_sample_ns=[10, True, 30]),
            lambda report: report["results"][pair.CASE].update(p50_ns=21),
            lambda report: report["results"][pair.CASE].update(p95_ns=29),
            lambda report: report["results"][pair.CASE].update(p95_ns=True),
            lambda report: report.update(unexpected=True),
        )
        for mutate in mutations:
            with self.subTest(mutate=mutate):
                report = raw_report()
                mutate(report)
                with self.assertRaises(pair.PairError):
                    pair.validate_raw_report(json.dumps(report).encode())

    def test_strict_json_rejects_duplicate_keys_nonfinite_and_invalid_utf8(self):
        for payload in (b'{"x":1,"x":2}', b'{"x":NaN}', b'{"x":Infinity}', b"\xff"):
            with self.subTest(payload=payload):
                with self.assertRaises(pair.PairError):
                    pair.strict_json_bytes(payload, "test JSON")

    def test_workload_comparison_and_pair_summary_keep_wins_losses_and_ties(self):
        first = raw_report()["workload"]
        second = dict(first)
        self.assertTrue(pair.workload_compatible([first, second]))
        second["catalog_sha256"] = "b" * 64
        self.assertFalse(pair.workload_compatible([first, second]))
        self.assertFalse(pair.workload_compatible([]))
        pairs = [
            {"valid_for_timing": True, "candidate_minus_baseline_ns": -10},
            {"valid_for_timing": True, "candidate_minus_baseline_ns": 20},
            {"valid_for_timing": True, "candidate_minus_baseline_ns": 0},
        ]
        summary = pair.summarize(pairs)
        self.assertEqual((summary["wins"], summary["losses"], summary["ties"]), (1, 1, 1))
        self.assertEqual(summary["median_candidate_minus_baseline_ns"], 0)

    def test_actual_build_provenance_when_prepared(self):
        if not (pair.BUILD_RUN / "build-pair-receipt.json").is_file():
            self.skipTest("root has not completed the source-bound clean release builds")
        metadata, arms = pair.load_build_provenance()
        self.assertEqual(metadata["build_driver_path"], str(DRIVER_PATH.resolve()))
        self.assertEqual(metadata["build_run"], str(pair.BUILD_RUN.resolve(strict=True)))
        self.assertEqual(set(arms), {"baseline", "candidate"})
        self.assertNotEqual(arms["baseline"]["test_binary"]["binary_sha256"], arms["candidate"]["test_binary"]["binary_sha256"])


if __name__ == "__main__":
    unittest.main()
