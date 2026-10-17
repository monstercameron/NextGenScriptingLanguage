#!/usr/bin/env python3
"""Compare Omarchy Bash originals with their Execute ports, case by case.

Each port lives in examples/omarchy-ports/<script>/ with:

  original.sh   the Bash script as upstream wrote it, with a copyright header
  port.program  the Execute port
  case.json     the cases: argv, inputs, environment, files, stdin, and fake binaries

For every case the harness runs the Bash original and the port in two fresh sandbox
directories. Both get the same argv and environment, the same initial files, and a
PATH whose first entry holds fake versions of the external commands the case names.
Each fake appends its name and argv to a shared log and prints its configured output.
The harness then compares exit status, stdout, stderr, the fake-call log, and the
final file tree of each run, byte for byte.

Unix only. The fakes are POSIX shell scripts, so this runs in CI on Linux, not on
Windows. Standard library only.

Usage:
  python3 tools/port_compare.py                 all ports under examples/omarchy-ports
  python3 tools/port_compare.py --only NAME     one port directory
  python3 tools/port_compare.py --binary PATH   the execute binary to test
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import shutil
import stat
import subprocess
import sys
import tempfile
from pathlib import Path
from typing import Any

ROOT = Path(__file__).resolve().parents[1]
PORTS_DIR = ROOT / "examples" / "omarchy-ports"
MANIFESTS = ROOT / "manifests"
RUN_TIMEOUT = 30

# Fake every run has, so a port's bookkeeping can always be checked.
FAKE_PROLOGUE = """#!/bin/sh
name=$(basename "$0")
{
  printf '%s' "$name"
  for argument in "$@"; do
    printf ' [%s]' "$argument"
  done
  printf '\\n'
} >> "$PORT_FAKE_LOG"
"""


def fake_script(name: str, spec: dict[str, Any]) -> str:
    """A shell script that logs its call, then runs the case's body or prints its output."""
    lines = [FAKE_PROLOGUE]
    if "stdout" in spec:
        lines.append(f"printf '%s' {shell_quote(spec['stdout'])}\n")
    if "body" in spec:
        # The body's last command sets the fake's status, unless the case fixes one.
        lines.append(spec["body"] + "\n")
        if "status" in spec:
            lines.append(f"exit {int(spec['status'])}\n")
    else:
        lines.append(f"exit {int(spec.get('status', 0))}\n")
    return "".join(lines)


def shell_quote(text: str) -> str:
    return "'" + text.replace("'", "'\\''") + "'"


def tree_digest(root: Path) -> dict[str, str]:
    """Relative path to a digest of each file, directory, or symlink under root."""
    found: dict[str, str] = {}
    if not root.exists():
        return found
    for current, directories, files in os.walk(root):
        for name in sorted(directories + files):
            path = Path(current) / name
            relative = path.relative_to(root).as_posix()
            if path.is_symlink():
                found[relative] = "symlink:" + os.readlink(path)
            elif path.is_dir():
                found[relative] = "dir"
            else:
                digest = hashlib.sha256(path.read_bytes()).hexdigest()
                found[relative] = f"file:{digest}"
    return found


class Sandbox:
    """One fresh directory tree for one run."""

    def __init__(self, case: dict[str, Any]):
        self.path = Path(tempfile.mkdtemp(prefix="port-compare-"))
        self.home = self.path / "home"
        self.work = self.path / "work"
        self.bin = self.path / "bin"
        self.log = self.path / "fake.log"
        for directory in (self.home, self.work, self.bin):
            directory.mkdir()
        self.log.write_text("")
        for name, spec in case.get("fakes", {}).items():
            script = self.bin / name
            script.write_text(fake_script(name, spec))
            script.chmod(script.stat().st_mode | stat.S_IEXEC | stat.S_IXGRP | stat.S_IXOTH)
        for relative, content in case.get("files", {}).items():
            root_name, _, inner = relative.partition("/")
            root = {"home": self.home, "work": self.work}.get(root_name, self.work)
            target = root / (inner if root_name in ("home", "work") else relative)
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_text(content)

    def environment(self, case: dict[str, Any]) -> dict[str, str]:
        env = {
            "PATH": f"{self.bin}:/usr/bin:/bin",
            "HOME": str(self.home),
            "LANG": "C.UTF-8",
            "LC_ALL": "C.UTF-8",
            "PORT_FAKE_LOG": str(self.log),
        }
        env.update(case.get("env", {}))
        return env

    def snapshot(self) -> dict[str, Any]:
        return {
            "fake_calls": self.log.read_text(),
            "home": tree_digest(self.home),
            "work": tree_digest(self.work),
        }

    def remove(self) -> None:
        shutil.rmtree(self.path, ignore_errors=True)


