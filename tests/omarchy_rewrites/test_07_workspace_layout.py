"""Safe differential tests: real jq/coreutils, owned HOME and desktop mocks."""
import hashlib
import json
import os
from pathlib import Path
import shutil
import stat
import subprocess
import sys
import tempfile
import unittest

PROJECT = Path(__file__).resolve().parents[2]
FIXTURE = PROJECT / "tests/fixtures/omarchy-rewrites/07-workspace-layout-toggle"
PROGRAM = PROJECT / "examples/omarchy-rewrites/07-workspace-layout-toggle.program"
DEFAULT_EXECUTE = PROJECT / "target/release/execute"
EXECUTE = Path(os.environ.get("EXECUTE_TEST_BINARY", DEFAULT_EXECUTE))
SOURCE_SHA256 = "0e35529ed0391c2a990f5cca02a4a08a6edf471ba65aa1b65c53c3eb1f28244c"


class WorkspaceLayoutTests(unittest.TestCase):
    def setUp(self):
        self.assertTrue(EXECUTE.is_file(), f"Set EXECUTE_TEST_BINARY: {EXECUTE}")
        self.temp = tempfile.TemporaryDirectory(prefix="omarchy-workspace-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.bin = self.root / "bin"
        self.bin.mkdir()
        self.home = self.root / "owned home with spaces"
        self.home.mkdir()
        self.env = {"PATH": str(self.bin), "HOME": str(self.home),
                    "FIXTURE_ROOT": str(self.root), "LC_ALL": "C",
                    "PYTHONDONTWRITEBYTECODE": "1"}
        for name in ("awk", "jq", "mkdir", "printf", "false"):
            target = shutil.which(name)
            self.assertIsNotNone(target, f"Required native dependency: {name}")
            (self.bin / name).symlink_to(target)
        mock = (FIXTURE / "mock_command.py").read_text()
        mock = "#!" + sys.executable + "\n" + mock.split("\n", 1)[1]
        for name in ("hyprctl", "omarchy-notification-send"):
            path = self.bin / name
            path.write_text(mock)
            path.chmod(0o755)

    def invoke(self, command, layout_symlink=False):
        shutil.rmtree(self.home)
        self.home.mkdir()
        if layout_symlink:
            layouts = self.home / ".local/state/omarchy/workspace-layouts"
            layouts.mkdir(parents=True)
            target = self.home / "layout target.lua"
            target.write_text("old layout\n")
            (layouts / "2.lua").symlink_to(target)
        (self.root / "commands.jsonl").unlink(missing_ok=True)
        proc = subprocess.run(command, cwd=self.root, env=self.env,
                              stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                              timeout=10, check=False)
        log = self.root / "commands.jsonl"
        return {"status": proc.returncode, "stdout": proc.stdout.decode(),
                "stderr": proc.stderr.decode(),
                "commands": [json.loads(line) for line in log.read_text().splitlines()] if log.exists() else [],
                "files": {str(p.relative_to(self.home)): p.read_bytes().decode("utf-8")
                          for p in sorted(self.home.rglob("*")) if p.is_file()},
                "modes": {str(p.relative_to(self.home)): stat.S_IMODE(p.stat().st_mode)
                          for p in sorted(self.home.rglob("*"))
                          if p.is_file() and not p.is_symlink()},
                "symlinks": {str(p.relative_to(self.home)): os.readlink(p)
                             for p in sorted(self.home.rglob("*")) if p.is_symlink()},
                "directories": [str(p.relative_to(self.home))
                                for p in sorted(self.home.rglob("*")) if p.is_dir()]}

    def compare(self, workspace_id=2, layout="dwindle", *, first=None, second=None,
                eval_response=None, keyword_response=None, notification=None,
                args=(), diagnostic=False, workspace_responses=None,
                layout_symlink=False):
        config = {
            "workspaces": [
                {"stdout": json.dumps({"id": workspace_id, "tiledLayout": "ignored"} if first is None else first) + "\n"},
                {"stdout": json.dumps({"id": 999, "tiledLayout": layout} if second is None else second) + "\n"},
            ],
            "eval": {"stdout": "hidden eval stdout\n"} if eval_response is None else eval_response,
            "keyword": {"stdout": "keyword applied\n"} if keyword_response is None else keyword_response,
            "notification": {"stdout": "notification sent\n"} if notification is None else notification,
        }
        if workspace_responses is not None:
            config["workspaces"] = workspace_responses
        (self.root / "config.json").write_text(json.dumps(config))
        original = self.invoke(["/bin/bash", str(FIXTURE / "upstream.sh"), *args], layout_symlink)
        rewrite = self.invoke([str(EXECUTE), str(PROGRAM), "--", *args], layout_symlink)
        for key in ("status", "stdout", "symlinks", "directories"):
            self.assertEqual(original[key], rewrite[key], key)
        if diagnostic:
            self.assertTrue(rewrite["stderr"].startswith(original["stderr"]))
            self.assertIn("failed", rewrite["stderr"])
        else:
            self.assertEqual(original["stderr"], rewrite["stderr"])
        if original["files"]:
            new_layout = "scrolling" if layout == "dwindle" else "dwindle"
            # Both exact Lua bytes and command/file ordering are asserted.
            expected_file = f".local/state/omarchy/workspace-layouts/{workspace_id}.lua"
            expected_bytes = f'hl.workspace_rule({{ workspace = "{workspace_id}", layout = "{new_layout}" }})\n'
            expected_files = {expected_file: expected_bytes}
            lock_file = f".local/state/omarchy/workspace-layouts/.{workspace_id}.lua.execute-applet.lock"
            if layout_symlink:
                expected_files["layout target.lua"] = expected_bytes
                self.assertEqual(original["symlinks"], {expected_file: str(self.home / "layout target.lua")})
            self.assertEqual(original["files"], expected_files)
            # Atomic writes retain a private stable sibling lock file. Strip
            # only that asserted artifact when comparing observations made by
            # later mock calls; all application files remain byte-for-byte
            # compared with the pinned Bash oracle.
            self.assertIn(lock_file, rewrite["files"])
            self.assertEqual(rewrite["files"][lock_file], "")
            self.assertEqual(rewrite["modes"][lock_file], 0o600)
            execute_commands = json.loads(json.dumps(rewrite["commands"]))
            for command in execute_commands:
                command.get("home_files", {}).pop(lock_file, None)
            self.assertEqual(original["commands"], execute_commands, "commands")
            expected_rewrite_files = dict(expected_files)
            expected_rewrite_files[lock_file] = ""
            self.assertEqual(rewrite["files"], expected_rewrite_files)
            initial_files = ({expected_file: "old layout\n", "layout target.lua": "old layout\n"}
                             if layout_symlink else {})
            self.assertEqual(original["commands"][0]["home_files"], initial_files)
            self.assertEqual(original["commands"][1]["home_files"], initial_files)
            for command in original["commands"][2:]:
                self.assertEqual(command["home_files"], original["files"])
        else:
            self.assertEqual(original["files"], rewrite["files"], "files")
            self.assertEqual(original["commands"], rewrite["commands"], "commands")
        evidence = os.environ.get("OMARCHY_REWRITE_EVIDENCE_DIR")
        if evidence:
            destination = Path(evidence)
            destination.mkdir(parents=True, exist_ok=True)
            (destination / (self._testMethodName + ".json")).write_text(json.dumps(
                {"case": self._testMethodName, "config": config, "arguments": args,
                 "upstream": original, "execute": rewrite}, ensure_ascii=False, indent=2) + "\n")
        return original, rewrite

    def test_untouched_source(self):
        self.assertEqual(hashlib.sha256((FIXTURE / "upstream.sh").read_bytes()).hexdigest(), SOURCE_SHA256)

    def test_dwindle_to_scrolling(self):
        original, _ = self.compare()
        self.assertEqual([entry["argv"] for entry in original["commands"]], [
            ["activeworkspace", "-j"], ["activeworkspace", "-j"],
            ["eval", 'hl.workspace_rule({ workspace = "2", layout = "scrolling" })'],
            ["-g", "󱂬", "Workspace layout set to scrolling"],
        ])

    def test_scrolling_to_dwindle(self):
        self.compare(layout="scrolling")

    def test_other_layout_to_dwindle(self):
        self.compare(layout='other layout; $(touch unsafe)')
        self.assertFalse((self.root / "unsafe").exists())

    def test_missing_layout_is_other(self):
        self.compare(layout=None, second={"id": 3})

    def test_negative_id(self):
        self.compare(workspace_id=-42)

    def test_numeric_string_id_preserves_spelling(self):
        self.compare(workspace_id="-0042")

    def test_id_trailing_lf_matches_bash_substitution(self):
        self.compare(workspace_id="23", first={"id": "23\n\n"})

    def test_layout_trailing_lf_matches_bash_substitution(self):
        self.compare(second={"tiledLayout": "dwindle\n\n"})

    def test_eval_failure_keyword_fallback(self):
        original, _ = self.compare(eval_response={"status": 7, "stdout": "hidden output\n"})
        self.assertEqual(original["commands"][3]["argv"], ["keyword", "workspace", "2, layout:scrolling"])
        self.assertEqual(original["stdout"], "keyword applied\nnotification sent\n")

    def test_keyword_failure_still_notifies(self):
        self.compare(eval_response={"status": 7}, keyword_response={
            "status": 9, "stdout": "fallback partial\n", "stderr": "keyword error\n"})

    def test_notification_failure(self):
        self.compare(notification={"status": 23, "stderr": "notification error\n"}, diagnostic=True)

    def test_invalid_id_has_no_effects(self):
        original, _ = self.compare(workspace_id="../../outside", diagnostic=True)
        self.assertEqual(original["status"], 1)
        self.assertEqual(len(original["commands"]), 1)
        self.assertEqual(original["files"], {})
        self.assertEqual(original["directories"], [])

    def test_missing_id_has_no_effects(self):
        original, _ = self.compare(first={"tiledLayout": "dwindle"}, diagnostic=True)
        self.assertEqual(len(original["commands"]), 1)
        self.assertEqual(original["directories"], [])

    def test_id_with_spaces_rejected(self):
        self.compare(workspace_id=" 2 ", diagnostic=True)

    def test_nonnumeric_json_ids_rejected(self):
        for workspace_id in (None, True, [], {}, 2.5):
            with self.subTest(workspace_id=workspace_id):
                self.compare(workspace_id=workspace_id, diagnostic=True)

    def test_malformed_second_query_uses_explicit_upstream_default(self):
        self.compare(layout="", workspace_responses=[
            {"stdout": json.dumps({"id": 2}) + "\n"},
            {"stdout": "{ malformed JSON\n"},
        ])

    def test_atomic_state_update_follows_existing_symlink(self):
        original, rewrite = self.compare(layout_symlink=True)
        self.assertEqual(original["symlinks"], {".local/state/omarchy/workspace-layouts/2.lua": str(self.home / "layout target.lua")})
        self.assertEqual(rewrite["files"]["layout target.lua"],
                         'hl.workspace_rule({ workspace = "2", layout = "scrolling" })\n')

    def test_ignored_arguments_remain_literal(self):
        self.compare(args=("", "--help", "a b", "$(touch unsafe)", "*"))
        self.assertFalse((self.root / "unsafe").exists())

    def test_nonzero_workspace_queries_with_valid_data(self):
        self.compare(workspace_responses=[
            {"status": 7, "stdout": '{"id":2}\n', "stderr": "first query warning\n"},
            {"status": 8, "stdout": '{"tiledLayout":"dwindle"}\n', "stderr": "second query warning\n"},
        ])

    def test_empty_first_query_stops_before_effects(self):
        self.compare(workspace_responses=[{"stdout": "", "status": 7}], diagnostic=True)

    def test_empty_second_query_uses_dwindle(self):
        self.compare(layout="", workspace_responses=[
            {"stdout": '{"id":2}\n'}, {"stdout": "", "status": 7},
        ])

    def test_eval_stderr_redirection_matches_bash_on_success_and_fallback(self):
        for status in (0, 7):
            with self.subTest(status=status):
                original, rewrite = self.compare(eval_response={
                    "status": status,
                    "stdout": "hidden eval stdout\n",
                    "stderr": "eval warning\n",
                })
                self.assertEqual(original["stderr"], "")
                self.assertEqual(rewrite["stderr"], "")
                if status == 7:
                    self.assertEqual(
                        original["commands"][3]["argv"],
                        ["keyword", "workspace", "2, layout:scrolling"],
                    )
                else:
                    self.assertFalse(any(
                        command["argv"][:2] == ["keyword", "workspace"]
                        for command in original["commands"]
                    ))


if __name__ == "__main__":
    unittest.main()
