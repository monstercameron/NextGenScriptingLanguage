#!/usr/bin/env python3
"""Regression checks for the sandbox-denial lane (issue #457).

The lane must fail closed: a filter or cfg regression that runs zero tests,
a missing fixture, or a skipped sandbox probe must all turn the job red.
These checks pin the workflow guards and prove the shared log guard rejects
each silent-green shape while accepting a real passing log.
"""

from __future__ import annotations

import os
import pathlib
import subprocess
import sys
import tempfile
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[1]
WORKFLOW_PATH = pathlib.Path(
    os.environ.get("SANDBOX_DENIAL_WORKFLOW", ROOT / ".github/workflows/sandbox-denial.yml")
)
GUARD = ROOT / "tools" / "sandbox_test_guard.py"
TARGETS = ("lane_v8_sandbox_denial", "lane_v8_untrusted", "lane_v8_hosts")

GOOD_LOG = """\
running 2 tests
test untrusted_mode_denies_an_ungranted_write ... ok
test untrusted_mode_denies_an_ungranted_network_connection ... ok

test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.12s
"""

ZERO_LOG = """\
running 0 tests

test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
"""

SKIPPED_LOG = (
    GOOD_LOG + "skipping untrusted network denial: OS sandbox facility unavailable\n"
)

NO_SUMMARY_LOG = "   Compiling execute v0.1.0\n    Finished release profile\n"


class SandboxDenialWorkflowTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.workflow = WORKFLOW_PATH.read_text(encoding="utf-8")
        linux, macos, windows = cls._split_jobs(cls.workflow)
        cls.linux = linux
        cls.macos = macos
        cls.windows = windows

    @staticmethod
    def _split_jobs(workflow: str) -> tuple[str, str, str]:
        linux = workflow.split("  linux-denial:\n", maxsplit=1)[1]
        linux, remainder = linux.split("  macos-denial:\n", maxsplit=1)
        macos, windows = remainder.split("  windows-refusal:\n", maxsplit=1)
        return linux, macos, windows

    def run_guard(self, contents: str | None, *args: str) -> subprocess.CompletedProcess[str]:
        with tempfile.TemporaryDirectory() as directory:
            if contents is None:
                log = pathlib.Path(directory) / "missing.log"
            else:
                log = pathlib.Path(directory) / "cargo.log"
                log.write_text(contents, encoding="utf-8")
            return subprocess.run(
                [sys.executable, str(GUARD), str(log), *args],
                capture_output=True,
                text=True,
                check=False,
            )

    def test_every_cargo_step_routes_its_log_through_the_guard(self) -> None:
        self.assertEqual(self.workflow.count("python3 tools/sandbox_test_guard.py"), 5)

    def test_cli_binaries_run_and_guard_per_binary_on_every_os(self) -> None:
        loop = "for target in lane_v8_sandbox_denial lane_v8_untrusted lane_v8_hosts"
        for job in (self.linux, self.macos, self.windows):
            with self.subTest(job=job.splitlines()[0]):
                self.assertIn(loop, job)
                self.assertIn('--test "$target"', job)

    def test_sandbox_skip_stays_mandatory_on_unix(self) -> None:
        for job in (self.linux, self.macos):
            self.assertIn('EXECUTE_REQUIRE_OS_SANDBOX: "1"', job)
        guard = GUARD.read_text(encoding="utf-8")
        self.assertIn('re.compile(r"skipping ")', guard)

    def test_linux_checks_the_fixed_python_paths_the_tests_use(self) -> None:
        self.assertIn("test -f /usr/bin/python3 || test -f /bin/python3", self.linux)

    def test_macos_nc_fixture_check_is_hard(self) -> None:
        self.assertIn("test -f /usr/bin/nc", self.macos)
        self.assertNotIn("nc -h", self.macos)
        self.assertNotIn("|| true", self.macos)

    def test_windows_cli_loop_runs_under_bash(self) -> None:
        self.assertIn("shell: bash", self.windows)

    def test_guard_accepts_a_passing_log(self) -> None:
        result = self.run_guard(GOOD_LOG, "passing (Linux)")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_guard_accepts_a_multi_digit_pass_count(self) -> None:
        result = self.run_guard(GOOD_LOG.replace("2 passed", "12 passed"), "passing (macOS)")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_guard_rejects_zero_tests(self) -> None:
        result = self.run_guard(ZERO_LOG, "empty (Linux)")
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn("zero tests", result.stdout + result.stderr)

    def test_guard_rejects_a_log_without_a_test_summary(self) -> None:
        result = self.run_guard(NO_SUMMARY_LOG, "build-only (Linux)")
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)

    def test_guard_rejects_skips_even_with_passes(self) -> None:
        result = self.run_guard(SKIPPED_LOG, "skipped (macOS)")
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn("skipped", result.stdout + result.stderr)

    def test_guard_rejects_a_missing_log(self) -> None:
        result = self.run_guard(None, "missing (Windows)")
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)

    def test_guard_rejects_bad_usage(self) -> None:
        result = subprocess.run(
            [sys.executable, str(GUARD)], capture_output=True, text=True, check=False
        )
        self.assertEqual(result.returncode, 2)


if __name__ == "__main__":
    unittest.main()
