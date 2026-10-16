#!/usr/bin/env python3
"""Opt-in RuntimeGraphExecutor to real Quickshell button acceptance."""

from __future__ import annotations

import argparse
import hashlib
import io
import json
import os
import platform
import shutil
import signal
import stat
import subprocess
import sys
import tempfile
import threading
import time
import uuid
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Callable

ROOT = Path(__file__).resolve().parents[1]
DEFAULT_EXECUTE = ROOT / "target" / "release" / "execute"
OPT_IN_ENV = "EXECUTE_OMARCHY_DISPOSABLE"
SESSION_MARKER_ENV = "EXECUTE_OMARCHY_SESSION_ID"
EXPECTED_CANCEL_EXIT = 1
EXPECTED_CANCEL_DIAGNOSTIC = "graph session cancelled"
WINDOW_TITLE = "Execute generic renderer"
COMMAND_TIMEOUT, STARTUP_TIMEOUT, SYNC_TIMEOUT = 3.0, 30.0, 2.0
CANCEL_TIMEOUT, REAP_TIMEOUT, POLL_INTERVAL = 15.0, 5.0, 0.1
MAX_TRACKED_PROCESSES = 256

sys.path.insert(0, str(ROOT / "tools"))
import desktop_demo  # noqa: E402
import omarchy_acceptance  # noqa: E402


@dataclass(frozen=True)
class ProcIdentity:
    pid: int
    starttime: int


@dataclass(frozen=True)
class ProcRecord:
    identity: ProcIdentity
    ppid: int
    name: str
    executable: str
    argv: tuple[str, ...]

    def receipt(self) -> dict[str, Any]:
        role, mode = classify_process(self)
        result = {"pid": self.identity.pid, "starttime_ticks": self.identity.starttime,
                  "parent_pid": self.ppid, "name": self.name[:128], "role": role}
        if mode: result["worker_mode"] = mode
        return result


def parse_proc_stat(text: str) -> tuple[int, str, int, int, str]:
    left, right = text.find("("), text.rfind(")")
    if left < 1 or right <= left:
        raise ValueError("malformed /proc stat record")
    fields = text[right + 2 :].split()
    if len(fields) <= 19:
        raise ValueError("short /proc stat record")
    return int(text[:left]), text[left + 1:right], int(fields[1]), int(fields[19]), fields[0]


def read_proc_record(pid: int, proc_root: Path = Path("/proc")) -> ProcRecord | None:
    directory = proc_root / str(pid)
    try:
        stat_pid, name, ppid, starttime, state = parse_proc_stat((directory / "stat").read_text(encoding="ascii"))
        argv = tuple(part.decode("utf-8", "replace") for part in (directory / "cmdline").read_bytes().split(b"\0") if part)
        executable = os.readlink(directory / "exe")
    except (OSError, UnicodeError, ValueError):
        return None
    return ProcRecord(ProcIdentity(pid, starttime), ppid, name, executable, argv) if stat_pid == pid else None


def read_process_table(proc_root: Path = Path("/proc")) -> dict[int, ProcRecord]:
    try:
        entries = list(proc_root.iterdir())
    except OSError:
        return {}
    records: dict[int, ProcRecord] = {}
    for entry in entries:
        if entry.name.isdecimal():
            record = read_proc_record(int(entry.name), proc_root)
            if record: records[record.identity.pid] = record
    return records


def owned_descendants(root: ProcIdentity, table: dict[int, ProcRecord]) -> list[ProcRecord]:
    record = table.get(root.pid)
    if record is None or record.identity != root:
        return []
    children: dict[int, list[ProcRecord]] = {}
    for child in table.values(): children.setdefault(child.ppid, []).append(child)
    result, pending, seen = [], [root.pid], {root.pid}
    while pending:
        for child in children.get(pending.pop(), []):
            if child.identity.pid in seen: continue
            seen.add(child.identity.pid); result.append(child); pending.append(child.identity.pid)
    return sorted(result, key=lambda item: (item.ppid, item.identity.pid))


