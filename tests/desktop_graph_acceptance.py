"""Offline tests for the live graph acceptance guard and process evidence."""

from __future__ import annotations

import json
import pathlib
import tempfile
import unittest
from types import SimpleNamespace
from typing import Any
from unittest import mock


ROOT = pathlib.Path(__file__).resolve().parents[1]
HARNESS_PATH = ROOT / "tools" / "desktop_graph_acceptance.py"

import importlib.util
import sys

SPEC = importlib.util.spec_from_file_location("desktop_graph_acceptance", HARNESS_PATH)
if SPEC is None or SPEC.loader is None:
    raise RuntimeError("could not load desktop graph acceptance harness")
HARNESS = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = HARNESS
SPEC.loader.exec_module(HARNESS)


def write_proc(
    proc_root: pathlib.Path,
    pid: int,
    parent_pid: int,
    starttime: int,
    name: str,
    executable: str,
    argv: list[str],
    state: str = "S",
) -> None:
    directory = proc_root / str(pid)
    directory.mkdir()
    stat_fields = [state, str(parent_pid)] + ["0"] * 18
    stat_fields[19] = str(starttime)
    (directory / "stat").write_text(f"{pid} ({name}) " + " ".join(stat_fields), encoding="ascii")
    (directory / "cmdline").write_bytes(b"\0".join(item.encode() for item in argv) + b"\0")
    (directory / "exe").symlink_to(executable)


