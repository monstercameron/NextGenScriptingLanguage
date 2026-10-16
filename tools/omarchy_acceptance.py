#!/usr/bin/env python3
"""Opt-in real-session Quickshell smoke harness for disposable Omarchy runners.

Without --run-in-disposable-omarchy this prints a PENDING report and runs no
desktop commands. A live run also requires the operator marker
EXECUTE_OMARCHY_DISPOSABLE=1. It writes only below the owned XDG runtime
directory, isolates Quickshell's XDG config/data/cache/state roots, and removes
its temporary tree during cleanup. It does not install packages or edit the
user's shell/compositor configuration.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import platform
import re
import shutil
import signal
import stat
import subprocess
import sys
import tempfile
import time
import uuid
from pathlib import Path
from typing import Any


REPO = Path(__file__).resolve().parents[1]
DEFAULT_BINARY = REPO / "target" / "release" / "execute"
OPT_IN_ENV = "EXECUTE_OMARCHY_DISPOSABLE"
SESSION_MARKER_ENV = "EXECUTE_OMARCHY_SESSION_ID"
PROTOCOL = "execute.renderer"
PROTOCOL_VERSION = 1
FRAME_LIMIT = 1024 * 1024
WIDGET_LIMIT = 256
EVENT_LIMIT = 256
COMMAND_TIMEOUT = 3.0
RENDERER_LOG_TAIL_BYTES = 64 * 1024
IPC_DIAGNOSTIC_TAIL_BYTES = 8 * 1024
RENDERED_WIDGET_TEXT_LIMIT = 256
RENDERED_WIDGET_RESPONSE_LIMIT = 2048
RENDERED_WIDGET_TIMEOUT = 3.0
RENDERED_WIDGET_POLL_INTERVAL = 0.05
TEST_OBSERVER_IDENTITY = "execute.acceptance.rendered-widget-observer.v1"
TEST_OBSERVER_WIDGETS = {
    "desktop_demo::clock_text": "text",
    "desktop_demo::action_button": "button",
}


def capture_command_output_tail(payload: bytes) -> dict[str, Any]:
    tail = payload[-IPC_DIAGNOSTIC_TAIL_BYTES:]
    return {
        "status": "captured",
        "total_bytes": len(payload),
        "captured_bytes": len(tail),
        "truncated": len(payload) > len(tail),
        "tail_utf8": tail.decode("utf-8", "replace"),
    }


def build_test_observer_qml(qml_text: str, ipc_target: str, observer_target: str) -> str:
    """Add a private, read-only observer to the acceptance QML copy only."""
    target_marker = 'property string ipcTarget: "__EXECUTE_RENDERER_IPC_TARGET__"'
    repeater_anchor = "            Repeater {\n                model: widgetModel"
    dispatch_anchor = "    function dispatch(frameText: string): string {\n"
    observer_marker = f"// {TEST_OBSERVER_IDENTITY}"
    if qml_text.count(target_marker) != 1:
        raise RuntimeError("QML source must contain exactly one renderer IPC target placeholder")
    if qml_text.count(repeater_anchor) != 1 or qml_text.count(dispatch_anchor) != 1:
        raise RuntimeError("QML observer injection anchors must each occur exactly once")
    if observer_marker in qml_text or "acceptanceWidgetRepeater" in qml_text:
        raise RuntimeError("QML source already contains acceptance observer instrumentation")

    generated = qml_text.replace(
        target_marker,
        f'property string ipcTarget: {json.dumps(ipc_target)}',
        1,
    )
    generated = generated.replace(
        repeater_anchor,
        "            Repeater {\n                id: acceptanceWidgetRepeater\n                model: widgetModel",
        1,
    )
    observer_qml = "\n".join(
        [
            "    IpcHandler {",
            f"        target: {json.dumps(observer_target)}",
            "",
            "        function inspectRenderedWidget(widgetKey: string): string {",
            "            return root.inspectRenderedWidget(widgetKey);",
            "        }",
            "    }",
            "",
            f"    {observer_marker}",
            "    function inspectRenderedWidget(widgetKey: string): string {",
            '        const allowed = ["desktop_demo::clock_text", "desktop_demo::action_button"];',
            "        if (typeof widgetKey !== \"string\" || allowed.indexOf(widgetKey) < 0)",
            '            return JSON.stringify({status: "rejected", widget_key: ""});',
            "",
            "        for (let index = 0; index < widgetModel.count; index++) {",
            "            const loader = acceptanceWidgetRepeater.itemAt(index);",
            "            if (!loader || loader.widgetKey !== widgetKey)",
            "                continue;",
            "            if (!loader.item)",
            '                return JSON.stringify({status: "not_loaded", widget_key: widgetKey});',
            "",
            "            const renderedText = loader.item.text;",
            f"            if (typeof renderedText !== \"string\" || renderedText.length > {RENDERED_WIDGET_TEXT_LIMIT})",
            '                return JSON.stringify({status: "invalid_text", widget_key: widgetKey});',
            '            const renderedEnabled = loader.componentName === "button" ? loader.item.enabled : null;',
            '            if (loader.componentName === "button" && typeof renderedEnabled !== "boolean")',
            '                return JSON.stringify({status: "invalid_enabled", widget_key: widgetKey});',
            "            return JSON.stringify({",
            '                status: "loaded",',
            "                widget_key: widgetKey,",
            "                component: loader.componentName,",
            "                text: renderedText,",
            "                enabled: renderedEnabled",
            "            });",
            "        }",
            '        return JSON.stringify({status: "not_found", widget_key: widgetKey});',
            "    }",
            "",
            "",
        ]
    )
    return generated.replace(dispatch_anchor, observer_qml + dispatch_anchor, 1)


def decode_rendered_widget_response(payload: bytes, expected_key: str) -> dict[str, Any]:
    if len(payload) > RENDERED_WIDGET_RESPONSE_LIMIT:
        raise RuntimeError("rendered-widget observer response exceeds byte limit")
    try:
        response = json.loads(payload.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise RuntimeError(f"rendered-widget observer returned malformed JSON: {error}") from error
    if not isinstance(response, dict):
        raise RuntimeError("rendered-widget observer response must be an object")
    if response.get("widget_key") != expected_key:
        raise RuntimeError("rendered-widget observer response key did not match")
    status = response.get("status")
    if not isinstance(status, str):
        raise RuntimeError("rendered-widget observer response status must be a string")
    if status in {"not_loaded", "not_found"}:
        return {"status": status, "widget_key": expected_key}
    if status != "loaded":
        raise RuntimeError(f"rendered-widget observer failed with status {str(status)[:64]}")
    expected_component = TEST_OBSERVER_WIDGETS.get(expected_key)
    if expected_component is None or response.get("component") != expected_component:
        raise RuntimeError("rendered-widget observer returned an unsupported component")
    text = response.get("text")
    if not isinstance(text, str) or len(text) > RENDERED_WIDGET_TEXT_LIMIT:
        raise RuntimeError("rendered-widget observer returned invalid or oversized text")
    enabled = response.get("enabled")
    if expected_component == "button":
        if not isinstance(enabled, bool):
            raise RuntimeError("rendered button observer did not return a boolean enabled state")
    elif enabled is not None:
        raise RuntimeError("rendered text observer returned an unexpected enabled state")
    return {
        "status": "loaded",
        "widget_key": expected_key,
        "component": expected_component,
        "text": text,
        "enabled": enabled,
    }


def inspect_rendered_widget(
    binary: str,
    config_path: Path,
    observer_target: str,
    widget_key: str,
) -> dict[str, Any]:
    if widget_key not in TEST_OBSERVER_WIDGETS:
        raise RuntimeError("rendered-widget observer key is not in the fixed acceptance allowlist")
    command = [
        binary,
        "ipc",
        "--path",
        str(config_path),
        "call",
        observer_target,
        "inspectRenderedWidget",
        widget_key,
    ]
    try:
        completed = subprocess.run(
            command,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=RENDERED_WIDGET_TIMEOUT,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        raise RuntimeError(
            f"rendered-widget observer command failed: {type(error).__name__}: {str(error)[:240]}"
        ) from error
    if completed.returncode != 0:
        stdout = capture_command_output_tail(completed.stdout)["tail_utf8"]
        stderr = capture_command_output_tail(completed.stderr)["tail_utf8"]
        raise RuntimeError(
            f"rendered-widget observer exited {completed.returncode}; "
            f"stdout tail={stdout[-400:]!r}; stderr tail={stderr[-400:]!r}"
        )
    return decode_rendered_widget_response(completed.stdout, widget_key)


def wait_for_rendered_widget(
    binary: str,
    config_path: Path,
    observer_target: str,
    widget_key: str,
    *,
    text: str | None = None,
    enabled: bool | None = None,
    timeout: float = RENDERED_WIDGET_TIMEOUT,
) -> dict[str, Any]:
    deadline = time.monotonic() + timeout
    polls = 0
    last_actual: dict[str, Any] | None = None
    last_error: str | None = None
    while True:
        polls += 1
        try:
            actual = inspect_rendered_widget(binary, config_path, observer_target, widget_key)
            last_actual = actual
            last_error = None
            if actual.get("status") == "loaded":
                text_matches = text is None or actual.get("text") == text
                enabled_matches = enabled is None or actual.get("enabled") is enabled
                if text_matches and enabled_matches:
                    expected: dict[str, Any] = {}
                    if text is not None:
                        expected["text"] = text
                    if enabled is not None:
                        expected["enabled"] = enabled
                    return {
                        "status": "pass",
                        "widget_key": widget_key,
                        "expected": expected,
                        "actual": actual,
                        "polls": polls,
                    }
        except RuntimeError as error:
            last_error = str(error)[:512]
            break
        if time.monotonic() >= deadline:
            break
        time.sleep(min(RENDERED_WIDGET_POLL_INTERVAL, max(0.0, deadline - time.monotonic())))
    expected = {}
    if text is not None:
        expected["text"] = text
    if enabled is not None:
        expected["enabled"] = enabled
    return {
        "status": "fail",
        "widget_key": widget_key,
        "expected": expected,
        "actual": last_actual,
        "polls": polls,
        "error": last_error or "rendered child properties did not reach the expected state before the deadline",
    }


def sha256_file(path: Path) -> str | None:
    try:
        digest = hashlib.sha256()
        with path.open("rb") as source:
            for block in iter(lambda: source.read(1024 * 1024), b""):
                digest.update(block)
        return digest.hexdigest()
    except OSError:
        return None


def capture_renderer_log_tail(path: Path) -> dict[str, Any]:
    try:
        with path.open("rb") as source:
            total_bytes = os.fstat(source.fileno()).st_size
            offset = max(0, total_bytes - RENDERER_LOG_TAIL_BYTES)
            source.seek(offset)
            tail = source.read(RENDERER_LOG_TAIL_BYTES)
        return {
            "status": "captured",
            "path": path.name,
            "total_bytes": total_bytes,
            "captured_bytes": len(tail),
            "truncated": offset > 0,
            "tail_utf8": tail.decode("utf-8", "replace"),
        }
    except FileNotFoundError as error:
        return {
            "status": "missing",
            "path": path.name,
            "total_bytes": None,
            "captured_bytes": 0,
            "truncated": False,
            "error": {
                "type": type(error).__name__,
                "errno": error.errno,
                "message": error.strerror or str(error),
            },
        }
    except OSError as error:
        return {
            "status": "read_error",
            "path": path.name,
            "total_bytes": None,
            "captured_bytes": 0,
            "truncated": False,
            "error": {
                "type": type(error).__name__,
                "errno": error.errno,
                "message": error.strerror or str(error),
            },
        }


def source_hashes() -> dict[str, Any]:
    candidates = [
        Path("src/desktop.rs"),
        Path("desktop/quickshell/shell.qml"),
        Path("manifests/desktop.quickshell.json"),
        Path("manifests/desktop.clock_local.json"),
        Path("manifests/desktop.clock_remote.json"),
        Path("manifests/desktop.clock_format.json"),
        Path("manifests/desktop.fixture_sync.json"),
        Path("manifests/descriptors/desktop.clock_remote.descriptor.json"),
        Path("manifests/descriptors/desktop.fixture_sync.descriptor.json"),
        Path("examples/desktop/clock.program.in"),
        Path("examples/desktop/workers.py"),
        Path("examples/desktop/fixture_server.py"),
        Path("tests/fixtures/quickshell_protocol_mock.py"),
        Path("tools/omarchy_acceptance.py"),
        Path("Cargo.toml"),
        Path("Cargo.lock"),
    ]
    files: dict[str, str | None] = {}
    aggregate = hashlib.sha256()
    for relative in candidates:
        path = REPO / relative
        digest = sha256_file(path)
        files[str(relative)] = digest
        if digest is not None:
            aggregate.update(str(relative).encode("utf-8"))
            aggregate.update(b"\0")
            aggregate.update(digest.encode("ascii"))
            aggregate.update(b"\n")
    return {"files": files, "aggregate_sha256": aggregate.hexdigest()}


def command_version(command: list[str], timeout: float = 4.0) -> dict[str, Any]:
    try:
        completed = subprocess.run(
            command,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=timeout,
            check=False,
            text=True,
            encoding="utf-8",
            errors="replace",
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        return {"available": False, "error": str(error)}
    return {
        "available": completed.returncode == 0,
        "returncode": completed.returncode,
        "stdout": completed.stdout[:4000].strip(),
        "stderr": completed.stderr[:2000].strip(),
    }


def parse_hyprland_version(version_output: str) -> tuple[int, int, int]:
    match = re.search(r"(?m)^Hyprland\s+v?(\d+)\.(\d+)\.(\d+)(?:\s|$)", version_output)
    if match is None:
        raise ValueError("could not parse the Hyprland version string")
    return tuple(int(part) for part in match.groups())


def focus_hyprland_window(
    hyprctl: str,
    version_output: str,
    window_address: str,
    *,
    command_runner: Any = None,
) -> dict[str, Any]:
    """Focus one actual Hyprland client and verify it became active before input."""
    runner = command_runner or subprocess.run
    try:
        version_tuple = parse_hyprland_version(version_output)
    except ValueError as error:
        return {
            "status": "fail",
            "hyprland_version_output": version_output[:1000],
            "focus_form": "unsupported_version",
            "window_address": window_address,
            "reason": str(error),
        }

    version = ".".join(str(part) for part in version_tuple)
    if not re.fullmatch(r"0[xX][0-9a-fA-F]+", window_address):
        return {
            "status": "fail",
            "hyprland_version": version,
            "focus_form": "invalid_window_address",
            "window_address": window_address[:256],
            "reason": "Hyprland returned a window address outside the expected hexadecimal form",
        }

    if version_tuple >= (0, 55, 0):
        focus_form = "lua_dispatcher_object"
        focus_command = [
            hyprctl,
            "dispatch",
            f"hl.dsp.focus({{ window = 'address:{window_address}' }})",
        ]
    else:
        focus_form = "legacy_named_dispatcher"
        focus_command = [hyprctl, "dispatch", "focuswindow", f"address:{window_address}"]

    evidence: dict[str, Any] = {
        "status": "fail",
        "hyprland_version": version,
        "focus_form": focus_form,
        "focus_command": focus_command,
        "window_address": window_address,
    }
    try:
        focused = runner(
            focus_command,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=COMMAND_TIMEOUT,
            check=False,
            text=True,
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        evidence["reason"] = f"Hyprland focus command failed to run: {error}"
        return evidence

    evidence["focus_returncode"] = focused.returncode
    evidence["focus_stdout"] = focused.stdout.strip()[:1000]
    evidence["focus_stderr"] = focused.stderr.strip()[:1000]
    if focused.returncode != 0 or focused.stdout.strip() != "ok":
        evidence["reason"] = "Hyprland did not acknowledge the focus command with ok"
        return evidence

    active_command = [hyprctl, "-j", "activewindow"]
    evidence["active_window_command"] = active_command
    try:
        active = runner(
            active_command,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=COMMAND_TIMEOUT,
            check=False,
            text=True,
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        evidence["reason"] = f"Hyprland active-window query failed to run: {error}"
        return evidence

    evidence["active_window_returncode"] = active.returncode
    evidence["active_window_stderr"] = active.stderr.strip()[:1000]
    if active.returncode != 0:
        evidence["reason"] = "Hyprland active-window query failed"
        return evidence
    try:
        active_data = json.loads(active.stdout)
    except json.JSONDecodeError as error:
        evidence["reason"] = f"Hyprland active-window query returned invalid JSON: {error}"
        return evidence
    active_address = active_data.get("address") if isinstance(active_data, dict) else None
    evidence["active_window_address"] = active_address
    if active_address != window_address:
        evidence["reason"] = "Hyprland did not report the requested Quickshell window as active"
        return evidence

    evidence["active_window_verified"] = True
    evidence["status"] = "pass"
    return evidence


def read_os_release() -> dict[str, str]:
    result: dict[str, str] = {}
    try:
        for line in Path("/etc/os-release").read_text(encoding="utf-8").splitlines():
            if "=" not in line or line.startswith("#"):
                continue
            key, value = line.split("=", 1)
            result[key] = value.strip().strip('"')
    except OSError:
        pass
    return result


def check_runtime_dir() -> tuple[Path | None, str | None]:
    raw = os.environ.get("XDG_RUNTIME_DIR", "")
    if not raw:
        return None, "XDG_RUNTIME_DIR is unset"
    candidate = Path(raw)
    try:
        info = candidate.lstat()
    except OSError as error:
        return None, f"XDG_RUNTIME_DIR cannot be inspected: {error}"
    if stat.S_ISLNK(info.st_mode) or not stat.S_ISDIR(info.st_mode):
        return None, "XDG_RUNTIME_DIR must be a real directory, not a symlink"
    if info.st_uid != os.getuid():
        return None, "XDG_RUNTIME_DIR is not owned by the current user"
    if stat.S_IMODE(info.st_mode) & 0o077:
        return None, "XDG_RUNTIME_DIR permissions must exclude group and other users"
    try:
        return candidate.resolve(strict=True), None
    except OSError as error:
        return None, f"XDG_RUNTIME_DIR cannot be resolved: {error}"


def inspect_live_session(
    runtime_dir: Path,
    hyprctl: str,
    busctl: str,
    loginctl: str,
) -> tuple[dict[str, Any], list[str]]:
    """Read the active session surfaces without changing compositor state."""
    errors: list[str] = []
    evidence: dict[str, Any] = {
        "wayland_socket": {"status": "pending"},
        "hyprland": {"status": "pending"},
        "user_dbus": {"status": "pending"},
        "login_session": {"status": "pending"},
    }

    wayland_name = os.environ.get("WAYLAND_DISPLAY", "")
    wayland_path = Path(wayland_name)
    if not wayland_name:
        errors.append("WAYLAND_DISPLAY is unset")
    else:
        candidate = wayland_path if wayland_path.is_absolute() else runtime_dir / wayland_path
        try:
            resolved = candidate.resolve(strict=True)
            resolved.relative_to(runtime_dir)
            info = resolved.stat()
            if not stat.S_ISSOCK(info.st_mode) or info.st_uid != os.getuid():
                raise RuntimeError("Wayland socket must be a user-owned socket under XDG_RUNTIME_DIR")
            evidence["wayland_socket"] = {
                "status": "pass",
                "sha256": hashlib.sha256(os.fsencode(resolved.name)).hexdigest(),
            }
        except (OSError, RuntimeError, ValueError) as error:
            errors.append(f"Wayland socket validation failed: {error}")
            evidence["wayland_socket"] = {"status": "blocked", "reason": str(error)}

    signature = os.environ.get("HYPRLAND_INSTANCE_SIGNATURE", "")
    if not signature or any(character not in "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789_-" for character in signature):
        errors.append("HYPRLAND_INSTANCE_SIGNATURE is missing or malformed")
    else:
        hypr_directory = runtime_dir / "hypr" / signature
        try:
            info = hypr_directory.lstat()
            if stat.S_ISLNK(info.st_mode) or not stat.S_ISDIR(info.st_mode) or info.st_uid != os.getuid():
                raise RuntimeError("Hyprland runtime directory is not a user-owned real directory")
            monitors = subprocess.run(
                [hyprctl, "-j", "monitors"],
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                timeout=COMMAND_TIMEOUT,
                check=True,
                text=True,
            )
            monitor_data = json.loads(monitors.stdout)
            if not isinstance(monitor_data, list) or not monitor_data:
                raise RuntimeError("Hyprland reported no active monitors")
            evidence["hyprland"] = {
                "status": "pass",
                "active_monitors": len(monitor_data),
                "instance_signature_sha256": hashlib.sha256(signature.encode()).hexdigest(),
            }
        except (OSError, subprocess.SubprocessError, ValueError, RuntimeError) as error:
            errors.append(f"Hyprland session validation failed: {error}")
            evidence["hyprland"] = {"status": "blocked", "reason": str(error)}

    try:
        bus = subprocess.run(
            [busctl, "--user", "--no-pager", "--no-legend", "list"],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=COMMAND_TIMEOUT,
            check=True,
        )
        rows = [line for line in bus.stdout.splitlines() if line.strip()]
        evidence["user_dbus"] = {
            "status": "pass",
            "service_count": len(rows),
            "service_listing_sha256": hashlib.sha256(bus.stdout).hexdigest(),
        }
    except (OSError, subprocess.SubprocessError) as error:
        errors.append(f"user D-Bus validation failed: {error}")
        evidence["user_dbus"] = {"status": "blocked", "reason": str(error)}

    session_id = os.environ.get("XDG_SESSION_ID", "")
    try:
        session = subprocess.run(
            [loginctl, "show-session", session_id, "--no-pager", "--property=Type", "--property=Class", "--property=Remote", "--property=State", "--property=Seat"],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=COMMAND_TIMEOUT,
            check=True,
            text=True,
        )
        properties = dict(
            line.split("=", 1) for line in session.stdout.splitlines() if "=" in line
        )
        if properties.get("Type") != "wayland" or properties.get("Remote") != "no":
            raise RuntimeError("login session is not a local Wayland session")
        evidence["login_session"] = {
            "status": "pass",
            "type": properties.get("Type"),
            "class": properties.get("Class"),
            "remote": properties.get("Remote"),
            "state": properties.get("State"),
            "seat_present": bool(properties.get("Seat")),
            "session_id_sha256": hashlib.sha256(session_id.encode()).hexdigest(),
        }
    except (OSError, subprocess.SubprocessError, RuntimeError) as error:
        errors.append(f"login session validation failed: {error}")
        evidence["login_session"] = {"status": "blocked", "reason": str(error)}

    return evidence, errors


def base_report(binary: Path) -> dict[str, Any]:
    source = source_hashes()
    binary_hash = sha256_file(binary)
    return {
        "schema_version": 1,
        "harness": "execute.omarchy_acceptance",
        "status": "pending",
        "native_desktop_certification": "PENDING",
        "host": {
            "platform": platform.platform(),
            "python": sys.version.split()[0],
            "uid": os.getuid(),
        },
        "source": source,
        "binary": {
            "path": str(binary),
            "sha256": binary_hash,
            "exists": binary.is_file(),
        },
        "findings": {
            "A11": "implementation_present_native_certification_pending",
            "O01": "implementation_present_native_certification_pending",
            "O02": "implementation_present_native_certification_pending",
            "O03": "implementation_present_native_certification_pending",
            "O04": "implementation_present_native_certification_pending",
            "O05": "implementation_present_native_certification_pending",
            "O06": "partial_implementation_native_certification_pending",
            "O07": "implementation_present_native_certification_pending",
        },
        "checks": {},
        "pending_reasons": [],
    }


def pending_report(binary: Path, reason: str, *, opted_in: bool) -> dict[str, Any]:
    report = base_report(binary)
    report["checks"] = {
        "opt_in": {
            "status": "pass" if opted_in else "pending",
            "reason": "explicit disposable-session opt-in was supplied" if opted_in else reason,
        },
        "quickshell_wayland_smoke": {"status": "pending", "reason": reason},
        "applet_lifecycle": {"status": "pending", "reason": "no disposable Arch/Omarchy runner is available in this workspace"},
        "display_changes": {"status": "pending", "reason": "requires a real isolated display hotplug scenario"},
        "login_logout": {"status": "pending", "reason": "requires separate real login and logout sessions"},
        "compositor_reload": {"status": "pending", "reason": "requires a disposable live Hyprland reload scenario"},
        "session_teardown": {"status": "pending", "reason": "requires a real disposable session shutdown run"},
        "unattended_deployment": {"status": "pending", "reason": "requires package install, update, rollback, and recovery on the target OS"},
    }
    report["pending_reasons"] = [reason]
    return report


class RendererClient:
    def __init__(self, binary: str, process: subprocess.Popen[bytes], session_id: str, config_path: Path):
        self.binary = binary
        self.process = process
        self.session_id = session_id
        self.config_path = config_path
        self.ipc_target = f"executeDesktop-{session_id}"
        self.generation = 1
        self.version = 0
        self.request_id = 0
        self.revision = 0
        self.cursor = 0
        self.ipc_diagnostics: dict[str, Any] | None = None
        self._ipc_inventory_captured = False

    def frame(self, body: dict[str, Any], *, negotiation: bool = False, revision: int | None = None) -> dict[str, Any]:
        self.request_id += 1
        return {
            "protocol": PROTOCOL,
            "version": 0 if negotiation else self.version,
            "request_id": self.request_id,
            "session_id": self.session_id,
            "generation": self.generation,
            "revision": self.revision if revision is None else revision,
            "body": body,
        }

    def call(self, request: dict[str, Any], *, allow_error: bool = False) -> dict[str, Any]:
        request_body = request.get("body")
        is_negotiation = isinstance(request_body, dict) and request_body.get("kind") == "negotiate"
        request_id = request.get("request_id")
        request_kind = request_body.get("kind") if isinstance(request_body, dict) else None
        attempt: dict[str, Any] = {
            "request_id": request_id if type(request_id) is int and 0 <= request_id <= (1 << 63) - 1 else None,
            "request_kind": request_kind[:64] if isinstance(request_kind, str) else None,
            "response_kind": None,
            "status": "started",
        }
        if self.ipc_diagnostics is None:
            self.ipc_diagnostics = {}
        self.ipc_diagnostics["last_attempt"] = attempt
        if self.process.poll() is not None:
            attempt.update({"status": "process_exited", "returncode": self.process.returncode})
            raise RuntimeError(f"Quickshell process exited with status {self.process.returncode}")
        encoded = json.dumps(request, separators=(",", ":"), ensure_ascii=False)
        if len(encoded.encode("utf-8")) > FRAME_LIMIT:
            attempt.update({"status": "request_rejected", "failure": "frame_too_large"})
            raise RuntimeError("request exceeds frame size limit")
        command = [
            self.binary,
            "ipc",
            "--path",
            str(self.config_path),
            "call",
            self.ipc_target,
            "request",
            encoded,
        ]
        try:
            completed = subprocess.run(
                command,
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                timeout=COMMAND_TIMEOUT,
                check=False,
            )
        except (OSError, subprocess.TimeoutExpired) as error:
            attempt.update({
                "status": "command_timeout" if isinstance(error, subprocess.TimeoutExpired) else "command_error",
                "failure_type": type(error).__name__,
                "failure_detail": str(error)[:160],
            })
            raise RuntimeError(f"Quickshell IPC failed: {error}") from error
        # Before completion, call remains the previous completed result. Once
        # subprocess.run returns, both keys share this one canonical record.
        attempt.update({
            "argv": command[:-1] + ["<request frame omitted>"],
            "returncode": completed.returncode,
            "stdout": capture_command_output_tail(completed.stdout),
            "stderr": capture_command_output_tail(completed.stderr),
            "status": "command_failed" if completed.returncode != 0 else "response_pending",
        })
        self.ipc_diagnostics["call"] = attempt
        if completed.returncode != 0:
            if not is_negotiation:
                self.capture_ipc_inventory()
            stderr = attempt["stderr"]["tail_utf8"]
            stdout = attempt["stdout"]["tail_utf8"]
            details = []
            if stderr:
                details.append(f"stderr: {stderr[-1000:]}")
            if stdout:
                details.append(f"stdout: {stdout[-1000:]}")
            detail = "; ".join(details) if details else "stdout and stderr were empty"
            raise RuntimeError(f"Quickshell IPC exited {completed.returncode}: {detail}")
        if len(completed.stdout) > FRAME_LIMIT:
            attempt.update({"status": "invalid_response", "failure": "response_too_large"})
            raise RuntimeError("renderer response exceeds frame size limit")
        try:
            response = json.loads(completed.stdout.decode("utf-8"))
        except (UnicodeDecodeError, json.JSONDecodeError) as error:
            attempt.update({
                "status": "invalid_response",
                "failure": "invalid_json",
                "failure_detail": str(error)[:160],
            })
            raise RuntimeError(f"invalid Quickshell IPC response: {error}") from error
        if not isinstance(response, dict):
            attempt.update({"status": "invalid_response", "failure": "response_not_object"})
            raise RuntimeError("renderer response must be a JSON object")
        response_body = response.get("body")
        response_kind = response_body.get("kind") if isinstance(response_body, dict) else None
        attempt["response_kind"] = response_kind[:64] if isinstance(response_kind, str) else None
        attempt["status"] = "response_parsed"
        for field in ("protocol", "request_id", "session_id", "generation", "revision"):
            expected = request[field]
            if response.get(field) != expected:
                attempt.update({
                    "status": "response_mismatch",
                    "failure": "request_field_mismatch",
                    "failure_detail": field,
                })
                attempt["mismatch_field"] = field
                raise RuntimeError(f"renderer response {field} did not match request")
        if response.get("protocol") != PROTOCOL:
            attempt.update({
                "status": "response_mismatch",
                "failure": "unsupported_protocol",
                "failure_detail": "protocol",
            })
            attempt["mismatch_field"] = "protocol"
            raise RuntimeError("renderer response protocol did not match")
        if not is_negotiation and response.get("version") != self.version:
            attempt.update({
                "status": "response_mismatch",
                "failure": "version_mismatch",
                "failure_detail": "version",
            })
            attempt["mismatch_field"] = "version"
            raise RuntimeError("renderer response protocol version changed within the session")
        body = response.get("body")
        if not isinstance(body, dict):
            attempt.update({
                "status": "invalid_response",
                "failure": "response_body_missing",
            })
            raise RuntimeError("renderer response body is missing")
        if body.get("kind") == "error" and not allow_error:
            attempt.update({"status": "renderer_error", "failure": "renderer_error"})
            raise RuntimeError(f"renderer rejected request ({body.get('code')}): {body.get('message')}")
        attempt["status"] = "accepted"
        return response

    def negotiate(self) -> None:
        request = self.frame(
            {
                "kind": "negotiate",
                "supported": {"minimum": PROTOCOL_VERSION, "maximum": PROTOCOL_VERSION},
                "max_frame_bytes": FRAME_LIMIT,
                "max_widgets": WIDGET_LIMIT,
                "max_queued_events": EVENT_LIMIT,
            },
            negotiation=True,
            revision=0,
        )
        deadline = time.monotonic() + 8.0
        last_error: Exception | None = None
        while time.monotonic() < deadline:
            if self.process.poll() is not None:
                raise RuntimeError(f"Quickshell exited before IPC readiness ({self.process.returncode})")
            try:
                response = self.call(request, allow_error=True)
                body = response["body"]
                if body.get("kind") == "error":
                    raise RuntimeError(f"renderer negotiation failed: {body.get('code')}: {body.get('message')}")
                if body.get("kind") != "negotiated" or response.get("version") != PROTOCOL_VERSION:
                    raise RuntimeError("renderer returned an invalid negotiation response")
                self.version = response["version"]
                return
            except Exception as error:  # startup retries are restricted to idempotent negotiation
                last_error = error
                time.sleep(0.08)
        if self.ipc_diagnostics is not None:
            self.capture_ipc_inventory()
        raise RuntimeError(f"Quickshell IPC did not become ready: {last_error}")

    def capture_ipc_inventory(self) -> None:
        if self._ipc_inventory_captured or self.ipc_diagnostics is None:
            return
        self._ipc_inventory_captured = True
        command = [self.binary, "ipc", "--path", str(self.config_path), "show"]
        try:
            completed = subprocess.run(
                command,
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                timeout=COMMAND_TIMEOUT,
                check=False,
            )
            inventory: dict[str, Any] = {
                "status": "captured",
                "argv": command,
                "returncode": completed.returncode,
                "stdout": capture_command_output_tail(completed.stdout),
                "stderr": capture_command_output_tail(completed.stderr),
            }
        except (OSError, subprocess.TimeoutExpired) as error:
            inventory = {
                "status": "command_error",
                "argv": command,
                "error": {
                    "type": type(error).__name__,
                    "message": str(error),
                },
            }
            if isinstance(error, subprocess.TimeoutExpired):
                if isinstance(error.stdout, bytes):
                    inventory["stdout"] = capture_command_output_tail(error.stdout)
                if isinstance(error.stderr, bytes):
                    inventory["stderr"] = capture_command_output_tail(error.stderr)
        self.ipc_diagnostics["inventory"] = inventory

    def mutation(self, body: dict[str, Any]) -> tuple[dict[str, Any], int]:
        revision = self.revision + 1
        request = self.frame(body, revision=revision)
        response = self.call(request)
        if response["body"].get("kind") != "applied":
            raise RuntimeError("renderer mutation did not receive an applied acknowledgement")
        self.revision = revision
        return response, revision

    def poll(self, limit: int = 16) -> list[dict[str, Any]]:
        request = self.frame({"kind": "poll_events", "after_cursor": self.cursor, "limit": limit})
        response = self.call(request)
        body = response["body"]
        if body.get("kind") != "event_batch" or body.get("overflow"):
            raise RuntimeError("renderer event queue overflowed or returned an invalid batch")
        events = body.get("events")
        next_cursor = body.get("next_cursor")
        if not isinstance(events, list) or not isinstance(next_cursor, int) or next_cursor < self.cursor:
            raise RuntimeError("renderer event cursor was invalid")
        if events:
            ack_request = self.frame({"kind": "ack_events", "through_cursor": next_cursor})
            ack = self.call(ack_request)
            if ack["body"].get("kind") != "applied":
                raise RuntimeError("renderer did not acknowledge the event cursor")
            self.cursor = next_cursor
        return events


def build_contract(component: str) -> dict[str, Any]:
    if component == "button":
        return {
            "component": "button",
            "properties": {
                "schema": {
                    "type": "record",
                    "fields": {"text": {"type": "text"}, "enabled": {"type": "boolean"}},
                    "required": ["text", "enabled"],
                },
                "delivery": "single",
                "required": False,
            },
            "signals": {
                "clicked": {
                    "schema": {"type": "record", "fields": {}, "required": []},
                    "delivery": "events",
                    "required": False,
                }
            },
        }
    return {
        "component": "text",
        "properties": {
            "schema": {"type": "record", "fields": {"text": {"type": "text"}}, "required": ["text"]},
            "delivery": "single",
            "required": False,
        },
        "signals": {},
    }


def start_quickshell(qs: str, config: Path, log_path: Path, environment: dict[str, str]) -> tuple[subprocess.Popen[bytes], Any, Any]:
    stdout = log_path.with_suffix(".stdout.log").open("wb")
    stderr = log_path.with_suffix(".stderr.log").open("wb")
    try:
        process = subprocess.Popen(
            [qs, "--path", str(config)],
            stdin=subprocess.DEVNULL,
            stdout=stdout,
            stderr=stderr,
            env=environment,
            start_new_session=True,
        )
    except Exception:
        for handle in (stdout, stderr):
            try:
                handle.close()
            except OSError:
                pass
        raise
    return process, stdout, stderr


def stop_process(process: subprocess.Popen[bytes] | None) -> dict[str, Any]:
    if process is None:
        return {"status": "not_started"}
    if process.poll() is not None:
        return {"status": "already_exited", "returncode": process.returncode}
    try:
        os.killpg(process.pid, signal.SIGTERM)
        process.wait(timeout=3.0)
        return {"status": "terminated", "returncode": process.returncode, "pid": process.pid}
    except subprocess.TimeoutExpired:
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        process.wait(timeout=3.0)
        return {"status": "killed_after_timeout", "returncode": process.returncode, "pid": process.pid}
    except ProcessLookupError:
        process.wait(timeout=3.0)
        return {"status": "exited_during_cleanup", "returncode": process.returncode, "pid": process.pid}


def invoke_live(binary: Path, report: dict[str, Any]) -> None:
    os_release = read_os_release()
    marker = os.environ.get(SESSION_MARKER_ENV, "")
    report["target"] = {
        "os_release_id": os_release.get("ID"),
        "os_release_version": os_release.get("VERSION_ID"),
        "session_type": os.environ.get("XDG_SESSION_TYPE"),
        "wayland_display_present": bool(os.environ.get("WAYLAND_DISPLAY")),
        "hyprland_signature_present": bool(os.environ.get("HYPRLAND_INSTANCE_SIGNATURE")),
        "dbus_session_present": bool(os.environ.get("DBUS_SESSION_BUS_ADDRESS")),
        "xdg_session_id_present": bool(os.environ.get("XDG_SESSION_ID")),
        "disposable_session_marker_sha256": hashlib.sha256(marker.encode()).hexdigest() if marker else None,
        "isolation_attestation": "operator marker supplied; harness cannot independently prove VM/session isolation",
    }
    runtime_dir, runtime_error = check_runtime_dir()
    required_errors: list[str] = []
    if os.getuid() == 0:
        required_errors.append("refusing to run as root")
    if os_release.get("ID") != "arch":
        required_errors.append("target operating system must identify as Arch Linux")
    if os.environ.get("XDG_SESSION_TYPE") != "wayland":
        required_errors.append("XDG_SESSION_TYPE must be wayland")
    for key in ("WAYLAND_DISPLAY", "HYPRLAND_INSTANCE_SIGNATURE", "DBUS_SESSION_BUS_ADDRESS", "XDG_SESSION_ID"):
        if not os.environ.get(key):
            required_errors.append(f"required isolated session variable {key} is missing")
    if runtime_error:
        required_errors.append(runtime_error)
    if not binary.is_file() or not os.access(binary, os.X_OK):
        required_errors.append("candidate execute binary is missing or not executable")

    qs_path = shutil.which("qs")
    hyprctl_path = shutil.which("hyprctl")
    busctl_path = shutil.which("busctl")
    loginctl_path = shutil.which("loginctl")
    tool_checks = {
        "qs": command_version([qs_path, "--version"]) if qs_path else {"available": False, "error": "qs not found"},
        "hyprctl": command_version([hyprctl_path, "version"]) if hyprctl_path else {"available": False, "error": "hyprctl not found"},
        "busctl": command_version([busctl_path, "--version"]) if busctl_path else {"available": False, "error": "busctl not found"},
        "loginctl": command_version([loginctl_path, "--version"]) if loginctl_path else {"available": False, "error": "loginctl not found"},
    }
    report["tool_versions"] = tool_checks
    for name, details in tool_checks.items():
        if not details.get("available"):
            required_errors.append(f"required target-session tool {name} is unavailable")
    if not marker.startswith("disposable-") or any(
        character not in "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789_-" for character in marker
    ):
        required_errors.append(f"{SESSION_MARKER_ENV} must be a unique disposable- prefixed identifier")

    if not required_errors:
        session_evidence, session_errors = inspect_live_session(
            runtime_dir,
            hyprctl_path,
            busctl_path,
            loginctl_path,
        )
        report["session_evidence"] = session_evidence
        required_errors.extend(session_errors)
    if required_errors:
        report["status"] = "pending"
        report["checks"] = {
            "opt_in": {"status": "pass", "reason": "disposable runner marker and command-line opt-in were supplied"},
            "target_preflight": {"status": "blocked", "reasons": required_errors},
            "quickshell_wayland_smoke": {"status": "pending", "reason": "preflight did not establish a disposable Arch/Hyprland session"},
            "applet_lifecycle": {"status": "pending", "reason": "preflight did not establish a disposable Arch/Hyprland session"},
            "display_changes": {"status": "pending", "reason": "requires a real isolated display hotplug scenario"},
            "login_logout": {"status": "pending", "reason": "requires separate real login and logout sessions"},
            "compositor_reload": {"status": "pending", "reason": "requires a disposable live Hyprland reload scenario"},
            "session_teardown": {"status": "pending", "reason": "no owned renderer process was started"},
            "unattended_deployment": {"status": "pending", "reason": "requires package install, update, rollback, and recovery on the target OS"},
        }
        report["pending_reasons"] = required_errors
        return

    assert runtime_dir is not None
    run_id = uuid.uuid4().hex
    temp_path: Path | None = None
    process: subprocess.Popen[bytes] | None = None
    client: RendererClient | None = None
    process_logs: list[Any] = []
    renderer_log_paths: list[tuple[str, Path, Path]] = []
    outcomes: dict[str, Any] = {}
    cleanup: dict[str, Any] = {"status": "not_started"}
    try:
        temp_path = Path(tempfile.mkdtemp(prefix=f"execute-omarchy-{run_id[:10]}-", dir=runtime_dir))
        os.chmod(temp_path, 0o700)
        temp_info = temp_path.lstat()
        if temp_info.st_uid != os.getuid() or stat.S_IMODE(temp_info.st_mode) != 0o700:
            raise RuntimeError("owned temporary directory has unexpected owner or mode")
        qml_source = REPO / "desktop" / "quickshell" / "shell.qml"
        renderer_session_id = f"accept-{run_id[:16]}"
        ipc_target = f"executeDesktop-{renderer_session_id}"
        observer_target = f"executeDesktopObserver-{renderer_session_id}"
        qml_text = qml_source.read_text(encoding="utf-8")
        generated_qml = build_test_observer_qml(qml_text, ipc_target, observer_target)
        qml_copy = temp_path / "shell.qml"
        qml_copy.write_text(generated_qml, encoding="utf-8")
        os.chmod(qml_copy, 0o600)
        report["renderer_launch"] = {
            "argv": [qs_path, "--path", str(qml_copy)],
            "source_qml_sha256": sha256_file(qml_source),
            "launched_qml_sha256": sha256_file(qml_copy),
            "test_observer": {
                "identity": TEST_OBSERVER_IDENTITY,
                "target": observer_target,
                "method": "inspectRenderedWidget",
                "widget_keys": sorted(TEST_OBSERVER_WIDGETS),
                "response_limit_bytes": RENDERED_WIDGET_RESPONSE_LIMIT,
                "text_limit_characters": RENDERED_WIDGET_TEXT_LIMIT,
                "injection": "private QML copy: fixed read-only IpcHandler and Repeater.itemAt(index).item property lookup",
            },
        }
        logs = temp_path / "logs"
        logs.mkdir(mode=0o700)
        isolated_env = os.environ.copy()
        isolated_env.update(
            {
                "XDG_CONFIG_HOME": str(temp_path / "config"),
                "XDG_CACHE_HOME": str(temp_path / "cache"),
                "XDG_STATE_HOME": str(temp_path / "state"),
                "XDG_DATA_HOME": str(temp_path / "data"),
                "QS_DISABLE_FILE_WATCHER": "1",
            }
        )
        for key in ("XDG_CONFIG_HOME", "XDG_CACHE_HOME", "XDG_STATE_HOME", "XDG_DATA_HOME"):
            Path(isolated_env[key]).mkdir(mode=0o700)
        for directory in temp_path.iterdir():
            os.chmod(directory, 0o700)

        renderer_log_paths.append(
            ("initial", logs / "renderer.stdout.log", logs / "renderer.stderr.log")
        )
        process, stdout_log, stderr_log = start_quickshell(qs_path, qml_copy, logs / "renderer", isolated_env)
        process_logs.extend((stdout_log, stderr_log))
        client = RendererClient(qs_path, process, renderer_session_id, qml_copy)
        client.negotiate()
        outcomes["protocol_negotiation"] = {"status": "pass", "version": client.version, "generation": client.generation}

        def verify_rendered_widget(
            name: str,
            widget_key: str,
            *,
            text: str | None = None,
            enabled: bool | None = None,
        ) -> dict[str, Any]:
            result = wait_for_rendered_widget(
                qs_path,
                qml_copy,
                observer_target,
                widget_key,
                text=text,
                enabled=enabled,
            )
            outcomes[name] = result
            if result["status"] != "pass":
                raise RuntimeError(f"rendered child property proof failed for {widget_key}: {result.get('error')}")
            return result

        text_contract = build_contract("text")
        button_contract = build_contract("button")
        _, text_revision = client.mutation(
            {"kind": "instantiate", "applet_id": "desktop_demo", "widget_id": "clock_text", "contract": text_contract, "properties": {"text": "09:41"}}
        )
        verify_rendered_widget(
            "rendered_clock_create",
            "desktop_demo::clock_text",
            text="09:41",
        )
        _, patch_revision = client.mutation(
            {"kind": "patch", "applet_id": "desktop_demo", "widget_id": "clock_text", "properties": {"text": "09:42"}}
        )
        verify_rendered_widget(
            "rendered_clock_patch",
            "desktop_demo::clock_text",
            text="09:42",
        )
        _, destroy_revision = client.mutation(
            {"kind": "destroy", "applet_id": "desktop_demo", "widget_id": "clock_text"}
        )
        _, button_revision = client.mutation(
            {"kind": "instantiate", "applet_id": "desktop_demo", "widget_id": "action_button", "contract": button_contract, "properties": {"text": "Acknowledge", "enabled": True}}
        )
        verify_rendered_widget(
            "rendered_button_create_enabled",
            "desktop_demo::action_button",
            text="Acknowledge",
            enabled=True,
        )
        _, disabled_revision = client.mutation(
            {"kind": "patch", "applet_id": "desktop_demo", "widget_id": "action_button", "properties": {"enabled": False}}
        )
        verify_rendered_widget(
            "rendered_button_disabled",
            "desktop_demo::action_button",
            enabled=False,
        )
        outcomes["widget_create_update_destroy"] = {
            "status": "pass",
            "text_revision": text_revision,
            "patch_revision": patch_revision,
            "button_revision": button_revision,
            "destroy_revision": destroy_revision,
            "disabled_revision": disabled_revision,
        }

        click_path = shutil.which("wtype")
        if not click_path:
            outcomes["quickshell_button_event"] = {
                "status": "fail",
                "reason": "wtype is unavailable; enabled/disabled real-input proof cannot run",
            }
            raise RuntimeError("wtype is required for the enabled/disabled actual-input acceptance proof")

        clients = subprocess.run(
            [hyprctl_path, "clients", "-j"],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=COMMAND_TIMEOUT,
            check=True,
            text=True,
        )
        windows = json.loads(clients.stdout)
        match = next((window for window in windows if window.get("title") == "Execute generic renderer"), None)
        if match is None or not match.get("address"):
            raise RuntimeError("Quickshell surface was not found in Hyprland client list")
        window_address = str(match["address"])

        focus_attempts: list[dict[str, Any]] = []

        def focus_renderer() -> dict[str, Any]:
            evidence = focus_hyprland_window(
                hyprctl_path,
                tool_checks["hyprctl"]["stdout"],
                window_address,
            )
            focus_attempts.append(evidence)
            outcomes["hyprland_window_focus"] = {
                "status": evidence["status"],
                "attempts": focus_attempts.copy(),
            }
            if evidence["status"] != "pass":
                raise RuntimeError(f"Hyprland focus was not verified: {evidence.get('reason', 'unknown failure')}")
            return evidence

        def send_tab_return() -> dict[str, Any]:
            command = [click_path, "-k", "Tab", "-k", "Return"]
            completed = subprocess.run(
                command,
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                timeout=COMMAND_TIMEOUT,
                check=False,
            )
            evidence = {"argv": command, "returncode": completed.returncode}
            if completed.returncode != 0:
                stdout = capture_command_output_tail(completed.stdout)["tail_utf8"][-400:]
                stderr = capture_command_output_tail(completed.stderr)["tail_utf8"][-400:]
                raise RuntimeError(
                    f"wtype keyboard input failed ({completed.returncode}); "
                    f"stdout tail={stdout!r}; stderr tail={stderr!r}"
                )
            return evidence

        focus_renderer()
        disabled_input_command = send_tab_return()
        disabled_events: list[dict[str, Any]] = []
        disabled_deadline = time.monotonic() + 1.0
        while time.monotonic() < disabled_deadline:
            disabled_events.extend(client.poll())
            if time.monotonic() < disabled_deadline:
                time.sleep(0.1)
        unexpected_disabled_clicks = [
            event
            for event in disabled_events
            if event.get("applet_id") == "desktop_demo"
            and event.get("widget_id") == "action_button"
            and event.get("signal") == "clicked"
        ]
        disabled_input_status = {
            "status": "pass" if not unexpected_disabled_clicks else "fail",
            "input": ["Tab", "Return"],
            "enabled": False,
            "source_revision": disabled_revision,
            "command": disabled_input_command,
            "observed_event_count": len(disabled_events),
            "unexpected_button_clicks": unexpected_disabled_clicks[:4],
        }
        outcomes["disabled_button_input"] = disabled_input_status

        _, enabled_revision = client.mutation(
            {"kind": "patch", "applet_id": "desktop_demo", "widget_id": "action_button", "properties": {"enabled": True}}
        )
        verify_rendered_widget(
            "rendered_button_reenabled",
            "desktop_demo::action_button",
            enabled=True,
        )
        focus_renderer()
        enabled_input_command = send_tab_return()
        positive_deadline = time.monotonic() + 5.0
        positive_events: list[dict[str, Any]] = []
        matched_event: dict[str, Any] | None = None
        while time.monotonic() < positive_deadline and matched_event is None:
            positive_events.extend(client.poll())
            matched_event = next(
                (
                    event
                    for event in positive_events
                    if event.get("applet_id") == "desktop_demo"
                    and event.get("widget_id") == "action_button"
                    and event.get("signal") == "clicked"
                    and event.get("generation") == client.generation
                    and event.get("source_revision") == enabled_revision
                    and event.get("payload") == {}
                ),
                None,
            )
            if matched_event is None:
                time.sleep(0.1)
        wrong_revision_clicks = [
            event
            for event in positive_events
            if event.get("applet_id") == "desktop_demo"
            and event.get("widget_id") == "action_button"
            and event.get("signal") == "clicked"
            and event is not matched_event
        ]
        click_status = {
            "status": "pass" if matched_event is not None and not unexpected_disabled_clicks and not wrong_revision_clicks else "fail",
            "input": ["Tab", "Return"],
            "disabled_source_revision": disabled_revision,
            "enabled_source_revision": enabled_revision,
            "source_revision": matched_event.get("source_revision") if matched_event is not None else None,
            "cursor": matched_event.get("cursor") if matched_event is not None else None,
            "command": enabled_input_command,
            "disabled_input": disabled_input_status,
            "wrong_revision_clicks": wrong_revision_clicks[:4],
        }
        outcomes["quickshell_button_event"] = click_status
        if click_status["status"] != "pass":
            raise RuntimeError("enabled/disabled actual Wayland input did not produce the expected typed click event")

        restart_widget = {"applet_id": "desktop_demo", "widget_id": "restart_text", "contract": text_contract, "properties": {"text": "replayed"}}
        _, first_generation_revision = client.mutation({"kind": "instantiate", **restart_widget})
        cleanup_first = stop_process(process)
        process = None
        for handle in process_logs:
            handle.close()
        process_logs.clear()

        client.generation += 1
        renderer_log_paths.append(
            ("restart", logs / "renderer-restart.stdout.log", logs / "renderer-restart.stderr.log")
        )
        process, stdout_log, stderr_log = start_quickshell(qs_path, qml_copy, logs / "renderer-restart", isolated_env)
        process_logs.extend((stdout_log, stderr_log))
        client.process = process
        client.version = 0
        client.revision = first_generation_revision
        client.negotiate()
        _, replay_revision = client.mutation({"kind": "instantiate", **restart_widget})
        if replay_revision <= first_generation_revision:
            raise RuntimeError("renderer replay did not advance the widget revision")
        outcomes["renderer_restart_replay"] = {
            "status": "pass",
            "old_generation": 1,
            "new_generation": client.generation,
            "revision_before_restart": first_generation_revision,
            "replayed_revision": replay_revision,
            "first_process_cleanup": cleanup_first,
        }
        stale_request = client.frame(
            {"kind": "poll_events", "after_cursor": 0, "limit": 1},
            revision=client.revision,
        )
        stale_request["generation"] = 1
        stale_response = client.call(stale_request, allow_error=True)
        if stale_response["body"].get("kind") != "error" or stale_response["body"].get("code") != "stale_session":
            raise RuntimeError("restarted renderer did not reject an old generation frame")
        outcomes["stale_generation_rejected"] = {"status": "pass", "old_generation": 1, "current_generation": client.generation}

        _, final_destroy_revision = client.mutation({"kind": "destroy", "applet_id": "desktop_demo", "widget_id": "restart_text"})
        outcomes["widget_destroy_after_restart"] = {"status": "pass", "revision": final_destroy_revision}
        report["run_id"] = run_id
        report["checks"] = {
            "opt_in": {"status": "pass", "reason": "explicit disposable-session marker and flag were supplied"},
            "target_preflight": {"status": "pass", "reason": "Arch, Wayland, Hyprland, D-Bus, and owned runtime directory were verified"},
            "quickshell_wayland_smoke": {
                "status": "pass",
                "results": [
                    "negotiation",
                    "create/update with actual Loader child text observed as 09:41 then 09:42",
                    "button child enabled observed true, false, then true",
                    "disabled real keyboard input emitted no button event",
                    "reenabled real keyboard input emitted the typed button event at the final enabled revision",
                    "destroy",
                    "renderer restart replay",
                    "old generation rejected",
                ],
            },
            "applet_lifecycle": {
                "status": "partial",
                "results": ["widget create/update/destroy", "rendered child property transitions", "renderer restart"],
                "reason": "full workflow and login/logout remain pending",
            },
            "button_signal": click_status,
            "display_changes": {"status": "pending", "reason": "requires a real isolated display hotplug scenario"},
            "login_logout": {"status": "pending", "reason": "requires separate real login and logout sessions"},
            "compositor_reload": {"status": "pending", "reason": "requires a disposable live Hyprland reload scenario"},
            "session_teardown": {"status": "pending", "reason": "owned renderer teardown is checked below; full desktop session teardown remains untested"},
            "unattended_deployment": {"status": "pending", "reason": "requires package install, update, rollback, and recovery on the target OS"},
        }
        report["results"] = outcomes
        report["status"] = "completed_with_pending_gates"
        report["pending_reasons"] = [
            "live smoke evidence does not certify display hotplug, login/logout, compositor reload, session teardown, or unattended deployment",
            "native desktop certification remains PENDING until all named Omarchy scenarios run in an isolated target environment",
        ]
    except Exception as error:
        report["status"] = "failed"
        report["results"] = outcomes
        report["failure"] = str(error)
        report["pending_reasons"] = ["native desktop certification remains PENDING; live session smoke failed or stopped early"]
    finally:
        final_stop = stop_process(process)
        for handle in process_logs:
            try:
                handle.close()
            except OSError:
                pass
        report["cleanup"] = {"renderer": final_stop}
        report["renderer_diagnostics"] = {
            "tail_limit_bytes_per_stream": RENDERER_LOG_TAIL_BYTES,
            "logs": [
                {
                    "stage": stage,
                    "stdout": capture_renderer_log_tail(stdout_path),
                    "stderr": capture_renderer_log_tail(stderr_path),
                }
                for stage, stdout_path, stderr_path in renderer_log_paths
            ],
        }
        if client is not None and client.ipc_diagnostics is not None:
            report["renderer_ipc_diagnostics"] = client.ipc_diagnostics
        if temp_path is not None:
            try:
                parent = temp_path.parent.resolve(strict=True)
                info = temp_path.lstat()
                safe_owned_temp = (
                    parent == runtime_dir
                    and temp_path.name.startswith("execute-omarchy-")
                    and stat.S_ISDIR(info.st_mode)
                    and not stat.S_ISLNK(info.st_mode)
                    and info.st_uid == os.getuid()
                )
                if safe_owned_temp:
                    shutil.rmtree(temp_path)
                    report["cleanup"]["temporary_directory"] = {
                        "status": "removed",
                        "path_sha256": hashlib.sha256(os.fsencode(temp_path)).hexdigest(),
                    }
                else:
                    report["cleanup"]["temporary_directory"] = {
                        "status": "pending",
                        "reason": "temporary path ownership or containment check failed",
                    }
            except OSError as error:
                report["cleanup"]["temporary_directory"] = {"status": "failed", "reason": str(error)}
        else:
            report["cleanup"]["temporary_directory"] = {"status": "not_created"}


def emit(report: dict[str, Any], evidence: Path | None) -> None:
    if evidence is not None:
        if evidence.exists():
            report["evidence_write"] = {"status": "failed", "reason": "evidence destination already exists"}
        else:
            report["evidence_write"] = {"status": "written", "path": str(evidence)}
            try:
                evidence.parent.mkdir(parents=True, exist_ok=True)
                rendered = json.dumps(report, indent=2, sort_keys=True)
                descriptor = os.open(evidence, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
                with os.fdopen(descriptor, "w", encoding="utf-8") as destination:
                    destination.write(rendered)
                    destination.write("\n")
            except OSError as error:
                report["evidence_write"] = {"status": "failed", "reason": str(error)}
    print(json.dumps(report, indent=2, sort_keys=True))


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--run-in-disposable-omarchy",
        action="store_true",
        help="allow the live smoke only after the disposable-session environment marker is present",
    )
    parser.add_argument("--binary", type=Path, default=DEFAULT_BINARY, help="candidate execute binary to fingerprint")
    parser.add_argument("--evidence", type=Path, help="write a machine-readable JSON copy to a new path")
    args = parser.parse_args()

    report = base_report(args.binary)
    if not args.run_in_disposable_omarchy:
        report = pending_report(
            args.binary,
            "live acceptance is opt-in and this invocation did not name a disposable Arch/Omarchy session",
            opted_in=False,
        )
        emit(report, args.evidence)
        return 0
    if os.environ.get(OPT_IN_ENV) != "1" or not os.environ.get(SESSION_MARKER_ENV):
        report = pending_report(
            args.binary,
            f"set {OPT_IN_ENV}=1 and {SESSION_MARKER_ENV} to a unique disposable-session label only inside the isolated runner",
            opted_in=False,
        )
        emit(report, args.evidence)
        return 0

    invoke_live(args.binary.resolve(), report)
    emit(report, args.evidence)
    return 0 if report.get("status") != "failed" else 1


if __name__ == "__main__":
    raise SystemExit(main())
