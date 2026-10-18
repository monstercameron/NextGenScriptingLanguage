"""Isolated differential browser tests: no host XDG command can be reached."""
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
FIXTURE = PROJECT / "tests/fixtures/omarchy-rewrites/06-default-browser"
PROGRAM = PROJECT / "examples/omarchy-rewrites/06-default-browser.program"
DEFAULT_EXECUTE = PROJECT / "target/release/execute"
EXECUTE = Path(os.environ.get("EXECUTE_TEST_BINARY", DEFAULT_EXECUTE))
SOURCE_SHA256 = "4c8254a3fdf4948013901caec2ed847e2eb580b045a4b1322271ab2d94c1bc1f"
BROWSERS = (
    ("chromium", "chromium.desktop", "Chromium", ""),
    ("chrome", "google-chrome.desktop", "Chrome", "󰊯"),
    ("brave", "brave-browser.desktop", "Brave", "󰖟"),
    ("brave-origin", "brave-origin.desktop", "Brave Origin", "󰖟"),
    ("edge", "microsoft-edge.desktop", "Edge", "󰇩"),
    ("firefox", "firefox.desktop", "Firefox", "󰈹"),
    ("zen", "zen.desktop", "Zen", "󰖟"),
)
USAGE = "Usage: omarchy-default-browser <chromium|chrome|brave|brave-origin|edge|firefox|zen>\n"


