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


def sha256_file(path: Path) -> str | None:
    try:
        digest = hashlib.sha256()
        with path.open("rb") as source:
            for block in iter(lambda: source.read(1024 * 1024), b""):
                digest.update(block)
        return digest.hexdigest()
    except OSError:
        return None


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
    def __init__(self, binary: str, process: subprocess.Popen[bytes], session_id: str):
        self.binary = binary
        self.process = process
        self.session_id = session_id
        self.ipc_target = f"executeDesktop-{session_id}"
        self.generation = 1
        self.version = 0
        self.request_id = 0
        self.revision = 0
        self.cursor = 0

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
        if self.process.poll() is not None:
            raise RuntimeError(f"Quickshell process exited with status {self.process.returncode}")
        encoded = json.dumps(request, separators=(",", ":"), ensure_ascii=False)
        if len(encoded.encode("utf-8")) > FRAME_LIMIT:
            raise RuntimeError("request exceeds frame size limit")
        try:
            completed = subprocess.run(
                [self.binary, "ipc", "call", self.ipc_target, "request", encoded],
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                timeout=COMMAND_TIMEOUT,
                check=False,
            )
        except (OSError, subprocess.TimeoutExpired) as error:
            raise RuntimeError(f"Quickshell IPC failed: {error}") from error
        if len(completed.stdout) > FRAME_LIMIT:
            raise RuntimeError("renderer response exceeds frame size limit")
        if completed.returncode != 0:
            detail = completed.stderr.decode("utf-8", "replace")[:1000]
            raise RuntimeError(f"Quickshell IPC exited {completed.returncode}: {detail}")
        try:
            response = json.loads(completed.stdout.decode("utf-8"))
        except (UnicodeDecodeError, json.JSONDecodeError) as error:
            raise RuntimeError(f"invalid Quickshell IPC response: {error}") from error
        for field in ("protocol", "request_id", "session_id", "generation", "revision"):
            expected = request[field]
            if response.get(field) != expected:
                raise RuntimeError(f"renderer response {field} did not match request")
        if response.get("protocol") != PROTOCOL:
            raise RuntimeError("renderer response protocol did not match")
        if not negotiation and response.get("version") != self.version:
            raise RuntimeError("renderer response protocol version changed within the session")
        body = response.get("body")
        if not isinstance(body, dict):
            raise RuntimeError("renderer response body is missing")
        if body.get("kind") == "error" and not allow_error:
            raise RuntimeError(f"renderer rejected request ({body.get('code')}): {body.get('message')}")
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
        raise RuntimeError(f"Quickshell IPC did not become ready: {last_error}")

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
    process = subprocess.Popen(
        [qs, "--path", str(config)],
        stdin=subprocess.DEVNULL,
        stdout=stdout,
        stderr=stderr,
        env=environment,
        start_new_session=True,
    )
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
    process_logs: list[Any] = []
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
        qml_text = qml_source.read_text(encoding="utf-8")
        marker = 'property string ipcTarget: "__EXECUTE_RENDERER_IPC_TARGET__"'
        if qml_text.count(marker) != 1:
            raise RuntimeError("QML source must contain exactly one renderer IPC target placeholder")
        qml_copy = temp_path / "shell.qml"
        qml_copy.write_text(
            qml_text.replace(marker, f"property string ipcTarget: {json.dumps(ipc_target)}"),
            encoding="utf-8",
        )
        os.chmod(qml_copy, 0o600)
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

        process, stdout_log, stderr_log = start_quickshell(qs_path, qml_copy, logs / "renderer", isolated_env)
        process_logs.extend((stdout_log, stderr_log))
        client = RendererClient(qs_path, process, renderer_session_id)
        client.negotiate()
        outcomes["protocol_negotiation"] = {"status": "pass", "version": client.version, "generation": client.generation}

        text_contract = build_contract("text")
        button_contract = build_contract("button")
        _, text_revision = client.mutation(
            {"kind": "instantiate", "applet_id": "desktop_demo", "widget_id": "clock_text", "contract": text_contract, "properties": {"text": "09:41"}}
        )
        _, patch_revision = client.mutation(
            {"kind": "patch", "applet_id": "desktop_demo", "widget_id": "clock_text", "properties": {"text": "09:42"}}
        )
        _, button_revision = client.mutation(
            {"kind": "instantiate", "applet_id": "desktop_demo", "widget_id": "action_button", "contract": button_contract, "properties": {"text": "Acknowledge", "enabled": True}}
        )
        _, destroy_revision = client.mutation(
            {"kind": "destroy", "applet_id": "desktop_demo", "widget_id": "clock_text"}
        )
        outcomes["widget_create_update_destroy"] = {
            "status": "pass",
            "text_revision": text_revision,
            "patch_revision": patch_revision,
            "button_revision": button_revision,
            "destroy_revision": destroy_revision,
        }

        click_path = shutil.which("wtype")
        click_status = {"status": "pending", "reason": "wtype is unavailable; physical button event was not exercised"}
        if click_path:
            try:
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
                focus_evidence = focus_hyprland_window(
                    hyprctl_path,
                    tool_checks["hyprctl"]["stdout"],
                    str(match["address"]),
                )
                outcomes["hyprland_window_focus"] = focus_evidence
                if focus_evidence["status"] != "pass":
                    raise RuntimeError(f"Hyprland focus was not verified: {focus_evidence.get('reason', 'unknown failure')}")
                key_result = subprocess.run(
                    [click_path, "-k", "Tab", "-k", "Return"],
                    stdin=subprocess.DEVNULL,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.PIPE,
                    timeout=COMMAND_TIMEOUT,
                    check=True,
                    text=True,
                )
                del key_result
                deadline = time.monotonic() + 5.0
                events: list[dict[str, Any]] = []
                while time.monotonic() < deadline and not events:
                    events = client.poll()
                    if not events:
                        time.sleep(0.1)
                matched_event = next(
                    (
                        event
                        for event in events
                        if event.get("applet_id") == "desktop_demo"
                        and event.get("widget_id") == "action_button"
                        and event.get("signal") == "clicked"
                        and event.get("generation") == client.generation
                        and event.get("source_revision") == button_revision
                        and event.get("payload") == {}
                    ),
                    None,
                )
                if matched_event is None:
                    raise RuntimeError("no typed button event arrived after actual Wayland key input")
                click_status = {"status": "pass", "cursor": matched_event["cursor"], "source_revision": button_revision}
            except Exception as error:
                click_status = {"status": "fail", "reason": str(error)}
        outcomes["quickshell_button_event"] = click_status

        restart_widget = {"applet_id": "desktop_demo", "widget_id": "restart_text", "contract": text_contract, "properties": {"text": "replayed"}}
        _, first_generation_revision = client.mutation({"kind": "instantiate", **restart_widget})
        cleanup_first = stop_process(process)
        process = None
        for handle in process_logs:
            handle.close()
        process_logs.clear()

        client.generation += 1
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
            "quickshell_wayland_smoke": {"status": "pass", "results": ["negotiation", "create", "typed patch", "destroy", "renderer restart replay", "old generation rejected"]},
            "applet_lifecycle": {"status": "partial", "results": ["widget create/update/destroy", "renderer restart"], "reason": "full workflow and login/logout remain pending"},
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
