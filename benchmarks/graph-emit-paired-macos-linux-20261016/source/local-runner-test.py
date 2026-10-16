"""Offline contract tests; no Rust binaries or benchmark timings are used."""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

import emit_perf_pair as runner


def synthetic_report() -> dict:
    cases = []
    for case_index, spec in enumerate(runner.CASES):
        input_hash = hashlib.sha256(f"input-{case_index}".encode()).hexdigest()
        oracle = [
            {
                "sink": f"sink_{sink_index:02}.in",
                "event_count": spec["events"],
                "canonical_stream_sha256": hashlib.sha256(f"{case_index}-{sink_index}".encode()).hexdigest(),
                "representative_event": {"payload": {"synthetic": case_index}, "sequence": 1},
            }
            for sink_index in range(spec["sink_count"])
        ]
        samples = [
            {
                "repetition": repetition,
                "warmup": False,
                "duration_ns": 1_000_000 + repetition * 100_000,
                "input_payloads_sha256": input_hash,
                "delivered": spec["events"] * spec["sink_count"],
                "dropped": 0,
                "exact_correctness_oracle": json.loads(json.dumps(oracle)),
            }
            for repetition in range(runner.INNER_SAMPLES)
        ]
        cases.append(
            {
                **spec,
                "queue_policy": "lossless",
                "queue_capacity_per_sink": spec["events"],
                "queue_storage_preallocated": True,
                "input_payloads_sha256": input_hash,
                "samples_in_execution_order": samples,
            }
        )
    return {
        "format": runner.HARNESS_FORMAT,
        "method": "synthetic test fixture, never a benchmark observation",
        "warmup_repetitions": 0,
        "measured_repetitions": 3,
        "cases": cases,
    }


def report_bytes(report: dict | None = None) -> bytes:
    return json.dumps(report or synthetic_report(), allow_nan=False).encode("utf-8")


