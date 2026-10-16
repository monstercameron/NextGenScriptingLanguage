from __future__ import annotations

import json
from pathlib import Path
import tempfile
import unittest

import build_line_framing_pair as build
import line_framing_pair as pair


class LineFramingPairTests(unittest.TestCase):
    def test_run_plan_pins_current_preparation_inputs(self) -> None:
        plan = pair.read_json(pair.RUN_PLAN_PATH, "run plan")
        self.assertEqual(plan["paired_runner_sha256"], pair.sha256_file(Path(pair.__file__).resolve()))
        self.assertEqual(plan["offline_test_file_sha256"], pair.sha256_file(Path(__file__).resolve()))
        self.assertEqual(plan["build_only_workflow"]["script_sha256"], pair.sha256_file(pair.BUILD_DRIVER_PATH))
        self.assertEqual(plan["candidate_product_patch_sha256"], pair.sha256_file(pair.HERE.parent / "line-framing-memchr.patch"))
        self.assertEqual(plan["benchmark_harness_sha256"], pair.HARNESS_SHA256)

    def test_differential_preflight_requires_exactly_one_named_test(self) -> None:
        output = (
            f"running 1 test\ntest {pair.DIFFERENTIAL_TEST} ... ok\n"
            "test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 297 filtered out\n"
        ).encode()
        self.assertTrue(pair.differential_test_output_passes(output))
        self.assertFalse(pair.differential_test_output_passes(b"running 0 tests\ntest result: ok. 0 passed; 0 failed\n"))

    def test_build_driver_accepts_only_the_prepared_worktree_changes(self) -> None:
        build.expected_tracked_changes("baseline", build.BASELINE)
        build.expected_tracked_changes("candidate", build.CANDIDATE)
        self.assertEqual([target["key"] for target in build.BUILD_TARGETS["baseline"]], [pair.HARNESS_NAME])
        self.assertEqual(
            [target["key"] for target in build.BUILD_TARGETS["candidate"]],
            [pair.HARNESS_NAME, "line_framing_lib_test"],
        )
        lib_target = build.BUILD_TARGETS["candidate"][1]
        self.assertEqual((lib_target["kind"], lib_target["crate_type"], lib_target["source"]), ("lib", "lib", "src/lib.rs"))

    def test_artifact_specs_match_cargo_receipt_target_spec(self) -> None:
        self.assertEqual(pair.ARTIFACT_SPECS[pair.HARNESS_NAME], {
            "key": "native_stream_status_bench",
            "cargo_args": ["--test", "native_stream_status_bench"],
            "target_name": "native_stream_status_bench",
            "kind": "test",
            "crate_type": "bin",
            "source": "tests/native_stream_status_bench.rs",
        })
        self.assertEqual(pair.ARTIFACT_SPECS["line_framing_lib_test"], {
            "key": "line_framing_lib_test",
            "cargo_args": ["--lib"],
            "target_name": "flow",
            "kind": "lib",
            "crate_type": "lib",
            "source": "src/lib.rs",
        })

    def test_source_identity_pins_exact_baseline_and_candidate(self) -> None:
        baseline = pair.source_identity("baseline", pair.BASELINE)
        candidate = pair.source_identity("candidate", pair.CANDIDATE)
        self.assertEqual(baseline["commit"], pair.BASE_COMMIT)
        self.assertIsNone(baseline["product_patch_sha256"])
        self.assertEqual(candidate["product_patch_sha256"], pair.PRODUCT_PATCH_SHA256)
        self.assertEqual(baseline["harness_sha256"], pair.HARNESS_SHA256)
        self.assertEqual(candidate["harness_sha256"], pair.HARNESS_SHA256)
        self.assertNotEqual(baseline["fingerprint_sha256"], candidate["fingerprint_sha256"])

    def test_schedule_is_balanced_and_pinned(self) -> None:
        schedule = pair.pair_schedule()
        self.assertEqual(len(schedule), 51)
        self.assertEqual([row[1] for row in schedule[:3]], ["warmup"] * 3)
        measured = [row[2] for row in schedule[3:]]
        self.assertEqual(measured.count(("baseline", "candidate")), 24)
        self.assertEqual(measured.count(("candidate", "baseline")), 24)
        self.assertEqual(schedule[3][2], ("candidate", "baseline"))
        self.assertEqual((pair.INNER_WARMUPS, pair.INNER_SAMPLES), (0, 3))
        self.assertEqual((pair.TIMEOUT_SECONDS, pair.RECORDS, pair.STAGES), (120, 131_072, (1, 4, 8)))

    def test_report_validator_requires_all_exact_output_oracles(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            binary = Path(directory) / "bench"
            stage_binary = Path(directory) / "cat"
            binary.write_bytes(b"bench")
            stage_binary.write_bytes(b"cat")
            revision = "candidate:base:fingerprint:patch"
            stage_hash = pair.sha256_file(stage_binary)
            metadata = pair.expected_report_metadata(revision, binary, stage_binary, stage_hash)
            input_bytes, input_hash = pair.expected_fixture()
            samples = []
            rotations = ((1, 4, 8), (4, 8, 1), (8, 1, 4))
            for round_index, order in enumerate(rotations):
                for stages in order:
                    samples.append({
                        "round": round_index,
                        "stages": stages,
                        "elapsed_ns": 1000 + stages,
                        "result_records": pair.RECORDS,
                        "callback_records": pair.RECORDS,
                        "launched_processes": stages,
                        "bytes": input_bytes,
                        "output_sha256": input_hash,
                        "successful_statuses": True,
                        "result_counts_match": True,
                        "launch_count_matches": True,
                        "callback_counts_match": True,
                        "output_matches": True,
                        "sample_valid": True,
                        "error": None,
                    })
            report = {
                **metadata,
                "warmup_samples": [],
                "raw_samples": samples,
                "paired_plan": pair.expected_paired_plan(),
            }
            identity = {
                "binary": str(binary),
                "stage_binary": str(stage_binary),
                "stage_binary_sha256": stage_hash,
            }
            accepted = pair.validate_raw_report(
                json.dumps(report, separators=(",", ":")).encode(), identity, revision
            )
            self.assertEqual(len(accepted["raw_samples"]), 9)
            bad = json.loads(json.dumps(report))
            bad["raw_samples"][0]["output_sha256"] = "0" * 64
            with self.assertRaises(pair.PairError):
                pair.validate_raw_report(json.dumps(bad).encode(), identity, revision)

    def test_cargo_artifact_validator_rejects_wrong_kind(self) -> None:
        root = pair.BASELINE.resolve(strict=True)
        spec = pair.ARTIFACT_SPECS[pair.HARNESS_NAME]
        executable = Path(pair.FIXED_BUILD_ENV["CARGO_TARGET_DIR"]) / pair.CARGO_BUILD_TARGET / "release" / "deps" / "bench-test"
        artifact = {
            "reason": "compiler-artifact",
            "fresh": False,
            "target": {
                "name": spec["target_name"], "test": True,
                "kind": [spec["kind"]], "crate_types": [spec["crate_type"]],
                "src_path": str(root / spec["source"]),
            },
            "profile": {"test": True, "opt_level": "3", "debug_assertions": False},
            "manifest_path": str(root / "Cargo.toml"),
            "package_id": f"path+{root.as_uri()}#execute@0.9.0-rc.5",
            "executable": str(executable), "filenames": [str(executable)],
        }
        target_dir = Path(pair.FIXED_BUILD_ENV["CARGO_TARGET_DIR"])
        self.assertEqual(
            pair._validate_cargo_artifact(artifact, root, target_dir, pair.HARNESS_NAME),
            executable.resolve(strict=False),
        )
        malformed = json.loads(json.dumps(artifact))
        malformed["target"]["kind"] = ["lib"]
        with self.assertRaises(pair.PairError):
            pair._validate_cargo_artifact(malformed, root, target_dir, pair.HARNESS_NAME)

    def test_saved_library_test_copy_hash_is_bound_outside_shared_target(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            build_dir = Path(directory).resolve(strict=True)
            saved = build_dir / "candidate-line_framing_lib_test-test-binary"
            saved.write_bytes(b"immutable-test-binary")
            saved.chmod(0o755)
            item = {"saved_binary": str(saved), "saved_binary_sha256": pair.sha256_file(saved)}
            cargo_binary = Path(pair.FIXED_BUILD_ENV["CARGO_TARGET_DIR"]) / pair.CARGO_BUILD_TARGET / "release" / "deps" / "lib-test"
            actual, digest = pair._validate_saved_binary_receipt(
                item, "candidate", build_dir, cargo_binary, "line_framing_lib_test"
            )
            self.assertEqual(actual, saved)
            self.assertEqual(digest, item["saved_binary_sha256"])
            item["saved_binary_sha256"] = "0" * 64
            with self.assertRaises(pair.PairError):
                pair._validate_saved_binary_receipt(
                    item, "candidate", build_dir, cargo_binary, "line_framing_lib_test"
                )


if __name__ == "__main__":
    unittest.main()
