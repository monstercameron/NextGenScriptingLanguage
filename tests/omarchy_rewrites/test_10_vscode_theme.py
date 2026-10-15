#!/usr/bin/env python3
"""Pinned oracle + owned HOME/editor doubles. No installed editor is ever invoked."""
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
FIXTURE = ROOT / 'tests/fixtures/omarchy-rewrites/10-theme-set-vscode'
PROGRAM = ROOT / 'examples/omarchy-rewrites/10-theme-set-vscode.program'
DEFAULT_BINARY = ROOT / "target/release/execute"
EVIDENCE_ROOT = os.environ.get('OMARCHY_REWRITE_EVIDENCE_DIR')
SETTINGS = ['.config/Code/User/settings.json', '.config/Code - Insiders/User/settings.json', '.config/VSCodium/User/settings.json', '.config/Cursor/User/settings.json']
EXTENSIONS = ['.vscode/extensions', '.vscode-insiders/extensions', '.vscode-oss/extensions', '.cursor/extensions']
EDITORS = ['code', 'code-insiders', 'codium', '/usr/bin/cursor']
MOCK = '''#!{python}
import json, os, pathlib, sys
name = pathlib.Path(sys.argv[0]).name
args = sys.argv[1:]
s = json.loads(pathlib.Path(os.environ['MOCK_STATE']).read_text())
with open(os.environ['MOCK_LOG'], 'a') as f:
    f.write(json.dumps([name, *args]) + '\\n')
if name == 'omarchy-toggle-enabled':
    sys.exit(0 if args[0] in s.get('skips', []) else 1)
if name == 'omarchy-cmd-present':
    status = s.get('present_status', {{}}).get(args[0])
    sys.exit(status if status is not None else (0 if args[0] in s.get('present', []) else 1))
if args == ['--list-extensions']:
    sys.stdout.write(s.get('installed', ''))
    sys.stderr.write(s.get('list_stderr', ''))
    sys.exit(s.get('list_status', 0))
if args[:1] == ['--install-extension']:
    # Simulate an editor write under this test's HOME only.
    if s.get('install_status', 0) == 0:
        pathlib.Path(os.environ['HOME'], name + '-installed').write_text(args[1])
    sys.stdout.write(s.get('install_stdout', ''))
    sys.stderr.write(s.get('install_stderr', ''))
    sys.exit(s.get('install_status', 0))
raise SystemExit('unexpected editor argv')
'''
JQ_RACE = '''#!@PYTHON@
import os, pathlib, sys
if sys.argv[1:] == ['.'] and os.environ.get('RACE_REGISTRY'):
    pathlib.Path(os.environ['RACE_REGISTRY']).write_text(
        '[{"identifier":{"id":"concurrent.extension"},"version":"9"}]\\n')
jq = os.environ['REAL_JQ']
os.execv(jq, [jq, *sys.argv[1:]])
'''


