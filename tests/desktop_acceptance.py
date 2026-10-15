#!/usr/bin/env python3
"""Offline guard tests for the opt-in Omarchy acceptance harness."""

from __future__ import annotations

import json
import os
import pathlib
import subprocess
import sys
import tempfile
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[1]
HARNESS = ROOT / "tools" / "omarchy_acceptance.py"


class DesktopAcceptanceHarnessTests(unittest.TestCase):
    def run_harness(self, *arguments: str) -> tuple[subprocess.CompletedProcess[str], dict[str, object]]:
        environment = os.environ.copy()
        environment.pop("EXECUTE_OMARCHY_DISPOSABLE", None)
        environment.pop("EXECUTE_OMARCHY_SESSION_ID", None)
        completed = subprocess.run(
            [sys.executable, str(HARNESS), *arguments],
            cwd=ROOT,
            env=environment,
            check=False,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=5,
        )
        self.assertEqual(completed.returncode, 0, completed.stderr)
        return completed, json.loads(completed.stdout)

    def test_default_is_pending_and_does_not_probe_a_desktop(self) -> None:
        _, report = self.run_harness()
        self.assertEqual(report["native_desktop_certification"], "PENDING")
        self.assertEqual(report["status"], "pending")
        self.assertNotIn("target", report)
        self.assertIn("A11", report["findings"])
        self.assertIn("O07", report["findings"])
        source_files = report["source"]["files"]
        self.assertTrue(source_files)
        self.assertTrue(all(value is not None and len(value) == 64 for value in source_files.values()))

    def test_cli_opt_in_without_disposable_marker_stays_pending(self) -> None:
        _, report = self.run_harness("--run-in-disposable-omarchy")
        self.assertEqual(report["native_desktop_certification"], "PENDING")
        self.assertEqual(report["checks"]["quickshell_wayland_smoke"]["status"], "pending")

    def test_evidence_contains_the_final_machine_report_and_is_exclusive(self) -> None:
        with tempfile.TemporaryDirectory(prefix="desktop acceptance report ") as directory:
            evidence = pathlib.Path(directory) / "evidence.json"
            completed, report = self.run_harness("--evidence", str(evidence))
            saved = json.loads(evidence.read_text(encoding="utf-8"))
            self.assertEqual(saved, report)
            self.assertEqual(report["evidence_write"]["status"], "written")
            self.assertEqual(evidence.stat().st_mode & 0o777, 0o600)
            before = evidence.read_bytes()
            _, second = self.run_harness("--evidence", str(evidence))
            self.assertEqual(second["evidence_write"]["status"], "failed")
            self.assertEqual(evidence.read_bytes(), before)
            self.assertEqual(json.loads(completed.stdout), report)


if __name__ == "__main__":
    unittest.main()
