#!/usr/bin/env python3
"""Regression checks for the nightly flaky-test detection job (#89).

The job itself lives in .github/workflows/flaky.yml: the suite runs twenty
times on every OS with cargo-nextest and no retries, and tools/collect_flaky.py
aggregates the JUnit reports, names every test that ever failed (with its
output), and exits nonzero so the job fails. These checks pin that contract:
the repeat wiring, the no-retry profile, and the reporter's exit behavior.
"""

from __future__ import annotations

import json
import re
import sys
import tempfile
import unittest
from contextlib import redirect_stdout
from io import StringIO
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "tools"))
import collect_flaky


EXPECTED_OS = ("ubuntu-24.04", "macos-15", "windows-2022")
EXPECTED_RUNS = 20


def write_run(root: Path, run: str, cases: list[tuple[str, str, str, str, str]]) -> None:
    """Write one downloaded JUnit report. Each case is
    (classname, name, status, failure_detail, captured_output); status is
    "passed" or "failed".
    """
    folder = root / f"junit-{run}"
    folder.mkdir(parents=True, exist_ok=True)
    body = []
    for classname, name, status, detail, output in cases:
        if status == "failed":
            inner = f'<failure message="{detail}">{detail}</failure>'
            if output:
                inner += f"<system-err>{output}</system-err>"
        else:
            inner = ""
        body.append(f'<testcase classname="{classname}" name="{name}">{inner}</testcase>')
    (folder / "junit.xml").write_text(
        f"<testsuites><testsuite>{''.join(body)}</testsuite></testsuites>", encoding="utf-8"
    )


def run_collector(root: Path, *extra: str) -> tuple[int, str, Path, Path]:
    """Run the collector over one report directory; return
    (exit_code, stdout_summary, json_path, summary_path).
    """
    json_path = root / "flaky-report.json"
    summary_path = root / "summary.md"
    stdout = StringIO()
    with redirect_stdout(stdout):
        code = collect_flaky.main(
            [str(root), "--json", str(json_path), "--summary", str(summary_path), *extra]
        )
    return code, stdout.getvalue(), json_path, summary_path


class CollectorExitTests(unittest.TestCase):
    def test_all_passing_runs_report_zero(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            write_run(root, "ubuntu-24.04-1", [("s", "case", "passed", "", "")])
            write_run(root, "ubuntu-24.04-2", [("s", "case", "passed", "", "")])
            code, stdout, _, summary_path = run_collector(root)
            summary = summary_path.read_text(encoding="utf-8")
        self.assertEqual(code, 0)
        self.assertIn("No test failed in any run", stdout)
        self.assertIn("No test failed in any run", summary)

    def test_flaky_test_is_reported_with_output_and_fails(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            write_run(root, "ubuntu-24.04-1", [("s", "case", "failed", "boom", "child stderr")])
            write_run(root, "ubuntu-24.04-2", [("s", "case", "passed", "", "")])
            code, stdout, json_path, _ = run_collector(root)
            document = json.loads(json_path.read_text(encoding="utf-8"))
        self.assertEqual(code, 1)
        self.assertIn("**flaky**", stdout)
        self.assertIn("boom", stdout)
        self.assertIn("child stderr", stdout)
        self.assertEqual(document["tests"]["s case"]["status"], "flaky")

    def test_consistent_failure_is_failing(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for run in ("ubuntu-24.04-1", "ubuntu-24.04-2"):
                write_run(root, run, [("s", "case", "failed", "boom", "")])
            code, stdout, _, _ = run_collector(root)
        self.assertEqual(code, 1)
        self.assertIn("**failing**", stdout)

    def test_missing_run_report_fails_the_job(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            write_run(root, "ubuntu-24.04-1", [("s", "case", "passed", "", "")])
            code, stdout, _, _ = run_collector(
                root, "--expect-runs", "2", "--expect-os", "ubuntu-24.04"
            )
        self.assertEqual(code, 1)
        self.assertIn("junit-ubuntu-24.04-2", stdout)

    def test_unreadable_report_fails_the_job(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            folder = root / "junit-ubuntu-24.04-1"
            folder.mkdir(parents=True, exist_ok=True)
            (folder / "junit.xml").write_text("not xml at all", encoding="utf-8")
            code, stdout, _, _ = run_collector(root)
        self.assertEqual(code, 1)
        self.assertIn("Unreadable reports", stdout)

    def test_mismatched_expect_flags_are_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaises(SystemExit) as raised:
                collect_flaky.main([directory, "--expect-runs", "2"])
        self.assertEqual(raised.exception.code, 2)


class NightlyJobContractTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.workflow = (ROOT / ".github/workflows/flaky.yml").read_text(encoding="utf-8")
        cls.profile = (ROOT / ".config/nextest.toml").read_text(encoding="utf-8")

    def test_repeat_job_is_scheduled_nightly_and_dispatchable(self) -> None:
        self.assertIn("schedule:", self.workflow)
        self.assertIn("cron:", self.workflow)
        self.assertIn("workflow_dispatch:", self.workflow)

    def test_repeat_job_covers_every_os(self) -> None:
        self.assertEqual(self.workflow.count("os: [ubuntu-24.04, macos-15, windows-2022]"), 2)
        for platform in EXPECTED_OS:
            self.assertIn(f"--expect-os {platform}", self.workflow)

    def test_repeat_job_runs_twenty_times_with_no_retries(self) -> None:
        match = re.search(r"(?m)^\s*run: \[(.*?)\]\s*$", self.workflow)
        self.assertIsNotNone(match, "flaky.yml must pin the repeat count in its matrix")
        runs = [entry.strip() for entry in match.group(1).split(",")]
        self.assertEqual(len(runs), EXPECTED_RUNS)
        self.assertIn("--profile flaky", self.workflow)
        self.assertIn(f"--expect-runs {EXPECTED_RUNS}", self.workflow)

    def test_repeat_job_reports_failures_with_output_and_fails(self) -> None:
        self.assertIn("python3 tools/collect_flaky.py junit", self.workflow)
        self.assertIn("$GITHUB_STEP_SUMMARY", self.workflow)
        self.assertIn("flaky-report.json", self.workflow)

    def test_flaky_nextest_profile_disables_retries_and_keeps_failure_output(self) -> None:
        profile = self.profile.split("[profile.flaky]", maxsplit=1)[1]
        profile = profile.split("[", maxsplit=1)[0]
        self.assertIn("retries = 0", profile)
        junit = self.profile.split("[profile.flaky.junit]", maxsplit=1)[1]
        self.assertIn("store-failure-output = true", junit)


if __name__ == "__main__":
    unittest.main()
