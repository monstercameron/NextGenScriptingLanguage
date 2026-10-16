#!/usr/bin/env python3
"""Offline guard tests for the opt-in Omarchy acceptance harness."""

from __future__ import annotations

import json
import os
import importlib.util
import pathlib
import subprocess
import sys
import tempfile
import unittest
from types import SimpleNamespace


ROOT = pathlib.Path(__file__).resolve().parents[1]
HARNESS = ROOT / "tools" / "omarchy_acceptance.py"
SPEC = importlib.util.spec_from_file_location("omarchy_acceptance", HARNESS)
if SPEC is None or SPEC.loader is None:
    raise RuntimeError("could not load acceptance harness for focused helper tests")
ACCEPTANCE = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = ACCEPTANCE
SPEC.loader.exec_module(ACCEPTANCE)


class HyprlandFocusCompatibilityTests(unittest.TestCase):
    def test_lua_dispatch_uses_documented_object_then_verifies_active_address(self) -> None:
        calls: list[list[str]] = []
        responses = [
            SimpleNamespace(returncode=0, stdout="ok\n", stderr=""),
            SimpleNamespace(returncode=0, stdout=json.dumps({"address": "0xabc"}), stderr=""),
        ]

        def runner(command: list[str], **_kwargs: object) -> SimpleNamespace:
            calls.append(command)
            return responses.pop(0)

        result = ACCEPTANCE.focus_hyprland_window(
            "/usr/bin/hyprctl",
            "Hyprland 0.56.2 built from branch v0.56.2 at commit efb5099",
            "0xabc",
            command_runner=runner,
        )
        self.assertEqual(
            calls,
            [
                ["/usr/bin/hyprctl", "dispatch", "hl.dsp.focus({ window = 'address:0xabc' })"],
                ["/usr/bin/hyprctl", "-j", "activewindow"],
            ],
        )
        self.assertEqual(result["focus_form"], "lua_dispatcher_object")
        self.assertEqual(result["focus_command"], calls[0])
        self.assertEqual(result["active_window_command"], calls[1])
        self.assertEqual(result["active_window_address"], "0xabc")
        self.assertTrue(result["active_window_verified"])

    def test_legacy_version_form_is_selected_and_mismatch_fails_closed(self) -> None:
        calls: list[list[str]] = []
        responses = [
            SimpleNamespace(returncode=0, stdout="ok\n", stderr=""),
            SimpleNamespace(returncode=0, stdout=json.dumps({"address": "0xdef"}), stderr=""),
        ]

        def runner(command: list[str], **_kwargs: object) -> SimpleNamespace:
            calls.append(command)
            return responses.pop(0)

        result = ACCEPTANCE.focus_hyprland_window(
            "/usr/bin/hyprctl",
            "Hyprland 0.54.3 built from branch v0.54.3",
            "0xabc",
            command_runner=runner,
        )
        self.assertEqual(
            calls[0],
            ["/usr/bin/hyprctl", "dispatch", "focuswindow", "address:0xabc"],
        )
        self.assertEqual(calls[1], ["/usr/bin/hyprctl", "-j", "activewindow"])
        self.assertEqual(result["focus_form"], "legacy_named_dispatcher")
        self.assertEqual(result["status"], "fail")
        self.assertFalse(result.get("active_window_verified", False))

    def test_failed_lua_dispatch_does_not_query_or_accept_an_active_window(self) -> None:
        calls: list[list[str]] = []

        def runner(command: list[str], **_kwargs: object) -> SimpleNamespace:
            calls.append(command)
            return SimpleNamespace(returncode=1, stdout="", stderr="unsupported dispatcher")

        result = ACCEPTANCE.focus_hyprland_window(
            "/usr/bin/hyprctl",
            "Hyprland 0.56.2 built from branch v0.56.2",
            "0xabc",
            command_runner=runner,
        )
        self.assertEqual(len(calls), 1)
        self.assertEqual(result["status"], "fail")
        self.assertEqual(result["focus_form"], "lua_dispatcher_object")
        self.assertFalse(result.get("active_window_verified", False))


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