class DefaultBrowserTests(unittest.TestCase):
    def setUp(self):
        self.assertTrue(EXECUTE.is_file(), f"Set EXECUTE_TEST_BINARY: {EXECUTE}")
        self.temp = tempfile.TemporaryDirectory(prefix="omarchy-browser-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        bindir = self.root / "bin"
        bindir.mkdir()
        (self.root / "home").mkdir()
        self.env = {"PATH": str(bindir), "HOME": str(self.root / "home"),
                    "BROWSER": "sentinel $(touch unsafe); browser", "FIXTURE_ROOT": str(self.root),
                    "LC_ALL": "C", "TZ": "UTC"}
        for name in ("env", "jq", "printf", "false"):
            tool = shutil.which(name)
            self.assertIsNotNone(tool, name)
            (bindir / name).symlink_to(tool)
        mock = (FIXTURE / "mock_command.py").read_text()
        mock = "#!" + sys.executable + "\n" + mock.split("\n", 1)[1]
        for name in ("xdg-settings", "omarchy-notification-send"):
            path = bindir / name
            path.write_text(mock)
            path.chmod(0o755)

    def invoke(self, command):
        for name in ("commands.jsonl", "notifications.jsonl", "get-count", "browser-state"):
            (self.root / name).unlink(missing_ok=True)
        proc = subprocess.run(command, cwd=self.root, env=self.env, capture_output=True,
                              timeout=10, check=False)
        def records(name):
            path = self.root / name
            return [json.loads(line) for line in path.read_text().splitlines()] if path.exists() else []
        state = self.root / "browser-state"
        return {"status": proc.returncode, "stdout": proc.stdout.decode(),
                "stderr": proc.stderr.decode(), "commands": records("commands.jsonl"),
                "notifications": records("notifications.jsonl"),
                "browser_state": state.read_text() if state.exists() else None,
                "home_files": sorted(str(p.relative_to(self.root / "home"))
                                     for p in (self.root / "home").rglob("*")),
                "unsafe_exists": (self.root / "unsafe").exists()}

    def compare(self, args=(), get=None, set_response=None, notification=None,
                stdout_difference=False, failure_diagnostic=False):
        config = {"get": get or [{"stdout": "firefox.desktop\n"}],
                  "set": set_response or {}, "notification": notification or {}}
        (self.root / "config.json").write_text(json.dumps(config))
        original = self.invoke(["/bin/bash", str(FIXTURE / "upstream.sh"), *args])
        rewrite = self.invoke([str(EXECUTE), str(PROGRAM), "--", *args])
        for field in ("status", "commands", "notifications", "browser_state", "home_files", "unsafe_exists"):
            self.assertEqual(original[field], rewrite[field], field)
        if stdout_difference:
            self.assertEqual(rewrite["stdout"], "")
            self.assertNotEqual(original["stdout"], rewrite["stdout"])
        else:
            self.assertEqual(original["stdout"], rewrite["stdout"])
        if failure_diagnostic:
            self.assertTrue(rewrite["stderr"].startswith(original["stderr"]))
            self.assertIn("failed", rewrite["stderr"])
        else:
            self.assertEqual(original["stderr"], rewrite["stderr"])
        self.assertFalse(rewrite["unsafe_exists"])
        evidence = os.environ.get("OMARCHY_REWRITE_EVIDENCE_DIR")
        if evidence:
            destination = Path(evidence)
            destination.mkdir(parents=True, exist_ok=True)
            suffix = hashlib.sha256(json.dumps([args, config]).encode()).hexdigest()[:12]
            (destination / (self._testMethodName + "-" + suffix + ".json")).write_text(json.dumps(
                {"case": self._testMethodName, "arguments": args, "config": config,
                 "upstream": original, "execute": rewrite}, ensure_ascii=False, indent=2) + "\n")
        return original, rewrite

    def test_untouched_source(self):
        self.assertEqual(hashlib.sha256((FIXTURE / "upstream.sh").read_bytes()).hexdigest(), SOURCE_SHA256)

    def test_all_known_queries(self):
        for selection, desktop, _, _ in BROWSERS:
            with self.subTest(browser=selection):
                original, _ = self.compare(get=[{"stdout": desktop + "\n"}])
                self.assertEqual(original["stdout"], selection + "\n")
                self.assertEqual(original["commands"], [{"command": "xdg-settings",
                    "argv": ["get", "default-web-browser"], "browser_present": False}])

    def test_known_query_trailing_newlines_and_failed_first_probe(self):
        self.compare(get=[{"stdout": "firefox.desktop\n\n\n", "status": 9, "stderr": "probe failed\n"}])
        self.compare(get=[{"stdout": "zen.desktop"}])

    def test_unknown_query_repeats_probe_and_returns_second_stdout(self):
        original, _ = self.compare(get=[{"stdout": "custom.desktop\n"}, {"stdout": "second.desktop\n"}])
        self.assertEqual(len(original["commands"]), 2)
        self.assertEqual(original["stdout"], "second.desktop\n")

    def test_unknown_query_failure_returns_zero_and_literal_bytes(self):
        literal = '$(touch unsafe); * "quoted" \\path\nsecond line\n\n'
        original, _ = self.compare(get=[{"stdout": literal, "status": 4, "stderr": "first\n"},
                                        {"stdout": literal, "status": 7, "stderr": "second\n"}])
        self.assertEqual(original["status"], 0)
        self.assertEqual(original["stdout"], literal)

    def test_all_known_selections(self):
        for selection, desktop, name, glyph in BROWSERS:
            with self.subTest(browser=selection):
                original, _ = self.compare(args=(selection,))
                self.assertEqual(original["commands"], [
                    {"command": "xdg-settings", "argv": ["set", "default-web-browser", desktop], "browser_present": False},
                    {"command": "omarchy-notification-send", "argv": ["-g", glyph, name + " is now the default browser"], "browser_present": True}])
                self.assertEqual(original["browser_state"], desktop)

    def test_extra_literal_metachar_arguments_are_ignored(self):
        self.compare(args=("firefox", "$(touch unsafe)", "*", "a b", "a\nb", "", "--help"))

    def test_invalid_selection_and_empty_positional_known_stdout_limit(self):
        for selection in ("invalid", "", "$(touch unsafe); *", "--help", "firefox\n"):
            with self.subTest(selection=selection):
                original, _ = self.compare(args=(selection,), failure_diagnostic=True)
                self.assertEqual(original["stdout"], USAGE)
                self.assertEqual(original["status"], 1)
                self.assertEqual(original["commands"], [])

    def test_set_failure_maps_to_one_and_does_not_notify(self):
        original, _ = self.compare(args=("chrome",), set_response={"status": 19, "stderr": "set rejected\n"},
                                   failure_diagnostic=True)
        self.assertEqual(original["status"], 1)
        self.assertEqual(original["notifications"], [])
        self.assertIsNone(original["browser_state"])

    def test_set_failure_stdout_known_finite_output_limit(self):
        self.compare(args=("zen",), set_response={"status": 19, "stdout": "partial set\n"},
                     stdout_difference=True, failure_diagnostic=True)

    def test_successful_set_and_notification_stdout_are_concatenated(self):
        original, _ = self.compare(args=("brave-origin",), set_response={"stdout": "set output\n"},
                                   notification={"stdout": "notification output\n"})
        self.assertEqual(original["stdout"], "set output\nnotification output\n")

    def test_notification_failure_preserves_native_status_and_side_effect(self):
        original, _ = self.compare(args=("edge",), notification={"status": 23, "stderr": "notify failed\n"},
                                   failure_diagnostic=True)
        self.assertEqual(original["status"], 23)
        self.assertEqual(original["browser_state"], "microsoft-edge.desktop")

    def test_notification_failure_stdout_known_finite_output_limit(self):
        self.compare(args=("firefox",), set_response={"stdout": "set output\n"},
                     notification={"status": 23, "stdout": "partial notification\n"},
                     stdout_difference=True, failure_diagnostic=True)

    def test_typed_selection_override(self):
        (self.root / "config.json").write_text(json.dumps({"get": [{}], "set": {}, "notification": {}}))
        result = self.invoke([str(EXECUTE), str(PROGRAM), "--input", "selection=zen",
                              "--input", 'supplied_arguments=["zen"]'])
        self.assertEqual(result["status"], 0, result["stderr"])
        self.assertEqual(result["browser_state"], "zen.desktop")


if __name__ == "__main__":
    unittest.main()
