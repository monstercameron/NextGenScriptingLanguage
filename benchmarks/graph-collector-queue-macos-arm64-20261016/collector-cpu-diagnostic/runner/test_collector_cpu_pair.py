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
import collector_cpu_pair as pair


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
    config_files = [
        {
            "path": f"/tmp/cpu-diag-config-{index}",
            "present": False,
            "resolved_path": None,
            "sha256": None,
            "symlink_target": None,
        }
        for index in range(2)
    ]
    config_fingerprint_body = {
        "schema": pair.CONFIG_FINGERPRINT_SCHEMA,
        "cargo_home": {
            "path": pair.FIXED_BUILD_ENVIRONMENT["CARGO_HOME"],
            "config_files": config_files,
        },
        "source_roots": {
            side: {
                "root": f"/tmp/{side}",
                "home_is_ancestor": False,
                "config_files": config_files,
            }
            for side in ("baseline", "candidate")
        },
    }
    config_fingerprint = {
        **config_fingerprint_body,
        "sha256": pair.sha256_bytes(pair.canonical_json(config_fingerprint_body)),
    }
    return {
        "rustc_vv": "rustc 1.99.0 test",
        "cargo_version": "cargo 1.99.0 test",
        "selected_build_environment": {
            "PATH": "/Users/earlcameron/Documents/Codex/2026-10-12/you/work/rust/cargo/bin:/usr/bin",
            "HOME": "/Users/earlcameron",
            "TMPDIR": "/tmp",
            "CARGO_HOME": "/Users/earlcameron/Documents/Codex/2026-10-12/you/work/rust/cargo",
            "RUSTUP_HOME": "/Users/earlcameron/Documents/Codex/2026-10-12/you/work/rust/rustup",
            "RUSTUP_TOOLCHAIN": "stable-aarch64-apple-darwin",
            "CARGO_BUILD_JOBS": "4",
            "CARGO_TARGET_DIR": target_dir,
        },
        "cargo_configuration_fingerprint": config_fingerprint,
        "captured_before_clean_and_build": True,
        "build_driver_sha256": "a" * 64,
    }