def identity_status(identity: ProcIdentity, proc_root: Path = Path("/proc")) -> str:
    try:
        pid, _, _, starttime, _ = parse_proc_stat(
            (proc_root / str(identity.pid) / "stat").read_text(encoding="ascii")
        )
    except FileNotFoundError:
        return "gone"
    except (OSError, UnicodeError, ValueError):
        return "unknown"
    return "present" if (pid, starttime) == (identity.pid, identity.starttime) else "gone"


def classify_process(record: ProcRecord) -> tuple[str, str | None]:
    executable = Path(record.executable).name.lower()
    argv = record.argv
    if executable in {"qs", "quickshell"} or (argv and Path(argv[0]).name.lower() in {"qs", "quickshell"}):
        return "quickshell_renderer", None
    if "workers.py" in " ".join(argv) and "--worker" in argv:
        index = argv.index("--worker")
        return "applet_worker", argv[index + 1] if index + 1 < len(argv) else None
    return "owned_child", None


def observe_tree(
    root: ProcIdentity, seen: dict[tuple[int, int], ProcRecord], pidfds: dict[tuple[int, int], int | None],
    proc_root: Path = Path("/proc"),
) -> list[ProcRecord]:
    descendants = owned_descendants(root, read_process_table(proc_root))
    if len(descendants) > MAX_TRACKED_PROCESSES:
        raise RuntimeError("owned process tree exceeded the bounded receipt capacity")
    for item in descendants:
        key = (item.identity.pid, item.identity.starttime)
        if key in seen: continue
        seen[key] = item
        descriptor = None
        if hasattr(os, "pidfd_open"):
            try: descriptor = os.pidfd_open(item.identity.pid, 0)
            except OSError: pass
        if descriptor is not None and read_proc_record(item.identity.pid, proc_root) != item:
            try: os.close(descriptor)
            except OSError: pass
            descriptor = None
        pidfds[key] = descriptor
    return descendants


def cancellation_result(returncode: int | None, stderr: str, *, forced_cleanup: bool) -> dict[str, Any]:
    diagnostic = EXPECTED_CANCEL_DIAGNOSTIC in stderr
    passed = returncode == EXPECTED_CANCEL_EXIT and diagnostic and not forced_cleanup
    return {"status": "pass" if passed else "fail", "returncode": returncode,
            "expected_returncode": EXPECTED_CANCEL_EXIT, "diagnostic_present": diagnostic,
            "expected_diagnostic": EXPECTED_CANCEL_DIAGNOSTIC, "forced_cleanup": forced_cleanup,
            "reason": None if passed else "graph did not complete the expected SIGINT cancellation path"}


def command(command: list[str], timeout: float = COMMAND_TIMEOUT) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        command, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        timeout=timeout, check=False, text=True, encoding="utf-8", errors="replace",
    )


def owned_windows(hyprctl: str, renderer_pids: set[int]) -> list[dict[str, Any]]:
    result = command([hyprctl, "clients", "-j"])
    if result.returncode:
        raise RuntimeError(f"hyprctl clients failed: {result.stderr.strip()[:800]}")
    try:
        clients = json.loads(result.stdout)
    except json.JSONDecodeError as error:
        raise RuntimeError(f"hyprctl clients returned invalid JSON: {error}") from error
    if not isinstance(clients, list):
        raise RuntimeError("hyprctl clients response was not a list")
    windows = {item["address"]: item for item in clients if isinstance(item, dict)
               and item.get("title") == WINDOW_TITLE and isinstance(item.get("pid"), int)
               and item["pid"] in renderer_pids and isinstance(item.get("address"), str)}
    return [windows[key] for key in sorted(windows)]


