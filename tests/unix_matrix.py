#!/usr/bin/env python3
"""Acceptance checks for the Unix differential proof, including false positives."""
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / 'benchmarks'))
import unix_matrix as proof


def observed(code=0, stdout=b'', stderr=b''):
    return dict(code=code, stdout=stdout, stderr=stderr)


class MatrixContracts(unittest.TestCase):
    def test_equal_but_wrong_output_fails_independent_oracle(self):
        case = proof.Case('wrong', 'test', (), expected=b'right')
        with self.assertRaisesRegex(proof.ProofFailure, 'independent output oracle'):
            proof.check(case, observed(stdout=b'wrong'), observed(stdout=b'wrong'))

    def test_wrong_success_status_or_stderr_cannot_match(self):
        case = proof.Case('wrong', 'test', ())
        for value in (observed(code=1), observed(stderr=b'unexpected')):
            with self.subTest(value=value), self.assertRaises(proof.ProofFailure):
                proof.check(case, value, value)

    def test_parse_error_or_wrong_code_cannot_masquerade_as_native_failure(self):
        case = proof.Case('failure', 'test', (), code=7, expected=b'',
                          difference='native_failure_diagnostic_and_buffered_stdout')
        for error in (b'execute: parsing failed', b'execute: failed (exit status: 3)'):
            with self.subTest(error=error), self.assertRaisesRegex(proof.ProofFailure, 'wrong native failure status'):
                proof.check(case, observed(7, stderr=error), observed(7))

    def test_generic_failure_cannot_hide_native_exit_code(self):
        case = proof.Case('failure', 'test', (), code=7, expected=b'',
                          difference='native_failure_diagnostic_and_buffered_stdout')
        with self.assertRaises(proof.ProofFailure):
            proof.check(case, observed(1, stderr=b'execute: exit status: 7'), observed(7))

    def test_native_stderr_and_failed_output_contract_are_checked(self):
        case = proof.Case('failure', 'test', (), code=7, expected=b'',
                          difference='native_failure_diagnostic_and_buffered_stdout')
        for value in (observed(7, stderr=b'execute: exit status: 7'),
                      observed(7, stdout=b'escaped', stderr=b'native error\nexecute: exit status: 7')):
            with self.subTest(value=value), self.assertRaises(proof.ProofFailure):
                proof.check(case, value, observed(7, stderr=b'native error\n'))

    def test_missing_or_wrong_native_spawn_fails(self):
        for events in ([], [{'binary': '/other'}], [{'binary': '/usr/bin/cat'}] * 2):
            with self.subTest(events=events), self.assertRaisesRegex(proof.ProofFailure, 'spawn identities'):
                proof.check_spawns('probe', [['/usr/bin/cat', 'file']], events)

    def test_glob_difference_has_two_explicit_oracles(self):
        case = proof.Case('glob', 'test', (), expected=b'part-*.txt\0', difference='no_implicit_glob')
        with self.assertRaisesRegex(proof.ProofFailure, 'glob contract mismatch'):
            proof.check(case, observed(stdout=case.expected), observed(stdout=case.expected))

    def test_plan_parser_preserves_argv_and_ignores_status_suffix(self):
        source = b'line 1: Start: "/usr/bin/printf" argv=["", "a\\nb", "$HOME"] and keep status as code\n'
        self.assertEqual(proof.plans(source), [['', 'a\nb', '$HOME']])

    def test_bad_budget_does_not_create_fixtures_or_report(self):
        for option, value in (('--samples', '0'), ('--samples', '101'), ('--warmups', '-1'), ('--warmups', '11')):
            with self.subTest(option=option, value=value), tempfile.TemporaryDirectory() as temporary:
                folder = Path(temporary)
                result = subprocess.run([sys.executable, '-O', str(ROOT / 'benchmarks/unix_matrix.py'),
                                         '--binary', '/missing', '--report', str(folder / 'report.json'), option, value],
                                        cwd=folder, env=dict(os.environ, TMPDIR=str(folder)), capture_output=True, timeout=5)
                self.assertEqual(result.returncode, 2)
                self.assertEqual(list(folder.iterdir()), [])


if __name__ == '__main__':
    unittest.main()
