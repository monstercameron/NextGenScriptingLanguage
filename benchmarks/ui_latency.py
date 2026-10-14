#!/usr/bin/env python3
"""Measure interactive UI submit-to-render latency through a fixed-size PTY."""

import argparse
import errno
import fcntl
import hashlib
import json
import math
import os
import pty
import select
import signal
import struct
import subprocess
import termios
import tempfile
import time
from pathlib import Path
from statistics import median

ROOT = Path(__file__).resolve().parents[1]
TRANSCRIPT_LIMIT = 4 * 1024 * 1024
# The handler clears the result before invoking native printf/tr so the full marker is rendered.
PROGRAM = '''open a screen titled "UI latency"
place app on the left
define a program named app
  show a text field named query with value ""
  show a text panel named result with text "Ready"
  whenever query is submitted
    show "" in result
    run printf with message query
    pipe to tr with from "[:lower:]" and to "[:upper:]"
    then show the result in result
  whenever key "Ctrl-Q" is pressed
    close the screen
'''


def sha256_file(path):
    digest = hashlib.sha256()
    with path.open("rb") as source:
        while chunk := source.read(1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def prepare_native_path(temp_root, catalog):
    native_bin = temp_root / "native-bin"
    native_bin.mkdir()
    records = {}
    for name in ("printf", "tr"):
        manifest_path = catalog / f"{name}.json"
        manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
        if manifest.get("name") != name or manifest.get("binary") != name:
            raise ValueError(f"unexpected native manifest for {name}: {manifest_path}")

        candidate = Path("/usr/bin") / name
        if not candidate.is_file():
            candidate = Path("/bin") / name
        if not candidate.is_file():
            raise FileNotFoundError(f"required absolute native executable is missing: {candidate}")
        executable = candidate.resolve(strict=True)
        if not executable.is_absolute() or not executable.is_file() or not os.access(executable, os.X_OK):
            raise PermissionError(f"native executable is not an absolute executable file: {executable}")

        link = native_bin / name
        link.symlink_to(executable)
        target = os.readlink(link)
        if not Path(target).is_absolute() or link.resolve(strict=True) != executable:
            raise RuntimeError(f"owned PATH symlink did not resolve to {executable}")
        records[name] = {
            "absolute_executable": str(executable),
            "executable_sha256": sha256_file(executable),
            "manifest_sha256": sha256_file(manifest_path),
            "owned_path_symlink": str(link),
        }
    return native_bin, records


def set_size(fd, columns, rows):
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, columns, 0, 0))