def attempt_graph_button(
    window: dict[str, Any], hyprctl: str, version: str, wtype: str, fixture: Any, *,
    command_runner: Callable[..., Any] = subprocess.run,
    monotonic: Callable[[], float] = time.monotonic,
    sleep: Callable[[float], None] = time.sleep,
    sync_timeout: float = SYNC_TIMEOUT,
) -> dict[str, Any]:
    before = int(fixture.sync_count)
    evidence = {"window_pid": window.get("pid"), "window_address": window.get("address"), "sync_count_before": before, "status": "fail"}
    focus = omarchy_acceptance.focus_hyprland_window(hyprctl, version, str(window.get("address")), command_runner=command_runner)
    evidence["focus"] = focus
    if focus.get("status") != "pass" or not focus.get("active_window_verified"):
        return {**evidence, "reason": "Hyprland focus was not verified; no Wayland input was sent"}
    key_command = [wtype, "-k", "Tab", "-k", "Return"]
    evidence["wayland_input_command"] = key_command
    try:
        sent = command_runner(
            key_command, stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=COMMAND_TIMEOUT, check=False, text=True,
        )
    except (OSError, subprocess.SubprocessError) as error:
        return {**evidence, "reason": f"wtype could not send Wayland key input: {error}"}
    if sent.returncode:
        return {**evidence, "reason": f"wtype failed: {str(sent.stderr).strip()[:800]}"}
    deadline = monotonic() + sync_timeout
    while True:
        after = int(fixture.sync_count)
        if after > before: return {**evidence, "status": "pass", "sync_count_after": after}
        now = monotonic()
        if now >= deadline:
            return {**evidence, "status": "no_sync", "sync_count_after": after,
                    "reason": "fixture sync_count did not advance after graph-owned Wayland input"}
        sleep(min(POLL_INTERVAL, deadline - now))


def instrument_fixture(fixture: Any) -> None:
    base = fixture.RequestHandlerClass
    fixture.operation_counts = {"desktop.clock_remote": 0, "desktop.fixture_sync": 0}
    fixture.operation_lock = threading.Lock()

    class CountingHandler(base):
        def do_POST(self) -> None:
            length = int(self.headers.get("Content-Length", "0"))
            if not 0 <= length <= 1_048_576:
                return super().do_POST()
            payload = self.rfile.read(length)
            try:
                operation = json.loads(payload).get("operation")
            except (UnicodeDecodeError, json.JSONDecodeError, AttributeError):
                operation = None
            if operation in fixture.operation_counts:
                with fixture.operation_lock:
                    fixture.operation_counts[operation] += 1
            self.rfile = io.BytesIO(payload)
            super().do_POST()

    fixture.RequestHandlerClass = CountingHandler


def preflight(binary: Path) -> tuple[dict[str, Any], list[str], dict[str, str]]:
    errors: list[str] = []
    os_release = omarchy_acceptance.read_os_release()
    runtime_dir, runtime_error = omarchy_acceptance.check_runtime_dir()
    if os.getuid() == 0: errors.append("refusing to run as root")
    if os_release.get("ID") != "arch": errors.append("target operating system must identify as Arch Linux")
    if os.environ.get("XDG_SESSION_TYPE") != "wayland": errors.append("XDG_SESSION_TYPE must be wayland")
    if runtime_error: errors.append(runtime_error)
    for key in ("WAYLAND_DISPLAY", "HYPRLAND_INSTANCE_SIGNATURE", "DBUS_SESSION_BUS_ADDRESS", "XDG_SESSION_ID"):
        if not os.environ.get(key):
            errors.append(f"required session variable {key} is missing")
    if not binary.is_file() or not os.access(binary, os.X_OK): errors.append("candidate execute binary is missing or not executable")
    paths = {name: shutil.which(name) for name in ("qs", "hyprctl", "wtype", "busctl", "loginctl")}
    for name, path in paths.items():
        if path is None: errors.append(f"required target-session tool {name} is unavailable")
    marker = os.environ.get(SESSION_MARKER_ENV, "")
    if not marker.startswith("disposable-") or any(c not in "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789_-" for c in marker):
        errors.append(f"{SESSION_MARKER_ENV} must be a unique disposable-prefixed identifier")
    target = {"os_release_id": os_release.get("ID"), "session_type": os.environ.get("XDG_SESSION_TYPE"),
              "runtime_dir_status": "pass" if runtime_dir else "blocked",
              "disposable_session_marker_sha256": hashlib.sha256(marker.encode()).hexdigest() if marker else None}
    available_paths = {name: path for name, path in paths.items() if path is not None}
    if errors:
        target["desktop_probes"] = "not_run_static_preflight_failed"
        return target, errors, available_paths
    evidence, session_errors = omarchy_acceptance.inspect_live_session(
        runtime_dir, paths["hyprctl"], paths["busctl"], paths["loginctl"]
    )
    target["session_evidence"] = evidence
    if session_errors:
        target["desktop_probes"] = "stopped_after_session_probe_failure"
        return target, session_errors, available_paths
    hypr_version = omarchy_acceptance.command_version([paths["hyprctl"], "version"])
    target["hyprland"] = hypr_version
    if not hypr_version.get("available"):
        errors.append("hyprctl version check failed")
    if errors:
        target["desktop_probes"] = "version_probe_failed"
        return target, errors, available_paths
    return target, errors, available_paths


