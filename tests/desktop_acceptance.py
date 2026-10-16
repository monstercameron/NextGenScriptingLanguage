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
from unittest import mock


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


class RendererLogDiagnosticsTests(unittest.TestCase):
    def test_startup_failure_logs_survive_temp_cleanup_with_bounded_utf8_tails(self) -> None:
        with tempfile.TemporaryDirectory(prefix="desktop renderer diagnostics ") as raw:
            root = pathlib.Path(raw)
            runtime_dir = root / "runtime"
            runtime_dir.mkdir(mode=0o700)
            runtime_dir = runtime_dir.resolve()
            binary = root / "execute"
            binary.write_bytes(b"offline fixture")
            binary.chmod(0o700)
            observed: dict[str, pathlib.Path] = {}
            limit = ACCEPTANCE.RENDERER_LOG_TAIL_BYTES
            stdout_payload = b"a" * limit + b"TAIL"
            stderr_payload = b"z" * (limit - 1) + b"\xff!"

            def start_renderer(_qs: str, _config: pathlib.Path, log_path: pathlib.Path, _environment: dict[str, str]):
                stdout_path = log_path.with_suffix(".stdout.log")
                stderr_path = log_path.with_suffix(".stderr.log")
                stdout = stdout_path.open("wb")
                stderr = stderr_path.open("wb")
                stdout.write(stdout_payload)
                stderr.write(stderr_payload)
                stdout.flush()
                stderr.flush()
                observed["temporary_directory"] = log_path.parent.parent
                process = SimpleNamespace(pid=1234, poll=lambda: 255, returncode=255)
                return process, stdout, stderr

            tools = {name: f"/usr/bin/{name}" for name in ("qs", "hyprctl", "busctl", "loginctl")}
            environment = {
                "XDG_SESSION_TYPE": "wayland",
                "WAYLAND_DISPLAY": "wayland-1",
                "HYPRLAND_INSTANCE_SIGNATURE": "offline-test",
                "DBUS_SESSION_BUS_ADDRESS": "unix:path=/tmp/dbus-offline-test",
                "XDG_SESSION_ID": "42",
                "EXECUTE_OMARCHY_SESSION_ID": "disposable-offline-test",
            }
            report = ACCEPTANCE.base_report(binary)
            with (
                mock.patch.dict(ACCEPTANCE.os.environ, environment),
                mock.patch.object(ACCEPTANCE, "read_os_release", return_value={"ID": "arch"}),
                mock.patch.object(ACCEPTANCE, "check_runtime_dir", return_value=(runtime_dir, None)),
                mock.patch.object(ACCEPTANCE, "inspect_live_session", return_value=({"session": {"status": "pass"}}, [])),
                mock.patch.object(ACCEPTANCE, "command_version", return_value={"available": True, "returncode": 0, "stdout": "Hyprland 0.56.2", "stderr": ""}),
                mock.patch.object(ACCEPTANCE.shutil, "which", side_effect=lambda name: tools.get(name)),
                mock.patch.object(ACCEPTANCE, "start_quickshell", side_effect=start_renderer),
            ):
                ACCEPTANCE.invoke_live(binary, report)

            self.assertEqual(report["status"], "failed")
            self.assertEqual(report["failure"], "Quickshell exited before IPC readiness (255)")
            self.assertEqual(report["cleanup"]["temporary_directory"]["status"], "removed", report["cleanup"])
            self.assertFalse(observed["temporary_directory"].exists())
            self.assertEqual(report["renderer_launch"]["argv"][:2], ["/usr/bin/qs", "--path"])
            self.assertEqual(len(report["renderer_launch"]["launched_qml_sha256"]), 64)

            diagnostics = report["renderer_diagnostics"]
            self.assertEqual(diagnostics["tail_limit_bytes_per_stream"], limit)
            self.assertEqual([item["stage"] for item in diagnostics["logs"]], ["initial"])
            stdout = diagnostics["logs"][0]["stdout"]
            self.assertEqual(stdout["status"], "captured")
            self.assertEqual(stdout["total_bytes"], limit + 4)
            self.assertEqual(stdout["captured_bytes"], limit)
            self.assertTrue(stdout["truncated"])
            self.assertTrue(stdout["tail_utf8"].endswith("TAIL"))
            stderr = diagnostics["logs"][0]["stderr"]
            self.assertEqual(stderr["status"], "captured")
            self.assertEqual(stderr["total_bytes"], limit + 1)
            self.assertEqual(stderr["captured_bytes"], limit)
            self.assertTrue(stderr["truncated"])
            self.assertTrue(stderr["tail_utf8"].endswith("\ufffd!"))

    def test_missing_and_unreadable_logs_have_structured_errors(self) -> None:
        with tempfile.TemporaryDirectory(prefix="desktop renderer log errors ") as raw:
            missing = ACCEPTANCE.capture_renderer_log_tail(pathlib.Path(raw) / "missing.stderr.log")
            self.assertEqual(missing["status"], "missing")
            self.assertEqual(missing["error"]["type"], "FileNotFoundError")
            with mock.patch.object(ACCEPTANCE.Path, "open", side_effect=PermissionError("denied")):
                unreadable = ACCEPTANCE.capture_renderer_log_tail(pathlib.Path(raw) / "locked.stderr.log")
            self.assertEqual(unreadable["status"], "read_error")
            self.assertEqual(unreadable["error"]["type"], "PermissionError")