class VSCodeThemeRewrite(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.binary = Path(os.environ.get('EXECUTE_TEST_BINARY', DEFAULT_BINARY)).resolve()
        if not cls.binary.is_file():
            raise RuntimeError('Set EXECUTE_TEST_BINARY to the current Execute binary')
        cls.gnu_sed = subprocess.run(['sed', '--version'], capture_output=True).returncode == 0

    def test_pinned_source_integrity(self):
        p = json.loads((FIXTURE / 'provenance.json').read_text())
        self.assertEqual(p['revision'], 'c668141e9c42b13c80c9ca4ea108e11708c5e8a5')
        for name, item in p['files'].items():
            self.assertEqual(hashlib.sha256((FIXTURE / 'upstream' / name).read_bytes()).hexdigest(), item['sha256'])

    def pair(self, name, state=None, descriptor=None, generated=None, settings=None, registry=None, obsolete=None, symlink=False, obsolete_symlink=False, cursor_mapping=False, registry_race=False, arguments=()):
        state = state or {}
        # Hardcoded Cursor may be reported present only when its binary cannot execute.
        if '/usr/bin/cursor' in state.get('present', []) and descriptor:
            self.assertFalse(__import__('re').fullmatch('[a-zA-Z0-9._-]+', str(descriptor.get('extension', ''))))
        result = {}
        with tempfile.TemporaryDirectory(prefix='vscode proof with spaces ') as temp:
            base = Path(temp)
            tools = base / 'private tools'
            tools.mkdir()
            for tool in ['jq', 'dirname', 'mkdir', 'ln', 'rm', 'grep', 'tee', 'printf', 'test', 'false', 'sed', 'mktemp', 'mv', 'cat', 'basename', 'awk']:
                if tool == 'jq' and registry_race:
                    jq_wrapper = tools / tool
                    jq_wrapper.write_text(JQ_RACE.replace('@PYTHON@', sys.executable))
                    jq_wrapper.chmod(0o700)
                else:
                    (tools / tool).symlink_to(shutil.which(tool))
            for tool in ['omarchy-toggle-enabled', 'omarchy-cmd-present', 'code', 'code-insiders', 'codium', 'cursor-mock']:
                path = tools / tool
                path.write_text(MOCK.format(python=sys.executable))
                path.chmod(0o700)
            config = base / 'state.json'
            config.write_text(json.dumps(state))
            for side in ['bash', 'execute']:
                home = base / (side + ' home')
                home.mkdir()
                theme = home / '.local/state/omarchy/current/theme'
                theme.mkdir(parents=True)
                if descriptor is not None:
                    (theme / 'vscode.json').write_text(json.dumps(descriptor) if not isinstance(descriptor, str) else descriptor)
                if generated is not None:
                    (theme / 'vscode-theme.json').write_text(json.dumps(generated))
                for index, path in enumerate(SETTINGS):
                    if settings is not None:
                        dest = home / path
                        dest.parent.mkdir(parents=True, exist_ok=True)
                        if symlink:
                            target = home / ('settings-target-' + str(index))
                            target.write_text(settings)
                            dest.symlink_to(target)
                        else:
                            dest.write_text(settings)
                for path_index, path in enumerate(EXTENSIONS):
                    extension_base = home / path
                    extension_base.mkdir(parents=True)
                    if registry is not None:
                        (extension_base / 'extensions.json').write_text(json.dumps(registry) if not isinstance(registry, str) else registry)
                    if obsolete is not None:
                        obsolete_path = extension_base / '.obsolete'
                        obsolete_contents = json.dumps(obsolete) if not isinstance(obsolete, str) else obsolete
                        if obsolete_symlink:
                            target = home / ('obsolete-target-' + str(path_index))
                            target.write_text(obsolete_contents)
                            obsolete_path.symlink_to(target)
                        else:
                            obsolete_path.write_text(obsolete_contents)
                log = base / (side + '.jsonl')
                env = {'HOME': str(home), 'PATH': str(tools), 'LC_ALL': 'C', 'PYTHONDONTWRITEBYTECODE': '1', 'MOCK_STATE': str(config), 'MOCK_LOG': str(log), 'TMPDIR': str(base)}
                if registry_race:
                    env['REAL_JQ'] = shutil.which('jq')
                    if side == 'execute':
                        env['RACE_REGISTRY'] = str(home / EXTENSIONS[0] / 'extensions.json')
                argv = ([os.environ.get('OMARCHY_TEST_BASH', '/bin/bash'), '--noprofile', '--norc', str(FIXTURE / 'upstream/bin/omarchy-theme-set-vscode')]
                        if side == 'bash' else [str(self.binary), str(PROGRAM), '--manifest-dir', str(ROOT / 'manifests')])
                if side == 'bash':
                    argv += list(arguments)
                if side == 'execute' and cursor_mapping:
                    argv += ['--input', 'cursor_command=cursor-mock']
                if side == 'execute':
                    argv += ['--', *arguments]
                completed = subprocess.run(argv, cwd=base, env=env, capture_output=True, timeout=30)
                self.assertFalse((base / 'FORBIDDEN').exists(), 'unwanted shell interpretation')
                snapshot = {}
                for path in home.rglob('*'):
                    key = str(path.relative_to(home))
                    if path.is_symlink():
                        snapshot[key] = {'link': os.readlink(path).replace(str(home), '$HOME')}
                    elif path.is_file():
                        try:
                            snapshot[key] = path.read_text().replace(str(home), '$HOME')
                        except UnicodeDecodeError:
                            snapshot[key] = {'binary_sha256': hashlib.sha256(path.read_bytes()).hexdigest()}
                calls = [json.loads(x) for x in log.read_text().splitlines()] if log.exists() else []
                result[side] = {'status': completed.returncode, 'stdout': completed.stdout.decode(), 'stderr': completed.stderr.decode().replace(str(home), '$HOME'), 'calls': calls, 'files': snapshot}
        if EVIDENCE_ROOT:
            evidence = Path(EVIDENCE_ROOT) / '10-theme-set-vscode'
            evidence.mkdir(parents=True, exist_ok=True)
            (evidence / (name + '.json')).write_text(json.dumps(result, indent=2))
        return result

    def assert_pair(self, pair, settings_changed=False):
        a, b = pair['bash'], pair['execute']
        self.assertEqual(a['calls'], b['calls'])
        self.assertEqual(a['stdout'], b['stdout'])
        if settings_changed:
            # Structural JSONC edits and registry JSON preserve semantic
            # behavior while formatting, object-key order, and lock files differ.
            self.assertEqual(b['status'], 0, b['stderr'])
            def other(files):
                filtered = {k: v for k, v in files.items()
                            if k not in SETTINGS and not k.startswith('settings-target-')
                            and not k.endswith('settings.json--follow-symlinks')
                            and '.execute-applet.lock' not in k
                            and not k.startswith('.local/state/execute/transactions/')}
                for path, value in filtered.items():
                    if path.endswith('/extensions.json'):
                        try:
                            filtered[path] = json.dumps(json.loads(value), sort_keys=True,
                                                        separators=(',', ':'))
                        except (json.JSONDecodeError, TypeError):
                            pass
                return filtered
            self.assertEqual(other(a['files']), other(b['files']))
        else:
            self.assertEqual(a['files'], b['files'])
            self.assertEqual(a['status'], b['status'])
            self.assertEqual(a['stderr'], b['stderr'])

    def transaction_journals(self, files):
        journals = []
        for path, contents in files.items():
            if not (path.startswith('.local/state/execute/transactions/')
                    and path.endswith('/journal.json')):
                continue
            envelope = json.loads(contents)
            data = envelope['data']
            canonical = json.dumps(data, separators=(',', ':'), ensure_ascii=False).encode()
            self.assertEqual(hashlib.sha256(canonical).hexdigest(), envelope['checksum'])
            self.assertEqual(data['state'], 'committed')
            journals.append(data)
        return journals

    def test_absent_editors_and_skip_order(self):
        self.assert_pair(self.pair('absent'))
        pair = self.pair('skip-code', {'skips': ['skip-vscode-theme-changes']})
        self.assert_pair(pair)
        self.assertNotIn(['omarchy-cmd-present', 'code'], pair['execute']['calls'])
        pair = self.pair('skip-all', {'skips': ['skip-vscode-theme-changes', 'skip-vscode-insiders-theme-changes', 'skip-codium-theme-changes', 'skip-cursor-theme-changes']})
        self.assertEqual(pair['bash']['status'], 1)
        self.assertEqual(pair['execute']['status'], 1)
        self.assertEqual(pair['bash']['calls'], pair['execute']['calls'])
        self.assertIn('exit status: 1', pair['execute']['stderr'])

    def test_descriptor_install_mapping_and_jsonc(self):
        settings = '{\n  // Preserve this comment; the key "workbench.colorTheme" here is inactive\n  "nested": {"workbench.colorTheme": "Nested",},\n  "workbench.colorTheme" : "Old",\n  "editor.fontSize": 14, // trailing comma legal in JSONC\n}\n'
        for suffix, descriptor, installed in [('install', {'name': 'A & B | Day', 'extension': 'publisher.theme'}, ''), ('listed', {'name': 'Night', 'extension': 'pub.Night'}, 'other.theme\npub.Night\n'), ('exact-match', {'name': 'Night', 'extension': 'pub.night'}, 'prefix.pub.night\n')]:
            pair = self.pair('descriptor-' + suffix, {'present': EDITORS[:3], 'installed': installed}, descriptor=descriptor, generated={'type': 'light'}, settings=settings)
            self.assert_pair(pair, settings_changed=True)
            for path in SETTINGS[:3]:
                self.assertIn('// Preserve this comment', pair['execute']['files'][path])
                self.assertIn('"' + descriptor['name'] + '"', pair['execute']['files'][path])
                self.assertIn('"nested": {"workbench.colorTheme": "Nested",}', pair['execute']['files'][path])
            self.assertFalse(any(k.endswith('package.json') for k in pair['execute']['files']))
            self.assertEqual(sum(c[1:2] == ['--install-extension'] for c in pair['execute']['calls']), 0 if suffix == 'listed' else 3)

    def test_jsonc_theme_names_preserve_valid_json_strings(self):
        labels = ['', 'bad"quote', 'bad\\slash', 'bad\ttab', 'bad\nline', 'trailing newline\n']
        for i, label in enumerate(labels):
            pair = self.pair('theme-label-' + str(i), {'present': ['code']},
                             descriptor={'name': label, 'extension': 'pub.theme'})
            if label:
                # JSONC stores the theme name as a correctly escaped JSON
                # string, including quotes, backslashes, and control chars.
                self.assert_pair(pair, settings_changed=True)
                self.assertIn(json.dumps(label, ensure_ascii=False),
                              pair['execute']['files'][SETTINGS[0]])
            else:
                self.assert_pair(pair)
        for i, extension in enumerate(['', 'bad id', ';touch FORBIDDEN', '$(touch FORBIDDEN)', '--flag', 'pub.good_1-theme']):
            pair = self.pair('extension-' + str(i), {'present': ['code']}, descriptor={'name': 'Valid', 'extension': extension})
            self.assert_pair(pair, settings_changed=True)
            calls = pair['execute']['calls']
            installs = [c for c in calls if c[1:2] == ['--install-extension']]
            self.assertEqual(len(installs), 1 if extension in ['--flag', 'pub.good_1-theme'] else 0)

    def test_non_string_theme_name_is_reported_and_routes_as_empty(self):
        pair = self.pair('non-string-theme-name', {'present': ['code']},
                         descriptor={'name': None, 'extension': 'pub.theme'},
                         settings='{"workbench.colorTheme":"Old"}\n')
        self.assertEqual(pair['execute']['status'], 0, pair['execute']['stderr'])
        self.assertIn('JSON property must be a string', pair['execute']['stderr'])
        self.assertNotIn('workbench.colorTheme', pair['execute']['files'][SETTINGS[0]])
        self.assertIn(['code', '--install-extension', 'pub.theme'], pair['execute']['calls'])

    def test_insertion_removal_and_symlink_preserve_jsonc(self):
        cases = [
            ({'name': 'New', 'extension': ''},
             '{\n // "workbench.colorTheme": "comment only"\n "editor.x": true,\n}\n',
             '// "workbench.colorTheme": "comment only"', 2),
            (None, '{\n // keep\n "workbench.colorTheme": "Old",\n "editor.x": true,\n}\n',
             '// keep', 0),
        ]
        for i, (descriptor, settings, comment, active_key_mentions) in enumerate(cases):
            pair = self.pair('jsonc-' + str(i), {'present': ['code']}, descriptor=descriptor, settings=settings, symlink=True)
            self.assert_pair(pair, settings_changed=True)
            files = pair['execute']['files']
            self.assertIn('link', files[SETTINGS[0]])
            self.assertIn(comment, files['settings-target-0'])
            self.assertEqual(files['settings-target-0'].count('"workbench.colorTheme"'), active_key_mentions)

    def test_jsonc_root_duplicates_comment_only_and_nested_keys(self):
        source = ('{\n  // "workbench.colorTheme": "comment"\n'
                  '  "nested": {"workbench.colorTheme": "nested",},\n'
                  '  "workbench.colorTheme": "first",\n'
                  '  "workbench.colorTheme": "last",\n}\n')
        pair = self.pair('jsonc-duplicate-set', {'present': ['code']},
                         descriptor={'name': 'Selected', 'extension': ''}, settings=source)
        self.assert_pair(pair, settings_changed=True)
        updated = pair['execute']['files'][SETTINGS[0]]
        self.assertIn('// "workbench.colorTheme": "comment"', updated)
        self.assertIn('"nested": {"workbench.colorTheme": "nested",}', updated)
        self.assertIn('"workbench.colorTheme": "first"', updated)
        self.assertIn('"workbench.colorTheme": "Selected"', updated)
        self.assertNotIn('"workbench.colorTheme": "last"', updated)

        pair = self.pair('jsonc-duplicate-remove', {'present': ['code']}, settings=source)
        self.assert_pair(pair, settings_changed=True)
        updated = pair['execute']['files'][SETTINGS[0]]
        self.assertIn('// "workbench.colorTheme": "comment"', updated)
        self.assertIn('"nested": {"workbench.colorTheme": "nested",}', updated)
        self.assertNotIn('"workbench.colorTheme": "first"', updated)
        self.assertNotIn('"workbench.colorTheme": "last"', updated)

    def test_malformed_jsonc_is_reported_and_left_unchanged(self):
        malformed = '{\n  // preserve\n  unquoted: "bad",\n}\n'
        pair = self.pair('malformed-jsonc', {'present': ['code']},
                         descriptor={'name': 'Safe', 'extension': 'pub.safe'}, settings=malformed)
        self.assertEqual(pair['execute']['status'], 0, pair['execute']['stderr'])
        self.assertEqual(pair['execute']['files'][SETTINGS[0]], malformed)
        self.assertIn('JSONC theme setting update', pair['execute']['stderr'])
        self.assertIn(['code', '--install-extension', 'pub.safe'], pair['execute']['calls'])

    def test_ignored_arguments_and_generated_light_lf_normalization(self):
        pair = self.pair('ignored-args-light', {'present': ['code']}, generated={'type': 'light\n'},
                         arguments=('ignored', '$(touch FORBIDDEN)'))
        self.assert_pair(pair, settings_changed=True)
        package = json.loads(pair['execute']['files'][EXTENSIONS[0] + '/omarchy-theme/package.json'])
        self.assertEqual(package['contributes']['themes'][0]['uiTheme'], 'vs')

    def test_generated_packages_registration_obsolete_and_types(self):
        for i, kind in enumerate(['light', 'dark', 'unexpected', None]):
            obsolete = {'local.omarchy-theme-1.0.0': True} if i == 0 else {'local.omarchy-theme-1.0.0': True, 'keep.extension-2': True}
            old = [{'identifier': {'id': 'local.omarchy-theme'}, 'version': '0'}, {'identifier': {'id': 'keep.extension'}, 'version': '2'}]
            pair = self.pair('generated-' + str(i), {'present': EDITORS}, generated={} if kind is None else {'type': kind}, registry=old, obsolete=obsolete)
            self.assert_pair(pair, settings_changed=True)
            for setting_path in SETTINGS:
                self.assertIn('\"workbench.colorTheme\": \"Omarchy\"', pair['execute']['files'][setting_path])
            for extension in EXTENSIONS:
                files = pair['execute']['files']
                package = json.loads(files[extension + '/omarchy-theme/package.json'])
                self.assertEqual(package['contributes']['themes'][0]['uiTheme'], 'vs' if kind == 'light' else 'vs-dark')
                self.assertEqual(len(json.loads(files[extension + '/extensions.json'])), 2)
                registry = json.loads(files[extension + '/extensions.json'])
                self.assertEqual([entry['identifier']['id'] for entry in registry],
                                 ['keep.extension', 'local.omarchy-theme'])
                self.assertEqual(registry[-1]['location'], {
                    '$mid': 1,
                    'fsPath': '$HOME' + '/' + extension + '/omarchy-theme',
                    'external': 'file://$HOME' + '/' + extension + '/omarchy-theme',
                    'path': '$HOME' + '/' + extension + '/omarchy-theme',
                    'scheme': 'file',
                })
                self.assertEqual(files[extension + '/omarchy-theme/themes/omarchy-color-theme.json']['link'], '$HOME/.local/state/omarchy/current/theme/vscode-theme.json')
                if i == 0:
                    self.assertNotIn(extension + '/.obsolete', files)
                else:
                    self.assertEqual(json.loads(files[extension + '/.obsolete']), {'keep.extension-2': True})
            if i == 0:
                journals = self.transaction_journals(pair['execute']['files'])
                journal_entries = [entry for journal in journals for entry in journal['entries']]
                self.assertEqual(len(journals), 4)
                self.assertEqual(len(journal_entries), 4)
                self.assertTrue(all(not entry['after_exists'] and entry['state'] == 'applied'
                                    for entry in journal_entries))
                suffixes = {
                    next((extension + '/.obsolete' for extension in EXTENSIONS
                          if bytes(entry['path']).decode('utf-8').endswith('/' + extension + '/.obsolete')), None)
                    for entry in journal_entries
                }
                self.assertEqual(suffixes, {extension + '/.obsolete' for extension in EXTENSIONS})

    def test_empty_obsolete_symlink_is_unlinked_without_following_target(self):
        obsolete = {'local.omarchy-theme-1.0.0': True}
        pair = self.pair('obsolete-symlink', {'present': ['code']}, generated={}, obsolete=obsolete,
                         obsolete_symlink=True)
        self.assert_pair(pair, settings_changed=True)
        self.assertNotIn(EXTENSIONS[0] + '/.obsolete', pair['bash']['files'])
        self.assertNotIn(EXTENSIONS[0] + '/.obsolete', pair['execute']['files'])
        self.assertEqual(json.loads(pair['execute']['files']['obsolete-target-0']), obsolete)
        self.assertEqual(pair['execute']['files']['obsolete-target-0'], pair['bash']['files']['obsolete-target-0'])

    def test_nonempty_obsolete_symlink_is_atomically_replaced(self):
        obsolete = {'local.omarchy-theme-1.0.0': True, 'keep.extension-2': True}
        pair = self.pair('obsolete-symlink-remaining', {'present': ['code']}, generated={}, obsolete=obsolete,
                         obsolete_symlink=True)
        self.assert_pair(pair, settings_changed=True)
        for side in ['bash', 'execute']:
            files = pair[side]['files']
            self.assertEqual(json.loads(files[EXTENSIONS[0] + '/.obsolete']), {'keep.extension-2': True})
            self.assertEqual(json.loads(files['obsolete-target-0']), obsolete)

    def test_registry_snapshot_conflict_preserves_concurrent_writer(self):
        old_registry = [
            {'identifier': {'id': 'local.omarchy-theme'}, 'version': '0'},
            {'identifier': {'id': 'keep.extension'}, 'version': '2'},
        ]
        pair = self.pair('registry-cas-conflict', {'present': ['code']}, generated={},
                         registry=old_registry, registry_race=True)
        self.assertEqual(pair['execute']['status'], 0, pair['execute']['stderr'])
        self.assertEqual(pair['execute']['calls'], pair['bash']['calls'])
        self.assertIn('extension registry update', pair['execute']['stderr'])
        bash_registry = json.loads(pair['bash']['files'][EXTENSIONS[0] + '/extensions.json'])
        execute_registry = json.loads(pair['execute']['files'][EXTENSIONS[0] + '/extensions.json'])
        self.assertEqual([entry['identifier']['id'] for entry in bash_registry],
                         ['keep.extension', 'local.omarchy-theme'])
        self.assertEqual(execute_registry, [
            {'identifier': {'id': 'concurrent.extension'}, 'version': '9'},
        ])

    def test_missing_generated_registry_creation_and_cursor_mapping(self):
        pair = self.pair('generated-new-registry', {'present': ['code']}, generated={})
        self.assert_pair(pair, settings_changed=True)
        self.assertEqual(len(json.loads(pair['execute']['files'][EXTENSIONS[0] + '/extensions.json'])), 1)
        pair = self.pair('cursor-explicit-map', {'present': ['cursor-mock']}, descriptor={'name': 'Cursor Theme', 'extension': 'pub.cursor'}, cursor_mapping=True)
        self.assertEqual(pair['execute']['status'], 0, pair['execute']['stderr'])
        self.assertIn(['cursor-mock', '--install-extension', 'pub.cursor'], pair['execute']['calls'])
        self.assertIn('Cursor Theme', pair['execute']['files'][SETTINGS[3]])
        self.assertNotIn(SETTINGS[3], pair['bash']['files'])

    def test_malformed_json_and_registries_preserve_unrelated_files(self):
        pair = self.pair('malformed-descriptor', {'present': ['code']}, descriptor='{broken json', generated={'type': 'light'})
        self.assertEqual(pair['execute']['status'], pair['bash']['status'])
        self.assertEqual(pair['execute']['calls'], pair['bash']['calls'])
        self.assertEqual(pair['execute']['files'], pair['bash']['files'])
        self.assertEqual(pair['execute']['stdout'], pair['bash']['stdout'])
        self.assertIn('theme descriptor JSON parse', pair['execute']['stderr'])
        self.assertFalse(any(k.endswith('package.json') for k in pair['execute']['files']))
        pair = self.pair('malformed-registries', {'present': ['code']}, generated={}, registry='{broken', obsolete='{broken')
        # JSON parse failure is classified in the program; `.obsolete` remains
        # jq-edited and keeps its native parse diagnostic.
        self.assertEqual(pair['execute']['status'], 0)
        self.assertEqual(pair['execute']['calls'], pair['bash']['calls'])
        for filename in ['extensions.json', '.obsolete']:
            key = EXTENSIONS[0] + '/' + filename
            self.assertEqual(pair['execute']['files'][key], '{broken')
            self.assertEqual(pair['execute']['files'][key], pair['bash']['files'][key])
        self.assertEqual(pair['execute']['stderr'].count('parse error'), 1)
        self.assertIn('extension registry JSON parse', pair['execute']['stderr'])
        self.assertEqual(pair['bash']['stderr'].count('parse error'), 2)

    def test_list_failure_and_suppressed_diagnostic_gap(self):
        pair = self.pair('failed-list', {'present': ['code'], 'list_status': 9, 'list_stderr': 'list failed\n'}, descriptor={'name': '', 'extension': 'pub.theme'})
        self.assertEqual(pair['execute']['status'], 0)
        self.assertEqual(pair['execute']['calls'], pair['bash']['calls'])
        self.assertEqual(pair['execute']['files'], pair['bash']['files'])
        self.assertIn('installed extension listing (installation attempted)', pair['execute']['stderr'])
        self.assertEqual(pair['bash']['stderr'], '')
        self.assertIn(['code', '--install-extension', 'pub.theme'], pair['execute']['calls'])

    def test_editor_presence_error_is_reported_and_editor_is_skipped(self):
        pair = self.pair('presence-error', {'present_status': {'code': 8}},
                         descriptor={'name': 'Theme', 'extension': 'pub.theme'})
        self.assertEqual(pair['execute']['status'], 0, pair['execute']['stderr'])
        self.assertNotIn(['code', '--list-extensions'], pair['execute']['calls'])
        self.assertNotIn(['code', '--install-extension', 'pub.theme'], pair['execute']['calls'])
        self.assertIn('editor presence check', pair['execute']['stderr'])

    def test_install_failure_is_reported_but_theme_setting_continues(self):
        pair = self.pair('failed-install', {'present': ['code'], 'install_status': 7, 'install_stderr': 'editor failed\n', 'install_stdout': 'discarded\n'}, descriptor={'name': 'Still written', 'extension': 'pub.theme'})
        self.assertEqual(pair['execute']['status'], 0, pair['execute']['stderr'])
        self.assertIn('Still written', pair['execute']['files'][SETTINGS[0]])
        self.assertEqual(pair['execute']['stdout'], '')
        self.assertIn('editor extension installation', pair['execute']['stderr'])
        self.assertNotIn('editor failed', pair['execute']['stderr'])
        self.assertEqual(pair['execute']['calls'], pair['bash']['calls'])


if __name__ == '__main__':
    unittest.main()
