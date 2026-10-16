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


class RenderedWidgetObserverTests(unittest.TestCase):
    def test_private_qml_injection_reads_actual_loader_child_properties(self) -> None:
        source = (ROOT / "desktop" / "quickshell" / "shell.qml").read_text(encoding="utf-8")
        renderer_target = "executeDesktop-acceptance"
        observer_target = "executeDesktopObserver-acceptance"

        generated = ACCEPTANCE.build_test_observer_qml(source, renderer_target, observer_target)

        self.assertIn(f'target: "{observer_target}"', generated)
        self.assertIn('property string ipcTarget: "executeDesktop-acceptance"', generated)
        self.assertEqual(generated.count("id: acceptanceWidgetRepeater"), 1)
        self.assertIn("acceptanceWidgetRepeater.itemAt(index)", generated)
        self.assertIn("const renderedText = loader.item.text;", generated)
        self.assertIn("loader.item.enabled", generated)
        self.assertIn(ACCEPTANCE.TEST_OBSERVER_IDENTITY, generated)
        self.assertNotIn(ACCEPTANCE.TEST_OBSERVER_IDENTITY, source)
        self.assertNotIn("acceptanceWidgetRepeater", source)
        self.assertIn('__EXECUTE_RENDERER_IPC_TARGET__', source)

    def test_private_qml_injection_fails_closed_on_ambiguous_source(self) -> None:
        source = (ROOT / "desktop" / "quickshell" / "shell.qml").read_text(encoding="utf-8")
        with self.assertRaisesRegex(RuntimeError, "anchors must each occur exactly once"):
            ACCEPTANCE.build_test_observer_qml(
                source + "    function dispatch(frameText: string): string {\n",
                "executeDesktop-acceptance",
                "executeDesktopObserver-acceptance",
            )

    def test_observer_response_is_bounded_and_checks_actual_property_types(self) -> None:
        text_response = {
            "status": "loaded",
            "widget_key": "desktop_demo::clock_text",
            "component": "text",
            "text": "09:41",
            "enabled": None,
        }
        button_response = {
            "status": "loaded",
            "widget_key": "desktop_demo::action_button",
            "component": "button",
            "text": "Acknowledge",
            "enabled": False,
        }
        self.assertEqual(
            ACCEPTANCE.decode_rendered_widget_response(
                json.dumps(text_response).encode(),
                "desktop_demo::clock_text",
            )["text"],
            "09:41",
        )
        self.assertIs(
            ACCEPTANCE.decode_rendered_widget_response(
                json.dumps(button_response).encode(),
                "desktop_demo::action_button",
            )["enabled"],
            False,
        )
        with self.assertRaisesRegex(RuntimeError, "exceeds byte limit"):
            ACCEPTANCE.decode_rendered_widget_response(
                b" " * (ACCEPTANCE.RENDERED_WIDGET_RESPONSE_LIMIT + 1),
                "desktop_demo::clock_text",
            )
        with self.assertRaisesRegex(RuntimeError, "malformed JSON"):
            ACCEPTANCE.decode_rendered_widget_response(b"not json", "desktop_demo::clock_text")
        button_response["enabled"] = "false"
        with self.assertRaisesRegex(RuntimeError, "boolean enabled state"):
            ACCEPTANCE.decode_rendered_widget_response(
                json.dumps(button_response).encode(),
                "desktop_demo::action_button",
            )

    def test_observer_call_uses_private_qml_selector_and_fixed_method(self) -> None:
        response = {
            "status": "loaded",
            "widget_key": "desktop_demo::action_button",
            "component": "button",
            "text": "Acknowledge",
            "enabled": True,
        }
        completed = SimpleNamespace(returncode=0, stdout=json.dumps(response).encode(), stderr=b"")
        config_path = pathlib.Path("/run/user/1000/acceptance/shell.qml")

        with mock.patch.object(ACCEPTANCE.subprocess, "run", return_value=completed) as run:
            actual = ACCEPTANCE.inspect_rendered_widget(
                "/usr/bin/qs",
                config_path,
                "executeDesktopObserver-acceptance",
                "desktop_demo::action_button",
            )

        self.assertEqual(
            run.call_args.args[0],
            [
                "/usr/bin/qs",
                "ipc",
                "--path",
                str(config_path),
                "call",
                "executeDesktopObserver-acceptance",
                "inspectRenderedWidget",
                "desktop_demo::action_button",
            ],
        )
        self.assertTrue(actual["enabled"])

    def test_state_wait_observes_a_later_actual_child_value(self) -> None:
        observed = [
            {
                "status": "loaded",
                "widget_key": "desktop_demo::clock_text",
                "component": "text",
                "text": "09:41",
                "enabled": None,
            },
            {
                "status": "loaded",
                "widget_key": "desktop_demo::clock_text",
                "component": "text",
                "text": "09:42",
                "enabled": None,
            },
        ]
        with mock.patch.object(
            ACCEPTANCE,
            "inspect_rendered_widget",
            side_effect=observed,
        ) as inspect:
            result = ACCEPTANCE.wait_for_rendered_widget(
                "/usr/bin/qs",
                pathlib.Path("/run/user/1000/acceptance/shell.qml"),
                "executeDesktopObserver-acceptance",
                "desktop_demo::clock_text",
                text="09:42",
                timeout=1.0,
            )

        self.assertEqual(result["status"], "pass")
        self.assertEqual(result["polls"], 2)
        self.assertEqual(result["actual"]["text"], "09:42")
        self.assertEqual(inspect.call_count, 2)


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
        self.assertIs(diagnostics["call"], diagnostics["last_attempt"])
        self.assertEqual(diagnostics["call"]["request_id"], request["request_id"])
        self.assertEqual(diagnostics["call"]["request_kind"], "create")
        self.assertEqual(diagnostics["call"]["argv"][-1], "<request frame omitted>")
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
        self.assertIs(diagnostics["call"], diagnostics["last_attempt"])
        self.assertEqual(diagnostics["call"]["request_id"], second_request["request_id"])
        self.assertEqual(diagnostics["call"]["request_kind"], "patch")
        self.assertEqual(diagnostics["call"]["stderr"]["tail_utf8"], "second error")
        self.assertEqual(diagnostics["inventory"]["returncode"], 0)
        self.assertIn("executeDesktop-accept-test", diagnostics["inventory"]["stdout"]["tail_utf8"])
        last_attempt = diagnostics["last_attempt"]
        self.assertEqual(
            {key: last_attempt[key] for key in ("request_id", "request_kind", "response_kind", "status", "returncode")},
            {"request_id": 2, "request_kind": "patch", "response_kind": None, "status": "command_failed", "returncode": 127},
        )
        self.assertEqual(last_attempt["stdout"]["tail_utf8"], "second failure")
        self.assertEqual(last_attempt["stderr"]["tail_utf8"], "second error")
        self.assertNotIn("attempts", diagnostics)

    def test_last_attempt_identifies_fatal_response_after_transient_startup_failure(self) -> None:
        process = SimpleNamespace(poll=lambda: None, returncode=None)
        config = pathlib.Path("/run/user/1000/execute-omarchy-test/shell.qml")
        client = ACCEPTANCE.RendererClient("/usr/bin/qs", process, "accept-test", config)
        negotiation = client.frame({"kind": "negotiate"}, negotiation=True)
        fatal = client.frame({"kind": "destroy", "widget_id": "clock_text"})
        raw_stdout = b"x" * (ACCEPTANCE.IPC_DIAGNOSTIC_TAIL_BYTES + 3) + b"NOT JSON"
        raw_stderr = b"e" * (ACCEPTANCE.IPC_DIAGNOSTIC_TAIL_BYTES + 2) + b"\xff!"
        responses = [
            SimpleNamespace(returncode=255, stdout=b"No running instances\n", stderr=b""),
            SimpleNamespace(returncode=0, stdout=raw_stdout, stderr=raw_stderr),
        ]

        with mock.patch.object(ACCEPTANCE.subprocess, "run", side_effect=responses):
            with self.assertRaisesRegex(RuntimeError, "Quickshell IPC exited 255"):
                client.call(negotiation)
            with self.assertRaisesRegex(RuntimeError, "invalid Quickshell IPC response"):
                client.call(fatal)

        diagnostics = client.ipc_diagnostics
        self.assertIsNotNone(diagnostics)
        call_diagnostics = diagnostics["call"]
        self.assertIs(call_diagnostics, diagnostics["last_attempt"])
        self.assertEqual(call_diagnostics["request_id"], fatal["request_id"])
        self.assertEqual(call_diagnostics["request_kind"], "destroy")
        self.assertEqual(call_diagnostics["response_kind"], None)
        self.assertEqual(call_diagnostics["returncode"], 0)
        self.assertEqual(call_diagnostics["status"], "invalid_response")
        self.assertEqual(call_diagnostics["failure"], "invalid_json")
        self.assertEqual(call_diagnostics["stdout"]["total_bytes"], len(raw_stdout))
        self.assertEqual(call_diagnostics["stdout"]["captured_bytes"], ACCEPTANCE.IPC_DIAGNOSTIC_TAIL_BYTES)
        self.assertTrue(call_diagnostics["stdout"]["truncated"])
        self.assertTrue(call_diagnostics["stdout"]["tail_utf8"].endswith("NOT JSON"))
        self.assertEqual(call_diagnostics["stderr"]["total_bytes"], len(raw_stderr))
        self.assertTrue(call_diagnostics["stderr"]["tail_utf8"].endswith("\ufffd!"))
        self.assertEqual(call_diagnostics["argv"][-1], "<request frame omitted>")
        last_attempt = diagnostics["last_attempt"]
        self.assertEqual(
            {key: last_attempt[key] for key in ("request_id", "request_kind", "response_kind", "status", "returncode", "failure")},
            {"request_id": 2, "request_kind": "destroy", "response_kind": None, "status": "invalid_response", "returncode": 0, "failure": "invalid_json"},
        )
        self.assertEqual(last_attempt["failure_detail"], "Expecting value: line 1 column 1 (char 0)")
        self.assertEqual(last_attempt["stdout"]["total_bytes"], len(raw_stdout))
        self.assertEqual(last_attempt["stdout"]["captured_bytes"], ACCEPTANCE.IPC_DIAGNOSTIC_TAIL_BYTES)
        self.assertTrue(last_attempt["stdout"]["truncated"])
        self.assertTrue(last_attempt["stdout"]["tail_utf8"].endswith("NOT JSON"))
        self.assertEqual(last_attempt["stderr"]["total_bytes"], len(raw_stderr))
        self.assertEqual(last_attempt["stderr"]["captured_bytes"], ACCEPTANCE.IPC_DIAGNOSTIC_TAIL_BYTES)
        self.assertTrue(last_attempt["stderr"]["truncated"])
        self.assertTrue(last_attempt["stderr"]["tail_utf8"].endswith("\ufffd!"))
        self.assertEqual(last_attempt["request_kind"], "destroy")
        self.assertEqual(last_attempt["request_id"], fatal["request_id"])

    def test_last_attempt_request_kind_is_bounded(self) -> None:
        process = SimpleNamespace(poll=lambda: None, returncode=None)
        config = pathlib.Path("/run/user/1000/execute-omarchy-test/shell.qml")
        client = ACCEPTANCE.RendererClient("/usr/bin/qs", process, "accept-test", config)

        request = client.frame({"kind": "x" * 2_000_000})
        request["request_id"] = 1 << 80
        with mock.patch.object(ACCEPTANCE.subprocess, "run") as run:
            with self.assertRaisesRegex(RuntimeError, "request exceeds frame size limit"):
                client.call(request)

        run.assert_not_called()
        self.assertIsNone(client.ipc_diagnostics["last_attempt"]["request_id"])
        self.assertEqual(len(client.ipc_diagnostics["last_attempt"]["request_kind"]), 64)
        self.assertEqual(client.ipc_diagnostics["last_attempt"]["status"], "request_rejected")
        self.assertNotIn("attempts", client.ipc_diagnostics)


