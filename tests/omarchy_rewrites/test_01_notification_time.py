"""Differential contract tests for the untouched upstream and Execute rewrite."""
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

PROJECT = Path(__file__).resolve().parents[2]
FIXTURE = PROJECT / "tests/fixtures/omarchy-rewrites/01-notification-time"
PROGRAM = PROJECT / "examples/omarchy-rewrites/01-notification-time.program"
DEFAULT_EXECUTE = PROJECT / "target/release/execute"
EXECUTE = Path(os.environ.get("EXECUTE_TEST_BINARY", DEFAULT_EXECUTE))
DATE_FORMAT = "+%A %H:%M  ·  %d %B %Y  ·  Week %V"
SOURCE_SHA256 = "91628c65ea319fdd3fe4cfc4bdd725e7053550faffa5f817c5ad1786bd5ed249"


class NotificationTimeTests(unittest.TestCase):
    def setUp(self):
        self.assertTrue(EXECUTE.is_file(), f"Set EXECUTE_TEST_BINARY: {EXECUTE}")
        self.temp = tempfile.TemporaryDirectory(prefix="omarchy-time-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.bin = self.root / "bin"
        self.bin.mkdir()
        (self.root / "home").mkdir()
        (self.root / "omarchy").mkdir()
        self.env = {
            "PATH": str(self.bin), "HOME": str(self.root / "home"),
            "OMARCHY_PATH": str(self.root / "omarchy"),
            "FIXTURE_ROOT": str(self.root), "LC_ALL": "C", "TZ": "UTC",
            "PYTHONDONTWRITEBYTECODE": "1",
        }
        # The test uses the real jq text filter; it never mocks rewritten logic.
        jq = shutil.which("jq")
        self.assertIsNotNone(jq, "jq is a native dependency of this rewrite")
        (self.bin / "jq").symlink_to(jq)
        mock = (FIXTURE / "mock_command.py").read_text()
        mock = "#!" + sys.executable + "\n" + mock.split("\n", 1)[1]
        for name in ("date", "omarchy-notification-send"):
            path = self.bin / name
            path.write_text(mock)
            path.chmod(0o755)

    def invoke(self, command):
        for name in ("commands.jsonl", "notifications.jsonl"):
            (self.root / name).unlink(missing_ok=True)
        proc = subprocess.run(command, cwd=self.root, env=self.env,
                              stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                              timeout=10, check=False)
        observation = {
            "status": proc.returncode,
            "stdout": proc.stdout.decode(), "stderr": proc.stderr.decode(),
            "commands": [json.loads(line) for line in
                         (self.root / "commands.jsonl").read_text().splitlines()],
            "notifications": [json.loads(line) for line in
                              (self.root / "notifications.jsonl").read_text().splitlines()],
            "home_files": sorted(str(p.relative_to(self.root / "home"))
                                 for p in (self.root / "home").rglob("*")),
            "omarchy_files": sorted(str(p.relative_to(self.root / "omarchy"))
                                    for p in (self.root / "omarchy").rglob("*")),
        }
        return observation

    def compare(self, date_stdout="Wednesday 09:41  ·  14 October 2026  ·  Week 42\n",
                date_status=0, date_stderr="", notification_status=0,
                notification_stdout="", notification_stderr="", args=()):
        config = {
            "date": {"stdout": date_stdout, "stderr": date_stderr, "status": date_status},
            "notification": {"stdout": notification_stdout,
                             "stderr": notification_stderr, "status": notification_status},
        }
        (self.root / "config.json").write_text(json.dumps(config))
        original = self.invoke(["/bin/bash", str(FIXTURE / "upstream.sh"), *args])
        rewrite = self.invoke([str(EXECUTE), str(PROGRAM), "--", *args])
        expected_summary = date_stdout.rstrip("\n")
        self.assertEqual(original["commands"], [
            {"command": "date", "argv": [DATE_FORMAT]},
            {"command": "omarchy-notification-send", "argv": ["-g", "", "-u", "low", expected_summary]},
        ])
        for field in ("status", "stdout", "commands", "notifications", "home_files", "omarchy_files"):
            self.assertEqual(original[field], rewrite[field], field)
        self.assertEqual(original["home_files"], [])
        self.assertEqual(original["omarchy_files"], [])
        if notification_status:
            # Execute preserves exit status but adds its own native failure diagnostic.
            self.assertTrue(rewrite["stderr"].startswith(original["stderr"]))
            self.assertIn("failed", rewrite["stderr"])
        else:
            self.assertEqual(original["stderr"], rewrite["stderr"])
        evidence = os.environ.get("OMARCHY_REWRITE_EVIDENCE_DIR")
        if evidence:
            destination = Path(evidence)
            destination.mkdir(parents=True, exist_ok=True)
            (destination / (self._testMethodName + ".json")).write_text(json.dumps(
                {"case": self._testMethodName, "config": config, "arguments": args,
                 "upstream": original, "execute": rewrite}, ensure_ascii=False, indent=2) + "\n")

    def test_untouched_source(self):
        self.assertEqual(hashlib.sha256((FIXTURE / "upstream.sh").read_bytes()).hexdigest(), SOURCE_SHA256)

    def test_success(self):
        self.compare(notification_stdout="recorded\n")

    def test_literal_summary_and_newlines(self):
        self.compare(date_stdout='$(touch unsafe); * "quote" \\path\nsecond line\n\n\n')
        self.assertFalse((self.root / "unsafe").exists())

    def test_empty_date(self):
        self.compare(date_stdout="\n\n")

    def test_date_failure_still_notifies(self):
        self.compare(date_stdout="partial time\n\n", date_status=7, date_stderr="date fixture failure\n")

    def test_notification_failure(self):
        self.compare(notification_status=23, notification_stderr="notification fixture failure\n")

    def test_ignored_literal_arguments(self):
        self.compare(args=("", "--help", "$(touch unsafe)", "a b", "a\nb", "*"))
        self.assertFalse((self.root / "unsafe").exists())


if __name__ == "__main__":
    unittest.main()