def wait_ready(
    process: subprocess.Popen[bytes], root: ProcIdentity, producer: str, fixture: Any,
    hyprctl: str, seen: dict[tuple[int, int], ProcRecord], pidfds: dict[tuple[int, int], int | None],
) -> tuple[list[dict[str, Any]], dict[str, Any]]:
    expected = {"clock_format"} | ({"clock_local"} if producer == "local" else set())
    deadline = time.monotonic() + STARTUP_TIMEOUT
    latest: dict[str, Any] = {}
    while time.monotonic() < deadline:
        if process.poll() is not None: raise RuntimeError(f"Execute exited before graph readiness ({process.returncode})")
        descendants = observe_tree(root, seen, pidfds)
        renderers = [item for item in descendants if classify_process(item)[0] == "quickshell_renderer"]
        modes = {mode for item in descendants for role, mode in [classify_process(item)] if role == "applet_worker" and mode}
        windows = owned_windows(hyprctl, {item.identity.pid for item in renderers}) if renderers else []
        clock_calls = fixture.operation_counts["desktop.clock_remote"]
        latest = {
            "renderer_process_count": len(renderers), "worker_modes": sorted(modes),
            "expected_worker_modes": sorted(expected), "owned_window_count": len(windows),
            "remote_clock_fixture_calls": clock_calls,
        }
        if expected.issubset(modes) and renderers and len(windows) == 2 and (producer == "local" or clock_calls > 0): return windows, latest
        time.sleep(POLL_INTERVAL)
    raise TimeoutError(f"graph did not establish its workers and two owned windows: {latest}")


def force_cleanup(
    process: subprocess.Popen[bytes], root: ProcIdentity | None,
    seen: dict[tuple[int, int], ProcRecord], pidfds: dict[tuple[int, int], int | None],
) -> dict[str, Any]:
    actions: list[dict[str, Any]] = []
    if process.poll() is None:
        process.terminate()
        actions.append({"pid": process.pid, "signal": "SIGTERM", "target": "execute"})
        try:
            process.wait(timeout=2.0)
        except subprocess.TimeoutExpired:
            process.kill()
            actions.append({"pid": process.pid, "signal": "SIGKILL", "target": "execute"})
            process.wait(timeout=2.0)
    descendants = [(record.identity, pidfds.get(key)) for key, record in seen.items()]
    for item, descriptor in descendants:
        status = identity_status(item)
        if status == "gone": continue
        result = signal_owned_identity(item, signal.SIGTERM, descriptor) if status == "present" else status
        actions.append({"pid": item.pid, "starttime_ticks": item.starttime,
                        "signal": "SIGTERM" if result == "signaled" else "none", "status": result})
    deadline = time.monotonic() + 1.0
    while time.monotonic() < deadline and any(identity_status(item) != "gone" for item, _ in descendants): time.sleep(0.05)
    for item, descriptor in descendants:
        status = identity_status(item)
        if status == "gone": continue
        result = signal_owned_identity(item, signal.SIGKILL, descriptor) if status == "present" else status
        actions.append({"pid": item.pid, "starttime_ticks": item.starttime,
                        "signal": "SIGKILL" if result == "signaled" else "none", "status": result})
    try: process.wait(timeout=2.0)
    except subprocess.TimeoutExpired: pass
    return {"status": "forced_cleanup_used", "actions": actions}


