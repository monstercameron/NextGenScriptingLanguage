#!/usr/bin/env python3
"""Explicit Unix-only live CLI deadline proofs with owned synthetic peers.

This takes about 35 seconds. It runs no .program, native applet or model provider.
The baseline must precede the write fix; select --baseline-response-bounded
when its response-read deadline is already fixed. This is a bounded
availability regression proof, not a throughput benchmark or hard real-time test.
"""
import argparse
from datetime import datetime, timezone
import hashlib
import json
import platform
from pathlib import Path
import select
import socket
import subprocess
import tempfile
import time


def digest(data):
    return hashlib.sha256(data).hexdigest()


def probe(binary, root, environment, mode, expect_bounded):
    address = root / "s"
    payload = b"x" * (800 * 1024)
    text_file = root / "payload.txt"
    text_file.write_bytes(payload)
    args = [str(binary), "live"]
    if mode == "response_drip":
        args += ["read", "/user"]
        observer_seconds = 13.2
    else:
        args += ["create", "/user/payload", "--text-file", str(text_file)]
        observer_seconds = 3.5
    args += ["--socket", str(address)]
    child = None
    peer = None
    received = bytearray()
    sent = 0
    progress_events = 0
    observed_alive = False
    observed_at = None
    started = time.monotonic()
    with socket.socket(socket.AF_UNIX) as listener:
        listener.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 4096)
        listener.bind(str(address))
        listener.listen(1)
        listener.settimeout(3)
        try:
            child = subprocess.Popen(
                args,
                cwd=root,
                env=environment,
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
            )
            peer, _ = listener.accept()
            peer.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 4096)
            receive_buffer = peer.getsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF)
            peer.setblocking(False)
            if mode == "response_drip":
                request_deadline = started + 3
                while b"\n" not in received:
                    if time.monotonic() >= request_deadline:
                        raise RuntimeError("owned peer request deadline")
                    if select.select([peer], [], [], 0.02)[0]:
                        chunk = peer.recv(4096)
                        if not chunk:
                            raise RuntimeError("request ended before LF")
                        received.extend(chunk)
                        if len(received) > 4096:
                            raise RuntimeError("unexpected read request size")
                if json.loads(received) != {"op": "read", "path": "/user"}:
                    raise RuntimeError("unexpected CLI read request")
                prefix = b'{"ok":true,"result":"'
                sent += peer.send(prefix)
            next_progress = time.monotonic()
            while child.poll() is None:
                now = time.monotonic()
                if now - started >= observer_seconds:
                    observed_alive = True
                    observed_at = now - started
                    break
                if now >= next_progress:
                    try:
                        if mode == "response_drip":
                            sent += peer.send(b"a")
                            progress_events += 1
                        elif select.select([peer], [], [], 0)[0]:
                            chunk = peer.recv(1024)
                            if not chunk:
                                break
                            received.extend(chunk)
                            progress_events += 1
                            if len(received) > 1024 * 1024:
                                raise RuntimeError("request exceeded fixture bound")
                    except (BrokenPipeError, ConnectionResetError):
                        break
                    next_progress = now + 0.1
                time.sleep(0.01)
            elapsed = time.monotonic() - started
            # Baseline observation deliberately ends by closing this exact peer;
            # it is not a measured baseline timeout. Never leave owned children.
            peer.close()
            peer = None
            stdout, stderr = child.communicate(timeout=3)
            response = json.loads(stdout)
            if child.returncode != 1 or response.get("ok") is not False or stderr:
                raise RuntimeError("CLI did not report a clean JSON transport error")
            message = response.get("error", {}).get("message", "")
            if expect_bounded:
                expected = 12 if mode == "response_drip" else 2
                if observed_alive or not expected - 0.5 <= elapsed <= expected + 1:
                    raise RuntimeError(
                        f"bounded client budget: mode={mode}, elapsed={elapsed:.3f}, alive={observed_alive}, error={message}"
                    )
                if "live transport deadline exceeded" not in message:
                    raise RuntimeError("bounded client missing deadline diagnostic")
            elif not observed_alive:
                raise RuntimeError("baseline did not outlive observation window")
            if progress_events < 5:
                raise RuntimeError("insufficient ongoing peer byte progress")
            if mode == "request_drain" and b"\n" in received:
                raise RuntimeError("fixture failed to keep request write incomplete")
            return {
                "elapsed_before_peer_close_seconds": elapsed,
                "observer_seconds": observer_seconds,
                "alive_at_observer": observed_alive,
                "observed_at_seconds": observed_at,
                "progress_events": progress_events,
                "peer_received_bytes": len(received),
                "peer_received_sha256": digest(received),
                "peer_sent_bytes": sent,
                "request_lf_received": b"\n" in received,
                "CLI_exit": child.returncode,
                "CLI_error": response,
                "CLI_stderr_bytes": len(stderr),
                "owned_child_reaped": child.poll() is not None,
                "peer_closed": True,
                "requested_socket_receive_buffer_bytes": 4096,
                "actual_socket_receive_buffer_bytes": receive_buffer,
            }
        finally:
            if peer is not None:
                peer.close()
            if child is not None:
                if child.poll() is None:
                    child.kill()
                child.communicate(timeout=3)
            address.unlink(missing_ok=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--baseline", type=Path, required=True)
    parser.add_argument("--candidate", type=Path, required=True)
    parser.add_argument("--report", type=Path, required=True)
    parser.add_argument(
        "--baseline-response-bounded",
        action="store_true",
        help="baseline already enforces12s response budget; its request write remains unbounded",
    )
    args = parser.parse_args()
    if not hasattr(socket, "AF_UNIX") or platform.system() == "Windows":
        parser.error("these owned peer proofs require Unix")
    binaries = {
        name: getattr(args, name).resolve() for name in ["baseline", "candidate"]
    }
    report = {
        "schema": "execute.security-live-client/1",
        "recorded_at_utc": datetime.now(timezone.utc).isoformat(),
        "platform": platform.platform(),
        "harness_sha256": digest(Path(__file__).read_bytes()),
        "binaries_sha256": {
            name: digest(path.read_bytes()) for name, path in binaries.items()
        },
        "scope": "Same-user synthetic Unix peers; actual CLI wire I/O, no programs or providers",
        "limits": "Observed elapsed wire budgets, not connect/CPU/filesystem deadlines or hard real time; baseline is closed at observer cutoff",
        "baseline_response_bounded": args.baseline_response_bounded,
        "probes": {},
    }
    try:
        # Short path also stays below the macOS sockaddr_un path limit.
        with tempfile.TemporaryDirectory(prefix="exlive-", dir="/tmp") as temporary:
            root = Path(temporary)
            environment = {
                "PATH": "/usr/bin:/bin",
                "LC_ALL": "C",
                "LANG": "C",
                "HOME": temporary,
                "TMPDIR": temporary,
            }
            report["environment"] = {
                **environment,
                "HOME": "OWNED_FIXTURE",
                "TMPDIR": "OWNED_FIXTURE",
            }
            report["text_fixture_bytes"] = 800 * 1024
            report["text_fixture_sha256"] = digest(b"x" * (800 * 1024))
            for mode in ["response_drip", "request_drain"]:
                report["probes"][mode] = {}
                for name, binary in binaries.items():
                    report["probes"][mode][name] = probe(
                        binary,
                        root,
                        environment,
                        mode,
                        name == "candidate"
                        or (mode == "response_drip" and args.baseline_response_bounded),
                    )
            if list(root.glob("s")):
                raise RuntimeError("owned socket was not removed")
        report["passed"] = True
    except Exception as error:
        report["passed"] = False
        report["failure"] = str(error)
        raise
    finally:
        args.report.write_text(json.dumps(report, indent=2) + "\n")


if __name__ == "__main__":
    main()
