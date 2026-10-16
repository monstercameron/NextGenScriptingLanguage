from __future__ import annotations

import hashlib
import json
from pathlib import Path
import sys
import tempfile
import unittest
from types import SimpleNamespace
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
import graph_recv_any_e2e_pair as pair
sys.path.insert(0, str(pair.CAMPAIGN / "work"))
import build_graph_recv_any_e2e_pair as build_pair


FIXTURE_HASH = "a" * 64


def make_event(port: dict[str, str], sequence: int) -> dict[str, object]:
    return {
        "port": port,
        "input": None,
        "generation": pair.GENERATION,
        "revision": pair.REVISION,
        "sequence": sequence,
        "sensitivity": ["unknown"],
        "remote": None,
    }


def make_verification(record_count: int = 3, sequence_offset: int = 0) -> dict[str, object]:
    source_sequences = [10 + sequence_offset + 10 * index for index in range(record_count)]
    consumers = []
    next_output = 10 + sequence_offset + 10 * record_count
    for consumer_index in range(2):
        consumer = f"consumer-{consumer_index}"
        inputs = [
            {
                "sequence": sequence,
                "generation": pair.GENERATION,
                "revision": pair.REVISION,
                "source_port_matches": True,
                "input_port_matches": True,
                "sensitivity_unknown_only": True,
                "remote_absent": True,
            }
            for sequence in source_sequences
        ]
        input_hash = hashlib.sha256(
            b"".join(sequence.to_bytes(8, "little") for sequence in source_sequences)
        ).hexdigest()
        outputs = []
        for event_index, source_sequence in enumerate(source_sequences):
            sequence = next_output + 2 * event_index + consumer_index
            if sequence <= source_sequence:
                sequence = source_sequence + record_count * 10 + consumer_index + 1
            outputs.append(make_event({"node": consumer, "port": "stdout"}, sequence))
        consumers.append(
            {
                "consumer": consumer,
                "input_event_count": record_count,
                "input_sequence_sha256": input_hash,
                "input_source_port": {"node": "producer", "port": "stdout"},
                "input_target_port": {"node": consumer, "port": "events"},
                "input_sensitivity": ["unknown"],
                "input_remote_metadata": None,
                "input_event_identities": inputs,
                "output_event_count": record_count,
                "output_payload_sha256": FIXTURE_HASH,
                "output_event_identities": outputs,
            }
        )
    max_sequence = max(
        [*source_sequences]
        + [event["sequence"] for consumer in consumers for event in consumer["output_event_identities"]]
    )
    return {
        "terminal_event_count": record_count * 2 + 1,
        "source_status_event_count": 1,
        "source_status_identity": make_event({"node": "producer", "port": "status"}, max_sequence + 1),
        "consumers": consumers,
    }


def make_build_metadata(target_dir: str) -> dict[str, object]:
    return {
        "rustc_vv": "rustc 1.99.0 test",
        "cargo_version": "cargo 1.99.0 test",
        "selected_build_environment": {
            "PATH": "/toolchain/bin",
            "CARGO_HOME": "/cargo",
            "RUSTUP_HOME": "/rustup",
            "RUSTUP_TOOLCHAIN": "stable-aarch64-apple-darwin",
            "CARGO_BUILD_JOBS": "4",
            "CARGO_TARGET_DIR": target_dir,
            "RUSTFLAGS": "-C opt-level=3",
        },
        "captured_before_clean_and_build": True,
    }


