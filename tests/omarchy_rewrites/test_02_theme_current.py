"""Bounded native-command parity checks for the pinned theme-current rewrite."""
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
FIXTURE = ROOT / "tests/fixtures/omarchy-rewrites/02-theme-current"
PROGRAM = ROOT / "examples/omarchy-rewrites/02-theme-current.program"
DEFAULT_BINARY = ROOT / "target/release/execute"
BINARY = Path(os.environ.get("EXECUTE_TEST_BINARY", str(DEFAULT_BINARY))).resolve()
SED_EXPRESSION = r"s/(^|-)([a-z])/\1\u\2/g; s/-/ /g"


class ThemeCurrentTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        if not BINARY.is_file():
            raise RuntimeError(f"Set EXECUTE_TEST_BINARY to an existing Execute binary: {BINARY}")
        cls.tools = {name: shutil.which(name) for name in ("printf", "test", "sed")}
        if not all(cls.tools.values()):
            raise RuntimeError("Native printf, test and sed are required")
        cls.bash = shutil.which("bash")
        if not cls.bash:
            raise RuntimeError("Bash is required for the untouched upstream oracle")
        cls.cases = json.loads((FIXTURE / "oracles.json").read_text())
        probe = subprocess.run(
            [cls.tools["sed"], "-E", SED_EXPRESSION], input=b"rose-pine\n",
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=True, timeout=10,
            env={"LC_ALL": "C", "PATH": "/usr/bin:/bin"},
        )
        dialects = {b"Rose Pine\n": "gnu", b"urose upine\n": "bsd"}
        if probe.stdout not in dialects:
            raise RuntimeError(f"Unsupported native sed dialect: {probe.stdout!r}")
        cls.dialect = dialects[probe.stdout]

    def test_pinned_source_hash(self):
        provenance = json.loads((FIXTURE / "provenance.json").read_text())
        self.assertEqual(hashlib.sha256((FIXTURE / "upstream.sh").read_bytes()).hexdigest(),
                         provenance["source_sha256"])

    def run_case(self, case, override_home=False, positional=()):
        with tempfile.TemporaryDirectory(prefix="theme-current-") as temporary:
            work = Path(temporary)
            home = work / case.get("home_name", "home")
            theme_path = home / ".local/state/omarchy/current/theme.name"
            theme_path.parent.mkdir(parents=True)
            kind = case.get("kind", "file")
            if kind == "file":
                theme_path.write_bytes(case["contents"].encode())
            elif kind == "directory":
                theme_path.mkdir()
            elif kind == "symlink":
                target = work / "owned theme.txt"
                target.write_bytes(case["contents"].encode())
                theme_path.symlink_to(target)
            elif kind != "missing":
                raise AssertionError(kind)

            # The PATH contains only recorders that exec the actual native tools.
            bin_path = work / "bin"
            bin_path.mkdir()
            wrapper = f"#!{sys.executable}\n" + (FIXTURE / "argv_probe.py").read_text()
            for name in self.tools:
                executable = bin_path / name
                executable.write_text(wrapper)
                executable.chmod(0o755)
            log = work / "argv.jsonl"
            env = {
                "HOME": str(home), "PATH": str(bin_path), "LC_ALL": "C",
                "OMARCHY_TEST_ARGV_LOG": str(log),
                "OMARCHY_TEST_REAL_TOOLS": json.dumps(self.tools),
            }

            def run(command, process_env=None):
                log.write_text("")
                result = subprocess.run(command, cwd=work, env=process_env or env, stdin=subprocess.DEVNULL,
                                        stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=15)
                calls = [json.loads(line) for line in log.read_text().splitlines()]
                return result, calls

            original, original_calls = run([self.bash, str(FIXTURE / "upstream.sh"), *positional])
            candidate_env = dict(env)
            extra_inputs = []
            if override_home:
                candidate_env["HOME"] = str(work / "unused environment home")
                extra_inputs = ["--input", f"home={home}"]
            candidate, candidate_calls = run([str(BINARY), str(PROGRAM), *extra_inputs,
                                               "--", *positional], candidate_env)
            self.assertEqual(original.returncode, 0, original.stderr)
            self.assertEqual(candidate.returncode, original.returncode, candidate.stderr)
            self.assertEqual(candidate.stdout, original.stdout)
            self.assertEqual(candidate.stderr, original.stderr)
            self.assertEqual(original.stdout, case["stdout"][self.dialect].encode())

            prefix = [["printf", "%s/.local/state/omarchy/current/theme.name", str(home)],
                      ["test", "-f", str(theme_path)]]
            if kind in ("file", "symlink"):
                sed_call = ["sed", "-E", SED_EXPRESSION, str(theme_path)]
                self.assertEqual(original_calls, [sed_call])
                self.assertEqual(candidate_calls, prefix + [sed_call])
            else:
                self.assertEqual(original_calls, [])  # Bash's test and echo are builtins.
                self.assertEqual(candidate_calls, prefix + [["printf", "%s\n", "Unknown"]])
            for marker in ("HOME_PWNED", "CONTENT_PWNED", "BACKTICK_PWNED"):
                self.assertFalse((work / marker).exists(), marker)

    def test_native_parity_and_literal_argv(self):
        for case in self.cases:
            with self.subTest(case=case["name"], sed=self.dialect):
                self.run_case(case)

    def test_ignored_positional_arguments_are_literal(self):
        self.run_case(self.cases[0], positional=("--ignored", "a b", "$(touch HOME_PWNED)"))

    def test_explicit_input_overrides_environment(self):
        self.run_case(self.cases[5], override_home=True)

    def test_static_check_and_input_declarations(self):
        result = subprocess.run([str(BINARY), "check", str(PROGRAM), "--json"],
                                stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=15)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(json.loads(result.stdout)["ok"])
        result = subprocess.run([str(BINARY), "inspect", str(PROGRAM)],
                                stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=15)
        self.assertEqual(result.returncode, 0, result.stderr)
        inputs = {item["name"]: item for item in json.loads(result.stdout)["inputs"]}
        self.assertEqual(inputs["home"]["source"], {"kind": "environment", "name": "HOME"})
        self.assertEqual(inputs["home"]["value"], "")
        self.assertEqual(inputs["arguments"]["source"], {"kind": "arguments"})
        self.assertEqual(inputs["arguments"]["value"], [])


if __name__ == "__main__":
    unittest.main()
