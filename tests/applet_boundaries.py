#!/usr/bin/env python3
"""Unit tests for the non-measuring applet-boundary report wrapper."""
import argparse
from pathlib import Path
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))

from benchmarks import applet_boundaries as harness


class AppletBoundaryHarnessTests(unittest.TestCase):
    def test_nearest_rank_summary_retains_sample_count_and_extrema(self):
        self.assertEqual(
            harness.summarize([9, 1, 8, 2, 3, 4, 5, 6, 7, 10]),
            {
                "sample_count": 10,
                "median": 5.5,
                "p95_nearest_rank": 10,
                "minimum": 1,
                "maximum": 10,
            },
        )

    def test_report_destination_requires_fresh_named_file_in_benchmarks(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            bench_dir = root / "benchmarks"
            bench_dir.mkdir()
            expected = bench_dir / "applet-boundaries-final.json"
            self.assertEqual(harness.validate_report_path(expected, bench_dir), expected)
            expected.write_text("already retained", encoding="utf-8")
            with self.assertRaises(FileExistsError):
                harness.validate_report_path(expected, bench_dir)
            with self.assertRaises(ValueError):
                harness.validate_report_path(bench_dir / "other.json", bench_dir)
            with self.assertRaises(ValueError):
                harness.validate_report_path(root / "applet-boundaries-outside.json", bench_dir)

    def test_raw_samples_require_all_backends_and_correctness_oracles(self):
        raw = {
            "schema": "execute.applet-boundaries.raw/1",
            "iterations": {
                "samples": 2,
                "warmups": 1,
                "interleaved_order": [
                    ["command", "in_process", "json_lines_worker"],
                    ["in_process", "json_lines_worker", "command"],
                    ["json_lines_worker", "command", "in_process"],
                ],
            },
            "backends": {
                name: {
                    "samples_ns": [10, 11],
                    "launches_timed_calls_total": {
                        "command": 3,
                        "in_process": 0,
                        "json_lines_worker": 1,
                    }[name],
                    "timed_calls_including_warmups": 3,
                    "launches_in_timed_sample_window": 2 if name == "command" else 0,
                }
                for name in ("command", "in_process", "json_lines_worker")
            },
            "launches": {
                "success_correctness_call_by_backend": {
                    "command": 1,
                    "in_process": 0,
                    "json_lines_worker": 1,
                },
                "absence_envelope_correctness_call_by_backend": {
                    "command": 1,
                    "in_process": 0,
                    "json_lines_worker": 1,
                },
                "timed_calls_and_worker_launches_by_backend": {
                    "command": 3,
                    "in_process": 0,
                    "json_lines_worker": 1,
                },
            },
            "correctness": {
                "success_backend_checks": ["command", "in_process", "json_lines_worker"],
                "absence_envelope_backends": ["command", "in_process", "json_lines_worker"],
                "absence_envelope_expected_value": {
                    "value": None,
                    "outcome": "absence",
                    "kind": "absence",
                    "effect_certainty": "known_response",
                    "status": 1,
                },
            },
            "worker_serialization": {
                "request_encode_samples_ns": [3.0, 3.5],
                "response_decode_samples_ns": [4.0, 4.5],
            },
        }
        harness._validate_raw_report(raw, samples=2, warmups=1)
        raw["launches"]["timed_calls_and_worker_launches_by_backend"]["json_lines_worker"] = 0
        with self.assertRaisesRegex(ValueError, "process launch counts"):
            harness._validate_raw_report(raw, samples=2, warmups=1)
        raw["launches"]["timed_calls_and_worker_launches_by_backend"]["json_lines_worker"] = 1
        raw["correctness"]["absence_envelope_backends"] = ["command"]
        with self.assertRaisesRegex(ValueError, "correctness checks"):
            harness._validate_raw_report(raw, samples=2, warmups=1)
        raw["correctness"]["absence_envelope_backends"] = ["command", "in_process", "json_lines_worker"]
        raw["correctness"]["absence_envelope_expected_value"].pop("effect_certainty")
        with self.assertRaisesRegex(ValueError, "complete absence result envelope"):
            harness._validate_raw_report(raw, samples=2, warmups=1)

    def test_source_digest_is_order_independent_and_names_files(self):
        first = {"src/a.rs": "a" * 64, "src/b.rs": "b" * 64}
        self.assertEqual(
            harness.combined_source_hash(first),
            harness.combined_source_hash(dict(reversed(list(first.items())))),
        )
        self.assertNotEqual(
            harness.combined_source_hash(first),
            harness.combined_source_hash({"src/c.rs": "a" * 64, "src/b.rs": "b" * 64}),
        )

    def test_measurement_requires_final_build_and_idle_window_confirmation(self):
        args = argparse.Namespace(
            final_release_build_confirmed=False,
            measurement_window_idle=False,
        )
        with self.assertRaisesRegex(ValueError, "final release build"):
            harness.run_benchmark(args)
        args.final_release_build_confirmed = True
        with self.assertRaisesRegex(ValueError, "builds and tests have stopped"):
            harness.run_benchmark(args)


if __name__ == "__main__":
    unittest.main(verbosity=2)
