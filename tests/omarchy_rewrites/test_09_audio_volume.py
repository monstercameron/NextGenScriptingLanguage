"""Pinned Bash versus Execute, with isolated audio commands and runtime files."""
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
FIXTURE = PROJECT / "tests/fixtures/omarchy-rewrites/09-audio-output-volume"
PROGRAM = PROJECT / "examples/omarchy-rewrites/09-audio-output-volume.program"
DEFAULT_EXECUTE = PROJECT / "target/release/execute"
EXECUTE = Path(os.environ.get("EXECUTE_TEST_BINARY", DEFAULT_EXECUTE))
SOURCE_SHA256 = "08611ec849c020625b784b360a84aed0bc43b93ba82c7f02616bf5e1917d8c80"
DEBOUNCE = "omarchy-audio-output-volume-mute-toggle.last"


class AudioVolumeTests(unittest.TestCase):
    def setUp(self):
        self.assertTrue(EXECUTE.is_file(), f"Set EXECUTE_TEST_BINARY: {EXECUTE}")
        self.temp = tempfile.TemporaryDirectory(prefix="omarchy-volume-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.bin = self.root / "bin"
        self.bin.mkdir()
        self.home = self.root / "owned home"
        self.home.mkdir()
        self.runtime = self.root / "runtime with spaces"
        self.runtime.mkdir()
        self.env = {"PATH": str(self.bin), "HOME": str(self.home),
                    "XDG_RUNTIME_DIR": str(self.runtime), "FIXTURE_ROOT": str(self.root),
                    "LC_ALL": "C", "PYTHONDONTWRITEBYTECODE": "1"}
        for name in ("jq", "awk", "test", "printf", "false"):
            target = shutil.which(name)
            self.assertIsNotNone(target, f"Required native dependency: {name}")
            (self.bin / name).symlink_to(target)
        mock = "#!" + sys.executable + "\n" + (FIXTURE / "mock_command.py").read_text().split("\n", 1)[1]
        for name in ("pactl", "omarchy-audio-output-sink", "omarchy-osd", "date"):
            path = self.bin / name
            path.write_text(mock)
            path.chmod(0o755)

    def invoke(self, command, initial, debounce_directory=False):
        shutil.rmtree(self.runtime)
        self.runtime.mkdir()
        for name, contents in initial.items():
            (self.runtime / name).write_text(contents)
        if debounce_directory:
            (self.runtime / DEBOUNCE).mkdir()
        (self.root / "commands.jsonl").unlink(missing_ok=True)
        proc = subprocess.run(command, cwd=self.root, env=self.env, stdin=subprocess.DEVNULL,
                              stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=15, check=False)
        log = self.root / "commands.jsonl"
        commands = [json.loads(line) for line in log.read_text().splitlines()] if log.exists() else []
        is_lock = lambda name: Path(name).name.endswith(".execute-applet.lock")
        for entry in commands:
            entry["runtime_files"] = {name: contents for name, contents in entry["runtime_files"].items()
                                      if not is_lock(name)}
        files = {str(p.relative_to(self.runtime)): p.read_text()
                 for p in sorted(self.runtime.rglob("*")) if p.is_file() and not is_lock(str(p.relative_to(self.runtime)))}
        locks = {str(p.relative_to(self.runtime)): p.read_text()
                 for p in sorted(self.runtime.rglob("*")) if p.is_file() and is_lock(str(p.relative_to(self.runtime)))}
        return {"status": proc.returncode, "stdout": proc.stdout.decode(), "stderr": proc.stderr.decode(),
                "commands": commands, "files": files, "locks": locks}

    def compare(self, args=("raise",), *, current="50", after="55", muted="no", initial=None,
                overrides=None, diagnostic=False, stdout_gap=False, stderr_gap=False, full_gap=False,
                debounce_directory=False):
        config = {"sink": {"stdout": "physical sink; $(touch unsafe)\n"},
                  "date": {"stdout": "1000000\n"},
                  "get-sink-volume": [{"stdout": f"Volume: front-left: 32768 / {current}% / -18.06 dB\n"},
                                      {"stdout": f"Volume: front-left: 32768 / {after}% / -18.06 dB\n"}],
                  "get-sink-mute": {"stdout": f"Mute: {muted}\n"},
                  "set-sink-mute": {}, "set-sink-volume": {}, "osd": {"stdout": "osd shown\n"}}
        config.update(overrides or {})
        (self.root / "config.json").write_text(json.dumps(config))
        initial = initial or {}
        original = self.invoke(["/bin/bash", str(FIXTURE / "upstream.sh"), *args], initial, debounce_directory)
        rewrite = self.invoke([str(EXECUTE), str(PROGRAM), "--", *args], initial, debounce_directory)
        if not full_gap:
            for key in ("status", "commands", "files"):
                self.assertEqual(original[key], rewrite[key], (key, rewrite["stderr"]))
            if stdout_gap:
                self.assertNotEqual(original["stdout"], rewrite["stdout"])
                self.assertEqual(rewrite["stdout"], "")
            else:
                self.assertEqual(original["stdout"], rewrite["stdout"], "stdout")
            if diagnostic:
                self.assertTrue(rewrite["stderr"].startswith(original["stderr"]))
                self.assertIn("failed", rewrite["stderr"])
            elif stderr_gap:
                self.assertNotEqual(original["stderr"], rewrite["stderr"])
            else:
                self.assertEqual(original["stderr"], rewrite["stderr"], "stderr")
        evidence = os.environ.get("OMARCHY_REWRITE_EVIDENCE_DIR")
        if evidence:
            destination = Path(evidence)
            destination.mkdir(parents=True, exist_ok=True)
            label = self._testMethodName + "-" + hashlib.sha256(json.dumps([args, config, initial], sort_keys=True).encode()).hexdigest()[:10]
            (destination / (label + ".json")).write_text(json.dumps(
                {"case": self._testMethodName, "config": config, "arguments": args, "initial": initial,
                 "gaps": {"diagnostic": diagnostic, "stdout": stdout_gap, "stderr": stderr_gap, "full": full_gap},
                 "upstream": original, "execute": rewrite}, ensure_ascii=False, indent=2) + "\n")
        self.assertFalse((self.root / "unsafe").exists())
        return original, rewrite

    def test_pinned_source(self):
        self.assertEqual(hashlib.sha256((FIXTURE / "upstream.sh").read_bytes()).hexdigest(), SOURCE_SHA256)

    def test_raise_default_step(self):
        original, _ = self.compare()
        self.assertEqual([entry["argv"] for entry in original["commands"]], [
            [], ["get-sink-volume", "physical sink; $(touch unsafe)"],
            ["set-sink-mute", "physical sink; $(touch unsafe)", "0"],
            ["set-sink-volume", "physical sink; $(touch unsafe)", "55%"],
            ["get-sink-volume", "physical sink; $(touch unsafe)"],
            ["get-sink-mute", "physical sink; $(touch unsafe)"],
            ["-i", "volume-high", "-p", "55"]])

    def test_lower_default_step(self):
        original, _ = self.compare(("lower",), after="45")
        self.assertEqual(original["commands"][3]["argv"][-1], "45%")

    def test_custom_step_and_extra_args(self):
        original, _ = self.compare(("+17", "ignored", "$(touch unsafe)"), after="67")
        self.assertEqual(original["commands"][3]["argv"][-1], "67%")

    def test_upper_clamp(self):
        original, _ = self.compare(("+20",), current="95", after="100")
        self.assertEqual(original["commands"][3]["argv"][-1], "100%")

    def test_lower_clamp_and_muted_icon(self):
        original, _ = self.compare(("-20",), current="5", after="0")
        self.assertEqual(original["commands"][3]["argv"][-1], "0%")
        self.assertEqual(original["commands"][-1]["argv"], ["-i", "volume-muted", "-p", "0"])

    def test_clamp_is_directional(self):
        for action, current, expected in (("+5", "-20", "-15"), ("-5", "150", "145")):
            with self.subTest(action=action):
                original, _ = self.compare((action,), current=current, after=expected)
                self.assertEqual(original["commands"][3]["argv"][-1], expected + "%")

    def test_zero_step(self):
        self.compare(("+0",), after="50")

    def test_muted_suffix(self):
        original, _ = self.compare(muted="anything yes")
        self.assertEqual(original["commands"][-1]["argv"][1], "volume-muted")

    def test_volume_parser_first_percent_on_first_line(self):
        original, _ = self.compare(overrides={"get-sink-volume": [
            {"stdout": "Volume: one 12% two 90%\nextra 99%\n"}, {"stdout": "Volume: 17%\n"}]})
        self.assertEqual(original["commands"][3]["argv"][-1], "17%")

    def test_unreadable_current_stops_before_effects(self):
        original, _ = self.compare(overrides={"get-sink-volume": {"stdout": "no percentages\n"}}, diagnostic=True)
        self.assertEqual(original["status"], 1)
        self.assertEqual(len(original["commands"]), 2)

    def test_percent_on_second_line_is_unreadable(self):
        self.compare(overrides={"get-sink-volume": {"stdout": "line one\nVolume: 50%\n"}}, diagnostic=True)

    def test_pactl_query_failure_with_valid_output_continues(self):
        self.compare(overrides={"get-sink-volume": {"status": 7, "stdout": "Volume: 50%\n"}})

    def test_sink_failure_with_nonempty_output_continues(self):
        self.compare(overrides={"sink": {"status": 7, "stdout": "fallback sink\n\n"}})

    def test_sink_failure_empty_stops(self):
        original, _ = self.compare(overrides={"sink": {"status": 7}}, diagnostic=True)
        self.assertEqual(len(original["commands"]), 1)

    def test_no_argument_failure_stdout_gap(self):
        original, _ = self.compare((), diagnostic=True, stdout_gap=True)
        self.assertEqual(original["commands"], [])

    def test_explicit_empty_argument_failure_stdout_gap(self):
        self.compare(("", "raise"), diagnostic=True, stdout_gap=True)

    def test_unknown_action_failure_stdout_gap(self):
        for action in ("+", "--5", "5", "+1.5", "Raise", "$(touch unsafe)", "-2\n"):
            with self.subTest(action=action):
                self.compare((action,), diagnostic=True, stdout_gap=True)

    def test_mute_toggle_writes_timestamp_before_effect(self):
        original, rewrite = self.compare(("mute-toggle",), current="50")
        self.assertEqual(original["files"], {DEBOUNCE: "1000000\n"})
        self.assertEqual(original["commands"][2]["runtime_files"], original["files"])
        self.assertEqual(original["commands"][2]["argv"][-1], "toggle")
        self.assertEqual(rewrite["locks"], {"." + DEBOUNCE + ".execute-applet.lock": ""})

    def test_debounce_write_failure_is_reported_but_does_not_block_mute(self):
        original, rewrite = self.compare(("mute-toggle",), debounce_directory=True,
                                         full_gap=True)
        self.assertEqual(original["status"], 0)
        self.assertEqual(rewrite["status"], 0)
        self.assertIn(["set-sink-mute", "physical sink; $(touch unsafe)", "toggle"],
                      [entry["argv"] for entry in rewrite["commands"]])
        self.assertTrue(any("debounce" in entry.lower() or "write" in entry.lower()
                            for entry in rewrite["stderr"].splitlines()), rewrite["stderr"])

    def test_debounce_below_and_at_250ms(self):
        for elapsed in (0, 1, 249, 250, 251, -1):
            with self.subTest(elapsed=elapsed):
                old = str(1000000 - elapsed) + "\n"
                original, _ = self.compare(("mute-toggle",), initial={DEBOUNCE: old})
                self.assertEqual(len(original["commands"]), 2 if elapsed < 250 else 6)
                self.assertEqual(original["files"][DEBOUNCE], old if elapsed < 250 else "1000000\n")

    def test_debounce_read_trims_ifs_and_ignores_later_lines(self):
        self.compare(("mute-toggle",), initial={DEBOUNCE: " \t999900\t \n1\n"})

    def test_debounce_empty_file_defaults_zero(self):
        self.compare(("mute-toggle",), initial={DEBOUNCE: ""})

    def test_set_failures_still_reach_osd_and_preserve_output(self):
        original, _ = self.compare(overrides={
            "set-sink-mute": {"status": 4, "stdout": "unmute partial\n", "stderr": "unmute error\n"},
            "set-sink-volume": {"status": 8, "stdout": "volume partial\n", "stderr": "volume error\n"}})
        self.assertEqual(original["stdout"], "unmute partial\nvolume partial\nosd shown\n")

    def test_toggle_failure_still_reaches_osd(self):
        self.compare(("mute-toggle",), overrides={"set-sink-mute": {"status": 8, "stderr": "toggle error\n"}})

    def test_missing_after_percent_displays_zero(self):
        original, _ = self.compare(overrides={"get-sink-volume": [
            {"stdout": "Volume: 50%\n"}, {"stdout": "unreadable\n"}]})
        self.assertEqual(original["commands"][-1]["argv"], ["-i", "volume-muted", "-p", "0"])

    def test_osd_failure_preserves_status_with_output_gap(self):
        original, _ = self.compare(overrides={"osd": {"status": 13, "stdout": "partial\n", "stderr": "osd error\n"}}, diagnostic=True, stdout_gap=True)
        self.assertEqual(original["status"], 13)

    def test_pactl_query_stderr_redirection_matches_bash(self):
        self.compare(overrides={
            "get-sink-volume": [
                {"stdout": "Volume: front-left: 32768 / 50% / -18.06 dB\n", "stderr": "volume warning\n"},
                {"stdout": "Volume: front-left: 32768 / 55% / -18.06 dB\n", "stderr": "volume warning\n"},
            ],
            "get-sink-mute": {"status": 7, "stderr": "mute warning\n"},
        })

    def test_octal_step_matches_bash_literal(self):
        original, rewrite = self.compare(("+010",), after="58")
        self.assertEqual(original["commands"][3]["argv"][-1], "58%")
        self.assertEqual(rewrite["commands"][3]["argv"][-1], "58%")

    def test_invalid_octal_step_failure_gap(self):
        original, rewrite = self.compare(("+08",), full_gap=True)
        self.assertEqual(len(original["commands"]), 5)
        self.assertEqual(len(rewrite["commands"]), 2)
        self.assertNotEqual(original["status"], rewrite["status"])
        self.assertIn("volume step parse", rewrite["stderr"])

    def test_invalid_current_identifier_arithmetic_gap(self):
        original, rewrite = self.compare(current="garbage", full_gap=True)
        self.assertEqual(original["commands"][3]["argv"][-1], "5%")
        self.assertNotEqual(original["status"], rewrite["status"])
        self.assertEqual(len(rewrite["commands"]), 2)
        self.assertIn("current volume parse", rewrite["stderr"])

    def test_invalid_debounce_identifier_arithmetic_gap(self):
        original, rewrite = self.compare(("mute-toggle",), initial={DEBOUNCE: "garbage\n"}, full_gap=True)
        self.assertEqual(original["files"][DEBOUNCE], "1000000\n")
        self.assertEqual(rewrite["files"][DEBOUNCE], "garbage\n")
        self.assertNotEqual(original["status"], rewrite["status"])


if __name__ == "__main__":
    unittest.main()