class LinuxProcEvidenceTests(unittest.TestCase):
    def test_proc_stat_parses_comm_with_parentheses_and_starttime(self) -> None:
        fields = ["S", "10"] + ["0"] * 18
        fields[19] = "98765"
        parsed = HARNESS.parse_proc_stat(f"42 (worker (clock)) " + " ".join(fields))
        self.assertEqual(parsed, (42, "worker (clock)", 10, 98765, "S"))

    def test_owned_tree_classifies_renderers_and_workers_by_stable_identity(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            proc_root = pathlib.Path(raw)
            write_proc(proc_root, 100, 1, 10, "execute", "/usr/bin/execute", ["execute", "run"])
            write_proc(proc_root, 101, 100, 20, "qs", "/usr/bin/qs", ["qs", "--path", "/run/private.qml"])
            write_proc(
                proc_root,
                102,
                100,
                30,
                "python3",
                "/usr/bin/python3",
                ["python3", "-u", "examples/desktop/workers.py", "--worker", "clock_format"],
            )
            table = HARNESS.read_process_table(proc_root)
            descendants = HARNESS.owned_descendants(HARNESS.ProcIdentity(100, 10), table)
            self.assertEqual({record.identity.pid for record in descendants}, {101, 102})
            classified = {HARNESS.classify_process(record) for record in descendants}
            self.assertEqual(classified, {("quickshell_renderer", None), ("applet_worker", "clock_format")})
            self.assertEqual(HARNESS.identity_status(HARNESS.ProcIdentity(102, 30), proc_root), "present")
            self.assertEqual(HARNESS.identity_status(HARNESS.ProcIdentity(102, 31), proc_root), "gone")

    def test_reused_root_pid_does_not_claim_old_descendants(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            proc_root = pathlib.Path(raw)
            write_proc(proc_root, 100, 1, 11, "execute", "/usr/bin/execute", ["execute"])
            write_proc(proc_root, 101, 100, 20, "qs", "/usr/bin/qs", ["qs"])
            table = HARNESS.read_process_table(proc_root)
            self.assertEqual(HARNESS.owned_descendants(HARNESS.ProcIdentity(100, 10), table), [])

    def test_zombie_identity_is_alive_even_when_exe_link_is_unavailable(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            proc_root = pathlib.Path(raw)
            write_proc(proc_root, 102, 100, 30, "worker", "/usr/bin/python3", ["python3", "--worker"], state="Z")
            (proc_root / "102" / "exe").unlink()
            identity = HARNESS.ProcIdentity(102, 30)
            self.assertEqual(HARNESS.identity_status(identity, proc_root), "present")
            (proc_root / "102" / "stat").unlink()
            self.assertEqual(HARNESS.identity_status(identity, proc_root), "gone")

    def test_unreadable_proc_identity_fails_closed_in_teardown_evidence(self) -> None:
        identity = HARNESS.ProcIdentity(102, 30)
        with mock.patch.object(HARNESS.Path, "read_text", side_effect=PermissionError("proc stat denied")), \
             mock.patch.object(HARNESS, "REAP_TIMEOUT", 0.0):
            self.assertEqual(HARNESS.identity_status(identity), "unknown")
            self.assertEqual(HARNESS.wait_gone([identity]), [{
                "pid": 102, "starttime_ticks": 30, "identity_status": "unknown"
            }])

    def test_first_process_observation_retains_pidfd_for_later_cleanup(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            proc_root = pathlib.Path(raw)
            write_proc(proc_root, 100, 1, 10, "execute", "/usr/bin/execute", ["execute"])
            write_proc(proc_root, 101, 100, 20, "qs", "/usr/bin/qs", ["qs"])
            seen: dict[tuple[int, int], Any] = {}
            pidfds: dict[tuple[int, int], int | None] = {}
            with mock.patch.object(HARNESS.os, "pidfd_open", create=True, return_value=17) as opened:
                HARNESS.observe_tree(HARNESS.ProcIdentity(100, 10), seen, pidfds, proc_root)
                HARNESS.observe_tree(HARNESS.ProcIdentity(100, 10), seen, pidfds, proc_root)
            self.assertEqual(pidfds[(101, 20)], 17)
            self.assertEqual(opened.call_count, 1)

    def test_pidfds_are_closed_after_cleanup(self) -> None:
        with mock.patch.object(HARNESS.os, "close") as close:
            HARNESS.close_pidfds({(101, 20): 17, (102, 30): None})
        close.assert_called_once_with(17)

    def test_forced_cleanup_signals_through_pidfd_only_after_identity_recheck(self) -> None:
        identity = HARNESS.ProcIdentity(102, 30)
        with mock.patch.object(HARNESS.signal, "pidfd_send_signal", create=True) as send, \
             mock.patch.object(HARNESS, "identity_status", return_value="present"):
            self.assertEqual(HARNESS.signal_owned_identity(identity, HARNESS.signal.SIGTERM, 17), "signaled")
        send.assert_called_once_with(17, HARNESS.signal.SIGTERM)

    def test_pidfd_recheck_skips_a_reused_pid(self) -> None:
        identity = HARNESS.ProcIdentity(102, 30)
        with mock.patch.object(HARNESS.signal, "pidfd_send_signal", create=True) as send, \
             mock.patch.object(HARNESS, "identity_status", return_value="gone"):
            self.assertEqual(HARNESS.signal_owned_identity(identity, HARNESS.signal.SIGTERM, 17), "gone")
        send.assert_not_called()


class GraphInputTests(unittest.TestCase):
    def test_generated_program_uses_requested_graph_backends(self) -> None:
        for producer in ("local", "remote"):
            with self.subTest(producer=producer), tempfile.TemporaryDirectory() as raw:
                directory = pathlib.Path(raw)
                manifests, program = HARNESS.desktop_demo.write_run_inputs(
                    directory,
                    producer,
                    "quickshell",
                    23456,
                )
                source = program.read_text(encoding="utf-8")
                self.assertIn(f"run desktop.clock_{producer} as clock_source", source)
                self.assertIn("render quickshell:text as clock", source)
                self.assertIn("render quickshell:button", source)
                self.assertNotIn("{{", source)
                remote_manifest = json.loads((manifests / f"desktop.clock_{producer}.json").read_text(encoding="utf-8"))
                if producer == "remote":
                    self.assertEqual(remote_manifest["contract"]["remote"]["endpoint"], "http://127.0.0.1:23456")

    def test_only_verified_focus_can_send_wayland_keys_and_sync_must_advance(self) -> None:
        calls: list[list[str]] = []
        fixture = SimpleNamespace(sync_count=0)
        responses = [
            SimpleNamespace(returncode=0, stdout="ok\n", stderr=""),
            SimpleNamespace(returncode=0, stdout=json.dumps({"address": "0xabc"}), stderr=""),
        ]

        def runner(command: list[str], **_kwargs: Any) -> SimpleNamespace:
            calls.append(command)
            if command[0] == "/usr/bin/wtype":
                fixture.sync_count += 1
                return SimpleNamespace(returncode=0, stdout="", stderr="")
            return responses.pop(0)

        evidence = HARNESS.attempt_graph_button(
            {"pid": 101, "address": "0xabc"},
            "/usr/bin/hyprctl",
            "Hyprland 0.56.2 built from branch v0.56.2",
            "/usr/bin/wtype",
            fixture,
            command_runner=runner,
            monotonic=lambda: 1.0,
        )
        self.assertEqual(evidence["status"], "pass")
        self.assertTrue(evidence["focus"]["active_window_verified"])
        self.assertEqual(calls[0][0:2], ["/usr/bin/hyprctl", "dispatch"])
        self.assertEqual(calls[2], ["/usr/bin/wtype", "-k", "Tab", "-k", "Return"])
        self.assertEqual(fixture.sync_count, 1)

    def test_focus_failure_sends_no_wayland_input(self) -> None:
        calls: list[list[str]] = []

        def runner(command: list[str], **_kwargs: Any) -> SimpleNamespace:
            calls.append(command)
            return SimpleNamespace(returncode=1, stdout="", stderr="focus rejected")

        evidence = HARNESS.attempt_graph_button(
            {"pid": 101, "address": "0xabc"},
            "/usr/bin/hyprctl",
            "Hyprland 0.56.2 built from branch v0.56.2",
            "/usr/bin/wtype",
            SimpleNamespace(sync_count=0),
            command_runner=runner,
        )
        self.assertEqual(evidence["status"], "fail")
        self.assertNotIn("wayland_input_command", evidence)
        self.assertFalse(any(command[0] == "/usr/bin/wtype" for command in calls))


class CancellationContractTests(unittest.TestCase):
    def test_source_pins_sigint_diagnostic_and_generic_error_exit_status(self) -> None:
        graph = (ROOT / "src" / "graph" / "transport.rs").read_text(encoding="utf-8")
        main = (ROOT / "src" / "main.rs").read_text(encoding="utf-8")
        self.assertIn('bail!("graph session cancelled")', graph)
        self.assertIn("let signal = signal_hook::consts::SIGINT;", main)
        self.assertIn(".unwrap_or(1)", main)

    def test_source_pinned_cancel_exit_and_diagnostic_pass(self) -> None:
        result = HARNESS.cancellation_result(1, "execute: graph session cancelled\n", forced_cleanup=False)
        self.assertEqual(result["status"], "pass")
        self.assertEqual(result["expected_returncode"], 1)
        self.assertTrue(result["diagnostic_present"])

    def test_natural_exit_without_cancel_diagnostic_fails(self) -> None:
        result = HARNESS.cancellation_result(0, "{}\n", forced_cleanup=False)
        self.assertEqual(result["status"], "fail")

    def test_forced_cleanup_always_fails_even_if_expected_diagnostic_exists(self) -> None:
        result = HARNESS.cancellation_result(1, "graph session cancelled", forced_cleanup=True)
        self.assertEqual(result["status"], "fail")
        self.assertTrue(result["forced_cleanup"])

    def test_failed_live_scenario_still_prints_and_writes_receipt(self) -> None:
        emitted: list[dict[str, Any]] = []
        with mock.patch.object(HARNESS.sys, "argv", ["desktop_graph_acceptance.py", "--run-in-disposable-arch"]), \
             mock.patch.dict(HARNESS.os.environ, {
                 HARNESS.OPT_IN_ENV: "1", HARNESS.SESSION_MARKER_ENV: "disposable-test"
             }), \
             mock.patch.object(HARNESS, "preflight", return_value=({"hyprland": {"stdout": ""}}, [], {})), \
             mock.patch.object(HARNESS, "run_scenario", return_value={"producer": "local", "status": "fail"}), \
             mock.patch.object(HARNESS, "emit", side_effect=lambda report, _path: emitted.append(report) or True):
            self.assertEqual(HARNESS.main(), 1)
        self.assertEqual(len(emitted), 1)
        self.assertEqual(emitted[0]["status"], "failed")
        self.assertEqual(emitted[0]["scenarios"][0]["status"], "fail")


class StaticPreflightTests(unittest.TestCase):
    def test_static_failures_short_circuit_before_desktop_probes(self) -> None:
        env = {
            "XDG_SESSION_TYPE": "wayland", "WAYLAND_DISPLAY": "wayland-1",
            "HYPRLAND_INSTANCE_SIGNATURE": "session-a", "DBUS_SESSION_BUS_ADDRESS": "unix:path=/tmp/bus",
            "XDG_SESSION_ID": "4",
        }
        with tempfile.TemporaryDirectory() as raw:
            binary = pathlib.Path(raw) / "execute"
            binary.write_text("candidate", encoding="utf-8")
            binary.chmod(0o700)
            for marker, os_id in (("not-disposable", "arch"), ("disposable-test", "fedora")):
                with self.subTest(marker=marker, os_id=os_id), \
                     mock.patch.dict(HARNESS.os.environ, {**env, HARNESS.SESSION_MARKER_ENV: marker}, clear=True), \
                     mock.patch.object(HARNESS.omarchy_acceptance, "read_os_release", return_value={"ID": os_id}), \
                     mock.patch.object(HARNESS.omarchy_acceptance, "check_runtime_dir", return_value=(pathlib.Path(raw), None)), \
                     mock.patch.object(HARNESS.shutil, "which", side_effect=lambda name: f"/usr/bin/{name}"), \
                     mock.patch.object(HARNESS.omarchy_acceptance, "inspect_live_session") as inspect, \
                     mock.patch.object(HARNESS.omarchy_acceptance, "command_version") as version:
                    _, errors, _ = HARNESS.preflight(binary)
                    self.assertTrue(errors)
                    inspect.assert_not_called()
                    version.assert_not_called()


if __name__ == "__main__":
    unittest.main()
