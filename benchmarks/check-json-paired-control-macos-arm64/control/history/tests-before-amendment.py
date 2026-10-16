"""Non-product regressions for the check-JSON paired control driver."""
from __future__ import annotations

import importlib.util
import json
import os
from pathlib import Path
import sys
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
                result = FakeCompleted(0, stdout, b"", 1000 + len(captures))
                captures.append(result)
                return result

            rows = self.harness.run_paired_workload(
                workload=workload,
                commands=commands,
                cwd=cwd,
                environment={"LC_ALL": "C", "LANG": "C", "TZ": "UTC"},
                samples=2,
                warmups=1,
                timeout_seconds=1.0,
                process_runner=fake_run,
            )
            enriched = control._attach_raw_bytes(
                self.harness,
                rows,
                [{
                    "status": item.status,
                    "stdout_bytes": len(item.stdout),
                    "stdout_sha256": self.harness.sha256_bytes(item.stdout),
                    "stdout_base64": control.base64.b64encode(item.stdout).decode("ascii"),
                    "stderr_bytes": len(item.stderr),
                    "stderr_sha256": self.harness.sha256_bytes(item.stderr),
                    "stderr_base64": control.base64.b64encode(item.stderr).decode("ascii"),
                    "elapsed_ns": item.elapsed_ns,
                } for item in captures],
            )
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


if __name__ == "__main__":
    unittest.main()