def signal_owned_identity(identity: ProcIdentity, sig: int, descriptor: int | None) -> str:
    if descriptor is None or not hasattr(signal, "pidfd_send_signal"):
        return "unsupported"
    try:
        status = identity_status(identity)
        if status != "present": return status
        signal.pidfd_send_signal(descriptor, sig)
        return "signaled"
    except ProcessLookupError:
        return "gone"
    except OSError:
        return "unknown"


def close_pidfds(pidfds: dict[tuple[int, int], int | None]) -> None:
    for descriptor in pidfds.values():
        if descriptor is None: continue
        try: os.close(descriptor)
        except OSError: pass


def wait_gone(identities: list[ProcIdentity]) -> list[dict[str, Any]]:
    deadline = time.monotonic() + REAP_TIMEOUT
    while True:
        remaining = [(item, identity_status(item)) for item in identities]
        remaining = [(item, status) for item, status in remaining if status != "gone"]
        if not remaining or time.monotonic() >= deadline:
            return [{"pid": item.pid, "starttime_ticks": item.starttime, "identity_status": status}
                    for item, status in remaining]
        time.sleep(POLL_INTERVAL)


def run_scenario(binary: Path, producer: str, version: str, commands: dict[str, str]) -> dict[str, Any]:
    runtime_dir, runtime_error = omarchy_acceptance.check_runtime_dir()
    if runtime_error or runtime_dir is None:
        return {"producer": producer, "status": "fail", "failure": runtime_error or "XDG_RUNTIME_DIR unavailable"}
    temp_path = Path(tempfile.mkdtemp(prefix=f"execute-desktop-graph-{uuid.uuid4().hex[:10]}-", dir=runtime_dir))
    os.chmod(temp_path, 0o700)
    scenario: dict[str, Any] = {"producer": producer, "status": "fail", "forced_cleanup": {"status": "not_used"}}
    fixture = process = server_thread = root = None
    seen: dict[tuple[int, int], ProcRecord] = {}
    pidfds: dict[tuple[int, int], int | None] = {}
    stderr_path, stderr_handle = temp_path / "execute.stderr.log", None
    try:
        info = temp_path.lstat()
        if not stat.S_ISDIR(info.st_mode) or info.st_uid != os.getuid() or stat.S_IMODE(info.st_mode) != 0o700:
            raise RuntimeError("private run directory has unexpected ownership or permissions")
        fixture = desktop_demo.load_fixture_server()(("127.0.0.1", 0))
        instrument_fixture(fixture)
        server_thread = threading.Thread(target=fixture.serve_forever, kwargs={"poll_interval": 0.1}, daemon=True)
        server_thread.start()
        manifests, program = desktop_demo.write_run_inputs(temp_path, producer, "quickshell", fixture.server_address[1])
        child_env = os.environ.copy()
        for key, child in (("XDG_CONFIG_HOME", "config"), ("XDG_CACHE_HOME", "cache"), ("XDG_STATE_HOME", "state"), ("XDG_DATA_HOME", "data")):
            child_env[key] = str(temp_path / child)
            Path(child_env[key]).mkdir(mode=0o700)
        child_env["QS_DISABLE_FILE_WATCHER"] = "1"
        stderr_handle = stderr_path.open("wb")
        process = subprocess.Popen(
            [str(binary), "run", str(program), "--manifest-dir", str(manifests)],
            cwd=ROOT, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=stderr_handle, env=child_env,
        )
        limit = time.monotonic() + 3.0
        root_record = None
        while root_record is None and time.monotonic() < limit:
            root_record = read_proc_record(process.pid)
            if root_record is None: time.sleep(0.02)
        if root_record is None:
            raise RuntimeError("could not read Execute's /proc identity")
        root = root_record.identity
        windows, ready = wait_ready(process, root, producer, fixture, commands["hyprctl"], seen, pidfds)
        live = observe_tree(root, seen, pidfds)
        expected = {"clock_format"} | ({"clock_local"} if producer == "local" else set())
        live_modes = {mode for item in live for role, mode in [classify_process(item)] if role == "applet_worker" and mode}
        live_renderers = [item for item in live if classify_process(item)[0] == "quickshell_renderer"]
        if not expected.issubset(live_modes) or not live_renderers:
            raise RuntimeError("clock/formatter workers and Quickshell renderer were not live before cancellation")
        scenario["graph_ready"] = ready
        scenario["workers"] = {"expected_modes": sorted(expected), "live_before_cancel": [x.receipt() for x in live if classify_process(x)[0] == "applet_worker"]}
        scenario["renderer_processes"] = {"live_before_cancel": [x.receipt() for x in live_renderers]}
        scenario["owned_processes_before_cancel"] = [x.receipt() for x in seen.values()]
        scenario["windows"] = [{key: item.get(key) for key in ("pid", "address", "title")} for item in windows]
        scenario["fixture"] = {"sync_count_before": int(fixture.sync_count),
                                "remote_clock_calls": int(fixture.operation_counts["desktop.clock_remote"])}
        attempts = []
        for window in windows:
            attempt = attempt_graph_button(window, commands["hyprctl"], version, commands["wtype"], fixture)
            attempts.append(attempt)
            if attempt["status"] == "pass": break
            if attempt["status"] == "fail": raise RuntimeError(attempt.get("reason", "focus or Wayland input failed"))
        scenario["button_input_attempts"] = attempts
        routed = next((item for item in attempts if item["status"] == "pass"), None)
        if routed is None:
            raise RuntimeError("graph-owned Wayland input did not advance fixture.sync_count")
        scenario["graph_button_route"] = {"status": "pass", "window_pid": routed["window_pid"],
                                           "window_address": routed["window_address"],
                                           "sync_count_before": routed["sync_count_before"],
                                           "sync_count_after": routed["sync_count_after"]}
        scenario["fixture"].update(sync_count_after=int(fixture.sync_count), operation_counts=dict(fixture.operation_counts))
        scenario["cancellation_target"] = {"pid": root.pid, "starttime_ticks": root.starttime, "signal": "SIGINT", "scope": "Execute PID only"}
        if process.poll() is not None: raise RuntimeError(f"Execute exited before cancellation ({process.returncode})")
        os.kill(process.pid, signal.SIGINT)
        try:
            code = process.wait(timeout=CANCEL_TIMEOUT)
        except subprocess.TimeoutExpired:
            scenario["cancellation"] = cancellation_result(None, "", forced_cleanup=True)
            raise RuntimeError("Execute did not exit after SIGINT before the deadline")
        stderr_handle.flush()
        cancellation = cancellation_result(code, stderr_path.read_text(encoding="utf-8", errors="replace"), forced_cleanup=False)
        scenario["cancellation"] = cancellation
        if cancellation["status"] != "pass": raise RuntimeError(cancellation["reason"])
        identities = [root] + [item.identity for item in seen.values()]
        survivors = wait_gone(identities)
        scenario["process_teardown"] = {"status": "pass" if not survivors else "fail",
                                        "observed_identity_count": len({(x.pid, x.starttime) for x in identities}),
                                        "surviving_identities": survivors,
                                        "proof": "Linux /proc PID and starttime_ticks; PID reuse is a different identity"}
        if survivors: raise RuntimeError("Execute or an observed owned descendant survived normal SIGINT cancellation")
        scenario["status"] = "pass"
    except Exception as error:
        scenario["failure"] = str(error)
        try: scenario["stderr_tail"] = stderr_path.read_text(encoding="utf-8", errors="replace")[-2000:]
        except OSError: pass
    finally:
        try:
            if process is not None and (process.poll() is None or any(
                identity_status(ProcIdentity(pid, start)) != "gone" for pid, start in seen
            )):
                try:
                    scenario["forced_cleanup"] = force_cleanup(process, root, seen, pidfds)
                except Exception as error:
                    scenario["forced_cleanup"] = {"status": "failed", "reason": str(error)}
                scenario["status"] = "fail"
                scenario.setdefault("failure", "forced process cleanup was required")
        finally:
            close_pidfds(pidfds)
        if fixture is not None:
            scenario.setdefault("fixture", {})["sync_count_after"] = int(fixture.sync_count)
            fixture.shutdown()
            fixture.server_close()
        if server_thread is not None:
            server_thread.join(timeout=3.0)
            if server_thread.is_alive():
                scenario["status"] = "fail"
                scenario.setdefault("failure", "fixture server thread did not stop")
        if stderr_handle is not None:
            stderr_handle.close()
        try:
            if temp_path.parent.resolve(strict=True) != runtime_dir or not stat.S_ISDIR(temp_path.lstat().st_mode):
                raise RuntimeError("private run directory containment check failed")
            shutil.rmtree(temp_path)
            scenario["temporary_directory"] = {"status": "removed"}
        except (OSError, RuntimeError) as error:
            scenario["status"] = "fail"
            scenario["temporary_directory"] = {"status": "failed", "reason": str(error)}
    return scenario


