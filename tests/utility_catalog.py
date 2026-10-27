#!/usr/bin/env python3
"""Exercise utility-runner boundaries without running user-authored programs."""
import importlib.util
import os
from pathlib import Path
import stat
import sys
import tempfile
import unittest

PROJECT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location('utility_runner', PROJECT / 'examples/ladder/run.py')
runner = importlib.util.module_from_spec(spec)
spec.loader.exec_module(runner)


class UtilityRunnerContracts(unittest.TestCase):
    def test_paths_cannot_escape_prepared_directory(self):
        for name in ['/tmp/outside', '../outside', 'nested/../../outside', '.utility/contract.json']:
            with self.subTest(name=name), self.assertRaises(ValueError):
                runner.relative_path(name)
        self.assertEqual(runner.relative_path('input/with space.txt'), Path('input/with space.txt'))

    def test_capture_overflow_stops_owned_runner(self):
        with tempfile.TemporaryDirectory() as name:
            old_limit = runner.MAX_CAPTURE
            runner.MAX_CAPTURE = 4096
            child = runner.Runner(Path(sys.executable), Path(name),
                                  ['-c', 'import os; os.write(1, b"x" * 65536)'], 5)
            try:
                with self.assertRaisesRegex(ValueError, 'stdout/stderr'):
                    child.finish()
            finally:
                child.close()
                runner.MAX_CAPTURE = old_limit
            self.assertIsNotNone(child.child.poll())

    def test_preparation_preserves_readme_input_and_refuses_populated_destination(self):
        with tempfile.TemporaryDirectory() as name:
            root = Path(name)
            source = root / 'sources'
            source.mkdir()
            (source / '001-copy.program').write_text('run cat with path "README.md"\nthen save to "reports/note.txt"\n')
            case = {'number': 1, 'file': '001-copy.program', 'title': 'Read a project note',
                    'purpose': 'Display an externally supplied note.',
                    'inputs': {'README.md': 'User supplied note'},
                    'outputs': {'stdout': 'Note bytes', 'reports/note.txt': 'Saved note'}, 'applets': ['cat'],
                    'fixtures': {'README.md': 'my input\n'}, 'expected': 'my input\n',
                    'files': {'reports/note.txt': 'my input\n'}}
            destination = root / 'prepared'
            runner.prepare(destination, case, source)
            self.assertEqual((destination / 'README.md').read_text(), 'my input\n')
            self.assertTrue((destination / '.utility/README.md').is_file())
            self.assertFalse((destination / 'reports').exists())
            self.assertEqual((destination / '.utility/expected/files/reports/note.txt').read_text(),
                             'my input\n')
            with self.assertRaisesRegex(ValueError, 'new or empty'):
                runner.prepare(destination, case, source)
            self.assertEqual((destination / 'README.md').read_text(), 'my input\n')

    def test_comment_normalization_preserves_hashes_inside_parameters(self):
        self.assertEqual(runner.normalized_source('# heading\nrun printf with message "a#b" # note\n'),
                         'run printf with message "a#b"')

    def test_checkpoint_key_is_isolated_private_and_exactly_audited(self):
        with tempfile.TemporaryDirectory() as name:
            cwd = Path(name)
            state_home = cwd / runner.ISOLATED_STATE_HOME
            state_dir = state_home / 'execute'
            state_dir.mkdir(parents=True, mode=0o700)
            key = cwd / runner.WHAT_IF_KEY
            key.write_bytes(b'k' * 32)
            if os.name == 'posix':
                state_home.chmod(0o700)
                state_dir.chmod(0o700)
                key.chmod(0o600)

            contents = runner.verify_isolated_state(cwd, expected=True)
            self.assertEqual(contents, b'k' * 32)
            runner.audit_outputs(cwd, {runner.WHAT_IF_KEY.as_posix()})

            if os.name == 'posix':
                key.chmod(0o644)
                with self.assertRaisesRegex(AssertionError, 'mode 0600'):
                    runner.verify_isolated_state(cwd, expected=True)
                key.chmod(0o600)

            (cwd / 'unrelated.txt').write_text('unexpected')
            with self.assertRaisesRegex(AssertionError, 'undeclared output file: unrelated.txt'):
                runner.audit_outputs(cwd, {runner.WHAT_IF_KEY.as_posix()})

    def test_unexpected_checkpoint_state_is_rejected_for_non_cache_case(self):
        with tempfile.TemporaryDirectory() as name:
            cwd = Path(name)
            (cwd / runner.ISOLATED_STATE_HOME).mkdir()
            with self.assertRaisesRegex(AssertionError, 'unexpected per-user state'):
                runner.verify_isolated_state(cwd, expected=False)


if __name__ == '__main__':
    unittest.main()
