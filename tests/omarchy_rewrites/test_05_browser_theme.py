#!/usr/bin/env python3
"""Pinned differential proof; all browser/policy/process commands are isolated fakes."""
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
FIXTURE = ROOT / 'tests/fixtures/omarchy-rewrites/05-theme-set-browser'
PROGRAM = ROOT / 'examples/omarchy-rewrites/05-theme-set-browser.program'
DEFAULT_BINARY = ROOT / "target/release/execute"
BROWSERS = ['chromium', 'google-chrome-stable', 'google-chrome', 'microsoft-edge-stable', 'brave', 'brave-origin']
MOCK = '''#!{python}
import json, os, pathlib, sys
name = pathlib.Path(sys.argv[0]).name
args = sys.argv[1:]
state = json.loads(pathlib.Path(os.environ['MOCK_STATE']).read_text())
with open(os.environ['MOCK_LOG'], 'a') as f:
    f.write(json.dumps([name, *args]) + '\\n')
if name == 'omarchy-cmd-present':
    sys.exit(0 if args[0] in state.get('installed', []) else 1)
if name == 'pgrep':
    if args[1] in state.get('running', []):
        print('12345')
        sys.exit(0)
    sys.exit(state.get('pgrep_status', 1))
if name == 'omarchy-theme-set-browser-policy':
    if state.get('policy_status', 0) == 0:
        dest = pathlib.Path(os.environ['HOME']) / 'fake managed policy' / 'color.json'
        dest.parent.mkdir(parents=True, exist_ok=True)
        dest.write_text('{{"BrowserThemeColor": "#%s", "BrowserColorScheme": "device"}}\\n' % args[0])
    sys.stdout.write(state.get('policy_stdout', ''))
    sys.stderr.write(state.get('policy_stderr', ''))
    sys.exit(state.get('policy_status', 0))
repeat = state.get('browser_repeat_stdout', 0)
if repeat:
    sys.stdout.write('x' * repeat)
else:
    sys.stdout.write(state.get('browser_stdout', ''))
sys.stderr.write(state.get('browser_stderr', ''))
sys.exit(state.get('browser_status', {{}}).get(name, 0))
'''


