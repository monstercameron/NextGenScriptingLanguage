#!/usr/bin/env python3
"""Regression checks for benchmark environment isolation and fixture budgets."""
import importlib.util
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location('unix_workloads', ROOT/'benchmarks/unix_workloads.py')
bench = importlib.util.module_from_spec(spec)
spec.loader.exec_module(bench)


class BenchmarkContracts(unittest.TestCase):
    def test_bash_startup_hook_cannot_run_in_child(self):
        with tempfile.TemporaryDirectory(prefix='execute-bench-contract-') as directory:
            root = Path(directory)
            hook = root/'hook.bash'
            hook.write_text('printf unexpected >hook-ran\n')
            script = root/'hello.bash'
            script.write_text("printf '%s\\n' safe\n")
            with patch.dict(os.environ, BASH_ENV=str(hook), ENV=str(hook)):
                result = bench.run([shutil.which('bash'),'--noprofile','--norc',str(script)], root, timeout=5)
                self.assertEqual(os.environ['BASH_ENV'], str(hook))
            self.assertEqual(result['code'], 0)
            self.assertEqual(result['stdout'], b'safe\n')
            self.assertFalse((root/'hook-ran').exists())

    def test_over_budget_fixture_fails_before_creation(self):
        with tempfile.TemporaryDirectory(prefix='execute-bench-budget-') as directory:
            result = subprocess.run([sys.executable,str(ROOT/'benchmarks/unix_workloads.py'),
                                     '--line-count','2000000'], cwd=directory,
                                    env=dict(bench.child_environment(),TMPDIR=directory),
                                    capture_output=True, timeout=5)
            self.assertEqual(result.returncode, 2)
            self.assertIn(b'line count exceeds finite-output budget', result.stderr)
            self.assertEqual(list(Path(directory).iterdir()), [])


if __name__ == '__main__':
    unittest.main()