class RendererIpcResponseTests(unittest.TestCase):
    def make_client(self) -> ACCEPTANCE.RendererClient:
        process = SimpleNamespace(poll=lambda: None, returncode=None)
        config = pathlib.Path("/run/user/1000/execute-omarchy-test/shell.qml")
        return ACCEPTANCE.RendererClient("/usr/bin/qs", process, "accept-test", config)

    def make_response(self, request: dict[str, object], version: int, body: dict[str, object]) -> dict[str, object]:
        response: dict[str, object] = {
            field: request[field]
            for field in ("protocol", "request_id", "session_id", "generation", "revision")
        }
        response.update({"version": version, "body": body})
        return response

    @staticmethod
    def completed(response: dict[str, object]) -> SimpleNamespace:
        return SimpleNamespace(returncode=0, stdout=json.dumps(response).encode("utf-8"), stderr=b"")

    def test_negotiation_accepts_supported_new_version(self) -> None:
        client = self.make_client()

        def respond(command: list[str], **_kwargs: object) -> SimpleNamespace:
            request = json.loads(command[-1])
            response = self.make_response(request, ACCEPTANCE.PROTOCOL_VERSION, {"kind": "negotiated"})
            return self.completed(response)

        with mock.patch.object(ACCEPTANCE.subprocess, "run", side_effect=respond) as run:
            client.negotiate()

        self.assertEqual(run.call_count, 1)
        self.assertEqual(client.version, ACCEPTANCE.PROTOCOL_VERSION)

    def test_ordinary_call_accepts_current_version(self) -> None:
        client = self.make_client()
        client.version = ACCEPTANCE.PROTOCOL_VERSION
        request = client.frame({"kind": "create"})
        response = self.make_response(request, client.version, {"kind": "applied"})

        with mock.patch.object(ACCEPTANCE.subprocess, "run", return_value=self.completed(response)):
            actual = client.call(request)

        self.assertEqual(actual, response)
        last_attempt = client.ipc_diagnostics["last_attempt"]
        self.assertEqual(
            {key: last_attempt[key] for key in ("request_id", "request_kind", "response_kind", "status", "returncode")},
            {"request_id": request["request_id"], "request_kind": "create", "response_kind": "applied", "status": "accepted", "returncode": 0},
        )
        self.assertEqual(last_attempt["stdout"]["tail_utf8"], json.dumps(response))
        self.assertEqual(last_attempt["stderr"]["captured_bytes"], 0)
        self.assertEqual(client.ipc_diagnostics["call"]["status"], "accepted")
        self.assertEqual(client.ipc_diagnostics["call"]["response_kind"], "applied")

    def test_ordinary_call_rejects_changed_version(self) -> None:
        client = self.make_client()
        client.version = ACCEPTANCE.PROTOCOL_VERSION
        request = client.frame({"kind": "create"})
        response = self.make_response(request, client.version + 1, {"kind": "applied"})

        with mock.patch.object(ACCEPTANCE.subprocess, "run", return_value=self.completed(response)):
            with self.assertRaisesRegex(RuntimeError, "protocol version changed within the session"):
                client.call(request)

        last_attempt = client.ipc_diagnostics["last_attempt"]
        call_diagnostics = client.ipc_diagnostics["call"]
        self.assertIs(call_diagnostics, last_attempt)
        self.assertEqual(last_attempt["status"], "response_mismatch")
        self.assertEqual(last_attempt["failure"], "version_mismatch")
        self.assertEqual(last_attempt["failure_detail"], "version")
        self.assertEqual(call_diagnostics["status"], "response_mismatch")
        self.assertEqual(call_diagnostics["mismatch_field"], "version")

    def test_missing_response_body_updates_both_diagnostic_records(self) -> None:
        client = self.make_client()
        client.version = ACCEPTANCE.PROTOCOL_VERSION
        request = client.frame({"kind": "create"})
        response = self.make_response(request, client.version, {"unexpected": "body"})
        response.pop("body")

        with mock.patch.object(ACCEPTANCE.subprocess, "run", return_value=self.completed(response)):
            with self.assertRaisesRegex(RuntimeError, "renderer response body is missing"):
                client.call(request)

        self.assertEqual(client.ipc_diagnostics["last_attempt"]["status"], "invalid_response")
        self.assertEqual(client.ipc_diagnostics["last_attempt"]["failure"], "response_body_missing")
        self.assertIs(client.ipc_diagnostics["call"], client.ipc_diagnostics["last_attempt"])
        self.assertEqual(client.ipc_diagnostics["call"]["status"], "invalid_response")
        self.assertEqual(client.ipc_diagnostics["call"]["failure"], "response_body_missing")

    def test_rejected_renderer_error_updates_both_diagnostic_records(self) -> None:
        client = self.make_client()
        client.version = ACCEPTANCE.PROTOCOL_VERSION
        request = client.frame({"kind": "patch"})
        response = self.make_response(
            request,
            client.version,
            {"kind": "error", "code": "stale_revision", "message": "version changed"},
        )

        with mock.patch.object(ACCEPTANCE.subprocess, "run", return_value=self.completed(response)):
            with self.assertRaisesRegex(RuntimeError, "renderer rejected request"):
                client.call(request)

        self.assertEqual(client.ipc_diagnostics["last_attempt"]["status"], "renderer_error")
        self.assertEqual(client.ipc_diagnostics["last_attempt"]["failure"], "renderer_error")
        self.assertIs(client.ipc_diagnostics["call"], client.ipc_diagnostics["last_attempt"])
        self.assertEqual(client.ipc_diagnostics["call"]["status"], "renderer_error")
        self.assertEqual(client.ipc_diagnostics["call"]["response_kind"], "error")

    def test_allow_error_returns_renderer_error_with_matching_request_fields(self) -> None:
        client = self.make_client()
        client.version = ACCEPTANCE.PROTOCOL_VERSION
        request = client.frame({"kind": "poll_events"})
        response = self.make_response(
            request,
            client.version,
            {"kind": "error", "code": "stale_session", "message": "generation changed"},
        )

        with mock.patch.object(ACCEPTANCE.subprocess, "run", return_value=self.completed(response)):
            actual = client.call(request, allow_error=True)

        self.assertEqual(actual, response)
        self.assertIs(client.ipc_diagnostics["call"], client.ipc_diagnostics["last_attempt"])
        self.assertEqual(client.ipc_diagnostics["last_attempt"]["status"], "accepted")
        self.assertEqual(client.ipc_diagnostics["call"]["status"], "accepted")


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