class BrowserThemeRewrite(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.binary = Path(os.environ.get('EXECUTE_TEST_BINARY', DEFAULT_BINARY)).resolve()
        if not cls.binary.is_file():
            raise RuntimeError('Set EXECUTE_TEST_BINARY to the current Execute binary')

    def test_pinned_source_integrity(self):
        provenance = json.loads((FIXTURE / 'provenance.json').read_text())
        self.assertEqual(provenance['revision'], 'c668141e9c42b13c80c9ca4ea108e11708c5e8a5')
        for name, record in provenance['files'].items():
            self.assertEqual(hashlib.sha256((FIXTURE / 'upstream' / name).read_bytes()).hexdigest(), record['sha256'])

    def invoke_pair(self, rgb=None, state=None, theme_directory=False, spaces=False,
                    theme_readable=True, arguments=()):
        state = state or {}
        with tempfile.TemporaryDirectory(prefix='browser proof with spaces ' if spaces else 'browser-proof-') as tmp:
            base = Path(tmp)
            bin_dir = base / 'mock tools'
            bin_dir.mkdir()
            for name in ['omarchy-cmd-present', 'pgrep', 'omarchy-theme-set-browser-policy', *BROWSERS]:
                tool = bin_dir / name
                tool.write_text(MOCK.format(python=sys.executable))
                tool.chmod(0o700)
            state_file = base / 'state.json'
            state_file.write_text(json.dumps(state))
            observations = {}
            for side in ['bash', 'execute']:
                home = base / (side + ' home with spaces' if spaces else side)
                home.mkdir()
                theme = home / '.local/state/omarchy/current/theme/chromium.theme'
                theme.parent.mkdir(parents=True)
                if theme_directory:
                    theme.mkdir()
                elif rgb is not None:
                    theme.write_text(rgb)
                if not theme_readable and theme.is_file():
                    theme.chmod(0)
                # Real-looking profiles verify this wrapper leaves profile settings untouched.
                for profile in ['chromium/Default', 'google-chrome/Profile 1', 'BraveSoftware/Brave-Browser/Default']:
                    prefs = home / '.config' / profile / 'Preferences'
                    prefs.parent.mkdir(parents=True)
                    prefs.write_text('{"browser":{"theme":{"color_scheme":2}},"sentinel":"keep"}\n')
                log = base / (side + '.jsonl')
                env = {'HOME': str(home), 'OMARCHY_PATH': str(FIXTURE / 'upstream'),
                       'PATH': str(bin_dir) + ':/usr/bin:/bin', 'LC_ALL': 'C', 'PYTHONDONTWRITEBYTECODE': '1',
                       'MOCK_STATE': str(state_file), 'MOCK_LOG': str(log)}
                argv = ([os.environ.get('OMARCHY_TEST_BASH', '/bin/bash'), '--noprofile', '--norc', str(FIXTURE / 'upstream/bin/omarchy-theme-set-browser')]
                        if side == 'bash' else [str(self.binary), str(PROGRAM), '--manifest-dir', str(ROOT / 'manifests'), '--', *arguments])
                if side == 'bash':
                    argv += list(arguments)
                result = subprocess.run(argv, cwd=base, env=env, capture_output=True, timeout=10)
                self.assertFalse((base / 'FORBIDDEN').exists(), 'ignored shell-looking arguments stay literal')
                calls = [json.loads(line) for line in log.read_text().splitlines()] if log.exists() else []
                files = {str(p.relative_to(home)): p.read_bytes() for p in home.rglob('*') if p.is_file()}
                observations[side] = (result, calls, files)
            return observations

    def assert_parity(self, observations, color):
        bash, execute = observations['bash'], observations['execute']
        self.assertEqual(execute[1:], bash[1:])
        self.assertEqual((execute[0].returncode, execute[0].stdout, execute[0].stderr),
                         (bash[0].returncode, bash[0].stdout, bash[0].stderr))
        self.assertEqual(execute[0].returncode, 0)
        self.assertEqual(execute[1][0], ['omarchy-theme-set-browser-policy', color])

    def test_dark_light_missing_malformed_rgb_and_spaces(self):
        cases = [(None, '1c2027'), ('28,32,39\n', '1c2027'), ('250, 249, 246\n', 'faf9f6'),
                 ('001,002,003\n', '010203'), (' \t0, 255, 16\n\n', '00ff10'),
                 ('#ffffff', '1c2027'), ('256,0,0', '1c2027'), ('1,2', '1c2027'),
                 ('-1,2,3', '1c2027'), ('1,2,3; touch forbidden', '1c2027'),
                 ('1,2,3\n4,5,6', '1c2027'), ('1000,2,3', '1c2027'), ('1,2,3\r\n', '010203'), ('1,\n2,3', '010203'), ('\u00a01,2,3', '1c2027'), ('', '1c2027')]
        for rgb, color in cases:
            with self.subTest(rgb=rgb):
                self.assert_parity(self.invoke_pair(rgb), color)
        self.assert_parity(self.invoke_pair(theme_directory=True), '1c2027')

    def test_space_paths_and_legacy_bash_redirect_gap(self):
        pair = self.invoke_pair('250,249,246', spaces=True)
        bash, execute = pair['bash'], pair['execute']
        if b'ambiguous redirect' in bash[0].stderr:
            # Apple's Bash 3.2 splits the original unquoted $(<$CHROMIUM_THEME).
            # Keep this failure visible rather than patching the untouched oracle.
            self.assertEqual(bash[1][0], ['omarchy-theme-set-browser-policy', '1c2027'])
            self.assertEqual(execute[1][0], ['omarchy-theme-set-browser-policy', 'faf9f6'])
            self.assertEqual(execute[0].stderr, b'')
            self.assertEqual((execute[0].returncode, bash[0].returncode), (0, 0))
            self.assertEqual(execute[1][1:], bash[1][1:])
            self.assertEqual({k:v for k,v in execute[2].items() if k != 'fake managed policy/color.json'},
                             {k:v for k,v in bash[2].items() if k != 'fake managed policy/color.json'})
        else:
            self.assert_parity(pair, 'faf9f6')

    def test_exact_refresh_argv_and_short_circuit_branches(self):
        running = ['chromium', 'chrome', 'msedge', 'brave', '/opt/brave-origin-bin/']
        state = {'installed': BROWSERS, 'running': running, 'browser_stdout': 'discarded refresh output\n'}
        pair = self.invoke_pair('10,20,30', state)
        self.assert_parity(pair, '0a141e')
        calls = pair['execute'][1]
        self.assertIn(['pgrep', '-f', '/opt/brave-origin-bin/'], calls)
        self.assertIn(['brave-origin', '--refresh-platform-policy', '--no-startup-window'], calls)
        self.assertNotIn(['omarchy-cmd-present', 'google-chrome'], calls)
        # A missing or stopped stable Chrome returns 0 from the Bash if statement.
        for configured in [[], ['google-chrome-stable']]:
            pair = self.invoke_pair('0,0,0', {'installed': configured})
            self.assert_parity(pair, '000000')
            self.assertNotIn(['omarchy-cmd-present', 'google-chrome'], pair['execute'][1])
        # An actual stable Chrome refresh failure is the only fallback trigger.
        state['browser_status'] = {'google-chrome-stable': 7, 'google-chrome': 9, 'chromium': 8, 'brave-origin': 3}
        pair = self.invoke_pair('255,255,255', state)
        self.assert_parity(pair, 'ffffff')
        self.assertIn(['google-chrome', '--refresh-platform-policy', '--no-startup-window'], pair['execute'][1])

    def test_ignored_arguments_and_large_refresh_output(self):
        payload = '$(touch FORBIDDEN)'
        pair = self.invoke_pair('1,2,3', {'installed': ['chromium'], 'running': ['chromium'],
                                         'browser_repeat_stdout': 17 * 1024 * 1024},
                                arguments=('', '--ignored', payload))
        self.assert_parity(pair, '010203')

    def test_unreadable_theme_file_falls_back_and_continues(self):
        with tempfile.NamedTemporaryFile() as probe:
            if os.access(probe.name, os.R_OK):
                self.skipTest('the current account bypasses file mode bits')
        pair = self.invoke_pair('250,249,246', {'installed': ['chromium'], 'running': ['chromium']},
                                theme_readable=False)
        bash, execute = pair['bash'], pair['execute']
        self.assertEqual(execute[1:], bash[1:])
        self.assertEqual((execute[0].returncode, bash[0].returncode), (0, 0))
        self.assertEqual(execute[1][0], ['omarchy-theme-set-browser-policy', '1c2027'])
        self.assertEqual(execute[0].stdout, bash[0].stdout)
        self.assertIn(b'Could not read browser theme file', execute[0].stderr)

    def test_policy_output_is_preserved_on_success(self):
        self.assert_parity(self.invoke_pair('1,2,3', {'policy_stdout': 'policy written\n', 'policy_stderr': 'policy note\n'}), '010203')

    def test_policy_failure_continues_refreshes_status_matches_diagnostic_gap(self):
        pair = self.invoke_pair('1,2,3', {'installed': BROWSERS, 'running': ['chromium', 'chrome'],
                                        'policy_status': 7, 'policy_stderr': 'policy failed\n'})
        bash, execute = pair['bash'], pair['execute']
        self.assertEqual(execute[1:], bash[1:])
        self.assertEqual((execute[0].returncode, bash[0].returncode), (1, 1))
        self.assertEqual(execute[0].stdout, bash[0].stdout)
        self.assertEqual(bash[0].stderr, b'policy failed\n')
        self.assertIn(b'policy failed\n', execute[0].stderr)
        self.assertIn(b'exit status: 1', execute[0].stderr)
        self.assertIn(['chromium', '--refresh-platform-policy', '--no-startup-window'], execute[1])

    def test_browser_stderr_is_discarded_at_the_process_boundary(self):
        pair = self.invoke_pair('1,2,3', {'installed': ['chromium'], 'running': ['chromium'], 'browser_stderr': 'browser diagnostic\n'})
        bash, execute = pair['bash'], pair['execute']
        self.assertEqual(execute[1:], bash[1:])
        self.assertEqual((execute[0].returncode, execute[0].stdout), (bash[0].returncode, bash[0].stdout))
        self.assertEqual(bash[0].stderr, b'')
        self.assertEqual(execute[0].stderr, b'')


if __name__ == '__main__':
    unittest.main()