class RendererIpcDiagnosticsTests(unittest.TestCase):
    def test_ipc_failure_selects_the_launched_path_and_captures_bounded_inventory(self) -> None:
        process = SimpleNamespace(poll=lambda: None, returncode=None)
        config = pathlib.Path("/run/user/1000/execute-omarchy-test/shell.qml")
        client = ACCEPTANCE.RendererClient("/usr/bin/qs", process, "accept-test", config)
        request = client.frame({"kind": "create"})
        limit = ACCEPTANCE.IPC_DIAGNOSTIC_TAIL_BYTES
        stdout_payload = b"x" * (limit + 4) + b"TARGET"
        stderr_payload = b"y" * (limit - 1) + b"\xff!"
        inventory_payload = b"target executeDesktop-accept-test\n  function request(frame: string): string\n"
        responses = [
            SimpleNamespace(returncode=255, stdout=stdout_payload, stderr=stderr_payload),
            SimpleNamespace(returncode=0, stdout=inventory_payload, stderr=b""),
        ]

        with mock.patch.object(ACCEPTANCE.subprocess, "run", side_effect=responses) as run:
            with self.assertRaisesRegex(RuntimeError, "Quickshell IPC exited 255") as raised:
                client.call(request)

        self.assertIn("stderr:", str(raised.exception))
        self.assertIn("stdout:", str(raised.exception))
        calls = [invocation.args[0] for invocation in run.call_args_list]
        prefix = ["/usr/bin/qs", "ipc", "--path", str(config)]
        self.assertEqual(calls[0][:4], prefix)
        self.assertEqual(calls[0][4:7], ["call", "executeDesktop-accept-test", "request"])
        self.assertEqual(json.loads(calls[0][7]), request)
        self.assertEqual(calls[1], prefix + ["show"])

        diagnostics = client.ipc_diagnostics
        self.assertIsNotNone(diagnostics)
        self.assertEqual(diagnostics["call"]["returncode"], 255)
        for stream_name, expected_total, expected_suffix in (
            ("stdout", len(stdout_payload), "TARGET"),
            ("stderr", len(stderr_payload), "\ufffd!"),
        ):
            stream = diagnostics["call"][stream_name]
            self.assertEqual(stream["total_bytes"], expected_total)
            self.assertEqual(stream["captured_bytes"], limit)
            self.assertTrue(stream["truncated"])
            self.assertTrue(stream["tail_utf8"].endswith(expected_suffix))
        inventory = diagnostics["inventory"]
        self.assertEqual(inventory["returncode"], 0)
        self.assertIn("executeDesktop-accept-test", inventory["stdout"]["tail_utf8"])

    def test_later_ipc_failure_preserves_inventory_without_recapturing(self) -> None:
        process = SimpleNamespace(poll=lambda: None, returncode=None)
        config = pathlib.Path("/run/user/1000/execute-omarchy-test/shell.qml")
        client = ACCEPTANCE.RendererClient("/usr/bin/qs", process, "accept-test", config)
        first_request = client.frame({"kind": "create"})
        second_request = client.frame({"kind": "patch"})
        inventory_payload = b"target executeDesktop-accept-test\n  function request(frame: string): string\n"
        responses = [
            SimpleNamespace(returncode=255, stdout=b"first failure", stderr=b"first error"),
            SimpleNamespace(returncode=0, stdout=inventory_payload, stderr=b""),
            SimpleNamespace(returncode=127, stdout=b"second failure", stderr=b"second error"),
        ]

        with mock.patch.object(ACCEPTANCE.subprocess, "run", side_effect=responses) as run:
            with self.assertRaisesRegex(RuntimeError, "Quickshell IPC exited 255"):
                client.call(first_request)
            with self.assertRaisesRegex(RuntimeError, "Quickshell IPC exited 127"):
                client.call(second_request)

        self.assertEqual(run.call_count, 3)
        diagnostics = client.ipc_diagnostics
        self.assertIsNotNone(diagnostics)
        self.assertEqual(diagnostics["call"]["returncode"], 127)
        self.assertEqual(diagnostics["call"]["stderr"]["tail_utf8"], "second error")
        self.assertEqual(diagnostics["inventory"]["returncode"], 0)
        self.assertIn("executeDesktop-accept-test", diagnostics["inventory"]["stdout"]["tail_utf8"])


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