class EmitPairTests(unittest.TestCase):
    def test_valid_four_case_report_returns_medians_and_full_oracle(self) -> None:
        parsed = runner.validate_harness_report(report_bytes())
        self.assertEqual(list(parsed["case_median_duration_ns"]), [case["name"] for case in runner.CASES])
        self.assertEqual(parsed["case_median_duration_ns"][runner.CASES[0]["name"]], 1_100_000.0)
        self.assertEqual(len(parsed["oracle"][runner.CASES[1]["name"]]["sinks"]), 8)

    def test_report_rejects_wrong_case_sample_counts_and_bad_delivery(self) -> None:
        original = synthetic_report()
        mutations = []
        missing = json.loads(json.dumps(original))
        missing["cases"].pop()
        mutations.append(("case count", missing))
        wrong_samples = json.loads(json.dumps(original))
        wrong_samples["cases"][0]["samples_in_execution_order"].pop()
        mutations.append(("sample count", wrong_samples))
        dropped = json.loads(json.dumps(original))
        dropped["cases"][0]["samples_in_execution_order"][0]["dropped"] = 1
        mutations.append(("dropped", dropped))
        for label, mutated in mutations:
            with self.subTest(label=label), self.assertRaises(runner.PairError):
                runner.validate_harness_report(report_bytes(mutated))

    def test_report_rejects_inner_sample_oracle_drift_and_boolean_count(self) -> None:
        changed_oracle = synthetic_report()
        changed_oracle["cases"][0]["samples_in_execution_order"][1]["exact_correctness_oracle"][0][
            "canonical_stream_sha256"
        ] = "0" * 64
        with self.assertRaisesRegex(runner.PairError, "oracle differs"):
            runner.validate_harness_report(report_bytes(changed_oracle))
        bool_count = synthetic_report()
        bool_count["cases"][0]["sink_count"] = True
        with self.assertRaisesRegex(runner.PairError, "unexpected sink_count"):
            runner.validate_harness_report(report_bytes(bool_count))

    def test_duplicate_nonfinite_and_malformed_json_are_rejected(self) -> None:
        for payload in (b'{"x":1,"x":2}', b'{"x":NaN}', b"not json"):
            with self.subTest(payload=payload), self.assertRaises(runner.PairError):
                runner.validate_harness_report(payload)

    def test_pair_order_balances_warmups_and_measured_pairs(self) -> None:
        order = [runner.interleaved_order(index) for index in range(runner.WARMUP_PAIRS + runner.MEASURED_PAIRS)]
        self.assertEqual(order[:3], [("baseline", "candidate"), ("candidate", "baseline"), ("baseline", "candidate")])
        measured = order[runner.WARMUP_PAIRS:]
        self.assertEqual(measured[0], ("candidate", "baseline"))
        self.assertEqual(measured.count(("baseline", "candidate")), 24)
        self.assertEqual(measured.count(("candidate", "baseline")), 24)
        self.assertEqual(runner.MEASURED_PAIRS, 48)

    def test_pair_rejects_oracle_mismatch_and_keeps_signed_losses(self) -> None:
        parsed = runner.validate_harness_report(report_bytes())
        baseline = {
            "attempt_id": "baseline",
            "successful": True,
            "oracle": parsed["oracle"],
            "case_median_duration_ns": parsed["case_median_duration_ns"],
        }
        candidate = {
            "attempt_id": "candidate",
            "successful": True,
            "oracle": json.loads(json.dumps(parsed["oracle"])),
            "case_median_duration_ns": {name: value + 50 for name, value in parsed["case_median_duration_ns"].items()},
        }
        pair = runner._pair_result(1, baseline, candidate)
        self.assertTrue(pair["valid_for_timing_comparison"])
        self.assertTrue(pair["full_input_and_oracle_equal"])
        self.assertEqual(pair["cases"][runner.CASES[0]["name"]]["candidate_minus_baseline_ns"], 50.0)
        summary = runner._summary([pair])
        self.assertEqual(summary[runner.CASES[0]["name"]]["candidate_losses"], 1)

        candidate["oracle"][runner.CASES[0]["name"]]["sinks"][0]["canonical_stream_sha256"] = "f" * 64
        rejected = runner._pair_result(1, baseline, candidate)
        self.assertFalse(rejected["valid_for_timing_comparison"])
        self.assertFalse(rejected["full_input_and_oracle_equal"])
        self.assertEqual(rejected["failure_reasons"]["oracle_mismatch"], True)

    def test_source_fingerprint_covers_vendor_and_nested_cargo_inputs(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            contents = {
                "Cargo.toml": b"[package]\nname='fixture'\nversion='0.1.0'\n",
                "src/lib.rs": b"pub fn fixture() {}\n",
                ".cargo/config.toml": b"[build]\n",
                ".cargo/nested/config.toml": b"[target]\n",
                "vendor/iana-time-zone/src/lib.rs": b"pub fn vendored() {}\n",
                "docs/readme.md": b"not a build input\n",
            }
            _make_repo(root, contents)
            fingerprint = runner.source_fingerprint(root)
            self.assertEqual(
                set(fingerprint["files"]),
                {"Cargo.toml", "src/lib.rs", ".cargo/config.toml", ".cargo/nested/config.toml", "vendor/iana-time-zone/src/lib.rs"},
            )

    def test_prepare_freezes_receipts_harness_and_source_before_any_run(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            baseline_root, candidate_root, base_commit, harness = _paired_source_roots(root)
            harness_file = root / "emit-perf-harness.rs"
            harness_file.write_bytes(harness)
            source_freeze = {
                "base_commit": base_commit,
                "identical_harness": True,
                "harness_sha256": runner.sha256_bytes(harness),
                "source_paths": {
                    str((arm / "src/graph/transport.rs").resolve()): runner.sha256_file(arm / "src/graph/transport.rs")
                    for arm in (baseline_root, candidate_root)
                },
            }
            freeze_file = root / "source-freeze.json"
            freeze_file.write_text(json.dumps(source_freeze), encoding="utf-8")
            baseline_receipt = _receipt(root, "baseline", baseline_root, base_commit, source_freeze)
            candidate_receipt = _receipt(root, "candidate", candidate_root, base_commit, source_freeze)
            prereg_path = root / "preregistration.json"
            args = argparse.Namespace(
                source_freeze=str(freeze_file),
                baseline_receipt=str(baseline_receipt),
                candidate_receipt=str(candidate_receipt),
                harness_source=str(harness_file),
                preregistration=str(prereg_path),
            )
            with patch.object(runner, "HARNESS_SHA256", runner.sha256_bytes(harness)):
                runner.prepare(args)
                prereg = runner._verify_preregistration(prereg_path)
            self.assertEqual(prereg["protocol"]["measured_pairs"], 48)
            self.assertEqual(prereg["inputs"]["baseline"]["binary_sha256"], json.loads(baseline_receipt.read_text())["sha256"])
            self.assertEqual(prereg["inputs"]["candidate"]["binary_sha256"], json.loads(candidate_receipt.read_text())["sha256"])

    def test_prepare_verification_rejects_changed_source_after_preregistration(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            baseline_root, candidate_root, base_commit, harness = _paired_source_roots(root)
            harness_file = root / "emit-perf-harness.rs"
            harness_file.write_bytes(harness)
            freeze = {
                "base_commit": base_commit,
                "identical_harness": True,
                "harness_sha256": runner.sha256_bytes(harness),
                "source_paths": {
                    str((arm / "src/graph/transport.rs").resolve()): runner.sha256_file(arm / "src/graph/transport.rs")
                    for arm in (baseline_root, candidate_root)
                },
            }
            freeze_file = root / "source-freeze.json"
            freeze_file.write_text(json.dumps(freeze), encoding="utf-8")
            baseline_receipt = _receipt(root, "baseline", baseline_root, base_commit, freeze)
            candidate_receipt = _receipt(root, "candidate", candidate_root, base_commit, freeze)
            prereg_path = root / "preregistration.json"
            args = argparse.Namespace(
                source_freeze=str(freeze_file),
                baseline_receipt=str(baseline_receipt),
                candidate_receipt=str(candidate_receipt),
                harness_source=str(harness_file),
                preregistration=str(prereg_path),
            )
            with patch.object(runner, "HARNESS_SHA256", runner.sha256_bytes(harness)):
                runner.prepare(args)
                with (candidate_root / "src/graph/transport.rs").open("ab") as handle:
                    handle.write(b"changed after freeze\n")
                with self.assertRaisesRegex(runner.PairError, "transport source hash mismatch"):
                    runner._verify_preregistration(prereg_path)

    def test_run_attempt_retains_synthetic_report_logs_and_process_cpu_wall(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            executable = _fake_executable(root, report_bytes())
            result = runner.run_attempt(
                {"binary": str(executable), "binary_sha256": runner.sha256_file(executable)},
                root / "attempt",
                {"attempt_id": "synthetic", "side": "baseline"},
            )
            self.assertTrue(result["successful"])
            self.assertGreater(result["elapsed_wall_ns"], 0)
            self.assertIn("user_seconds", result["child_cpu_seconds"])
            self.assertTrue((root / "attempt/started.json").is_file())
            self.assertTrue((root / "attempt/finished.json").is_file())
            self.assertTrue((root / "attempt/raw.json").is_file())

    def test_timeout_retains_partial_raw_and_logs_without_retry(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            executable = root / "fake-test"
            executable.write_text(
                f"#!{sys.executable}\nimport os,time\nopen(os.environ['EXECUTE_EMIT_PERF_REPORT'],'wb').write(b'{{')\nprint('started',flush=True)\ntime.sleep(3)\n",
                encoding="utf-8",
            )
            executable.chmod(0o755)
            with patch.object(runner, "INVOCATION_TIMEOUT_SECONDS", 1):
                result = runner.run_attempt(
                    {"binary": str(executable), "binary_sha256": runner.sha256_file(executable)},
                    root / "timeout",
                    {"attempt_id": "timeout", "side": "candidate"},
                )
            self.assertTrue(result["timed_out"])
            self.assertFalse(result["successful"])
            self.assertTrue((root / "timeout/started.json").is_file())
            self.assertTrue((root / "timeout/finished.json").is_file())
            self.assertEqual((root / "timeout/raw.json").read_bytes(), b"{")


def _make_repo(root: Path, contents: dict[str, bytes]) -> None:
    root.mkdir(parents=True, exist_ok=True)
    for relative, payload in contents.items():
        path = root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(payload)
    subprocess.run(["git", "-C", str(root), "init", "-q"], check=True)
    subprocess.run(["git", "-C", str(root), "add", "--", *contents], check=True)
    subprocess.run(
        ["git", "-C", str(root), "-c", "user.name=Offline Test", "-c", "user.email=offline@example.invalid", "commit", "-q", "-m", "fixture"],
        check=True,
    )


def _paired_source_roots(parent: Path) -> tuple[Path, Path, str, bytes]:
    base = parent / "base"
    harness = b"#[cfg(test)]\nmod frozen_emit_harness { }\n"
    _make_repo(
        base,
        {
            "Cargo.toml": b"[package]\nname='fixture'\nversion='0.1.0'\n",
            "src/lib.rs": b"pub fn fixture() {}\n",
            "src/graph/transport.rs": b"base transport\n",
        },
    )
    base_commit = subprocess.run(
        ["git", "-C", str(base), "rev-parse", "HEAD"], check=True, capture_output=True, text=True
    ).stdout.strip()
    roots = [parent / "baseline", parent / "candidate"]
    for root, arm in zip(roots, (b"baseline change\n", b"candidate change\n")):
        subprocess.run(["git", "clone", "-q", str(base), str(root)], check=True)
        transport = root / "src/graph/transport.rs"
        transport.write_bytes(transport.read_bytes() + arm + harness)
    return roots[0], roots[1], base_commit, harness


def _receipt(parent: Path, side: str, root: Path, commit: str, freeze: dict) -> Path:
    binary = parent / f"{side}-test-binary"
    binary.write_bytes(f"synthetic {side} test binary".encode())
    binary.chmod(0o755)
    transport = root / "src/graph/transport.rs"
    receipt = {
        "artifact": {
            "reason": "compiler-artifact",
            "manifest_path": str(root / "Cargo.toml"),
            "target": {"test": True, "kind": ["lib"], "src_path": str(root / "src/lib.rs")},
            "profile": {"test": True, "opt_level": "3", "debug_assertions": False},
            "fresh": False,
        },
        "saved_binary": str(binary),
        "sha256": runner.sha256_file(binary),
        "source_sha256": runner.sha256_file(transport),
    }
    path = parent / f"{side}-receipt.json"
    path.write_text(json.dumps(receipt), encoding="utf-8")
    return path


def _fake_executable(root: Path, payload: bytes) -> Path:
    executable = root / "fake-test"
    body = repr(payload.decode("utf-8"))
    executable.write_text(
        f"#!{sys.executable}\nimport os\nfrom pathlib import Path\nPath(os.environ['EXECUTE_EMIT_PERF_REPORT']).write_bytes({body}.encode())\nprint('synthetic test invocation')\n",
        encoding="utf-8",
    )
    executable.chmod(0o755)
    return executable


if __name__ == "__main__":
    unittest.main()
