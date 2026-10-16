"""Offline contract tests for graph_recv_any_pair; no Rust benchmark is run."""
from __future__ import annotations

import json
from pathlib import Path
import sys
import subprocess
import tempfile
import unittest

import graph_recv_any_pair as runner


# Synthetic values exercise only the validator's schema and arithmetic. These
# are not benchmark observations and are never written as campaign evidence.
def synthetic_report_bytes() -> bytes:
    cases = []
    for source_count, input_count, case_name in runner.CASE_SPECS:
        samples = []
        for sample_index, duration in enumerate((100_000_000, 110_000_000, 120_000_000)):
            samples.append(
                {
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
            )
        cases.append(
            {
                "case": case_name,
                "p50_ns_per_event": 110_000_000 / runner.EVENTS_PER_CASE,
                "samples": samples,
            }
        )
    return json.dumps(
        {"format_version": 1, "harness": runner.BENCHMARK_NAME, "cases": cases},
        allow_nan=False,
    ).encode("utf-8")


class BenchmarkContractTests(unittest.TestCase):
    def test_valid_six_case_contract_returns_case_metrics(self) -> None:
        metrics = runner.validate_benchmark_json(synthetic_report_bytes())
        self.assertEqual(list(metrics), [name for _, _, name in runner.CASE_SPECS])
        self.assertAlmostEqual(
            metrics[runner.CASE_SPECS[0][2]], 110_000_000 / runner.EVENTS_PER_CASE
        )

    def test_missing_case_is_rejected(self) -> None:
        report = json.loads(synthetic_report_bytes())
        report["cases"].pop()
        with self.assertRaisesRegex(runner.BenchmarkError, "exactly the six"):
            runner.validate_benchmark_json(json.dumps(report).encode())

    def test_case_identity_count_and_checksum_mismatches_are_rejected(self) -> None:
        for key, value in (("case", "sources=9,inputs=1"), ("events", 1), ("checksum", 1)):
            with self.subTest(key=key):
                report = json.loads(synthetic_report_bytes())
                if key == "case":
                    report["cases"][0][key] = value
                else:
                    report["cases"][0]["samples"][0][key] = value
                with self.assertRaises(runner.BenchmarkError):
                    runner.validate_benchmark_json(json.dumps(report).encode())

    def test_noninteger_format_version_is_rejected(self) -> None:
        report = json.loads(synthetic_report_bytes())
        report["format_version"] = 1.0
        with self.assertRaisesRegex(runner.BenchmarkError, "integer 1"):
            runner.validate_benchmark_json(json.dumps(report).encode())

    def test_nonfinite_duplicate_and_malformed_json_are_rejected(self) -> None:
        with self.assertRaises(runner.BenchmarkError):
            runner.validate_benchmark_json(b'{"x": NaN}')
        with self.assertRaises(runner.BenchmarkError):
            runner.validate_benchmark_json(b'{"x": 1, "x": 2}')
        with self.assertRaises(runner.BenchmarkError):
            runner.validate_benchmark_json(b"not json")

    def test_pair_order_alternates_without_dropping_repetitions(self) -> None:
        self.assertEqual(
            [runner.interleaved_order(i) for i in range(6)],
            [
                ("baseline", "candidate"),
                ("candidate", "baseline"),
                ("baseline", "candidate"),
                ("candidate", "baseline"),
                ("baseline", "candidate"),
                ("candidate", "baseline"),
            ],
        )

    def test_paired_summary_keeps_positive_loss_deltas(self) -> None:
        case_names = [name for _, _, name in runner.CASE_SPECS]
        base = {
            "successful": True,
            "attempt_id": "base",
            "case_p50_ns_per_event": {name: 100.0 for name in case_names},
        }
        candidate = {
            "successful": True,
            "attempt_id": "candidate",
            "case_p50_ns_per_event": {name: 125.0 for name in case_names},
        }
        pair = runner._comparison(1, base, candidate)
        self.assertTrue(pair["valid"])
        for name in case_names:
            self.assertEqual(pair["cases"][name]["candidate_minus_baseline_ns_per_event"], 25.0)
        summary = runner._summary([pair])
        for name in case_names:
            self.assertEqual(summary[name]["candidate_losses"], 1)
            self.assertEqual(summary[name]["candidate_improvements"], 0)
            self.assertEqual(summary[name]["median_paired_delta_ns_per_event"], 25.0)

    def test_mismatched_hash_is_rejected(self) -> None:
        with self.assertRaisesRegex(runner.BenchmarkError, "SHA-256 mismatch"):
            runner.require_sha256("0" * 64, "1" * 64, "fixture")

    def test_harness_suffix_requires_one_marker_and_checks_its_digest(self) -> None:
        marker = runner.HARNESS_MARKER
        source = b"prefix\n" + marker + b"\nbody\n"
        suffix = runner.harness_source_suffix(source)
        self.assertEqual(suffix, marker + b"\nbody\n")
        expected = runner.sha256_bytes(suffix)
        self.assertEqual(runner.verify_harness_source(source, expected), expected)
        with self.assertRaisesRegex(runner.BenchmarkError, "SHA-256 mismatch"):
            runner.verify_harness_source(source, "0" * 64)
        with self.assertRaisesRegex(runner.BenchmarkError, "found 0"):
            runner.harness_source_suffix(b"no benchmark module here")
        with self.assertRaisesRegex(runner.BenchmarkError, "found 2"):
            runner.harness_source_suffix(source + marker)

    def test_source_fingerprint_includes_tracked_vendor_and_all_cargo_config(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            files = {
                "Cargo.toml": b"[package]\nname='fixture'\nversion='0.1.0'\n",
                "src/lib.rs": b"pub fn fixture() {}\n",
                ".cargo/config.toml": b"[build]\n",
                ".cargo/nested/target-config.toml": b"[target]\n",
                "vendor/iana-time-zone/src/lib.rs": b"pub fn vendored() {}\n",
                "docs/README.md": b"excluded documentation\n",
            }
            for relative, payload in files.items():
                path = root / relative
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(payload)
            subprocess.run(["git", "-C", str(root), "init", "-q"], check=True)
            subprocess.run(["git", "-C", str(root), "add", "--", *files], check=True)

            fingerprint = runner.source_fingerprint(root)
            self.assertEqual(
                set(fingerprint["files"]),
                {
                    "Cargo.toml",
                    "src/lib.rs",
                    ".cargo/config.toml",
                    ".cargo/nested/target-config.toml",
                    "vendor/iana-time-zone/src/lib.rs",
                },
            )
            self.assertEqual(
                fingerprint["files"]["vendor/iana-time-zone/src/lib.rs"],
                runner.sha256_file(root / "vendor/iana-time-zone/src/lib.rs"),
            )

    def test_cargo_artifact_selection_binds_fresh_optimized_test_to_checkout(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            manifest = root / "Cargo.toml"
            source = root / "src" / "lib.rs"
            target_dir = root / "target"
            binary = target_dir / "release" / "deps" / "fixture-test"
            manifest.write_text("[package]\n", encoding="utf-8")
            source.parent.mkdir(parents=True)
            source.write_text("", encoding="utf-8")
            binary.parent.mkdir(parents=True)
            binary.write_bytes(b"synthetic executable")
            binary.chmod(0o755)

            artifact = {
                "reason": "compiler-artifact",
                "manifest_path": str(manifest),
                "target": {"test": True, "kind": ["lib"], "src_path": str(source)},
                "profile": {"test": True, "opt_level": "3", "debug_assertions": False},
                "fresh": False,
                "executable": str(binary),
            }
            selected = runner.select_test_binary([artifact], manifest, source, target_dir)
            self.assertEqual(selected, binary.resolve())

            mismatches = (
                ("profile.test", {"profile": {"test": False, "opt_level": "3", "debug_assertions": False}}),
                ("profile.opt_level", {"profile": {"test": True, "opt_level": "0", "debug_assertions": False}}),
                ("profile.debug_assertions", {"profile": {"test": True, "opt_level": "3", "debug_assertions": True}}),
                ("fresh", {"fresh": True}),
                ("manifest_path", {"manifest_path": str(root / "other" / "Cargo.toml")}),
                ("target.src_path", {"target": {"test": True, "kind": ["lib"], "src_path": str(root / "other.rs")}}),
            )
            for label, changes in mismatches:
                with self.subTest(mismatch=label):
                    rejected = {**artifact, **changes}
                    with self.assertRaisesRegex(runner.BenchmarkError, "exactly one"):
                        runner.select_test_binary([rejected], manifest, source, target_dir)

            with self.assertRaisesRegex(runner.BenchmarkError, "exactly one"):
                runner.select_test_binary([artifact, artifact], manifest, source, target_dir)

    def test_failed_process_retains_started_finished_logs_and_invalid_raw_json(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            attempt_dir = Path(temporary) / "failed"
            code = (
                "import os,sys; "
                "open(os.environ['FLOW_RECV_ANY_BENCH_JSON'],'wb').write(b'{'); "
                "print('stdout retained'); print('stderr retained',file=sys.stderr); sys.exit(3)"
            )
            result = runner.run_attempt(
                [sys.executable, "-c", code],
                attempt_dir,
                {"attempt_id": "synthetic-failure", "side": "baseline"},
                5,
            )
            self.assertFalse(result["successful"])
            self.assertEqual(result["returncode"], 3)
            self.assertTrue((attempt_dir / "started.json").is_file())
            self.assertTrue((attempt_dir / "finished.json").is_file())
            self.assertEqual((attempt_dir / "stdout.bin").read_bytes(), b"stdout retained\n")
            self.assertIn(b"stderr retained", (attempt_dir / "stderr.bin").read_bytes())
            self.assertEqual((attempt_dir / "raw.json").read_bytes(), b"{")
            self.assertIsNotNone(result["validation_error"])

    def test_timeout_retains_started_finished_logs_and_partial_raw_json(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            attempt_dir = Path(temporary) / "timeout"
            code = (
                "import os,sys,time; "
                "open(os.environ['FLOW_RECV_ANY_BENCH_JSON'],'wb').write(b'{partial'); "
                "print('started',flush=True); print('waiting',file=sys.stderr,flush=True); time.sleep(5)"
            )
            result = runner.run_attempt(
                [sys.executable, "-c", code],
                attempt_dir,
                {"attempt_id": "synthetic-timeout", "side": "candidate"},
                1,
            )
            self.assertTrue(result["timed_out"])
            self.assertFalse(result["successful"])
            self.assertTrue((attempt_dir / "started.json").is_file())
            self.assertTrue((attempt_dir / "finished.json").is_file())
            self.assertTrue((attempt_dir / "raw.json").is_file())
            self.assertIn(b"started", (attempt_dir / "stdout.bin").read_bytes())
            self.assertIn(b"waiting", (attempt_dir / "stderr.bin").read_bytes())


if __name__ == "__main__":
    unittest.main()