class PtyChild:
    def __init__(self, binary, program, cache_dir, catalog, cwd, native_path, work_deadline,
                 hard_deadline, io_timeout):
        self.master = None
        self.slave = None
        self.process = None
        self.work_deadline = work_deadline
        self.hard_deadline = hard_deadline
        self.io_timeout = io_timeout
        self.transcript = bytearray()
        self.started_ns = time.perf_counter_ns()
        self.master, self.slave = pty.openpty()
        try:
            set_size(self.slave, 100, 30)
            self.original_termios = termios.tcgetattr(self.slave)
            os.set_blocking(self.master, False)
            environment = {
                "TERM": "xterm-256color",
                "LANG": "C.UTF-8",
                "HOME": str(cwd),
                "PATH": str(native_path),
            }
            self.process = subprocess.Popen(
                [
                    str(binary),
                    "run",
                    str(program),
                    "--manifest-dir",
                    str(catalog),
                    "--cache-dir",
                    str(cache_dir),
                    "--command-timeout-ms",
                    "5000",
                    "--max-output-bytes",
                    "4096",
                ],
                stdin=self.slave,
                stdout=self.slave,
                stderr=self.slave,
                cwd=cwd,
                env=environment,
                close_fds=True,
                start_new_session=True,
            )
        except BaseException:
            self.close_fds()
            raise

    def operation_deadline(self):
        return min(self.work_deadline, time.monotonic() + self.io_timeout)

    @staticmethod
    def wait_fd(fd, *, reading, writing, deadline, label):
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError(f"deadline expired while {label}")
            try:
                readable, writable, _ = select.select(
                    [fd] if reading else [], [fd] if writing else [], [], remaining
                )
            except InterruptedError:
                continue
            if readable or writable:
                return
            raise TimeoutError(f"deadline expired while {label}")

    def send(self, data, deadline=None):
        deadline = self.operation_deadline() if deadline is None else deadline
        view = memoryview(data)
        while view:
            if deadline <= time.monotonic():
                raise TimeoutError("deadline expired writing to PTY")
            try:
                written = os.write(self.master, view)
                if written <= 0:
                    raise RuntimeError("PTY write made no progress")
                view = view[written:]
            except BlockingIOError:
                self.wait_fd(
                    self.master, reading=False, writing=True, deadline=deadline, label="writing to PTY"
                )
            except InterruptedError:
                continue
            except OSError as error:
                if error.errno == errno.EIO and self.process.poll() is not None:
                    raise RuntimeError("PTY closed while writing") from error
                raise

    def clear_transcript(self):
        self.transcript.clear()

    def read_until(self, needle, label, deadline=None):
        deadline = self.operation_deadline() if deadline is None else deadline
        while needle not in self.transcript:
            self.wait_fd(
                self.master, reading=True, writing=False, deadline=deadline, label=f"waiting for {label}"
            )
            try:
                chunk = os.read(self.master, 65536)
            except BlockingIOError:
                continue
            except InterruptedError:
                continue
            except OSError as error:
                if error.errno == errno.EIO and self.process.poll() is not None:
                    raise RuntimeError(f"PTY closed before {label}: {self.process.returncode}") from error
                raise
            if not chunk:
                raise RuntimeError(f"PTY reached EOF before {label}")
            self.transcript.extend(chunk)
            if len(self.transcript) > TRANSCRIPT_LIMIT:
                del self.transcript[: len(self.transcript) - TRANSCRIPT_LIMIT]

    def wait(self, deadline, label):
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise TimeoutError(f"deadline expired waiting for {label}")
        try:
            return self.process.wait(timeout=remaining)
        except subprocess.TimeoutExpired as error:
            raise TimeoutError(f"deadline expired waiting for {label}") from error

    def assert_restored(self, label):
        actual = termios.tcgetattr(self.slave)
        if actual != self.original_termios:
            raise AssertionError(f"terminal attributes were not restored after {label}")

    def signal_process_if_running(self, sig, label):
        if self.process.poll() is not None:
            raise RuntimeError(f"child exited before {label}")
        os.kill(self.process.pid, sig)

    def signal_group_if_running(self, sig):
        # poll() reaps an exited process; never signal its PID/PGID after that point.
        if self.process.poll() is not None:
            return
        try:
            os.killpg(self.process.pid, sig)
        except ProcessLookupError:
            pass

    def stop_process_group(self):
        if self.process.poll() is not None:
            return
        self.signal_group_if_running(signal.SIGTERM)
        term_deadline = min(self.hard_deadline, time.monotonic() + min(2.0, self.io_timeout))
        try:
            self.wait(term_deadline, "PTY child cleanup after SIGTERM")
            return
        except TimeoutError:
            pass
        self.signal_group_if_running(signal.SIGKILL)
        if self.process.poll() is None:
            self.wait(self.hard_deadline, "PTY child cleanup after SIGKILL")

    def close_fds(self):
        for name in ("master", "slave"):
            fd = getattr(self, name)
            if fd is not None:
                setattr(self, name, None)
                try:
                    os.close(fd)
                except OSError:
                    pass

    def close(self):
        try:
            if self.process is not None:
                self.stop_process_group()
        finally:
            self.close_fds()


def start_child(binary, program, temp_root, catalog, native_path, work_deadline, hard_deadline,
                io_timeout):
    cache_dir = temp_root / "cache"
    cache_dir.mkdir(exist_ok=True)
    return PtyChild(
        binary, program, cache_dir, catalog, temp_root, native_path, work_deadline, hard_deadline,
        io_timeout
    )


def graceful_quit(child, key, label):
    deadline = child.operation_deadline()
    started = time.perf_counter_ns()
    child.send(key, deadline)
    status = child.wait(deadline, f"{label} exit")
    elapsed_ms = (time.perf_counter_ns() - started) / 1_000_000
    if status != 0:
        raise RuntimeError(f"{label} exit status was {status}")
    child.assert_restored(label)
    return elapsed_ms


