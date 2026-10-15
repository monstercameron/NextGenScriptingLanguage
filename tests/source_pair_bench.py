#!/usr/bin/env python3
"""Non-measuring unit checks for the paired saved-source benchmark harness."""
from contextlib import redirect_stderr
import io
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch


ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))

from benchmarks import source_pair as harness


# Captured directly from the saved 0c57e22 baseline binary as a correctness
# fixture (not as timing evidence). Its terminal output omits the nonterminal
# joined.value event, so the sequence numbers intentionally contain a gap.
ACTUAL_BASELINE_GRAPH_REPORT = b'''{
  "dropped_messages": 0,
  "generation": 1,
  "launched_processes": 0,
  "outputs": [
    {
      "generation": 1,
      "payload": {"kind": "value", "value": {"Integer": 0}},
      "port": {"node": "joined", "port": "status"},
      "revision": 1,
      "sensitivity": ["unknown"],
      "sequence": 1
    },
    {
      "generation": 1,
      "payload": {"kind": "value", "value": {"Integer": 0}},
      "port": {"node": "cleaned", "port": "status"},
      "revision": 1,
      "sensitivity": ["unknown"],
      "sequence": 3
    },
    {
      "generation": 1,
      "payload": {"kind": "value", "value": {"String": "paired graph"}},
      "port": {"node": "cleaned", "port": "value"},
      "revision": 1,
      "sensitivity": ["unknown"],
      "sequence": 4
    }
  ],
  "protocol": "execute.graph-run/1",
  "revision": 1
}'''


