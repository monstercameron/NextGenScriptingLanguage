"""Owned-HOME parity checks for the pinned webapp removal rewrite."""
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
FIXTURE = ROOT / "tests/fixtures/omarchy-rewrites/08-webapp-remove"
PROGRAM = ROOT / "examples/omarchy-rewrites/08-webapp-remove.program"
DEFAULT_BINARY = ROOT / "target/release/execute"
BINARY = Path(os.environ.get("EXECUTE_TEST_BINARY", str(DEFAULT_BINARY))).resolve()


class WebappRemoveTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.bash = shutil.which("bash")
        cls.rm = shutil.which("rm")
        if not BINARY.is_file() or not cls.bash or not cls.rm or not shutil.which("jq"):
            raise RuntimeError("Existing Execute binary, Bash, rm and jq required")

    def test_source_hash(self):
        provenance = json.loads((FIXTURE / "provenance.json").read_text())
        self.assertEqual(hashlib.sha256((FIXTURE / "upstream.sh").read_bytes()).hexdigest(),
                         provenance["source_sha256"])

    def test_static_check(self):
        result = subprocess.run([str(BINARY), "check", str(PROGRAM), "--json"],
                                capture_output=True, timeout=15)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(json.loads(result.stdout)["ok"])

    def case(self, *, arguments=(), selection="", entries=None, notify="true",
             menu_status=0, notify_status=0, database_status=0, expected_status=0,
             expected_removed=(), expected_name=None, expected_menu=None,
             expected_launcher=None, source_stdout=b"", stdout_gap=False,
             missing_directory=False, notify_output=""):
        with tempfile.TemporaryDirectory(prefix="webapp-remove-") as temporary:
            owned = Path(temporary).resolve()
            self.assertTrue(owned.name.startswith("webapp-remove-"))
            home = owned / "home"
            bin_dir = owned / "bin"
            bin_dir.mkdir()
            for name in ("rm", "omarchy-menu-select", "omarchy-notification-send", "update-desktop-database"):
                path = bin_dir / name
                path.write_text(f"#!{sys.executable}\n" + (FIXTURE / "probe.py").read_text())
                path.chmod(0o755)
            desktop = home / ".local/share/applications"
            icon = home / ".local/share/icons/hicolor/256x256/apps"
            entries = entries if entries is not None else {"Alpha.desktop": "web", "nested/Space App.desktop": "handler", "Keep.desktop": "native"}
            log = owned / "argv.jsonl"
            env = {"HOME": str(home), "PATH": f"{bin_dir}:/usr/bin:/bin", "LC_ALL": "C",
                   "OMARCHY_REMOVE_NOTIFY": notify, "OMARCHY_TEST_ROOT": str(owned),
                   "OMARCHY_TEST_LOG": str(log), "OMARCHY_TEST_REAL_RM": self.rm,
                   "OMARCHY_TEST_SELECTION": selection, "OMARCHY_TEST_MENU_STATUS": str(menu_status),
                   "OMARCHY_TEST_NOTIFY_STATUS": str(notify_status), "OMARCHY_TEST_NOTIFY_OUTPUT": notify_output,
                   "OMARCHY_TEST_DATABASE_STATUS": str(database_status),
                   "PYTHONDONTWRITEBYTECODE": "1", "BASH_ENV": str(FIXTURE / "bash-env.sh")}

            def setup():
                if home.exists():
                    self.assertEqual(home.parent, owned)
                    shutil.rmtree(home)
                home.mkdir()
                if not missing_directory:
                    desktop.mkdir(parents=True)
                    for relative, kind in entries.items():
                        path = desktop / relative
                        self.assertTrue(path.resolve().is_relative_to(home))
                        path.parent.mkdir(parents=True, exist_ok=True)
                        command = {"web": "omarchy-launch-webapp", "handler": "omarchy-webapp-handler", "native": "native-app"}[kind]
                        path.write_text(f"[Desktop Entry]\nExec={command} https://example.test\n")
                    (desktop / "icons").mkdir(exist_ok=True)
                    icon.mkdir(parents=True)
                    for name in ("Alpha", "alpha", "Space App", "space-app", "space app", "Unknown", "unknown", "Weird $(touch PWNED) [*]", "weird-touch-pwned", "weird $(touch pwned) [*]"):
                        (icon / f"{name}.png").write_text("icon\n")
                        (desktop / "icons" / f"{name}.png").write_text("legacy\n")
                log.write_text("")

            def snapshot():
                return {str(p.relative_to(home)): ("dir" if p.is_dir() else p.read_bytes())
                        for p in sorted(home.rglob("*"))}

            outcomes = []
            initial = None
            for command in ([self.bash, str(FIXTURE / "upstream.sh"), *arguments],
                            [str(BINARY), str(PROGRAM), "--", *arguments]):
                setup()
                initial = snapshot()
                result = subprocess.run(command, cwd=owned, env=env, stdin=subprocess.DEVNULL,
                                        capture_output=True, timeout=20)
                calls = [json.loads(line) for line in log.read_text().splitlines()]
                outcomes.append((result, calls, snapshot()))
                self.assertFalse((owned / "PWNED").exists())
            (original, original_calls, original_tree), (candidate, calls, tree) = outcomes
            self.assertEqual(original.returncode, expected_status, original.stderr)
            self.assertEqual(candidate.returncode, expected_status, candidate.stderr)
            self.assertEqual(original.stdout, source_stdout)
            self.assertEqual(candidate.stdout, b"" if stdout_gap else original.stdout)
            if expected_status == 0:
                self.assertEqual(candidate.stderr, original.stderr)
            else:
                self.assertIn(b"exit status", candidate.stderr)
            self.assertEqual(calls, original_calls)
            self.assertEqual(tree, original_tree)
            removed = set(initial) - set(tree)
            self.assertEqual(removed, set(expected_removed) & set(initial))
            expected_calls = []
            if expected_menu is not None:
                expected_calls.append(["omarchy-menu-select", "Select web app to remove", *expected_menu,
                                       "--", "--width", "520", "--maxheight", "520"])
            if expected_name is not None:
                relative = expected_launcher
                if relative is None:
                    relative = next((k for k, v in entries.items()
                                     if v != "native" and Path(k).stem == expected_name), None)
                target = str(desktop / relative) if relative else f"{desktop}//{expected_name}.desktop"
                normalized = self.normalize(expected_name)
                expected_calls += [["rm", "-f", target], ["rm", "-f", str(icon / f"{normalized}.png"),
                                   str(icon / f"{expected_name}.png"), str(desktop / "icons" / f"{expected_name}.png")]]
                if notify != "false":
                    expected_calls.append(["omarchy-notification-send", "-g", "", "Web app removed", expected_name])
                if not notify_status:
                    expected_calls.append(["update-desktop-database", str(desktop) + "/"])
            self.assertEqual(calls, expected_calls)
            expected_tree = {key: value for key, value in initial.items()
                             if key not in set(expected_removed)}
            if expected_name is not None and not notify_status:
                expected_tree["database-refreshed"] = b"called\n"
            self.assertEqual(tree, expected_tree)

    @staticmethod
    def normalize(name):
        result = subprocess.run(["sed", r"s/[^[:alnum:]]\+/-/g; s/^-//; s/-$//"],
                                input=(name.lower() + "\n").encode(), capture_output=True, check=True)
        return result.stdout.decode().rstrip("\n")

    @classmethod
    def removals(cls, name, normalized, launcher=None):
        normalized = cls.normalize(name)
        result = {f".local/share/icons/hicolor/256x256/apps/{name}.png",
                  f".local/share/icons/hicolor/256x256/apps/{normalized}.png",
                  f".local/share/applications/icons/{name}.png"}
        if launcher:
            result.add(f".local/share/applications/{launcher}")
        return result

    def test_multiple_arguments_and_nested_launcher(self):
        self.case(arguments=("Space", "App"), expected_name="Space App",
                  expected_removed=self.removals("Space App", "space-app", "nested/Space App.desktop"))

    def test_suffix_removal_before_basename_keeps_indexed_nested_launcher(self):
        # Bash removes the final .desktop suffix from the full path before
        # taking its basename. For nested/.desktop that yields the display
        # name "nested". `basename path .desktop` yields ".desktop" instead,
        # which used to miss the indexed web launcher and remove the unrelated
        # same-named native launcher at the root.
        self.case(arguments=("nested",),
                  entries={"nested/.desktop": "web", "nested.desktop": "native"},
                  expected_name="nested", expected_launcher="nested/.desktop",
                  expected_removed=self.removals("nested", "nested", "nested/.desktop"))

    def test_menu_suffix_removal_before_basename_keeps_nested_identity(self):
        self.case(selection="nested\n",
                  entries={"nested/.desktop": "web", "nested.desktop": "native"},
                  expected_name="nested", expected_menu=["nested"],
                  expected_launcher="nested/.desktop",
                  expected_removed=self.removals("nested", "nested", "nested/.desktop"))

    def test_menu_sorted_multiple_apps(self):
        self.case(selection="Alpha\n", expected_name="Alpha", expected_menu=["Alpha", "Space App"],
                  expected_removed=self.removals("Alpha", "alpha", "Alpha.desktop"))

    def test_metacharacters_literal(self):
        name = "Weird $(touch PWNED) [*]"
        self.case(arguments=(name,), entries={f"nested/{name}.desktop": "web"}, expected_name=name,
                  expected_removed=self.removals(name, "weird-touch-pwned", f"nested/{name}.desktop"))

    def test_unknown_name_fallback_and_notifications_disabled(self):
        self.case(arguments=("Unknown",), expected_name="Unknown", notify="false",
                  expected_removed=self.removals("Unknown", "unknown"))

    def test_no_apps_and_missing_directory(self):
        for missing in (False, True):
            with self.subTest(missing=missing):
                self.case(entries={}, missing_directory=missing, expected_status=1,
                          source_stdout=b"No web apps to remove.\n", stdout_gap=True)

    def test_cancel_empty_selection(self):
        self.case(selection="\n", expected_menu=["Alpha", "Space App"], expected_status=1,
                  source_stdout=b"You must select a web app to remove.\n", stdout_gap=True)

    def test_empty_explicit_argument(self):
        self.case(arguments=("",), expected_status=1,
                  source_stdout=b"You must select a web app to remove.\n", stdout_gap=True)

    def test_menu_failure_stops_without_removal(self):
        self.case(menu_status=7, expected_status=7, expected_menu=["Alpha", "Space App"])

    def test_notification_failure_stops_before_cache(self):
        self.case(arguments=("Alpha",), expected_name="Alpha", notify_status=9, expected_status=9,
                  expected_removed=self.removals("Alpha", "alpha", "Alpha.desktop"))

    def test_notification_output_retained_and_cache_output_suppressed(self):
        self.case(arguments=("Alpha",), expected_name="Alpha", notify_output="notification output\n",
                  source_stdout=b"notification output\n",
                  expected_removed=self.removals("Alpha", "alpha", "Alpha.desktop"))

    def test_cache_failure_preserves_status_after_removal(self):
        self.case(arguments=("Alpha",), expected_name="Alpha", database_status=11, expected_status=11,
                  expected_removed=self.removals("Alpha", "alpha", "Alpha.desktop"))


if __name__ == "__main__":
    unittest.main()