def run_once(command: list[str], sandbox: Sandbox, case: dict[str, Any]) -> dict[str, Any]:
    stdin = case.get("stdin", "").encode()
    try:
        completed = subprocess.run(
            command,
            cwd=sandbox.work,
            env=sandbox.environment(case),
            input=stdin,
            capture_output=True,
            timeout=RUN_TIMEOUT,
        )
        status = completed.returncode
        stdout, stderr = completed.stdout, completed.stderr
    except subprocess.TimeoutExpired as error:
        status, stdout, stderr = -1, error.stdout or b"", (error.stderr or b"") + b"<timeout>"
    result = {"status": status, "stdout": stdout.decode(errors="replace"), "stderr": stderr.decode(errors="replace")}
    result.update(sandbox.snapshot())
    # Each run has its own temporary root, so paths under it differ between the two runs.
    root = str(sandbox.path)
    for key in ("stdout", "stderr", "fake_calls"):
        result[key] = result[key].replace(root, "<SANDBOX>")
    return result


def run_case(port: Path, case: dict[str, Any], binary: Path) -> list[str]:
    argv = case.get("argv", [])
    inputs = case.get("inputs", {})
    original = [ "bash", str(port / "original.sh"), *argv]
    program = str(port / "port.program")
    execute = [str(binary), "run", program, "--manifest-dir", str(MANIFESTS)]
    for name, value in inputs.items():
        execute += ["--input", f"{name}={value}"]
    execute += ["--", *argv]

    sandboxes = [Sandbox(case), Sandbox(case)]
    try:
        bash_result = run_once(original, sandboxes[0], case)
        port_result = run_once(execute, sandboxes[1], case)
    finally:
        for sandbox in sandboxes:
            sandbox.remove()
    return differences(bash_result, port_result)


def differences(expected: dict[str, Any], actual: dict[str, Any]) -> list[str]:
    problems = []
    for key in ("status", "stdout", "stderr", "fake_calls", "home", "work"):
        if expected[key] != actual[key]:
            problems.append(f"{key}: bash={expected[key]!r} port={actual[key]!r}")
    return problems


def load_cases(port: Path) -> list[dict[str, Any]]:
    data = json.loads((port / "case.json").read_text())
    if data.get("version") != 1:
        raise SystemExit(f"{port}: case.json version must be 1")
    return data["cases"]


def check_port(port: Path) -> None:
    for name in ("original.sh", "port.program", "case.json"):
        if not (port / name).is_file():
            raise SystemExit(f"{port}: missing {name}")
    header = (port / "original.sh").read_text().splitlines()[:8]
    if not any("MIT License" in line for line in header):
        raise SystemExit(f"{port}: original.sh needs its MIT license header")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--ports", type=Path, default=PORTS_DIR)
    parser.add_argument("--only", help="one port directory name")
    parser.add_argument("--binary", type=Path, default=None)
    args = parser.parse_args(argv)
    # Runs happen in sandboxes with their own working directory, so paths must be absolute.
    args.ports = args.ports.resolve()
    if os.name != "posix":
        raise SystemExit("port_compare runs the Bash originals and POSIX fakes; use a Unix host")
    binary = args.binary or next(
        (path for path in (ROOT / "target" / "release" / "execute", ROOT / "target" / "debug" / "execute") if path.is_file()),
        None,
    )
    if binary is None or not binary.is_file():
        raise SystemExit("build the execute binary first: cargo build --release --bin execute")
    binary = binary.resolve()

    ports = sorted(path for path in args.ports.iterdir() if path.is_dir() and (path / "case.json").is_file())
    if args.only:
        ports = [port for port in ports if port.name == args.only]
        if not ports:
            raise SystemExit(f"no port named {args.only!r}")
    failures = 0
    total = 0
    for port in ports:
        check_port(port)
        for case in load_cases(port):
            total += 1
            problems = run_case(port, case, binary)
            label = f"{port.name} :: {case['name']}"
            if problems:
                failures += 1
                print(f"FAIL {label}")
                for problem in problems:
                    print(f"     {problem}")
            else:
                print(f"ok   {label}")
    print(f"{total - failures}/{total} cases identical across {len(ports)} port(s)")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
