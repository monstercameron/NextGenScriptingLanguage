"""Differential, unprivileged tests of the pinned log analyzer rewrite.

Only grep's fixed upstream /tmp operand is redirected into a temporary tree.
The upstream Bash file is byte-identical to the pinned official script.
"""
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
FIXTURES = ROOT / "tests/fixtures/omarchy-rewrites/03-update-analyze-logs"
PROGRAM = ROOT / "examples/omarchy-rewrites/03-update-analyze-logs.program"
DEFAULT_BINARY = ROOT / "target/release/execute"
EXECUTE = Path(os.environ.get("EXECUTE_TEST_BINARY", str(DEFAULT_BINARY))).resolve()
BASH = Path(os.environ.get("BASH_TEST_BINARY", "/bin/bash")).resolve()
UPSTREAM_SHA256 = "d72930596e608ef47004fbf444b2088b5b9a3bf94e4fdfa26b2507eadcedad99"
UPDATE = "Updating linux initcpios"
SUCCESS = "Initcpio image generation successful"
WARNING = b"\x1b[31mError: Initramfs generation may have failed. Review logs before restart.\x1b[0m\n\n"

# A transport shim, not a replacement for the analyzer. It logs exact argv,
# redirects just the fixed upstream log operand, then runs the host native grep.
GREP_SHIM = r'''
import json, os, subprocess, sys
from pathlib import Path
args = sys.argv[1:]
log = os.environ["OWNED_UPDATE_LOG"]
normalized = ["$UPDATE_LOG" if a in (log, "/tmp/omarchy-update.log") else a for a in args]
with open(os.environ["GREP_TRACE"], "a") as out:
    out.write(json.dumps(["grep", *normalized]) + "\n")
args = [log if a == "/tmp/omarchy-update.log" else a for a in args]
forced = json.loads(os.environ["FORCED_GREP_STATUSES"])
if args[1] in forced:
    status = forced[args[1]]
    if status > 1:
        sys.stderr.write("fixture grep failure: " + args[1] + "\n")
    sys.exit(status)
result = subprocess.run([os.environ["REAL_GREP"], *args], capture_output=True)
sys.stdout.buffer.write(result.stdout)
sys.stderr.buffer.write(result.stderr)
sys.exit(result.returncode)
'''


class UpdateLogRewriteTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        if not EXECUTE.is_file():
            raise RuntimeError(f"Execute binary missing: {EXECUTE}; set EXECUTE_TEST_BINARY")
        # Apple Bash 3.2 does not implement echo's \e escape. Keep exact
        # separate stdout oracles there rather than claim byte equivalence.
        probe = subprocess.run([str(BASH), "-c", r"echo -e '\e'"], capture_output=True, timeout=10)
        if probe.stdout == b"\x1b\n":
            cls.upstream_warning = WARNING
        elif probe.stdout == b"\\e\n":
            cls.upstream_warning = WARNING.replace(b"\x1b", b"\\e")
        else:
            raise RuntimeError(f"Unsupported Bash echo behavior: {probe.stdout!r}")
        cls.real_grep = shutil.which("grep")
        cls.real_printf = shutil.which("printf")
        if not cls.real_grep or not cls.real_printf:
            raise RuntimeError("Native grep and printf are required")

    def test_upstream_pin(self):
        self.assertEqual(hashlib.sha256((FIXTURES / "upstream.sh").read_bytes()).hexdigest(), UPSTREAM_SHA256)

    def differential(self, fixture, calls, warning=False, forced=None, default_path=False, arguments=()):
        with tempfile.TemporaryDirectory(prefix="omarchy-03-") as directory:
            base = Path(directory)
            home = base / "home"
            home.mkdir()
            bin_dir = base / "bin"
            bin_dir.mkdir()
            log = base / "owned log ; $(never-run).log"
            if fixture is not None:
                log.write_bytes((FIXTURES / fixture).read_bytes())
            trace = base / "trace.jsonl"
            shim = bin_dir / "grep"
            shim.write_text(f"#!{sys.executable} -S\n" + GREP_SHIM)
            shim.chmod(0o755)
            (bin_dir / "printf").symlink_to(self.real_printf)
            env = {"HOME": str(home), "PATH": str(bin_dir), "LC_ALL": "C",
                   "OWNED_UPDATE_LOG": str(log), "GREP_TRACE": str(trace),
                   "FORCED_GREP_STATUSES": json.dumps(forced or {}), "REAL_GREP": self.real_grep,
                   "PYTHONDONTWRITEBYTECODE": "1"}
            before_log = log.read_bytes() if log.exists() else None
            commands = [[str(BASH), str(FIXTURES / "upstream.sh"), *arguments], [str(EXECUTE), str(PROGRAM)]]
            if not default_path:
                commands[1] += ["--input", f"update_log={log}"]
            if arguments:
                commands[1] += ["--", *arguments]
            results = []
            for command in commands:
                trace.unlink(missing_ok=True)
                result = subprocess.run(command, env=env, cwd=home, capture_output=True, timeout=10)
                recorded = [json.loads(line) for line in trace.read_text().splitlines()] if trace.exists() else []
                results.append((result.returncode, result.stdout, result.stderr, recorded))
                self.assertEqual(log.read_bytes() if log.exists() else None, before_log)
                self.assertEqual(list(home.iterdir()), [], "Analyzer must not write HOME")
                self.assertEqual(sorted(p.name for p in base.iterdir()),
                                 sorted(["home", "bin", "trace.jsonl"] + ([log.name] if fixture else [])))
            upstream, rewrite = results
            self.assertEqual(upstream[0], rewrite[0], "Bash/Execute status must agree")
            self.assertEqual(upstream[2:], rewrite[2:], "Bash/Execute stderr and native grep argv must agree")
            self.assertEqual(upstream[1], self.upstream_warning if warning else b"")
            if self.upstream_warning == WARNING or not warning:
                self.assertEqual(upstream[1], rewrite[1], "Bash/Execute stdout bytes must agree")
            status, stdout, stderr, recorded = results[1]
            self.assertEqual(status, 0)
            self.assertEqual(stdout, WARNING if warning else b"")
            self.assertEqual(recorded, [["grep", "-q", pattern, "$UPDATE_LOG"] for pattern in calls])
            if fixture is None:
                self.assertIn(str(log).encode(), stderr)
                self.assertIn(b"No such file or directory", stderr)
            elif forced and any(code > 1 for code in forced.values()):
                self.assertEqual(stderr, b"".join(("fixture grep failure: " + pattern + "\n").encode()
                                                for pattern in calls if forced.get(pattern, 0) > 1))
            else:
                self.assertEqual(stderr, b"")

    def test_failure_warns_with_exact_color_and_blank_line(self):
        self.differential("failed.log", [UPDATE, SUCCESS], warning=True)

    def test_success_suppresses_warning(self):
        self.differential("success.log", [UPDATE, SUCCESS])

    def test_unrelated_log_short_circuits(self):
        self.differential("unrelated.log", [UPDATE])

    def test_empty_log_short_circuits(self):
        self.differential("empty.log", [UPDATE])

    def test_matching_is_case_sensitive(self):
        self.differential("near-match.log", [UPDATE])

    def test_any_success_marker_suppresses_warning_even_before_update(self):
        self.differential("global-success.log", [UPDATE, SUCCESS])

    def test_missing_log_prints_grep_diagnostic_but_returns_zero(self):
        self.differential(None, [UPDATE])

    def test_first_grep_failure_short_circuits_and_returns_zero(self):
        self.differential("failed.log", [UPDATE], forced={UPDATE: 2})

    def test_second_grep_failure_still_warns_and_returns_zero(self):
        self.differential("success.log", [UPDATE, SUCCESS], warning=True, forced={SUCCESS: 2})

    def test_default_operand_preserved(self):
        self.differential("failed.log", [UPDATE, SUCCESS], warning=True, default_path=True)

    def test_upstream_ignores_arbitrary_positional_arguments(self):
        self.differential("failed.log", [UPDATE, SUCCESS], warning=True,
                          arguments=("", "--anything", "$(never-run)", "space argument"))

    def test_input_contract(self):
        result = subprocess.run([str(EXECUTE), "inspect", str(PROGRAM)], capture_output=True, timeout=10)
        self.assertEqual(result.returncode, 0, result.stderr)
        interface = json.loads(result.stdout)
        inputs = {value["name"]: value for value in interface["inputs"]}
        self.assertEqual(inputs["update_log"]["type"], "text")
        self.assertEqual(inputs["update_log"]["default"], "/tmp/omarchy-update.log")
        self.assertNotIn("source", inputs["update_log"])
        self.assertEqual(inputs["ignored_arguments"]["type"], "list of text")
        self.assertEqual(inputs["ignored_arguments"]["source"], {"kind": "arguments"})
        self.assertEqual(interface["stdin_mode"], "unused")


if __name__ == "__main__":
    unittest.main()
