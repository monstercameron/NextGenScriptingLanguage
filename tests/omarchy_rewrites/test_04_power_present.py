"""Run both implementations only against temporary, owned fake sysfs trees."""
import hashlib
import json
import os
from pathlib import Path
import shutil
import stat
import subprocess
import tempfile
import unittest

PROJECT = Path(__file__).resolve().parents[2]
FIXTURE = PROJECT / "tests/fixtures/omarchy-rewrites/04-power-present"
PROGRAM = PROJECT / "examples/omarchy-rewrites/04-power-present.program"
DEFAULT_EXECUTE = PROJECT / "target/release/execute"
EXECUTE = Path(os.environ.get("EXECUTE_TEST_BINARY", DEFAULT_EXECUTE))
SOURCE_SHA256 = "4691bfec9ec4123d4e5caa29e32c9b2dcbf72f2a39bdeb81121edfcad3e9a1c1"


class PowerPresentTests(unittest.TestCase):
    def setUp(self):
        self.assertTrue(EXECUTE.is_file(), f"Set EXECUTE_TEST_BINARY: {EXECUTE}")
        self.temp = tempfile.TemporaryDirectory(prefix="omarchy-power-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.supplies = self.root / 'fake power ; $(touch unsafe) [*]'
        self.supplies.mkdir()
        self.bin = self.root / "bin"
        self.bin.mkdir()
        # Real native tools only. No mocked controller or upstream delegation.
        for tool in ("find", "jq", "printf", "test"):
            native = shutil.which(tool)
            self.assertIsNotNone(native, f"Native dependency missing: {tool}")
            (self.bin / tool).symlink_to(native)
        self.env = {
            "PATH": str(self.bin), "HOME": str(self.root), "LC_ALL": "C",
            "OMARCHY_POWER_SUPPLY_PATH": str(self.supplies),
        }

    def supply(self, name="AC", kind="Mains\n", online="1\n"):
        path = self.supplies / name
        path.mkdir()
        if kind is not None:
            (path / "type").write_text(kind)
        if online is not None:
            (path / "online").write_text(online)
        return path

    def snapshot(self):
        result = {}
        for path in self.root.rglob("*"):
            info = path.lstat()
            entry = {"mode": info.st_mode}
            if stat.S_ISREG(info.st_mode):
                # A denied-read fixture remains untouched and is not read by QA.
                if info.st_mode & 0o444:
                    entry["sha256"] = hashlib.sha256(path.read_bytes()).hexdigest()
                entry["size"] = info.st_size
            elif stat.S_ISLNK(info.st_mode):
                entry["target"] = os.readlink(path)
            result[str(path.relative_to(self.root))] = entry
        return result

    def compare(self, expected, root=None, explicit=False, args=()):
        selected_root = root or self.supplies
        self.assertTrue(selected_root.is_relative_to(self.root))
        env = dict(self.env, OMARCHY_POWER_SUPPLY_PATH=str(selected_root))
        before = self.snapshot()
        upstream = subprocess.run(
            ["/bin/bash", str(FIXTURE / "upstream.sh"), *args],
            cwd=self.root, env=env, capture_output=True, timeout=10, check=False)
        self.assertEqual(self.snapshot(), before, "upstream changed the owned tree")
        command = [str(EXECUTE), str(PROGRAM)]
        if explicit:
            # An explicit safe path must beat a different safe environment root.
            empty = self.root / "environment-empty"
            empty.mkdir(exist_ok=True)
            env["OMARCHY_POWER_SUPPLY_PATH"] = str(empty)
            command += ["--input", f"power_supply_path={selected_root}"]
            before = self.snapshot()
        command += ["--", *args]
        rewritten = subprocess.run(command, cwd=self.root, env=env,
                                   capture_output=True, timeout=10, check=False)
        self.assertEqual(self.snapshot(), before, "rewrite changed the owned tree")
        self.assertEqual(upstream.returncode, expected)
        self.assertEqual(rewritten.returncode, expected, rewritten.stderr.decode())
        self.assertEqual(upstream.stdout, b"")
        self.assertEqual(rewritten.stdout, upstream.stdout)
        self.assertEqual(upstream.stderr, b"")
        if expected == 0:
            self.assertEqual(rewritten.stderr, b"")
        else:
            self.assertIn(b"execute:", rewritten.stderr)
            self.assertIn(b"failed", rewritten.stderr)
        evidence = os.environ.get("OMARCHY_REWRITE_EVIDENCE_DIR")
        if evidence:
            destination = Path(evidence) / "04-power-present"
            destination.mkdir(parents=True, exist_ok=True)
            (destination / (self._testMethodName + ".json")).write_text(json.dumps({
                "case": self._testMethodName,
                "upstream": {"status": upstream.returncode, "stdout": upstream.stdout.decode(),
                             "stderr": upstream.stderr.decode()},
                "execute": {"status": rewritten.returncode, "stdout": rewritten.stdout.decode(),
                            "stderr": rewritten.stderr.decode()},
                "tree_unchanged": True,
            }, indent=2) + "\n")

    def test_untouched_upstream(self):
        self.assertEqual(hashlib.sha256((FIXTURE / "upstream.sh").read_bytes()).hexdigest(), SOURCE_SHA256)

    def test_mains_online(self):
        self.supply()
        self.compare(0)

    def test_mains_offline(self):
        self.supply(online="0\n")
        self.compare(1)

    def test_usb_online(self):
        self.supply(kind="USB\n")
        self.compare(0)

    def test_usb_offline(self):
        self.supply(kind="USB\n", online="0\n")
        self.compare(1)

    def test_unsupported_type(self):
        self.supply(kind="Battery\n")
        self.compare(1)

    def test_empty_tree(self):
        self.compare(1)

    def test_missing_root(self):
        self.compare(1, root=self.root / "absent")

    def test_non_directory_root(self):
        path = self.root / "not a directory"
        path.write_text("fixture")
        self.compare(1, root=path)

    def test_missing_type(self):
        self.supply(kind=None)
        self.compare(1)

    def test_missing_online(self):
        self.supply(online=None)
        self.compare(1)

    def test_unreadable_type(self):
        path = self.supply() / "type"
        path.chmod(0)
        self.addCleanup(path.chmod, 0o600)
        if os.access(path, os.R_OK):
            self.skipTest("current user bypasses fixture read permissions")
        self.compare(1)

    def test_unreadable_root(self):
        self.supplies.chmod(0)
        self.addCleanup(self.supplies.chmod, 0o700)
        if os.access(self.supplies, os.R_OK):
            self.skipTest("current user bypasses fixture read permissions")
        self.compare(1)

    def test_unreadable_online(self):
        path = self.supply() / "online"
        path.chmod(0)
        self.addCleanup(path.chmod, 0o600)
        if os.access(path, os.R_OK):
            self.skipTest("current user bypasses fixture read permissions")
        self.compare(1)

    def test_multiple_entries_later_online(self):
        self.supply("00-offline", online="0\n")
        self.supply("01-battery", kind="Battery\n")
        self.supply("02-missing-type", kind=None)
        self.supply("03-usb", kind="USB\n")
        self.compare(0)

    def test_first_success_skips_later_reads(self):
        self.supply("00-online")
        later = self.supply("99-blocking", online=None)
        # A readable FIFO would hang jq/Bash if either evaluated it after success.
        os.mkfifo(later / "online")
        self.compare(0)

    def test_literal_space_metacharacter_and_newline_paths(self):
        self.supply('USB ; $(touch unsafe) [*] "quote"\nnext', kind="USB\n")
        self.compare(0)
        self.assertFalse((self.root / "unsafe").exists())

    def test_hidden_entries_are_not_globbed(self):
        self.supply(".hidden")
        self.compare(1)

    def test_newline_trimming_and_exact_values(self):
        self.supply("AC", kind="Mains\n\n\n", online="1\n\n\n")
        self.compare(0)

    def test_values_with_spaces_are_not_trimmed(self):
        self.supply(kind="Mains \n")
        self.supply("USB", kind="USB\n", online="1 \n")
        self.compare(1)

    def test_explicit_root_overrides_environment(self):
        self.supply()
        self.compare(0, explicit=True)

    def test_ignored_positional_arguments(self):
        self.supply()
        self.compare(0, args=("--help", "", "$(touch unsafe)", "a\nb", "*"))

    def test_symlink_root_and_supply(self):
        self.supply()
        (self.supplies / "USB link").symlink_to(self.supplies / "AC", target_is_directory=True)
        link = self.root / "root link"
        link.symlink_to(self.supplies, target_is_directory=True)
        self.compare(0, root=link)


if __name__ == "__main__":
    unittest.main()