def emit(report: dict[str, Any], evidence: Path | None) -> bool:
    ok = True
    if evidence is not None:
        try:
            evidence.parent.mkdir(parents=True, exist_ok=True)
            descriptor = os.open(evidence, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
            report["evidence_write"] = {"status": "written", "path": str(evidence)}
            with os.fdopen(descriptor, "w", encoding="utf-8") as out:
                json.dump(report, out, indent=2, sort_keys=True)
                out.write("\n")
        except OSError as error:
            report["evidence_write"] = {"status": "failed", "reason": str(error)}
            report["status"] = "failed"
            report["failure"] = "machine-readable evidence could not be written"
            ok = False
    print(json.dumps(report, indent=2, sort_keys=True))
    return ok


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--run-in-disposable-arch", action="store_true")
    parser.add_argument("--execute", type=Path, default=DEFAULT_EXECUTE)
    parser.add_argument("--evidence", type=Path)
    args = parser.parse_args()
    binary = args.execute.expanduser().resolve()
    report: dict[str, Any] = {
        "schema_version": 1, "harness": "execute.desktop_graph_acceptance", "status": "pending",
        "scope": "RuntimeGraphExecutor to real Quickshell windows to loopback fixture; native Omarchy certification is out of scope",
        "host": {"platform": platform.platform(), "python": sys.version.split()[0], "uid": os.getuid()},
        "binary": {"path": str(binary), "exists": binary.is_file(), "sha256": omarchy_acceptance.sha256_file(binary)},
        "scenarios": [],
    }
    if args.evidence is not None and args.evidence.exists():
        report.update(status="failed", failure="evidence destination already exists")
        emit(report, None)
        return 2
    if not args.run_in_disposable_arch or os.environ.get(OPT_IN_ENV) != "1" or not os.environ.get(SESSION_MARKER_ENV):
        report["checks"] = {"opt_in": "pending", "reason": f"requires --run-in-disposable-arch, {OPT_IN_ENV}=1, and {SESSION_MARKER_ENV}; no desktop commands were run"}
        return 0 if emit(report, args.evidence) else 1

    target, errors, paths = preflight(binary)
    report["target"] = target
    report["checks"] = {
        "opt_in": "pass",
        "target_preflight": {"status": "pass" if not errors else "blocked", "errors": errors},
        "graph_cancellation_contract": {
            "status": "source_pinned", "expected_exit": EXPECTED_CANCEL_EXIT,
            "expected_stderr": EXPECTED_CANCEL_DIAGNOSTIC,
            "source": "GraphSession::run_to_completion joins tasks then returns graph session cancelled; src/main.rs maps the error to exit 1",
        },
    }
    if errors:
        report.update(status="failed", failure="target session preflight failed")
        emit(report, args.evidence)
        return 1
    version = target["hyprland"].get("stdout", "")
    for producer in ("local", "remote"):
        report["scenarios"].append(run_scenario(binary, producer, version, paths))
    passed = all(item.get("status") == "pass" for item in report["scenarios"])
    report["status"] = "passed" if passed else "failed"
    if not passed:
        report["failure"] = "one or more graph-to-Quickshell acceptance scenarios failed"
    report["checks"].update({
        "local_clock_graph_button_route": report["scenarios"][0]["status"],
        "remote_clock_graph_button_route": report["scenarios"][1]["status"],
        "natural_cancellation_and_reaping": {"status": "pass" if passed else "fail", "forced_cleanup_is_failure": True},
    })
    evidence_written = emit(report, args.evidence)
    return 0 if passed and evidence_written else 1


if __name__ == "__main__":
    raise SystemExit(main())
