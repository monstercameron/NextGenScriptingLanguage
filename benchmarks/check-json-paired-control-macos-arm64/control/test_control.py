"""Non-product regressions for the check-JSON paired control driver."""
from __future__ import annotations

import importlib.util
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest

CONTROL = Path(__file__).resolve().parent
DRIVER_PATH = CONTROL / "driver.py"
spec = importlib.util.spec_from_file_location("check_json_reuse_control", DRIVER_PATH)
assert spec is not None and spec.loader is not None
control = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = control
spec.loader.exec_module(control)


class CheckJsonReuseControlTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        primary_root = os.environ.get("CHECK_JSON_PRIMARY_ROOT")
        if not primary_root:
            raise unittest.SkipTest("set CHECK_JSON_PRIMARY_ROOT to the source checkout containing benchmarks/source_pair.py")
        cls.primary = Path(primary_root).expanduser().resolve(strict=True)
        cls.harness = control._load_source_pair(cls.primary / "benchmarks" / "source_pair.py")

    def test_independent_json_contract_is_exact(self) -> None:
        payload = json.dumps({
            "protocol": "execute.check/1",
            "validation": "static",
            "ok": True,
            "path": "programs/check.program",
            "path_bytes": list(b"programs/check.program"),
            "diagnostics": [],
        }).encode()
        oracle = control.check_json_semantic_oracle("programs/check.program")
        self.assertEqual(oracle(payload)["protocol"], "execute.check/1")
        with self.assertRaises(AssertionError):
            oracle(payload.replace(b'"ok": true', b'"ok": false'))

    def test_fixture_and_workload_specs_are_fixed_and_inert(self) -> None:
        inventory = control.fixture_snapshot(self.harness)
        self.assertEqual(inventory["catalogs"]["full75"]["file_count"], 75)
        self.assertEqual(inventory["catalogs"]["inert-one-unused-cat"]["file_count"], 1)
        for steps in (1, 100):
            program = (control.FIXTURE_ROOT / "cases" / ("steps-%03d" % steps) / "programs" / "check.program").read_bytes()
            self.assertEqual(program.count(b'run "execute-check-json-reuse-nonexistent-command"'), steps)
            self.assertTrue(program.endswith(b'then save to "forbidden"\n'))
            self.assertNotIn(b'run "cat"', program)

    def test_all_eight_cells_use_existing_harness_with_fake_processes(self) -> None:
        class FakeCompleted:
            def __init__(self, status: int, stdout: bytes, stderr: bytes, elapsed_ns: int) -> None:
                self.status = status
                self.stdout = stdout
                self.stderr = stderr
                self.elapsed_ns = elapsed_ns

        for count, catalog_kind, json_mode in control.CELL_PLAN:
            workload, cwd, catalog = control._make_workload(self.harness, count, catalog_kind, json_mode)
            commands = {
                "baseline": control._commands(Path("/fake/baseline-execute"), count, catalog, json_mode),
                "candidate": control._commands(Path("/fake/candidate-execute"), count, catalog, json_mode),
            }
            captures = []

            def fake_run(command, run_cwd, environment, timeout_seconds):
                self.assertEqual(run_cwd, cwd)
                if "--json" in command:
                    stdout = json.dumps({
                        "protocol": "execute.check/1",
                        "validation": "static",
                        "ok": True,
                        "path": "programs/check.program",
                        "path_bytes": list(b"programs/check.program"),
                        "diagnostics": [],
                    }, separators=(",", ":")).encode()
                else:
                    stdout = b""
                return FakeCompleted(0, stdout, b"", 1000 + len(captures))

            recording_runner = control._make_capture_runner(
                self.harness,
                captures,
                samples=2,
                warmups=1,
                process_implementation=fake_run,
            )

            rows = self.harness.run_paired_workload(
                workload=workload,
                commands=commands,
                cwd=cwd,
                environment={"LC_ALL": "C", "LANG": "C", "TZ": "UTC"},
                samples=2,
                warmups=1,
                timeout_seconds=1.0,
                process_runner=recording_runner,
            )
            enriched = control._attach_raw_bytes(self.harness, rows, captures)
            self.assertEqual(len(enriched["raw_invocations"]), 8)
            self.assertEqual(rows["summary"]["baseline"]["sample_count"], 2)
            self.assertEqual(rows["summary"]["candidate"]["sample_count"], 2)
            self.assertEqual([row["order"] for row in rows["paired_order"]], [["baseline", "candidate"], ["candidate", "baseline"]])

    def test_cli_refuses_to_run_without_root_idle_window_confirmation(self) -> None:
        with self.assertRaises(SystemExit) as raised:
            control.main([
                "--primary-root", "/not-needed-without-confirmation",
                "--candidate-root", "/not-needed-without-confirmation",
                "--report", str(CONTROL / "evidence" / "must-not-exist.json"),
            ])
        self.assertEqual(raised.exception.code, 2)
        self.assertFalse((CONTROL / "evidence" / "must-not-exist.json").exists())

    def test_midcell_failure_retains_completed_cell_and_timeout_record(self) -> None:
        class FakeRunner:
            def __init__(self) -> None:
                self.calls = 0

            def __call__(self, command, cwd, environment, timeout_seconds):
                self.calls += 1
                if self.calls == 1:
                    live_report = json.loads(self.report_path.read_text(encoding="utf-8"))
                    self_outer.assertEqual(live_report["status"], "running")
                    self_outer.assertEqual(live_report["current_cell"]["name"], "001_steps-full75-ordinary")
                if self.calls == 7:
                    raise self_timeout("synthetic timeout after 1.0 seconds")
                if "--json" in command:
                    stdout = json.dumps({
                        "protocol": "execute.check/1",
                        "validation": "static",
                        "ok": True,
                        "path": "programs/check.program",
                        "path_bytes": list(b"programs/check.program"),
                        "diagnostics": [],
                    }, separators=(",", ":")).encode()
                else:
                    stdout = b""
                return self.harness.Completed(0, stdout, b"", 10_000 + self.calls)

        def self_timeout(message):
            return self.harness.BenchmarkError(message)

        with tempfile.TemporaryDirectory() as temporary:
            self_outer = self
            report_path = Path(temporary) / "partial-report.json"
            writer = control.IncrementalReport(self.harness, report_path)
            report = {
                "schema": "execute.check-json-reuse-paired-control/1",
                "status": "running",
                "workloads": {},
                "completed_cells": [],
                "current_cell": None,
            }
            writer.persist(report)
            runner = FakeRunner()
            runner.harness = self.harness
            runner.report_path = report_path
            try:
                result = control._collect_cells(
                    sp=self.harness,
                    report=report,
                    report_writer=writer,
                    binaries={
                        "baseline": Path("/fake/baseline-execute"),
                        "candidate": Path("/fake/candidate-execute"),
                    },
                    environment={"LC_ALL": "C", "LANG": "C", "TZ": "UTC"},
                    samples=1,
                    warmups=0,
                    timeout_seconds=1.0,
                    plan=control.CELL_PLAN[:2],
                    process_implementation=runner,
                )
            finally:
                writer.close()

            saved = json.loads(report_path.read_text(encoding="utf-8"))
            self.assertEqual(result, 1)
            self.assertEqual(saved["status"], "failed")
            self.assertEqual(saved["completed_cells"], ["001_steps-full75-ordinary"])
            self.assertIn("001_steps-full75-ordinary", saved["workloads"])
            previous = saved["workloads"]["001_steps-full75-ordinary"]["raw_invocations"]
            self.assertEqual(len(previous), 4)
            self.assertTrue(all(item["state"] == "completed" for item in previous))
            self.assertTrue(all("stdout_base64" in item and "stderr_base64" in item for item in previous))
            current = saved["current_cell"]
            self.assertEqual(current["name"], "001_steps-full75-json")
            self.assertEqual(current["state"], "failed")
            self.assertEqual(len(current["raw_invocations"]), 3)
            failure = current["raw_invocations"][-1]
            self.assertEqual(failure["phase"], "sample")
            self.assertEqual(failure["side"], "baseline")
            self.assertEqual(failure["state"], "failed")
            self.assertIsNone(failure["status"])
            self.assertIsNone(failure["elapsed_ns"])
            self.assertFalse(failure["output_available"])
            self.assertIn("synthetic timeout", failure["error"])
            self.assertEqual(runner.calls, 7)


if __name__ == "__main__":
    unittest.main()
