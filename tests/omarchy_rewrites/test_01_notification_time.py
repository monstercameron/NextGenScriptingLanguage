"""Differential contract tests for the untouched upstream and Execute rewrite."""
import calendar
import datetime
import hashlib
import json
import os
from pathlib import Path
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
# 2026-10-14T09:41:00Z: Wednesday, 14 October 2026, ISO week 42. The rewrite
# reads this instant through the injectable clock instead of spawning date.
FIXED_NOW = 1791970860
NOTIFICATION_ICON = ""


def fixed_summary():
    moment = datetime.datetime.fromtimestamp(
        FIXED_NOW, tz=datetime.timezone.utc)
    return moment.strftime("%A %H:%M  ·  %d %B %Y  ·  Week %V")


class NotificationTimeTests(unittest.TestCase):
    def setUp(self):
        self.assertTrue(EXECUTE.is_file(), f"Set EXECUTE_TEST_BINARY: {EXECUTE}")
        self.assertEqual(
            calendar.timegm((2026, 10, 14, 9, 41, 0, 0, 0, 0)), FIXED_NOW)
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
            # The rewrite reads the injectable clock in process; the upstream
            # mock below renders the same instant for the Bash comparison.
            "EXECUTE_NOW": str(FIXED_NOW),
            "PYTHONDONTWRITEBYTECODE": "1",
        }
        mock = (FIXTURE / "mock_command.py").read_text()
        mock = "#!" + sys.executable + "\n" + mock.split("\n", 1)[1]
        for name in ("date", "omarchy-notification-send"):
            path = self.bin / name
            path.write_text(mock)
            path.chmod(0o755)

    def invoke(self, command, extra_env=None):
        for name in ("commands.jsonl", "notifications.jsonl"):
            (self.root / name).unlink(missing_ok=True)
        env = dict(self.env)
        env.update(extra_env or {})
        proc = subprocess.run(command, cwd=self.root, env=env,
                              stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                              timeout=10, check=False)
        observation = {
            "status": proc.returncode, "stdout": proc.stdout.decode(),
            "stderr": proc.stderr.decode(),
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

    def rewrite_command(self, args):
        return [str(EXECUTE), str(PROGRAM), "--", *args]

    def compare_matching(self, date_stdout=None, notification_status=0,
                         notification_stdout="", notification_stderr="",
                         args=(), rewrite_extra_env=None):
        """Both sides notify the fixed-clock summary; the mock date pads it."""
        summary = fixed_summary()
        if date_stdout is None:
            date_stdout = summary + "\n"
        config = {
            "date": {"stdout": date_stdout, "stderr": "", "status": 0},
            "notification": {"stdout": notification_stdout,
                             "stderr": notification_stderr,
                             "status": notification_status},
        }
        (self.root / "config.json").write_text(json.dumps(config))
        original = self.invoke(["/bin/bash", str(FIXTURE / "upstream.sh"), *args])
        rewrite = self.invoke(self.rewrite_command(args), rewrite_extra_env)
        self.assertEqual(original["commands"], [
            {"command": "date", "argv": [DATE_FORMAT]},
            {"command": "omarchy-notification-send",
             "argv": ["-g", NOTIFICATION_ICON, "-u", "low",
                      date_stdout.rstrip("\n")]},
        ])
        # The rewrite spawns no clock or text-filter subprocess: the only
        # recorded command is the notification itself, carrying the same
        # summary the fixed clock renders.
        self.assertEqual(rewrite["commands"], [
            {"command": "omarchy-notification-send",
             "argv": ["-g", NOTIFICATION_ICON, "-u", "low", summary]},
        ])
        for field in ("status", "stdout", "commands", "notifications",
                      "home_files", "omarchy_files"):
            if field == "commands":
                continue
            self.assertEqual(original[field], rewrite[field], field)
        self.assertEqual(original["notifications"], [
            ["-g", NOTIFICATION_ICON, "-u", "low", summary]])
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
        self.compare_matching(notification_stdout="recorded\n")

    def test_trailing_newlines_match_bash_stripping(self):
        # Bash command substitution strips every trailing LF; time.format
        # emits no trailing newline, so extra LFs in the mock change nothing.
        self.compare_matching(date_stdout=fixed_summary() + "\n\n\n")

    def test_literal_summary_still_notifies(self):
        self.compare_matching(
            date_stdout='$(touch unsafe); * "quote" \\path\nsecond line\n\n\n',
            args=("", "--help", "$(touch unsafe)", "a b", "a\nb", "*"))
        self.assertFalse((self.root / "unsafe").exists())

    def test_empty_date_diverges_from_fixed_clock(self):
        # Upstream notifies with the empty stripped date; the rewrite keeps
        # the fixed-clock summary. The date boundary is gone by design.
        summary = fixed_summary()
        (self.root / "config.json").write_text(json.dumps({
            "date": {"stdout": "\n\n", "stderr": "", "status": 0},
            "notification": {"stdout": "recorded\n", "stderr": "", "status": 0},
        }))
        original = self.invoke(["/bin/bash", str(FIXTURE / "upstream.sh")])
        rewrite = self.invoke(self.rewrite_command(()))
        self.assertEqual(original["notifications"],
                         [["-g", NOTIFICATION_ICON, "-u", "low", ""]])
        self.assertEqual(rewrite["notifications"],
                         [["-g", NOTIFICATION_ICON, "-u", "low", summary]])
        self.assertEqual(rewrite["commands"], [
            {"command": "omarchy-notification-send",
             "argv": ["-g", NOTIFICATION_ICON, "-u", "low", summary]},
        ])

    def test_failing_date_is_ignored_by_the_rewrite(self):
        # Upstream still notifies with the partial date output; the rewrite
        # never spawns date, so its notification is unaffected.
        summary = fixed_summary()
        (self.root / "config.json").write_text(json.dumps({
            "date": {"stdout": "partial time\n\n", "stderr": "date fixture failure\n",
                     "status": 7},
            "notification": {"stdout": "", "stderr": "", "status": 0},
        }))
        original = self.invoke(["/bin/bash", str(FIXTURE / "upstream.sh")])
        rewrite = self.invoke(self.rewrite_command(()))
        self.assertEqual(original["notifications"],
                         [["-g", NOTIFICATION_ICON, "-u", "low", "partial time"]])
        self.assertEqual(rewrite["notifications"],
                         [["-g", NOTIFICATION_ICON, "-u", "low", summary]])
        self.assertNotIn("date",
                         [entry["command"] for entry in rewrite["commands"]])

    def test_notification_failure(self):
        self.compare_matching(notification_status=23,
                              notification_stderr="notification fixture failure\n")

    def test_ignored_literal_arguments(self):
        self.compare_matching(args=("", "--help", "$(touch unsafe)", "a b", "a\nb", "*"))
        self.assertFalse((self.root / "unsafe").exists())

    def test_now_flag_pins_the_clock_without_env(self):
        # `execute run --now EPOCH` pins the clock even when EXECUTE_NOW is
        # absent, so one-shot runs are deterministic without ambient state.
        summary = fixed_summary()
        (self.root / "config.json").write_text(json.dumps({
            "date": {"stdout": summary + "\n", "stderr": "", "status": 0},
            "notification": {"stdout": "recorded\n", "stderr": "", "status": 0},
        }))
        env = dict(self.env)
        del env["EXECUTE_NOW"]
        command = [str(EXECUTE), "run", str(PROGRAM), "--now", str(FIXED_NOW),
                   "--manifest-dir", str(PROJECT / "manifests")]
        # Run without the ambient variable: drop it from the child env.
        for name in ("commands.jsonl", "notifications.jsonl"):
            (self.root / name).unlink(missing_ok=True)
        proc = subprocess.run(command, cwd=self.root, env=env,
                              stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                              timeout=10, check=False)
        self.assertEqual(proc.returncode, 0, proc.stderr.decode())
        notifications = [json.loads(line) for line in
                         (self.root / "notifications.jsonl").read_text().splitlines()]
        self.assertEqual(notifications,
                         [["-g", NOTIFICATION_ICON, "-u", "low", summary]])


if __name__ == "__main__":
    unittest.main()
