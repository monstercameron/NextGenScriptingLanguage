#!/usr/bin/env python3
"""Pinned Omarchy interface proofs with fake backends, never desktop actions."""
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
FIXTURES = ROOT / "tests/fixtures/omarchy-v4.0.4"
PORTS = ROOT / "examples/omarchy-ports"
SPECS = {
    "omarchy-menu-clipboard": ["omarchy-shell", "shell", "toggle", "omarchy.clipboard"],
    "omarchy-menu-emoji": ["omarchy-shell", "shell", "toggle", "omarchy.emojis"],
    "omarchy-notification-weather": [
        "omarchy-shell",
        "shell",
        "toggle",
        "omarchy.weather",
    ],
    "omarchy-hyprland-window-gaps-toggle": [
        "omarchy-hyprland-toggle",
        "window-no-gaps",
    ],
    "omarchy-powerprofiles-init": ["omarchy-powerprofiles-set", "autodetect"],
    "omarchy-launch-terminal-herdr": ["omarchy-launch-terminal", "herdr"],
    "omarchy-restart-hyprsunset": ["omarchy-restart-app", "hyprsunset"],
    "omarchy-remove-gaming-xbox-cloud": ["omarchy-webapp-remove", "Xbox Cloud Gaming"],
    "omarchy-launch-nautilus": ["setsid", "uwsm-app", "--", "nautilus", "--new-window"],
    "omarchy-notification-dismiss": ["omarchy-shell", "-q", "notifications", "dismiss"],
}
BACKEND = """#!{python}
import json, os, pathlib, sys
pathlib.Path(os.environ["CALL"]).write_text(json.dumps([pathlib.Path(sys.argv[0]).name, *sys.argv[1:]], ensure_ascii=False))
os.write(1, os.environ["OUTPUT"].encode())
if os.environ["STATUS"] != "0":
    os.write(2, b"native fixture failure\\n")
sys.exit(int(os.environ["STATUS"]))
"""


class OmarchyInterfaces(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.binary = Path(
            os.environ.get("EXECUTE_TEST_BINARY", ROOT / "target/release/execute")
        ).resolve()
        if not cls.binary.is_file():
            raise RuntimeError("Build the source runtime or set EXECUTE_TEST_BINARY")

    def test_sources_match_the_pinned_inventory(self):
        inventory = json.loads((ROOT / "docs/omarchy-inventory.json").read_text())
        entries = {Path(entry["path"]).name: entry for entry in inventory["commands"]}
        for name in SPECS:
            with self.subTest(name=name):
                self.assertEqual(
                    hashlib.sha256((FIXTURES / "bin" / name).read_bytes()).hexdigest(),
                    entries[name]["sha256"],
                )

    def invoke(self, folder, name, arguments, status=0, output="success\n"):
        command = SPECS[name][0]
        backend = folder / command
        backend.write_text(BACKEND.format(python=sys.executable))
        backend.chmod(0o700)
        call = folder / "call.json"
        # No inherited shell functions, startup hooks, credentials or real tools.
        env = {
            "PATH": str(folder),
            "LC_ALL": "C",
            "CALL": str(call),
            "STATUS": str(status),
            "OUTPUT": output,
        }
        observations = {}
        for side, argv in {
            "bash": [
                "/bin/bash",
                "--noprofile",
                "--norc",
                "-p",
                str(FIXTURES / "bin" / name),
                *arguments,
            ],
            "execute": [
                str(self.binary),
                str(PORTS / (name + ".program")),
                "--manifest-dir",
                str(ROOT / "manifests"),
                "--",
                *arguments,
            ],
        }.items():
            call.unlink(missing_ok=True)
            result = subprocess.run(
                argv, cwd=folder, env=env, capture_output=True, timeout=5
            )
            self.assertTrue(
                call.is_file(),
                f"{side} did not invoke the isolated backend: {result.stderr!r}",
            )
            observations[side] = (result, json.loads(call.read_text()))
        return observations

    def test_valid_calls_preserve_literal_argv_output_and_status(self):
        values = ["spaces and 雪", "--help", "$(touch forbidden); *\n"]
        for name, native in SPECS.items():
            for value in values:
                with self.subTest(
                    name=name, value=value
                ), tempfile.TemporaryDirectory() as tmp:
                    folder = Path(tmp)
                    observations = self.invoke(folder, name, [value, "ignored extra"])
                    expected = native + (
                        [value] if name == "omarchy-notification-dismiss" else []
                    )
                    for side, (result, argv) in observations.items():
                        self.assertEqual(argv, expected, side)
                        self.assertEqual(
                            (result.returncode, result.stdout, result.stderr),
                            (0, b"success\n", b""),
                            side,
                        )
                    self.assertFalse((folder / "forbidden").exists())

    def test_native_failures_preserve_status_and_stdout_like_bash(self):
        # Uncaptured stdout streams as Bash does, and it is kept when the command fails.
        for name in SPECS:
            with self.subTest(name=name), tempfile.TemporaryDirectory() as tmp:
                observations = self.invoke(
                    Path(tmp), name, ["summary"], status=7, output="partial\n"
                )
                bash, bash_argv = observations["bash"]
                execute, execute_argv = observations["execute"]
                self.assertEqual(execute_argv, bash_argv)
                self.assertEqual((execute.returncode, bash.returncode), (7, 7))
                self.assertEqual(bash.stdout, b"partial\n")
                self.assertEqual(execute.stdout, bash.stdout)
                self.assertEqual(bash.stderr, b"native fixture failure\n")
                self.assertIn(b"native fixture failure", execute.stderr)
                self.assertIn(b"exit status: 7", execute.stderr)

    def test_missing_summary_is_an_explicit_cli_compatibility_gap(self):
        with tempfile.TemporaryDirectory() as tmp:
            folder = Path(tmp)
            for side, argv in {
                "bash": [
                    "/bin/bash",
                    "--noprofile",
                    "--norc",
                    "-p",
                    str(FIXTURES / "bin/omarchy-notification-dismiss"),
                ],
                "execute": [
                    str(self.binary),
                    str(PORTS / "omarchy-notification-dismiss.program"),
                    "--manifest-dir",
                    str(ROOT / "manifests"),
                ],
            }.items():
                result = subprocess.run(
                    argv,
                    cwd=folder,
                    env={"PATH": str(folder), "LC_ALL": "C"},
                    capture_output=True,
                    timeout=5,
                )
                self.assertEqual(result.returncode, 1)
                if side == "bash":
                    self.assertEqual(
                        result.stdout,
                        b"Usage: omarchy-notification-dismiss <summary>\n",
                    )
                    self.assertEqual(result.stderr, b"")
                else:
                    self.assertEqual(result.stdout, b"")
                    self.assertIn(b"missing required input", result.stderr)


if __name__ == "__main__":
    unittest.main()