class SourcePairHarnessTests(unittest.TestCase):
    def test_summary_uses_median_and_nearest_rank_p95(self):
        self.assertEqual(
            harness.summarize([7, 1, 5, 3, 9]),
            {
                "sample_count": 5,
                "median_ns": 5,
                "p95_nearest_rank_ns": 9,
                "minimum_ns": 1,
                "maximum_ns": 9,
            },
        )

    def test_interleaving_alternates_the_first_side(self):
        self.assertEqual(harness.interleaved_order(0), ("baseline", "candidate"))
        self.assertEqual(harness.interleaved_order(1), ("candidate", "baseline"))
        self.assertEqual(harness.interleaved_order(2), ("baseline", "candidate"))

    def test_report_path_and_writer_refuse_overwrite(self):
        with tempfile.TemporaryDirectory() as temporary:
            report = Path(temporary) / "source-pair.json"
            self.assertEqual(harness.validate_report_path(report), report)
            self.assertEqual(harness.write_report(report, {"ready": True}), report)
            with self.assertRaises(FileExistsError):
                harness.validate_report_path(report)
            with self.assertRaises(FileExistsError):
                harness.write_report(report, {"ready": False})
            self.assertEqual(harness.json.loads(report.read_text()), {"ready": True})

    def test_source_and_binary_fingerprints_detect_mutation(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source_dir = root / "src"
            source_dir.mkdir()
            (root / "Cargo.toml").write_text("[package]\nname='fixture'\n")
            source = source_dir / "lib.rs"
            source.write_text("pub fn value() -> u8 { 1 }\n")
            binary = root / "execute"
            binary.write_bytes(b"baseline executable bytes")

            before = {
                "source": harness.candidate_source_snapshot(root, "fixture revision"),
                "binary": harness.file_snapshot(binary, "fixture binary"),
            }
            harness.assert_artifacts_unchanged(before, dict(before))

            source.write_text("pub fn value() -> u8 { 2 }\n")
            after_source_change = {
                "source": harness.candidate_source_snapshot(root, "fixture revision"),
                "binary": harness.file_snapshot(binary, "fixture binary"),
            }
            with self.assertRaisesRegex(harness.BenchmarkError, "changed during measurement"):
                harness.assert_artifacts_unchanged(before, after_source_change)

            source.write_text("pub fn value() -> u8 { 1 }\n")
            binary.write_bytes(b"candidate executable bytes")
            after_binary_change = {
                "source": harness.candidate_source_snapshot(root, "fixture revision"),
                "binary": harness.file_snapshot(binary, "fixture binary"),
            }
            with self.assertRaisesRegex(harness.BenchmarkError, "changed during measurement"):
                harness.assert_artifacts_unchanged(before, after_binary_change)

    def test_oracle_failure_keeps_report_absent(self):
        if os.name == "nt":
            self.skipTest("saved interpreter fixture uses Unix executable names")
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            manifest_dirs = []
            for name in ("baseline-manifests", "candidate-manifests"):
                directory = root / name
                directory.mkdir()
                (directory / "fixture.json").write_text("{}\n")
                manifest_dirs.append(directory)

            binary_paths = []
            for name in ("execute-baseline", "execute-candidate"):
                binary = root / name
                binary.write_text("#!/bin/sh\nexit 0\n")
                binary.chmod(0o700)
                helper = binary.with_name("execute-applet")
                helper.write_text("#!/bin/sh\nexit 0\n")
                helper.chmod(0o700)
                binary_paths.append(binary)

            report = root / "must-not-exist.json"
            args = [
                "--baseline-binary", str(binary_paths[0]),
                "--candidate-binary", str(binary_paths[1]),
                "--baseline-manifest-dir", str(manifest_dirs[0]),
                "--candidate-manifest-dir", str(manifest_dirs[1]),
                "--baseline-revision", "test-baseline",
                "--candidate-revision", "test-candidate",
                "--report", str(report),
                "--samples", "10",
                "--warmups", "0",
                "--argv-counts", "1",
                "--transform-lines", "1",
                "--timeout-seconds", "1",
                "--command-timeout-ms", "500",
                "--candidate-build-confirmed",
                "--measurement-window-idle",
            ]

            def fingerprint_stub(_root, revision):
                return {"revision": revision, "combined_sha256": "fixture"}

            def bad_candidate(command, _cwd, _environment, _timeout):
                if command[0] == str(binary_paths[1]):
                    return harness.Completed(0, b"unexpected output", b"", 20)
                return harness.Completed(0, b"", b"", 10)

            error_output = io.StringIO()
            with patch.object(harness, "baseline_source_snapshot", side_effect=fingerprint_stub), patch.object(
                harness, "candidate_source_snapshot", side_effect=fingerprint_stub
            ), patch.object(harness, "run_process", side_effect=bad_candidate), redirect_stderr(error_output):
                status = harness.main(args)

            self.assertEqual(status, 1)
            self.assertIn("stdout differs from its exact byte oracle", error_output.getvalue())
            self.assertFalse(report.exists(), "a failed oracle must stop before report creation")

    def test_raw_samples_keep_alternation_warmups_and_summaries(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            program = root / "workload.program"
            program.write_text('run "true"\n')
            workload = harness.Workload(
                "fixture",
                program,
                [],
                b"ok\n",
                b"",
                lambda: {"files": {"input": "stable"}},
            )

            def fixed_runner(command, _cwd, _environment, _timeout):
                side = command[0]
                elapsed = 100 if side == "baseline" else 200
                return harness.Completed(0, b"ok\n", b"", elapsed)

            result = harness.run_paired_workload(
                workload,
                {"baseline": ["baseline"], "candidate": ["candidate"]},
                root,
                {"LC_ALL": "C"},
                samples=3,
                warmups=2,
                timeout_seconds=1,
                process_runner=fixed_runner,
            )

            self.assertEqual(
                [row["order"] for row in result["warmup_order"]],
                [["baseline", "candidate"], ["candidate", "baseline"]],
            )
            self.assertEqual(
                [row["order"] for row in result["paired_order"]],
                [["baseline", "candidate"], ["candidate", "baseline"], ["baseline", "candidate"]],
            )
            self.assertEqual(len(result["warmup_observations"]), 4)
            self.assertEqual(len(result["raw_paired_samples"]), 6)
            self.assertEqual(result["summary"]["baseline"]["median_ns"], 100)
            self.assertEqual(result["summary"]["candidate"]["median_ns"], 200)
            self.assertEqual(result["summary"]["baseline"]["p95_nearest_rank_ns"], 100)
            self.assertEqual(result["summary"]["candidate"]["p95_nearest_rank_ns"], 200)

    def test_graph_baseline_report_pairs_by_port_not_global_event_sequence(self):
        baseline_normalized = harness._graph_oracle(ACTUAL_BASELINE_GRAPH_REPORT)
        expected_ports = {
            "joined.status": {
                "payload": {"kind": "value", "value": {"Integer": 0}},
                "sensitivity": ["unknown"],
            },
            "cleaned.status": {
                "payload": {"kind": "value", "value": {"Integer": 0}},
                "sensitivity": ["unknown"],
            },
            "cleaned.value": {
                "payload": {"kind": "value", "value": {"String": "paired graph"}},
                "sensitivity": ["unknown"],
            },
        }
        self.assertEqual(baseline_normalized["outputs_by_port"], expected_ports)

        candidate_report = harness.json.loads(ACTUAL_BASELINE_GRAPH_REPORT)
        candidate_report["outputs"][1], candidate_report["outputs"][2] = (
            candidate_report["outputs"][2], candidate_report["outputs"][1]
        )
        for sequence, event in zip((2, 5, 8), candidate_report["outputs"]):
            event["sequence"] = sequence
        candidate_payload = harness.json.dumps(candidate_report, separators=(",", ":")).encode()
        self.assertEqual(harness._graph_oracle(candidate_payload), baseline_normalized)

        workload = harness.Workload(
            "typed_graph",
            None,
            [],
            None,
            b"",
            lambda: {"effects": "unchanged"},
            harness._graph_oracle,
        )

        def graph_runner(command, _cwd, _environment, _timeout):
            side = command[0]
            output = ACTUAL_BASELINE_GRAPH_REPORT if side == "baseline" else candidate_payload
            return harness.Completed(0, output, b"", 100 if side == "baseline" else 110)

        paired = harness.run_paired_workload(
            workload,
            {"baseline": ["baseline"], "candidate": ["candidate"]},
            Path("."),
            {"LC_ALL": "C"},
            samples=2,
            warmups=1,
            timeout_seconds=1,
            process_runner=graph_runner,
        )
        self.assertNotEqual(
            paired["preflight"]["baseline"]["stdout_sha256"],
            paired["preflight"]["candidate"]["stdout_sha256"],
        )
        self.assertEqual(
            paired["preflight"]["baseline"]["normalized_stdout_sha256"],
            paired["preflight"]["candidate"]["normalized_stdout_sha256"],
        )
        self.assertEqual(len(paired["raw_paired_samples"]), 4)
        self.assertEqual(
            {row["stdout_sha256"] for row in paired["raw_paired_samples"]},
            {
                harness.sha256_bytes(ACTUAL_BASELINE_GRAPH_REPORT),
                harness.sha256_bytes(candidate_payload),
            },
        )

        invalid_order = harness.json.loads(ACTUAL_BASELINE_GRAPH_REPORT)
        invalid_order["outputs"][1], invalid_order["outputs"][2] = (
            invalid_order["outputs"][2], invalid_order["outputs"][1]
        )
        with self.assertRaisesRegex(AssertionError, "strictly increasing sequence order"):
            harness._graph_oracle(harness.json.dumps(invalid_order).encode())

        invalid_identity = harness.json.loads(ACTUAL_BASELINE_GRAPH_REPORT)
        invalid_identity["outputs"][0]["revision"] = 2
        with self.assertRaisesRegex(AssertionError, "identity must match"):
            harness._graph_oracle(harness.json.dumps(invalid_identity).encode())

    def test_generated_match_workload_uses_exact_shared_metadata_catalog_and_top_k_oracle(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            _, workloads, inventory = harness.build_workloads(
                root,
                {name: Path("/fixture/tools") / name for name in ("true", "printf", "cat", "tr", "grep")},
                transform_lines=1,
                argv_counts=[1],
            )
            workload = next(item for item in workloads if item.name == "applet_match_1000_catalog_top_100")
            catalog = workload.manifest_directory
            retained_catalog = inventory["match_catalog"]
            retained = retained_catalog["files"]
            request_json = retained_catalog["request_json"]

            self.assertEqual(retained_catalog["manifest_count"], 1000)
            self.assertEqual(len(retained), 1000)
            self.assertEqual(len(harness._catalog_names_for_match_oracle(catalog)), 1000)
            self.assertEqual(len(set(retained)), 1000)
            first_manifest = harness.json.loads((catalog / sorted(retained)[0]).read_text())
            self.assertEqual(first_manifest["contract"]["version"], 1)
            self.assertEqual(first_manifest["contract"]["backend"], "command")
            self.assertEqual(first_manifest["contract"]["lifecycle"], {"kind": "oneshot"})
            self.assertEqual(first_manifest["parameters"], {"payload": {"type": "string"}})
            self.assertEqual(first_manifest["contract"]["ports"]["inputs"]["payload"], {
                "schema": {"type": "text"}, "delivery": "single"
            })
            self.assertEqual(first_manifest["binary"], "benchmark-only-never-invoked")

            baseline_command = harness._cli_command(
                Path("/fixture/baseline"), Path("/separate/baseline/catalog"), workload, 500
            )
            candidate_command = harness._cli_command(
                Path("/fixture/candidate"), Path("/separate/candidate/catalog"), workload, 500
            )
            self.assertEqual(baseline_command[:4], [
                "/fixture/baseline", "applets", "match", "--json"
            ])
            self.assertEqual(candidate_command[:4], [
                "/fixture/candidate", "applets", "match", "--json"
            ])
            self.assertEqual(baseline_command[4], candidate_command[4])
            self.assertEqual(
                baseline_command[baseline_command.index("--manifest-dir") + 1],
                candidate_command[candidate_command.index("--manifest-dir") + 1],
            )
            request = harness.json.loads(baseline_command[4])
            self.assertEqual(request, {
                "source": {"schema": {"type": "text"}},
                "target": {"schema": {"type": "text"}},
                "limit": 100,
            })
            self.assertNotIn("applet", request["source"])
            self.assertNotIn("applet", request["target"])

            names = sorted(retained)
            expected_rows = []
            for filename in names[:100]:
                name = filename[:-5]
                expected_rows.append({
                    "source": {
                        "applet": name, "port": "stdout", "schema": {"type": "text"},
                        "delivery": "single",
                    },
                    "target": {
                        "applet": name, "port": "payload", "schema": {"type": "text"},
                        "delivery": "single",
                    },
                    "compatible": True,
                    "score": 213,
                    "source_matches_request": True,
                    "target_matches_request": True,
                    "issues": [],
                    "suggested_converters": [],
                })
            valid_output = harness.json.dumps({
                "protocol": "execute.applet-matches/1",
                "validation": "declared_contracts",
                "matches": expected_rows,
            }).encode()
            workload.semantic_check(valid_output)

            wrong_score = harness.json.loads(valid_output)
            wrong_score["matches"][0]["score"] = 214
            with self.assertRaisesRegex(AssertionError, "exact lexical compatible top-100"):
                workload.semantic_check(harness.json.dumps(wrong_score).encode())
            wrong_order = harness.json.loads(valid_output)
            wrong_order["matches"][0], wrong_order["matches"][1] = (
                wrong_order["matches"][1], wrong_order["matches"][0]
            )
            with self.assertRaisesRegex(AssertionError, "exact lexical compatible top-100"):
                workload.semantic_check(harness.json.dumps(wrong_order).encode())

            before_catalog = {"catalog": harness.directory_snapshot(catalog, "fixture catalog")}
            manifest_path = catalog / sorted(retained)[0]
            manifest_path.write_text(manifest_path.read_text() + " ", encoding="utf-8")
            after_catalog = {"catalog": harness.directory_snapshot(catalog, "fixture catalog")}
            with self.assertRaisesRegex(harness.BenchmarkError, "changed during measurement"):
                harness.assert_artifacts_unchanged(before_catalog, after_catalog)


if __name__ == "__main__":
    unittest.main()
