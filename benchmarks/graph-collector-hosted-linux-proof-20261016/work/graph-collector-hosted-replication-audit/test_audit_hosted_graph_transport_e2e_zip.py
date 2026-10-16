from __future__ import annotations

import importlib.util
import copy
import tempfile
import unittest
import zipfile
from pathlib import Path


HERE = Path(__file__).resolve().parent
SCRIPT = HERE / "audit_hosted_graph_transport_e2e_zip.py"
SPEC = importlib.util.spec_from_file_location("hosted_graph_zip_audit", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
audit = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(audit)


class HostedGraphZipAuditTests(unittest.TestCase):
    def test_manual_protocol_matches_frozen_runner_source(self) -> None:
        runner_spec = importlib.util.spec_from_file_location(
            "frozen_graph_transport_pair", audit.PACKAGE / "graph_transport_e2e_pair.py"
        )
        self.assertIsNotNone(runner_spec)
        self.assertIsNotNone(runner_spec.loader)
        runner = importlib.util.module_from_spec(runner_spec)
        runner_spec.loader.exec_module(runner)
        self.assertEqual(audit.sha_file(audit.PACKAGE / "graph_transport_e2e_pair.py"), audit.RUNNER_SHA256)
        self.assertEqual(audit.expected_protocol(), runner._protocol())

    def test_hashes_all_members_and_compares_fixture_as_stream(self) -> None:
        with tempfile.TemporaryDirectory(prefix="graph-e2e-zip-audit-test-") as directory:
            path = Path(directory) / "sample.zip"
            fixture = b"fixed fixture bytes\n" * 128
            payload = b"report bytes\n" * 20
            with zipfile.ZipFile(path, "w", compression=zipfile.ZIP_DEFLATED) as output:
                output.writestr("evidence/run/raw.input.jsonl", fixture)
                output.writestr("evidence/run/raw.json", payload)
            archive = audit.Archive(path)
            ledger, digests = archive.hash_all(expected_fixture=fixture)
            self.assertEqual(len(ledger), 2)
            self.assertEqual(digests["evidence/run/raw.input.jsonl"], audit.sha_bytes(fixture))
            self.assertEqual(digests["evidence/run/raw.json"], audit.sha_bytes(payload))
            self.assertEqual(archive.read("evidence/run/raw.json"), payload)

    def test_fixture_mismatch_is_rejected_during_streaming_hash(self) -> None:
        with tempfile.TemporaryDirectory(prefix="graph-e2e-zip-audit-test-") as directory:
            path = Path(directory) / "bad.zip"
            with zipfile.ZipFile(path, "w", compression=zipfile.ZIP_DEFLATED) as output:
                output.writestr("run/raw.input.jsonl", b"tampered fixture")
            archive = audit.Archive(path)
            with self.assertRaises(audit.AuditError):
                archive.hash_all(expected_fixture=b"expected fixture")

    def test_build_pair_receipt_must_match_both_per_arm_results(self) -> None:
        arm_results = {
            side: {
                "side": side, "cargo_exit_status": 0, "timed_out": False,
                "target_directory_fresh": True, "source_stable": True,
                "artifact_selected": True, "receipt_written": True, "error": None,
            }
            for side in ("baseline", "candidate")
        }
        summary = {
            "schema": "execute.graph.transport-e2e.hosted-build-pair/1",
            "completed_at": "2026-10-16T09:00:00Z", "attempt": 1,
            "sides": copy.deepcopy(arm_results), "failed_sides": [], "no_retries": True,
        }
        audit.validate_build_pair_result(summary, arm_results)

        mutated = copy.deepcopy(summary)
        mutated["sides"]["candidate"]["receipt_written"] = False
        with self.assertRaises(audit.AuditError):
            audit.validate_build_pair_result(mutated, arm_results)

    def test_build_pair_receipt_rejects_attempt_retries_or_bad_timestamp(self) -> None:
        arms = {side: {"side": side} for side in ("baseline", "candidate")}
        summary = {
            "schema": "execute.graph.transport-e2e.hosted-build-pair/1",
            "completed_at": "2026-10-16T09:00:00Z", "attempt": 1,
            "sides": copy.deepcopy(arms), "failed_sides": [], "no_retries": True,
        }
        for key, value in (("attempt", 2), ("no_retries", False), ("completed_at", "yesterday")):
            mutated = copy.deepcopy(summary)
            mutated[key] = value
            with self.subTest(key=key), self.assertRaises(audit.AuditError):
                audit.validate_build_pair_result(mutated, arms)

    def test_arm_build_receipt_members_match_helper_layout(self) -> None:
        with tempfile.TemporaryDirectory(prefix="graph-e2e-zip-audit-test-") as directory:
            path = Path(directory) / "build-layout.zip"
            with zipfile.ZipFile(path, "w", compression=zipfile.ZIP_DEFLATED) as output:
                output.writestr("baseline/build-result.json", b"{}")
                output.writestr("candidate/target-freshness.json", b"{}")
                output.writestr("build/build-pair-result.json", b"{}")
            archive = audit.Archive(path)
            self.assertEqual(archive.find(audit.arm_build_member("baseline", "build-result.json")),
                             "baseline/build-result.json")
            self.assertEqual(archive.find(audit.arm_build_member("candidate", "target-freshness.json")),
                             "candidate/target-freshness.json")
            self.assertEqual(archive.find("build-pair-result.json"), "build/build-pair-result.json")
            with self.assertRaises(audit.AuditError):
                archive.find("build/baseline/build-result.json")
        with self.assertRaises(audit.AuditError):
            audit.arm_build_member("other", "build-result.json")

    def test_attempt_start_and_finish_times_come_from_separate_receipts(self) -> None:
        started = {"started_at": "2026-10-16T09:16:36.137Z"}
        finished = {"finished_at": "2026-10-16T09:17:06.030Z"}
        # The captured attempt report is the finished receipt and has no started_at;
        # that timestamp is carried in the paired started.json member.
        attempt_report = {"finished_at": finished["finished_at"]}
        self.assertNotIn("started_at", attempt_report)
        audit.validate_attempt_timestamps(started, finished, "fixture-attempt")
        with self.assertRaises(audit.AuditError):
            audit.validate_attempt_timestamps({}, finished, "fixture-attempt")
        with self.assertRaises(audit.AuditError):
            audit.validate_attempt_timestamps(
                {"started_at": "2026-10-16T09:17:06.031Z"}, finished, "fixture-attempt"
            )

    def test_host_version_preflight_uses_preregistered_build_metadata(self) -> None:
        versions = (
            "Python 3.12.3\n"
            "cargo 1.99.1 (5f94df478 2026-08-27)\n"
            "rustc 1.99.1 (58cf8f9c3 2026-10-13)\n"
            "binary: rustc\n"
        )
        metadata = {
            "cargo_version": "cargo 1.99.1 (5f94df478 2026-08-27)",
            "rustc_vv": "rustc 1.99.1 (58cf8f9c3 2026-10-13)\nhost: x86_64-unknown-linux-gnu\n",
        }
        audit.validate_host_tool_versions(versions, metadata)
        with self.assertRaises(audit.AuditError):
            audit.validate_host_tool_versions(versions.replace("cargo 1.99.1", "cargo 1.98.0"), metadata)


if __name__ == "__main__":
    unittest.main()