class CollectorCpuPairTests(unittest.TestCase):
    def test_fixture_and_schedule_are_frozen(self) -> None:
        fixture = pair.expected_fixture_bytes()
        self.assertEqual(fixture.count(b"\n"), pair.RECORD_COUNT)
        self.assertEqual(len(fixture), sum(len(line) + 1 for line in fixture.splitlines()))
        self.assertEqual(pair.pair_order(0), ("baseline", "candidate"))
        self.assertEqual(pair.pair_order(1), ("candidate", "baseline"))
        self.assertEqual(pair.pair_order(pair.WARMUP_PAIRS), ("candidate", "baseline"))
        self.assertEqual(pair.TOTAL_ATTEMPTS, 30)
        self.assertEqual(pair.MEASURED_PAIRS, 12)
        self.assertEqual(pair.MIN_FREE_BYTES, 512 * 1024**2)
        schedule = pair._protocol()["schedule"]
        self.assertEqual(len(schedule), 15)
        self.assertEqual(sum(item["phase"] == "warmup" for item in schedule), 3)
        self.assertEqual(sum(item["phase"] == "measurement" for item in schedule), 12)
        self.assertEqual(schedule[3]["order"], ["candidate", "baseline"])
        self.assertEqual(len({attempt_id for item in schedule for attempt_id in item["attempt_ids"]}), 30)

    def test_frozen_protocol_pins_controlled_build_provenance_sources(self) -> None:
        source = pair._frozen_protocol_source()
        self.assertEqual(source["path"], str((pair.EVIDENCE / "paired-protocol.json").resolve()))
        self.assertRegex(source["sha256"], r"^[0-9a-f]{64}$")

    def test_build_metadata_matches_except_target_directory(self) -> None:
        fixed_target = str(pair.CAMPAIGN / "work/perf-target-candidate")
        baseline = make_build_metadata(fixed_target)
        candidate = make_build_metadata(fixed_target)
        pair._validate_build_metadata_pair(baseline, candidate, baseline)

        changed_toolchain = make_build_metadata(fixed_target)
        changed_toolchain["cargo_version"] = "cargo 1.98.0 test"
        with self.assertRaisesRegex(pair.PairError, "toolchain or selected build environment differ"):
            pair._validate_build_metadata_pair(baseline, changed_toolchain, baseline)

        changed_environment = make_build_metadata(fixed_target)
        changed_environment["selected_build_environment"]["LANG"] = "fr_FR.UTF-8"
        with self.assertRaisesRegex(pair.PairError, "toolchain or selected build environment differ"):
            pair._validate_build_metadata_pair(baseline, changed_environment, baseline)

        forbidden_environment = make_build_metadata(fixed_target)
        forbidden_environment["selected_build_environment"]["RUSTFLAGS"] = "-C opt-level=2"
        with self.assertRaisesRegex(pair.PairError, "outside the controlled allowlist"):
            pair._validate_build_metadata_pair(baseline, forbidden_environment, baseline)

        changed_configuration = make_build_metadata(fixed_target)
        fingerprint = changed_configuration["cargo_configuration_fingerprint"]
        fingerprint["cargo_home"]["config_files"][0]["present"] = True
        fingerprint["cargo_home"]["config_files"][0]["resolved_path"] = "/tmp/cargo/config"
        fingerprint["cargo_home"]["config_files"][0]["sha256"] = "b" * 64
        body = {key: value for key, value in fingerprint.items() if key != "sha256"}
        fingerprint["sha256"] = pair.sha256_bytes(pair.canonical_json(body))
        with self.assertRaisesRegex(pair.PairError, "toolchain or selected build environment differ"):
            pair._validate_build_metadata_pair(baseline, changed_configuration, baseline)

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
            artifact_path = target_dir / "release" / "deps" / "collector_cpu_diag_bench-reused"
            artifact_path.parent.mkdir(parents=True)
            artifact_path.write_bytes(b"candidate later overwrites this Cargo path")
            saved_binary = root / "baseline-test-binary"
            saved_binary.write_bytes(b"immutable baseline test binary")
            saved_binary.chmod(0o755)

            artifact = {
                "reason": "compiler-artifact",
                "manifest_path": str(source_root / "Cargo.toml"),
                "target": {
                    "name": "collector_cpu_diag_bench",
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
        usage = {
            "process": {field: 0 for field in pair.USAGE_COUNTER_FIELDS},
            "reaped_children": {field: 0 for field in pair.USAGE_COUNTER_FIELDS},
        }
        samples = [
            {
                "duration_ns": 100 + index,
                "evidence": {"verification": {"sample": index}, "resource_usage_delta": usage},
            }
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
                    "resource_usage_delta": usage,
                    "dropped_messages": 0,
                    "launched_processes": 1,
                    "input_fixture_sha256": FIXTURE_HASH,
                    "verification_sha256": pair.sha256_bytes(pair.canonical_json(sample["evidence"]["verification"])),
                }
            )
        payload = b"".join(pair.canonical_json(row) + b"\n" for row in rows)
        result = pair.validate_journal(payload, samples, FIXTURE_HASH)
        self.assertEqual(result["row_count"], 2)

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
            process = SimpleNamespace(
                pid=43210,
                returncode=-9,
                communicate=mock.Mock(
                    side_effect=[
                        pair.subprocess.TimeoutExpired("test", 120, output=b"partial out", stderr=b"partial err"),
                        (b"partial out", b"partial err"),
                    ]
                ),
                kill=mock.Mock(),
            )
            with (
                mock.patch.object(pair, "_snapshot", side_effect=[{"stable": True}, {"stable": True}]),
                mock.patch.object(pair.subprocess, "Popen", return_value=process) as popen_mock,
                mock.patch.object(pair.os, "killpg") as kill_group_mock,
            ):
                result = pair.run_attempt(identity, attempt_dir, {"attempt_id": "timeout-attempt"}, {"PATH": "/bin"})
            self.assertEqual(popen_mock.call_count, 1)
            self.assertTrue(popen_mock.call_args.kwargs["start_new_session"])
            self.assertEqual(process.communicate.call_args_list[0].kwargs["timeout"], 120)
            self.assertEqual(
                process.communicate.call_args_list[1].kwargs["timeout"],
                pair.TIMEOUT_CLEANUP_COMMUNICATE_SECONDS,
            )
            kill_group_mock.assert_called_once_with(43210, pair.signal.SIGKILL)
            self.assertFalse(result["successful"])
            self.assertTrue(result["timed_out"])
            self.assertTrue(result["cleanup_confirmed"])
            self.assertFalse(result["abort_remaining"])
            self.assertEqual((attempt_dir / "stdout.bin").read_bytes(), b"partial out")
            self.assertEqual((attempt_dir / "stderr.bin").read_bytes(), b"partial err")
            self.assertTrue((attempt_dir / "finished.json").is_file())

    def test_timeout_with_unconfirmed_cleanup_is_bounded_and_aborts_remaining_slots(self) -> None:
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
            attempt_dir = Path(temporary) / "unconfirmed-timeout-attempt"
            stdout_pipe = mock.Mock()
            stderr_pipe = mock.Mock()
            process = SimpleNamespace(
                pid=43211,
                returncode=-9,
                stdout=stdout_pipe,
                stderr=stderr_pipe,
                communicate=mock.Mock(
                    side_effect=[
                        pair.subprocess.TimeoutExpired("test", 120, output=b"initial out", stderr=b"initial err"),
                        pair.subprocess.TimeoutExpired("test", 5, output=b"drain out", stderr=b"drain err"),
                        pair.subprocess.TimeoutExpired("test", 5, output=b"last out", stderr=b"last err"),
                    ]
                ),
                kill=mock.Mock(),
                wait=mock.Mock(side_effect=[pair.subprocess.TimeoutExpired("test", 5), None]),
            )
            with (
                mock.patch.object(pair, "_snapshot", side_effect=[{"stable": True}, {"stable": True}]),
                mock.patch.object(pair.subprocess, "Popen", return_value=process),
                mock.patch.object(pair.os, "killpg") as kill_group_mock,
            ):
                result = pair.run_attempt(
                    identity, attempt_dir, {"attempt_id": "unconfirmed-timeout-attempt"}, {"PATH": "/bin"}
                )
            kill_group_mock.assert_called_once_with(43211, pair.signal.SIGKILL)
            self.assertEqual(
                [call.kwargs["timeout"] for call in process.communicate.call_args_list],
                [120, pair.TIMEOUT_CLEANUP_COMMUNICATE_SECONDS, pair.TIMEOUT_CLEANUP_COMMUNICATE_SECONDS],
            )
            self.assertEqual(
                [call.kwargs["timeout"] for call in process.wait.call_args_list],
                [pair.TIMEOUT_CLEANUP_WAIT_SECONDS, pair.TIMEOUT_FINAL_WAIT_SECONDS],
            )
            stdout_pipe.close.assert_called_once()
            stderr_pipe.close.assert_called_once()
            self.assertFalse(result["successful"])
            self.assertTrue(result["timed_out"])
            self.assertFalse(result["cleanup_confirmed"])
            self.assertTrue(result["abort_remaining"])
            self.assertIn("cleanup was not confirmed", result["execution_error"])
            self.assertEqual((attempt_dir / "stdout.bin").read_bytes(), b"last out")
            self.assertEqual((attempt_dir / "stderr.bin").read_bytes(), b"last err")

    def test_unconfirmed_cleanup_marks_current_pair_partial_and_skips_its_other_arm(self) -> None:
        identity = {
            "commit": pair.BASELINE_COMMIT,
            "transport_sha256": "c" * 64,
            "source_fingerprint": {"sha256": "d" * 64, "files": {}},
            "binary_sha256": "e" * 64,
            "receipt_sha256": "f" * 64,
            "root": "/tmp/source",
            "build_metadata": make_build_metadata(str(pair.CAMPAIGN / "work/perf-target-candidate")),
        }
        identities = {
            "baseline": {"side": "baseline", **identity},
            "candidate": {"side": "candidate", **identity},
        }
        with tempfile.TemporaryDirectory() as temporary:
            temporary_root = Path(temporary)
            report_dir = temporary_root / "run-report"
            prereg_path = temporary_root / "preregistration.json"
            prereg_path.write_text("{}\n", encoding="utf-8")
            args = SimpleNamespace(preregistration=str(prereg_path), report_dir=str(report_dir))

            def abort_after_baseline(_identity: object, _attempt_dir: Path, metadata: dict[str, object], _env: object):
                return {
                    **metadata,
                    "successful": False,
                    "returncode": -9,
                    "timed_out": True,
                    "abort_remaining": True,
                    "execution_error": "timed out and cleanup was not confirmed",
                }

            with (
                mock.patch.object(
                    pair, "_verify_preregistration", return_value={"inputs": identities, "protocol": pair._protocol()}
                ),
                mock.patch.object(pair, "check_disk_space", return_value={"total_bytes": 1000, "used_bytes": 0, "free_bytes": 1000}),
                mock.patch.object(pair, "_snapshot", return_value={"stable": True}),
                mock.patch.object(pair, "run_attempt", side_effect=abort_after_baseline) as run_attempt_mock,
            ):
                result = pair.run(args)

            report = json.loads((report_dir / "paired-report.json").read_text(encoding="utf-8"))
            self.assertEqual(result, 1)
            self.assertEqual(run_attempt_mock.call_count, 1)
            self.assertEqual(report["attempt_count"], 1)
            self.assertEqual(report["unattempted_attempt_count"], pair.TOTAL_ATTEMPTS - 1)
            self.assertEqual(
                report["incomplete_pairs"][0]["attempted_attempt_ids"], [report["attempts"][0]["attempt_id"]]
            )
            self.assertEqual(report["incomplete_pairs"][0]["unattempted_attempt_ids"], ["warmup-01-2-candidate"])

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

    def test_cpu_delta_requires_exact_integer_counter_shape(self) -> None:
        valid = {
            "process": {field: 0 for field in pair.USAGE_COUNTER_FIELDS},
            "reaped_children": {field: 0 for field in pair.USAGE_COUNTER_FIELDS},
        }
        self.assertEqual(pair._validate_usage_delta(valid, "test"), valid)
        invalid = json.loads(json.dumps(valid))
        invalid["process"]["user_cpu_us"] = -1
        with self.assertRaisesRegex(pair.PairError, "integer"):
            pair._validate_usage_delta(invalid, "test")

    def test_pair_keeps_cpu_diagnostics_separate_from_correctness_oracle(self) -> None:
        def attempt(side: str, duration: int, cpu: int) -> dict[str, object]:
            usage = {
                "process": {
                    "user_cpu_us": cpu,
                    "system_cpu_us": 1,
                    "voluntary_context_switches": cpu,
                    "involuntary_context_switches": 0,
                },
                "reaped_children": {
                    "user_cpu_us": 0,
                    "system_cpu_us": 0,
                    "voluntary_context_switches": 0,
                    "involuntary_context_switches": 0,
                },
            }
            return {
                "successful": True,
                "attempt_id": side,
                "raw_duration_ns": [duration],
                "raw_resource_usage_delta": [usage],
                "oracle": {"fixture_sha256": FIXTURE_HASH, "verification": "same"},
            }

        result = pair._pair_result(
            pair.WARMUP_PAIRS,
            {"baseline": attempt("baseline", 100, 5), "candidate": attempt("candidate", 90, 4)},
        )
        self.assertTrue(result["valid_for_timing_comparison"])
        self.assertEqual(result["cases"]["whole_graph_session"]["candidate_minus_baseline_ns"], -10)
        self.assertEqual(
            result["cases"]["resource_usage"]["process"]["user_cpu_us"]["candidate_minus_baseline"], -1
        )
        self.assertIsNone(
            result["cases"]["resource_usage"]["reaped_children"]["user_cpu_us"]["candidate_over_baseline"]
        )
        valid = {
            "process": {field: 0 for field in pair.USAGE_COUNTER_FIELDS},
            "reaped_children": {field: 0 for field in pair.USAGE_COUNTER_FIELDS},
        }
        invalid = json.loads(json.dumps(valid))
        invalid["process"]["system_cpu_us"] = 1.5
        with self.assertRaisesRegex(pair.PairError, "integer"):
            pair._validate_usage_delta(invalid, "test")
        invalid = json.loads(json.dumps(valid))
        invalid["reaped_children"]["user_cpu_us"] = pair.I64_MAX + 1
        with self.assertRaisesRegex(pair.PairError, "integer"):
            pair._validate_usage_delta(invalid, "test")


if __name__ == "__main__":
    unittest.main()