class GraphRecvAnyPairTests(unittest.TestCase):
    def test_frozen_harness_and_composed_source_roots_match_pins(self) -> None:
        harness = (pair.CAMPAIGN / "evidence/graph-recv-any-single-queue/graph_recv_any_e2e_bench.rs").read_bytes()
        self.assertEqual(pair.sha256_bytes(harness), pair.HARNESS_SHA256)
        source = harness.decode("utf-8")
        self.assertIn(f'"format": "{pair.REPORT_FORMAT}"', source)
        self.assertIn(f'"method": "{pair.METHOD}"', source)

        baseline = pair._inspect_source(pair.BASELINE_ROOT, "baseline", harness)
        candidate = pair._inspect_source(pair.CANDIDATE_ROOT, "candidate", harness)
        pair._validate_source_pair(baseline, candidate)

    def test_build_driver_preserves_porcelain_spacing_for_prepared_worktrees(self) -> None:
        self.assertEqual(
            build_pair.verify_source("baseline", build_pair.BASELINE_ROOT),
            {"commit": pair.BASELINE_COMMIT, "transport_sha256": pair.EXPECTED_TRANSPORT_SHA256["baseline"]},
        )
        self.assertEqual(
            build_pair.verify_source("candidate", build_pair.CANDIDATE_ROOT),
            {"commit": pair.BASELINE_COMMIT, "transport_sha256": pair.EXPECTED_TRANSPORT_SHA256["candidate"]},
        )

    def test_fixture_and_schedule_are_frozen(self) -> None:
        fixture = pair.expected_fixture_bytes()
        self.assertEqual(fixture.count(b"\n"), pair.RECORD_COUNT)
        self.assertEqual(len(fixture), sum(len(line) + 1 for line in fixture.splitlines()))
        self.assertEqual(pair.pair_order(0), ("baseline", "candidate"))
        self.assertEqual(pair.pair_order(1), ("candidate", "baseline"))
        self.assertEqual(pair.pair_order(pair.WARMUP_PAIRS), ("candidate", "baseline"))
        self.assertEqual(pair.TOTAL_ATTEMPTS, 102)
        self.assertEqual(pair.MIN_FREE_BYTES, 3 * 1024**3)

    def test_build_metadata_requires_shared_target_directory(self) -> None:
        shared_target = str(pair.SHARED_TARGET_DIR)
        baseline = make_build_metadata(shared_target)
        candidate = make_build_metadata(shared_target)
        pair._validate_build_metadata_pair(baseline, candidate, baseline)

        separate_target = make_build_metadata("/build/candidate")
        with self.assertRaisesRegex(pair.PairError, "CARGO_TARGET_DIR differs from the frozen shared target path"):
            pair._validate_build_metadata_pair(baseline, separate_target, baseline)

        changed_toolchain = make_build_metadata(shared_target)
        changed_toolchain["cargo_version"] = "cargo 1.98.0 test"
        with self.assertRaisesRegex(pair.PairError, "toolchain or selected build environment differ"):
            pair._validate_build_metadata_pair(baseline, changed_toolchain, baseline)

        changed_environment = make_build_metadata(shared_target)
        changed_environment["selected_build_environment"]["RUSTFLAGS"] = "-C opt-level=2"
        with self.assertRaisesRegex(pair.PairError, "toolchain or selected build environment differ"):
            pair._validate_build_metadata_pair(baseline, changed_environment, baseline)

    def test_saved_binary_survives_cleaned_reusable_cargo_artifact_path(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source_root = root / "source"
            (source_root / "tests").mkdir(parents=True)
            (source_root / "Cargo.toml").write_text("[package]\nname='test'\n")
            harness = source_root / pair.HARNESS_RELATIVE
            harness.parent.mkdir(parents=True, exist_ok=True)
            harness.write_text("// frozen test\n")

            target_dir = root / "target"
            artifact_path = target_dir / "release" / "deps" / "graph_recv_any_e2e_bench-reused"
            artifact_path.parent.mkdir(parents=True)
            artifact_path.write_bytes(b"candidate later overwrites this Cargo path")
            saved_binary = root / "baseline-test-binary"
            saved_binary.write_bytes(b"immutable baseline test binary")
            saved_binary.chmod(0o755)

            artifact = {
                "reason": "compiler-artifact",
                "manifest_path": str(source_root / "Cargo.toml"),
                "target": {
                    "name": "graph_recv_any_e2e_bench",
                    "kind": ["test"],
                    "test": True,
                    "src_path": str(harness),
                },
                "profile": {"test": True, "opt_level": "3", "debug_assertions": False},
                "executable": str(artifact_path),
                "filenames": [str(artifact_path)],
                "fresh": False,
            }
            build_log = root / "build.jsonl"
            build_log.write_text(json.dumps(artifact) + "\n")
            metadata = make_build_metadata(str(target_dir))

            artifact_path.unlink()  # The next arm's cargo clean removes the shared target output.
            pinned_artifact = pair._artifact_from_build_log(build_log, "baseline")
            pair._validate_artifact_receipt(
                "baseline",
                artifact,
                pinned_artifact,
                metadata,
                source_root.resolve(),
                saved_binary,
                hashlib.sha256(saved_binary.read_bytes()).hexdigest(),
            )
            self.assertEqual(saved_binary.read_bytes(), b"immutable baseline test binary")
            changed_row = {**artifact, "fresh": True}
            with self.assertRaisesRegex(pair.PairError, "differs from the pinned Cargo build JSONL row"):
                pair._validate_artifact_receipt(
                    "baseline",
                    artifact,
                    changed_row,
                    metadata,
                    source_root.resolve(),
                    saved_binary,
                    hashlib.sha256(saved_binary.read_bytes()).hexdigest(),
                )

    def test_build_driver_accepts_only_fresh_optimized_harness_artifact(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary) / "source"
            (root / "tests").mkdir(parents=True)
            (root / "Cargo.toml").write_text("[package]\nname='execute'\n")
            harness = root / build_pair.HARNESS_RELATIVE
            harness.parent.mkdir(parents=True, exist_ok=True)
            harness.write_text("// frozen test\n")

            target_directory = Path(temporary) / "shared-target"
            executable = target_directory / "release" / "deps" / "graph_recv_any_e2e_bench-test"
            executable.parent.mkdir(parents=True)
            executable.write_bytes(b"test binary")
            original_target = build_pair.TARGET
            build_pair.TARGET = target_directory
            try:
                artifact = {
                    "reason": "compiler-artifact",
                    "manifest_path": str(root / "Cargo.toml"),
                    "target": {
                        "name": build_pair.TARGET_NAME,
                        "kind": ["test"],
                        "test": True,
                        "src_path": str(harness),
                    },
                    "profile": {"test": True, "opt_level": "3", "debug_assertions": False},
                    "executable": str(executable),
                    "filenames": [str(executable)],
                    "fresh": False,
                }
                self.assertEqual(build_pair.artifact_for([artifact], root.resolve(strict=True)), artifact)
                stale_artifact = {**artifact, "fresh": True}
                with self.assertRaisesRegex(RuntimeError, "not the exact fresh optimized"):
                    build_pair.artifact_for([stale_artifact], root.resolve(strict=True))
            finally:
                build_pair.TARGET = original_target

    def test_full_sequence_oracle_normalizes_only_global_sequences(self) -> None:
        left = pair.validate_verification(make_verification(), FIXTURE_HASH, record_count=3)
        right = pair.validate_verification(make_verification(sequence_offset=1000), FIXTURE_HASH, record_count=3)
        self.assertEqual(left, right)

    def test_oracle_rejects_reordered_events_and_wrong_payload_hash(self) -> None:
        reordered = make_verification()
        consumer = reordered["consumers"][0]
        consumer["output_event_identities"][1]["sequence"] = consumer["output_event_identities"][0]["sequence"] - 1
        with self.assertRaisesRegex(pair.PairError, "output order"):
            pair.validate_verification(reordered, FIXTURE_HASH, record_count=3)

        changed_payload = make_verification()
        changed_payload["consumers"][1]["output_payload_sha256"] = "b" * 64
        with self.assertRaisesRegex(pair.PairError, "payload checksum"):
            pair.validate_verification(changed_payload, FIXTURE_HASH, record_count=3)

    def test_strict_json_rejects_duplicate_keys(self) -> None:
        with self.assertRaisesRegex(pair.PairError, "duplicate JSON key"):
            pair.parse_json(b'{"duration_ns":1,"duration_ns":2}', "sample")

    def test_journal_requires_preparation_and_each_successful_raw_sample(self) -> None:
        samples = [
            {"duration_ns": 100 + index, "evidence": {"verification": {"sample": index}}}
            for index in range(pair.INNER_SAMPLES)
        ]
        rows = [
            {
                "status": "prepared",
                "planned_warmups": 0,
                "planned_samples": pair.INNER_SAMPLES,
                "record_count": pair.RECORD_COUNT,
                "input_fixture_sha256": FIXTURE_HASH,
            }
        ]
        for index, sample in enumerate(samples):
            rows.append(
                {
                    "phase": "sample",
                    "index": index,
                    "status": "ok",
                    "duration_ns": sample["duration_ns"],
                    "dropped_messages": 0,
                    "launched_processes": 1,
                    "input_fixture_sha256": FIXTURE_HASH,
                    "verification_sha256": pair.sha256_bytes(pair.canonical_json(sample["evidence"]["verification"])),
                }
            )
        payload = b"".join(pair.canonical_json(row) + b"\n" for row in rows)
        result = pair.validate_journal(payload, samples, FIXTURE_HASH)
        self.assertEqual(result["row_count"], 4)

        failed = list(rows)
        failed[1] = {"phase": "sample", "index": 0, "status": "runtime_failed", "duration_ns": 5, "error": "cancelled"}
        with self.assertRaisesRegex(pair.PairError, "unexpected schema"):
            pair.validate_journal(b"".join(pair.canonical_json(row) + b"\n" for row in failed), samples, FIXTURE_HASH)

    def test_timeout_is_preserved_as_one_failed_invocation_without_retry(self) -> None:
        identity = {
            "side": "baseline",
            "saved_binary": "/tmp/test-binary",
            "root": "/tmp/source",
            "receipt_path": "/tmp/receipt.json",
            "harness_path": "/tmp/harness.rs",
            "commit": pair.BASELINE_COMMIT,
            "transport_sha256": "c" * 64,
            "source_fingerprint": {"sha256": "d" * 64, "files": {}},
            "binary_sha256": "e" * 64,
            "receipt_sha256": "f" * 64,
        }
        with tempfile.TemporaryDirectory() as temporary:
            attempt_dir = Path(temporary) / "timeout-attempt"
            with (
                mock.patch.object(pair, "_snapshot", side_effect=[{"stable": True}, {"stable": True}]),
                mock.patch.object(pair.subprocess, "run", side_effect=pair.subprocess.TimeoutExpired("test", 120)) as run_mock,
            ):
                result = pair.run_attempt(identity, attempt_dir, {"attempt_id": "timeout-attempt"})
            self.assertEqual(run_mock.call_count, 1)
            self.assertEqual(run_mock.call_args.kwargs["timeout"], 120)
            self.assertFalse(result["successful"])
            self.assertTrue(result["timed_out"])
            self.assertTrue((attempt_dir / "stdout.bin").is_file())
            self.assertTrue((attempt_dir / "stderr.bin").is_file())
            self.assertTrue((attempt_dir / "finished.json").is_file())

    def test_main_preserves_nonzero_run_result(self) -> None:
        parser = mock.Mock()
        parser.parse_args.return_value = SimpleNamespace(function=lambda _args: 1)
        with mock.patch.object(pair, "_parser", return_value=parser):
            self.assertEqual(pair.main([]), 1)

    def test_preflight_rejects_low_free_space(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            usage = SimpleNamespace(total=10_000, used=9_000, free=1_000)
            with mock.patch.object(pair.shutil, "disk_usage", return_value=usage):
                with self.assertRaisesRegex(pair.PairError, "requires"):
                    pair.check_disk_space(Path(temporary))


if __name__ == "__main__":
    unittest.main()