def measure(binary, samples, io_timeout, total_timeout):
    catalog = (ROOT / "manifests").resolve(strict=True)
    for name in ("printf", "tr"):
        if not (catalog / f"{name}.json").is_file():
            raise FileNotFoundError(f"required known manifest is missing: {name}.json")
    binary = binary.resolve(strict=True)
    started = time.monotonic()
    hard_deadline = started + total_timeout
    cleanup_reserve = min(5.0, total_timeout / 10)
    work_deadline = hard_deadline - cleanup_reserve
    latencies_ms = []

    with tempfile.TemporaryDirectory(prefix="flow-ui-latency-") as directory:
        temp_root = Path(directory)
        program = temp_root / "latency.program"
        program.write_text(PROGRAM, encoding="utf-8")
        native_path, native_records = prepare_native_path(temp_root, catalog)

        child = start_child(
            binary, program, temp_root, catalog, native_path, work_deadline, hard_deadline, io_timeout
        )
        try:
            child.read_until(b"Ready", "initial panel")
            startup_ms = (time.perf_counter_ns() - child.started_ns) / 1_000_000
            for index in range(samples):
                if time.monotonic() >= work_deadline:
                    raise TimeoutError("benchmark exceeded its work deadline")
                token = f"uiwake{index:04d}"
                marker = token.upper().encode("ascii")
                child.send(b"\x15" + token.encode("ascii"))  # Ctrl-U, then a unique lowercase query.
                submit_deadline = child.operation_deadline()
                started_submit = time.perf_counter_ns()
                child.send(b"\r", submit_deadline)
                # Require the whole unique marker, not only changed digits in a diff frame.
                child.read_until(marker, f"complete uppercase result {token}", submit_deadline)
                latencies_ms.append((time.perf_counter_ns() - started_submit) / 1_000_000)
            quit_ms = graceful_quit(child, b"\x11", "Ctrl-Q")
        finally:
            child.close()

        # Exercise the independent Ctrl-C exit path and exact terminal restoration.
        ctrl_c = start_child(
            binary, program, temp_root, catalog, native_path, work_deadline, hard_deadline, io_timeout
        )
        try:
            ctrl_c.read_until(b"Ready", "Ctrl-C test initial panel")
            ctrl_c_ms = graceful_quit(ctrl_c, b"\x03", "Ctrl-C")
        finally:
            ctrl_c.close()

        # Exercise terminal resize delivery and verify the UI returns to its normal frame.
        resize = start_child(
            binary, program, temp_root, catalog, native_path, work_deadline, hard_deadline, io_timeout
        )
        try:
            resize.read_until(b"Ready", "resize test initial panel")
            resize.clear_transcript()
            set_size(resize.slave, 40, 12)
            resize.signal_process_if_running(signal.SIGWINCH, "small-terminal resize")
            resize.read_until(b"Resize", "small-terminal warning")
            resize.clear_transcript()
            set_size(resize.slave, 100, 30)
            resize.signal_process_if_running(signal.SIGWINCH, "restored-size resize")
            resize.read_until(b"UI latency", "restored-size frame")
            resize_quit_ms = graceful_quit(resize, b"\x11", "resize then Ctrl-Q")
        finally:
            resize.close()

    ordered = sorted(latencies_ms)

    def nearest_rank(fraction):
        return ordered[max(0, math.ceil(fraction * len(ordered)) - 1)]

    return {
        "samples": samples,
        "submit_to_full_uppercase_render_ms": {
            "median": median(ordered),
            "p95_nearest_rank": nearest_rank(0.95),
            "p99_nearest_rank": nearest_rank(0.99),
        },
        "input_reader_lifecycle_cost_ms": {
            "pty_setup_and_process_start_to_first_frame_including_reader_spawn": startup_ms,
            "ctrl_q_to_restored_terminal_exit_including_reader_join": quit_ms,
        },
        "control_checks": {
            "ctrl_c_to_restored_terminal_exit_ms": ctrl_c_ms,
            "resize_warning_and_restore": "passed",
            "resize_ctrl_q_to_restored_terminal_exit_ms": resize_quit_ms,
        },
        "pty": {"columns": 100, "rows": 30, "term": "xterm-256color"},
        "native_catalog": ["printf.json", "tr.json"],
        "native_executables": native_records,
        "forced_cleanup_limit": (
            "unverified if applets leave the PTY child process group or the benchmark runner is killed"
        ),
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True, type=Path, help="built execute binary")
    parser.add_argument("--samples", type=int, default=120, help="must be between 100 and 10000")
    parser.add_argument("--io-timeout", type=float, default=5.0, help="per-PTY wait bound, at most 10s")
    parser.add_argument("--total-timeout", type=float, default=120.0, help="whole-run bound, at most 600s")
    args = parser.parse_args()
    if not 100 <= args.samples <= 10000:
        parser.error("--samples must be between 100 and 10000")
    if not math.isfinite(args.io_timeout) or not 0 < args.io_timeout <= 10:
        parser.error("--io-timeout must be finite, positive, and at most 10 seconds")
    if not math.isfinite(args.total_timeout) or not 0 < args.total_timeout <= 600:
        parser.error("--total-timeout must be finite, positive, and at most 600 seconds")
    report = measure(args.binary, args.samples, args.io_timeout, args.total_timeout)
    print(json.dumps(report, indent=2, sort_keys=True))


if __name__ == "__main__":
    main()
