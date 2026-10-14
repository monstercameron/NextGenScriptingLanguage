#!/usr/bin/env python3
"""Prevent replacement proof harnesses from accepting incomplete or wrong evidence."""
import importlib.util
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
BINARY = Path(os.environ.get('EXECUTE_PROOF_BINARY', str(ROOT/'bin/execute'))).resolve()
spec = importlib.util.spec_from_file_location('replacement_proofs', ROOT/'benchmarks/replacement_proofs.py')
proof = importlib.util.module_from_spec(spec)
spec.loader.exec_module(proof)


class ProofContracts(unittest.TestCase):
    def test_budget_rejection_does_not_create_reports_or_fixtures(self):
        for option, value in [('--stream-bytes', '0'), ('--stream-bytes', '2000000001'),
                              ('--lifecycle-repetitions', '0'), ('--lifecycle-repetitions', '10001'),
                              ('--native-failure-code', '0'), ('--native-failure-code', '42')]:
            with self.subTest(option=option, value=value), tempfile.TemporaryDirectory() as tmp:
                folder = Path(tmp)
                p = subprocess.run([sys.executable, str(ROOT/'benchmarks/replacement_proofs.py'),
                                    '--report', str(folder/'report.json'), option, value],
                                   cwd=folder, env=dict(os.environ, TMPDIR=str(folder)),
                                   capture_output=True, timeout=5)
                self.assertEqual(p.returncode, 2)
                self.assertEqual(list(folder.iterdir()), [])

    def test_optimized_python_cannot_disable_acceptance_checks(self):
        with tempfile.TemporaryDirectory() as tmp:
            report = Path(tmp)/'report.json'
            p = subprocess.run([sys.executable, '-O', str(ROOT/'benchmarks/replacement_proofs.py'),
                                '--report', str(report)], capture_output=True, timeout=5)
            self.assertEqual(p.returncode, 2)
            self.assertIn(b'without -O', p.stderr)
            self.assertFalse(report.exists())

    def test_wrong_output_hash_cannot_pass_successful_native_execution(self):
        with tempfile.TemporaryDirectory(prefix='execute proof contract ') as tmp:
            folder = Path(tmp)
            shutil.copytree(ROOT/'manifests', folder/'manifests')
            (folder/'input.bin').write_bytes(b'known\x00\xff\n')
            with self.assertRaisesRegex(AssertionError, 'SHA-256 mismatch'):
                proof.run_case(BINARY, folder,
                               'stream from cat with path "input.bin"\nshow each line\n',
                               expected_code=0, expected_spawns=1, expected_hash='0'*64)

    def test_nonzero_parse_error_cannot_pass_as_native_failure(self):
        with tempfile.TemporaryDirectory(prefix='execute proof contract ') as tmp:
            folder = Path(tmp)
            shutil.copytree(ROOT/'manifests', folder/'manifests')
            with self.assertRaisesRegex(AssertionError, 'native spawn count'):
                proof.run_case(BINARY, folder,
                               'stream from missing-applet\nshow each line\n',
                               expected_code=1, expected_spawns=1)

    def test_partial_fixture_matches_independent_byte_sequence(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp)/'partial.bin'
            expected = bytes([*range(10), *range(11, 256), 10])*3 + bytes(range(7))
            actual_hash = proof.make_fixture(path, len(expected))
            self.assertEqual(path.read_bytes(), expected)
            import hashlib
            self.assertEqual(actual_hash, hashlib.sha256(expected).hexdigest())


if __name__ == '__main__':
    unittest.main()
