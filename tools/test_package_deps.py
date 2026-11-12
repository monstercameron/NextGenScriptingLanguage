#!/usr/bin/env python3
"""Contract tests for `package.py deps`: executables to distro packages (issue #425)."""
import json
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from package import (  # noqa: E402
    candidate_paths,
    executables_from_explain,
    explain_executables,
    resolve_depends,
    system_runner,
)

PACMAN_OUTPUT = {
    ("pacman", "-F", "usr/bin/ls"): "extra/coreutils 9.5-1\n    usr/bin/ls\n",
    ("pacman", "-F", "usr/sbin/ls"): "",
    ("pacman", "-F", "usr/bin/notify-send"): (
        "extra/libnotify 0.8.3-1\n    usr/bin/notify-send\n"
        "extra/notify-send-extra 1.0-1\n    usr/bin/notify-send\n"
    ),
    ("pacman", "-F", "usr/sbin/notify-send"): "",
    ("pacman", "-F", "usr/bin/omarchy-shell"): "",
    ("pacman", "-F", "usr/sbin/omarchy-shell"): "",
    ("pacman", "-F", "usr/bin/sh"): "core/bash 5.2-1\n    usr/bin/sh\n",
    ("pacman", "-F", "usr/sbin/sh"): "",
}

DPKG_OUTPUT = {
    ("dpkg", "-S", "/usr/bin/ls"): "coreutils: /usr/bin/ls\n",
    ("dpkg", "-S", "/usr/sbin/ls"): "",
    ("dpkg", "-S", "/usr/bin/notify-send"): (
        "libnotify-bin:amd64: /usr/bin/notify-send\n"
        "diversion by foo from: /usr/bin/notify-send\n"
    ),
    ("dpkg", "-S", "/usr/sbin/notify-send"): "",
    ("dpkg", "-S", "/usr/bin/omarchy-shell"): "",
    ("dpkg", "-S", "/usr/sbin/omarchy-shell"): "",
    ("dpkg", "-S", "/usr/bin/env"): "coreutils: /usr/bin/env\n",
    ("dpkg", "-S", "/usr/sbin/env"): "",
}


def fake_runner(table):
    def runner(argv):
        return table.get(tuple(argv), "")

    return runner


class CandidatePathTests(unittest.TestCase):
    def test_bare_names_look_in_system_bin_dirs(self):
        self.assertEqual(candidate_paths("ls"), ["usr/bin/ls", "usr/sbin/ls"])

    def test_absolute_names_are_queried_as_given(self):
        self.assertEqual(candidate_paths("/opt/tool/bin/x"), ["opt/tool/bin/x"])

    def test_relative_and_empty_names_are_not_system_executables(self):
        for name in ["./local", "bin/x", "", ".", ".."]:
            self.assertIsNone(candidate_paths(name), name)


class ExplainParseTests(unittest.TestCase):
    def test_reads_summary_executables(self):
        text = json.dumps({"summary": {"executables": ["ls", "notify-send"]}})
        self.assertEqual(executables_from_explain(text), ["ls", "notify-send"])

    def test_rejects_output_without_the_executables_list(self):
        with self.assertRaises(ValueError):
            executables_from_explain(json.dumps({"summary": {}}))
        with self.assertRaises(ValueError):
            executables_from_explain(json.dumps({"summary": {"executables": [1]}}))

    def test_explain_runs_the_command_with_json_flag(self):
        fake = (
            "import json, sys\n"
            "assert sys.argv[1] == 'explain' and sys.argv[3] == '--json'\n"
            "print(json.dumps({'summary': {'executables': ['ls']}}))\n"
        )
        with tempfile.TemporaryDirectory() as temp:
            script = Path(temp) / "fake_execute.py"
            script.write_text(fake, encoding="utf-8")
            names = explain_executables(
                [sys.executable, script], Path(temp) / "program.program"
            )
        self.assertEqual(names, ["ls"])


class ArchDependsTests(unittest.TestCase):
    def test_pacman_owners_become_depends_and_missing_is_flagged(self):
        depends, unresolved = resolve_depends(
            ["ls", "omarchy-shell", "sh"], "arch", fake_runner(PACMAN_OUTPUT)
        )
        self.assertEqual(depends, ["bash", "coreutils"])
        self.assertEqual(
            unresolved,
            [("omarchy-shell", "no arch package owns it")],
        )

    def test_two_owners_are_flagged_as_ambiguous_not_guessed(self):
        depends, unresolved = resolve_depends(
            ["notify-send"], "arch", fake_runner(PACMAN_OUTPUT)
        )
        self.assertEqual(depends, [])
        self.assertEqual(
            unresolved,
            [("notify-send", "ambiguous: libnotify, notify-send-extra")],
        )

    def test_relative_executable_is_flagged_without_a_query(self):
        calls = []

        def runner(argv):
            calls.append(argv)
            return ""

        depends, unresolved = resolve_depends(["./local-tool"], "arch", runner)
        self.assertEqual(depends, [])
        self.assertEqual(calls, [])
        self.assertEqual(len(unresolved), 1)


class DebianDependsTests(unittest.TestCase):
    def test_dpkg_owners_strip_architecture_and_missing_is_flagged(self):
        depends, unresolved = resolve_depends(
            ["ls", "env", "omarchy-shell"], "debian", fake_runner(DPKG_OUTPUT)
        )
        self.assertEqual(depends, ["coreutils"])
        self.assertEqual(
            unresolved,
            [("omarchy-shell", "no debian package owns it")],
        )

    def test_diversion_lines_do_not_count_as_owners(self):
        depends, unresolved = resolve_depends(
            ["notify-send"], "debian", fake_runner(DPKG_OUTPUT)
        )
        self.assertEqual(depends, ["libnotify-bin"])
        self.assertEqual(unresolved, [])


class SystemRunnerTests(unittest.TestCase):
    def run_exit(self, code):
        return system_runner(
            [sys.executable, "-c", "import sys; print('owned'); sys.exit(%d)" % code]
        )

    def test_exit_one_is_no_match(self):
        self.assertEqual(self.run_exit(1), "")

    def test_success_returns_stdout(self):
        self.assertEqual(self.run_exit(0), "owned\n")

    def test_other_failures_are_errors_not_silent_gaps(self):
        with self.assertRaises(RuntimeError):
            self.run_exit(2)


if __name__ == "__main__":
    unittest.main()
