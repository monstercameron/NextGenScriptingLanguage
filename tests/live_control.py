#!/usr/bin/env python3
"""Exercise the real CLI/socket and native runtime in owned temporary workspaces."""
import concurrent.futures
import errno
import fcntl
import hashlib
import pty
import struct
import termios
import threading
import json
import os
from pathlib import Path
import select
import signal
import socket
import stat
import subprocess
import sys
import tempfile
import time
import unittest

BINARY = Path(sys.argv.pop(1)).resolve() if len(sys.argv) > 1 and not sys.argv[1].startswith('-') else Path(__file__).resolve().parents[1] / 'bin/execute'
PROJECT = Path(__file__).resolve().parents[1]


class Session:
    def __init__(self, source, bindings=None, paused=True, manifests=None, headless=False, agent_policy=None):
        self.temp = tempfile.TemporaryDirectory(prefix='execute-live-')
        self.root = Path(self.temp.name)
        self.socket = self.root / 'control.sock'
        self.file = self.root / 'workflow.program'
        self.file.write_text(source)
        self.manifests = self.root / 'manifests'
        self.manifests.mkdir()
        for path in (PROJECT / 'manifests').glob('*.json'):
            (self.manifests / path.name).write_bytes(path.read_bytes())
        for name, manifest in (manifests or {}).items():
            (self.manifests / (name + '.json')).write_text(json.dumps(manifest))
        args = [str(BINARY), 'live', 'serve', str(self.file), '--socket', str(self.socket), '--manifest-dir', str(self.manifests)]
        if headless:
            args += ['--headless']
        if paused:
            args += ['--paused']
        if bindings is not None:
            args += ['--bindings', json.dumps(bindings)]
        if agent_policy is not None:
            self.policy = self.root / 'agents.json'
            self.policy.write_text(json.dumps(agent_policy))
            self.policy.chmod(0o600)
            args += ['--agent-policy', str(self.policy)]
        self.child = subprocess.Popen(args, cwd=self.root, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        if not select.select([self.child.stdout], [], [], 8)[0]:
            self.close()
            raise AssertionError('live server did not become ready')
        line = self.child.stdout.readline()
        if not line or not json.loads(line).get('event') == 'ready':
            self.close()
            raise AssertionError(f'bad ready response: {line!r}')

    def call(self, op, auth=None, **values):
        with socket.socket(socket.AF_UNIX) as client:
            client.settimeout(12)
            client.connect(str(self.socket))
            request = {'op': op, **values}
            if auth is not None:
                request['auth'] = auth
            client.sendall(json.dumps(request).encode() + b'\n')
            with client.makefile('rb') as reader:
                result = json.loads(reader.readline(16 * 1024 * 1024))
        return result

    def ok(self, op, **values):
        result = self.call(op, **values)
        assert result['ok'], result
        return result

    def read(self):
        return self.ok('read')['result']

    def until(self, predicate, timeout=6):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            response = self.ok('read')
            if predicate(response['result']):
                return response['result']
            self.ok('wait', after=response['revision'], timeout_ms=50)
        raise AssertionError(f'condition timed out: {self.read()}')

    def action(self, action):
        return self.ok('control', action=action)

    def close(self):
        if hasattr(self, 'child'):
            if self.child.poll() is None:
                try:
                    self.call('control', action='stop')
                    self.child.wait(timeout=5)
                except (OSError, ValueError, subprocess.TimeoutExpired):
                    self.child.terminate()
                    try:
                        self.child.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        self.child.kill()
                        self.child.wait(timeout=2)
            self.child.stdout.close()
            self.child.stderr.close()
        self.temp.cleanup()

    def __enter__(self):
        return self

    def __exit__(self, *_):
        self.close()


class LiveControl(unittest.TestCase):
    @staticmethod
    def fill_state(app, headroom=1024):
        # Match the compact UTF-8 JSON document budget, not the transport envelope.
        for index in range(9):
            size = len(json.dumps(app.read()['state'], ensure_ascii=False, separators=(',', ':')).encode())
            remaining = 8 * 1024 * 1024 - headroom - size
            if remaining <= 32:
                break
            app.ok('create', path=f'/user/padding{index}', value='x' * min(950000, remaining - 32))
        return len(json.dumps(app.read()['state'], ensure_ascii=False, separators=(',', ':')).encode())

    def test_policy_edits_cannot_test_secret_bindings_or_their_ancestors(self):
        secret = 'low-entropy-secret-guess'
        editor_token = 'e' * 64
        reader_token = 'r' * 64
        policy = {
            'version': 1,
            'agents': {
                'editor': {
                    'token_sha256': hashlib.sha256(editor_token.encode()).hexdigest(),
                    'permissions': ['edit'],
                },
                'reader_editor': {
                    'token_sha256': hashlib.sha256(reader_token.encode()).hexdigest(),
                    'permissions': ['read', 'edit', 'control'],
                },
            },
        }
        source = 'accept token as text from secret prompt "Token?"\nuse value token\n'
        with Session(source, {'token': secret}, agent_policy=policy) as app:
            credentials = {
                'editor': {'agent': 'editor', 'token': editor_token},
                'reader_editor': {'agent': 'reader_editor', 'token': reader_token},
            }
            self.assertFalse(app.call('read', auth=credentials['editor'])['ok'])
            visible = app.ok('read', auth=credentials['reader_editor'])
            self.assertEqual(visible['result']['state']['bindings']['token'], {'withheld': 'secret'})
            self.assertNotIn(secret, json.dumps(visible))

            attempts = [
                {'op': 'test', 'path': '/bindings/token', 'value': secret},
                {'op': 'test', 'path': '/bindings/token', 'value': 'incorrect-guess'},
                {'op': 'test', 'path': '/bindings', 'value': {'token': secret}},
                {'op': 'test', 'path': '/bindings/token/0', 'value': secret[0]},
                {'op': 'test', 'path': '/input', 'value': {'text': ''}},
                {'op': 'test', 'path': '/input', 'value': {'text': 'incorrect-guess'}},
                {'op': 'test', 'path': '/input/text', 'value': ''},
                {'op': 'test', 'path': '/input/text', 'value': 'incorrect-guess'},
            ]
            for agent, auth in credentials.items():
                failures = [app.call('patch', auth=auth, operations=[operation]) for operation in attempts]
                self.assertTrue(all(not result['ok'] for result in failures), (agent, failures))
                self.assertEqual(
                    [result['error'] for result in failures],
                    [{'code': 'secret_test_denied', 'message': 'patch test cannot target redacted state'}] * len(attempts),
                )
                self.assertTrue(all(secret not in json.dumps(result) for result in failures))

            # A rejected secret test rolls back earlier operations in its batch.
            rollback = app.call('patch', auth=credentials['editor'], operations=[
                {'op': 'add', 'path': '/user/should_rollback', 'value': True},
                attempts[0],
            ])
            self.assertEqual(rollback['error']['code'], 'secret_test_denied')
            self.assertFalse(app.ok('read', auth=credentials['reader_editor'])['result']['state']['user'].get('should_rollback', False))

            # Ordinary tests and redacted read-modify-write remain supported.
            app.ok('patch', auth=credentials['editor'], operations=[
                {'op': 'add', 'path': '/user/public', 'value': 'known'},
                {'op': 'test', 'path': '/user/public', 'value': 'known'},
            ])
            app.ok('patch', auth=credentials['reader_editor'], operations=[
                {'op': 'replace', 'path': '/bindings/token', 'value': {'withheld': 'secret'}},
            ])
            after = app.ok('read', auth=credentials['reader_editor'])
            self.assertEqual(after['result']['state']['bindings']['token'], {'withheld': 'secret'})
            self.assertNotIn(secret, json.dumps(after))

            # A public source path remains testable when it contains no
            # redacted text, even though the program has another secret field.
            app.ok('patch', auth=credentials['editor'], operations=[
                {'op': 'test', 'path': '/source', 'value': source},
            ])

        # Secret prompt defaults are redacted from source reads. Exact and
        # mismatching source guesses must receive the same fixed rejection.
        source_with_default = (
            'accept token as text from secret prompt "Token?" '
            f'with default "{secret}"\nuse value token\n'
        )
        with Session(source_with_default, {'token': secret}, agent_policy=policy) as app:
            visible = app.ok('read', auth=credentials['reader_editor'])
            self.assertNotIn(secret, json.dumps(visible))
            self.assertIn('[REDACTED]', visible['result']['state']['source'])
            guesses = [source_with_default, source_with_default.replace(secret, 'incorrect-guess')]
            for auth in credentials.values():
                failures = [app.call('patch', auth=auth, operations=[
                    {'op': 'test', 'path': '/source', 'value': guess},
                ]) for guess in guesses]
                self.assertEqual(
                    [result['error'] for result in failures],
                    [{'code': 'secret_test_denied', 'message': 'patch test cannot target redacted state'}] * len(guesses),
                )
                self.assertTrue(all(secret not in json.dumps(result) for result in failures))

        # Exercise a nonempty live input produced by a secret pipeline, rather
        # than only the empty input record present when a session starts.
        pipeline_source = (
            'accept token as text from secret prompt "Token?"\n'
            'use value token\n'
            'then "cat" as echoed\n'
            'run printf with message "finished"\n'
        )
        with Session(pipeline_source, {'token': secret}, agent_policy=policy) as app:
            runner = credentials['reader_editor']
            app.ok('control', auth=runner, action='resume')
            deadline = time.monotonic() + 8
            snapshot = None
            while time.monotonic() < deadline:
                snapshot = app.ok('read', auth=runner)['result']
                if snapshot['runtime']['status'] == 'completed':
                    break
                app.ok('wait', auth=runner, after=snapshot['revision'], timeout_ms=50)
            self.assertEqual(snapshot['runtime']['status'], 'completed', snapshot)
            self.assertEqual(snapshot['state']['input'], {'withheld': 'secret'})
            self.assertNotIn(secret, json.dumps(snapshot))

            guesses = [
                {'op': 'test', 'path': '/input', 'value': {'text': secret}},
                {'op': 'test', 'path': '/input', 'value': {'text': 'incorrect-guess'}},
                {'op': 'test', 'path': '/input/text', 'value': secret},
                {'op': 'test', 'path': '/input/text', 'value': 'incorrect-guess'},
            ]
            failures = [app.call('patch', auth=runner, operations=[guess]) for guess in guesses]
            self.assertEqual(
                [result['error'] for result in failures],
                [{'code': 'secret_test_denied', 'message': 'patch test cannot target redacted state'}] * len(guesses),
            )
            self.assertTrue(all(secret not in json.dumps(result) for result in failures))

    def test_stream_rejects_record_that_would_exceed_document_budget(self):
        source = 'stream from "printf" with arguments ["%s", "' + 'r' * 32768 + '"]\nshow each line\n'
        with Session(source) as app:
            before = self.fill_state(app)
            app.action('resume')
            state = app.until(lambda s: s['runtime']['status'] in ('failed', 'completed'))
            self.assertEqual(state['runtime']['status'], 'failed')
            self.assertIn('state exceeds 8 MiB', state['runtime']['error'])
            self.assertEqual(state['state']['input'], {'text': ''})
            self.assertEqual(state['runtime']['last_output'], {'text': ''})
            self.assertEqual(len(json.dumps(state['state'], ensure_ascii=False, separators=(',', ':')).encode()), before)

    def test_listener_binding_budget_failure_preserves_queued_record_before_effects(self):
        source = 'listen to stdin\nwhenever a line arrives as event\n  then "printf" with arguments ["effect"]\n'
        with Session(source) as app:
            record = {'text': 'r' * 32768}
            app.ok('send', value=record)
            before = self.fill_state(app)
            app.action('resume')
            state = app.until(lambda s: s['runtime']['status'] == 'failed' or s['runtime']['runs_completed'] == 1)
            self.assertEqual(state['runtime']['status'], 'failed')
            self.assertIn('state exceeds 8 MiB', state['runtime']['error'])
            self.assertEqual(state['state']['inbox'], [record])
            self.assertNotIn('event', state['state']['bindings'])
            self.assertEqual(state['state']['input'], {'text': ''})
            self.assertEqual(state['runtime']['completed_native_launches'], 0)
            self.assertEqual(len(json.dumps(state['state'], ensure_ascii=False, separators=(',', ':')).encode()), before)
            app.ok('delete', path='/user/padding0')
            app.action('resume')
            repaired = app.until(lambda s: s['runtime']['runs_completed'] == 1)
            self.assertEqual(repaired['state']['inbox'], [])
            self.assertEqual(repaired['runtime']['last_output'], {'text': 'effect'})
            self.assertEqual(repaired['runtime']['completed_native_launches'], 1)

    def test_ui_publication_capacity_failure_preserves_document_and_prior_widget(self):
        payload = 'r' * 32768
        source = ('open a screen titled "Capacity"\nplace panel on the left\ndefine a program named panel\n'
                  '  show a button named load labelled "Load"\n  show a text panel named result with text "before"\n'
                  '  whenever load is pressed\n    run "printf" with arguments ["%s", "' + payload + '"]\n'
                  '    then show the result in result\n')
        with Session(source, headless=True) as app:
            app.until(lambda s: bool(s['runtime']['frame']))
            self.fill_state(app)
            app.ok('event', value={'type': 'press', 'program': 'panel', 'widget': 'load'})
            app.action('resume')
            state = app.until(lambda s: s['runtime']['status'] == 'failed' and not s['runtime']['in_flight'])
            self.assertIn('UI state exceeds 8 MiB', state['runtime']['error'])
            self.assertTrue(state['runtime']['effects_may_be_incomplete'])
            self.assertEqual(state['state']['ui']['programs']['panel']['widgets']['result']['text'], 'before')
            self.assertLessEqual(len(json.dumps(state['state'], ensure_ascii=False, separators=(',', ':')).encode()), 8 * 1024 * 1024)
            self.assertEqual(len(state['state']['user']['padding0']), 950000)

    def test_waiting_long_interval_wakes_for_step_and_source_replacement(self):
        source = 'every 30 seconds\n  run "printf" with arguments ["old"]\n'
        with Session(source, paused=False) as app:
            app.until(lambda s: s['runtime']['status'] == 'waiting' and not s['runtime']['in_flight'])
            app.action('step')
            first = app.until(lambda s: s['runtime']['runs_completed'] == 1 and not s['runtime']['running'])
            self.assertEqual(first['runtime']['last_output'], {'text': 'old'})
            app.action('resume')
            app.until(lambda s: s['runtime']['status'] == 'waiting')
            app.ok('update', path='/source', value='every 2 milliseconds\n  run "printf" with arguments ["new"]\n')
            app.until(lambda s: s['runtime']['last_output'] == {'text': 'new'})
            app.action('pause')
            app.until(lambda s: s['runtime']['status'] == 'paused' and not s['runtime']['in_flight'])

    def test_short_native_interval_runs_serially_and_can_pause_and_resume(self):
        source = 'every 2 milliseconds\n  run "printf" with arguments ["tick"]\n'
        with Session(source) as app:
            app.action('resume')
            app.until(lambda s: s['runtime']['runs_completed'] >= 4)
            app.action('pause')
            first = app.until(lambda s: s['runtime']['status'] == 'paused' and not s['runtime']['in_flight'])
            self.assertEqual(first['runtime']['last_output'], {'text': 'tick'})
            self.assertGreaterEqual(first['runtime']['completed_native_launches'], 4)
            self.assertIsNone(first['runtime']['active_node'])
            previous = first['runtime']['runs_completed']
            app.action('resume')
            app.until(lambda s: s['runtime']['runs_completed'] > previous)
            app.action('pause')
            app.until(lambda s: s['runtime']['status'] == 'paused' and not s['runtime']['in_flight'])

    def test_queue_monitor_example_declares_inputs_and_reacts_to_typed_live_edits(self):
        example = PROJECT / 'examples/live'
        source = (example / 'queue-monitor.program').read_text()
        bindings = json.loads((example / 'queue-state.json').read_text())['bindings']
        with Session(source, bindings=bindings) as app:
            (app.root / 'queue.json').write_bytes((example / 'queue.json').read_bytes())
            app.action('resume')
            app.until(lambda s: s['runtime']['runs_completed'] >= 1)
            app.action('pause')
            app.until(lambda s: s['runtime']['status'] == 'paused' and not s['runtime']['in_flight'])
            first = json.loads((app.root / 'queue-report.json').read_text())
            self.assertEqual(first['overdue'], 1)
            self.assertEqual([job['id'] for job in first['jobs']], ['job-102'])
            self.assertFalse(app.call('update', path='/bindings/threshold', value=10)['ok'])
            app.ok('update', path='/bindings/threshold', value='10')
            previous_runs = app.read()['runtime']['runs_completed']
            app.action('resume')
            app.until(lambda s: s['runtime']['runs_completed'] > previous_runs)
            app.action('pause')
            app.until(lambda s: s['runtime']['status'] == 'paused' and not s['runtime']['in_flight'])
            second = json.loads((app.root / 'queue-report.json').read_text())
            self.assertEqual(second['overdue'], 2)
            self.assertEqual([job['id'] for job in second['jobs']], ['job-101', 'job-102'])

    def test_declared_defaults_are_real_editable_live_bindings(self):
        source = 'accept message as text with default "default"\nrun "printf" with arguments ["%s", message] as rendered\n'
        with Session(source) as app:
            self.assertEqual(app.read()['state']['bindings']['message'], 'default')
            app.ok('update', path='/bindings/message', value='edited input')
            app.action('resume')
            state = app.until(lambda s: s['runtime']['status'] == 'completed')
            self.assertEqual(state['state']['bindings']['rendered'], 'edited input')

    def test_declared_input_types_reject_live_edits_atomically(self):
        source = 'accept count as integer with default 1\naccept enabled as boolean with default false\naccept args as list of text with default ["%s", "original"]\nrun "printf" with arguments args\n'
        with Session(source) as app:
            for name, value in [('count', '2'), ('count', 1.5), ('count', True), ('enabled', 'true'), ('args', [1]), ('args', [['nested']])]:
                revision = app.ok('read')['revision']
                self.assertFalse(app.call('update', path=f'/bindings/{name}', value=value)['ok'])
                self.assertEqual(app.ok('read')['revision'], revision)
            self.assertFalse(app.call('delete', path='/bindings/count')['ok'])
            app.ok('update', path='/bindings/args', value=['%s', 'literal $HOME; *'])
            app.action('resume')
            state = app.until(lambda s: s['runtime']['status'] == 'completed')
            self.assertEqual(state['runtime']['last_output'], {'text': 'literal $HOME; *'})

    def test_declared_integer_input_survives_listener_cycles_and_live_edits(self):
        source = 'accept count as integer with default 1\nlisten to stdin\nwhenever a line arrives as event\n  run "printf" with arguments ["%s:%s", count, event]\n'
        with Session(source, paused=False) as app:
            app.ok('send', value={'text': 'first\n'})
            app.until(lambda s: s['runtime']['runs_completed'] == 1)
            app.ok('update', path='/bindings/count', value=2)
            app.ok('send', value={'text': 'second\n'})
            state = app.until(lambda s: s['runtime']['runs_completed'] == 2)
            self.assertEqual(state['runtime']['last_output'], {'text': '2:second\n'})
            self.assertEqual(state['state']['bindings']['count'], 2)

    def test_declared_list_input_supplies_a_managed_stream_source(self):
        source = 'accept args as list of text with default ["%s", "original"]\nstream from "printf" with arguments args\nshow each line\n'
        with Session(source) as app:
            app.ok('update', path='/bindings/args', value=['%s', 'changed stream input'])
            app.action('resume')
            state = app.until(lambda s: s['runtime']['status'] == 'completed')
            self.assertEqual(state['runtime']['last_output'], {'text': 'changed stream input'})
            self.assertEqual(state['runtime']['completed_native_launches'], 1)

    def test_direct_executable_bindings_drive_real_managed_execution(self):
        with Session('run "printf" with arguments ["%s", greeting] as rendered\n', bindings={'greeting': 'initial'}) as app:
            app.ok('update', path='/bindings/greeting', value='changed through live state')
            app.action('resume')
            state = app.until(lambda s: s['runtime']['status'] == 'completed')
            self.assertEqual(state['state']['bindings']['rendered'], 'changed through live state')
            self.assertEqual(state['runtime']['completed_native_launches'], 1)

    def test_direct_listener_source_and_handler_use_generic_native_calls(self):
        source = 'listen to "printf" with arguments ["one\\ntwo\\n"]\nwhenever a line arrives as event\n  then "tr" with arguments ["[:lower:]", "[:upper:]"]\n'
        with Session(source, paused=False) as app:
            state = app.until(lambda s: s['runtime']['status'] == 'completed')
            self.assertEqual(state['runtime']['runs_completed'], 2)
            self.assertEqual(state['runtime']['last_output'], {'text': 'TWO\n'})
            self.assertEqual(state['runtime']['completed_native_launches'], 3)

    def test_direct_tui_handler_uses_edited_widget_value_as_an_argv_item(self):
        source = '''open a screen titled "Direct commands"
place panel on the left
define a program named panel
  show a text field named message with value "initial"
  show a text panel named result
  show a button named send labelled "Send"
  whenever send is pressed
    run "printf" with arguments ["%s", message]
    then show the result in result
'''
        with Session(source, paused=False, headless=True) as app:
            app.ok('update', path='/ui/programs/panel/widgets/message/text', value='literal $HOME; *')
            app.ok('event', value={'type': 'press', 'program': 'panel', 'widget': 'send'})
            state = app.until(lambda s: s['state']['ui']['programs']['panel']['widgets']['result']['text'] == 'literal $HOME; *')
            self.assertEqual(state['runtime']['completed_native_launches'], 1)

    def test_cli_crud_revision_and_changes_feed_native_argv(self):
        source = 'run printf with message greeting as rendered\nthen save to "result.txt"\n'
        with Session(source, {'greeting': 'initial'}) as app:
            def cli(*args, success=True):
                run = subprocess.run([str(BINARY), 'live', *args, '--socket', str(app.socket)], capture_output=True, timeout=5)
                self.assertEqual(run.returncode, 0 if success else 1, run.stderr)
                return json.loads(run.stdout)
            result = cli('create', '/bindings/temporary', '--json', '42')
            revision = result['revision']
            self.assertEqual(cli('read', '/bindings/temporary')['result'], 42)
            cli('update', '/bindings/greeting', '--json', '"changed by agent"', '--if-revision', str(revision))
            conflict = cli('delete', '/bindings/temporary', '--if-revision', str(revision), success=False)
            self.assertEqual(conflict['error']['code'], 'conflict')
            cli('delete', '/bindings/temporary')
            app.action('step')
            state = app.until(lambda s: s['state']['cursor'] == 1 and not s['runtime']['in_flight'])
            self.assertEqual(state['state']['bindings']['rendered'], 'changed by agent\n')
            self.assertFalse((app.root / 'result.txt').exists())
            app.ok('update', path='/input', value={'text': 'edited pipeline bytes\n'})
            app.action('resume')
            app.until(lambda s: s['runtime']['status'] == 'completed')
            self.assertEqual((app.root / 'result.txt').read_bytes(), b'edited pipeline bytes\n')
            self.assertEqual(app.child.poll(), None)
            self.assertEqual(stat.S_IMODE(app.socket.stat().st_mode), 0o600)

    def test_atomic_patch_rollback_and_source_replacement(self):
        original = 'run printf with message "old"\n'
        with Session(original) as app:
            revision = app.ok('read')['revision']
            bad = app.call('patch', operations=[{'op': 'add', 'path': '/user/a', 'value': 1}, {'op': 'replace', 'path': '/source', 'value': 'run missing_binary\n'}])
            self.assertFalse(bad['ok'])
            self.assertEqual(app.ok('read')['revision'], revision)
            self.assertEqual(app.read()['state']['user'], {})
            source = 'run printf with message replacement\nthen save to "changed.txt"\n'
            app.ok('patch', if_revision=revision, operations=[{'op': 'add', 'path': '/bindings/replacement', 'value': 'new'}, {'op': 'replace', 'path': '/source', 'value': source}, {'op': 'test', 'path': '/cursor', 'value': 0}])
            app.action('resume')
            app.until(lambda s: s['runtime']['status'] == 'completed')
            self.assertEqual((app.root / 'changed.txt').read_bytes(), b'new\n')
            self.assertEqual(app.file.read_text(), original)

    def test_delete_required_value_rejected_and_binary_input_preserved(self):
        with Session('then cat\nthen save to "binary.out"\n', paused=True, bindings={'seed': ''}) as app:
            # Seed an explicit input before the first then invocation.
            app.ok('update', path='/input', value={'bytes': [0, 255, 10]})
            before = app.ok('read')['revision']
            self.assertFalse(app.call('delete', path='/cwd')['ok'])
            self.assertEqual(app.ok('read')['revision'], before)
            app.action('resume')
            app.until(lambda s: s['runtime']['status'] == 'completed')
            self.assertEqual((app.root / 'binary.out').read_bytes(), bytes([0, 255, 10]))

    def test_pipeline_group_cursor_and_binding_dependency_validation(self):
        source = 'run printf with message value\npipe to tr with from "a-z" and to "A-Z"\nthen save to "report.txt"\n'
        with Session(source, {'value': 'hello'}) as app:
            self.assertFalse(app.call('update', path='/cursor', value=1)['ok'])
            self.assertFalse(app.call('delete', path='/bindings/value')['ok'])
            app.action('step')
            state = app.until(lambda s: s['state']['cursor'] == 2 and not s['runtime']['in_flight'])
            self.assertEqual(state['state']['input'], {'text': 'HELLO\n'})
            self.assertEqual(state['runtime']['completed_native_launches'], 2)
            app.ok('delete', path='/bindings/value')

    def test_json_pointer_array_crud_nulls_and_concurrent_cas(self):
        with Session('run printf with message "probe"\n') as app:
            app.ok('create', path='/user/a~1b~0c', value=[1, 3])
            app.ok('create', path='/user/a~1b~0c/1', value=2)
            app.ok('delete', path='/user/a~1b~0c/0')
            self.assertEqual(app.ok('read', path='/user/a~1b~0c')['result'], [2, 3])
            app.ok('create', path='/bindings/nothing', value=None)
            self.assertIsNone(app.ok('read', path='/bindings/nothing')['result'])
            response = app.ok('read')
            with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
                responses = list(pool.map(lambda n: app.call('create', path=f'/user/race{n}', value=n, if_revision=response['revision']), [1, 2]))
            self.assertEqual(sum(r['ok'] for r in responses), 1)
            self.assertEqual([r['error']['code'] for r in responses if not r['ok']], ['conflict'])

    def test_cancel_reaps_owned_child_and_requires_explicit_retry(self):
        code = 'import os,time;open("pid","w").write(str(os.getpid()));time.sleep(30)'
        manifest = {'name': 'hold', 'binary': sys.executable, 'arguments': [{'literal': '-c'}, {'literal': code}]}
        with Session('run hold\nthen save to "must-not-exist"\n', manifests={'hold': manifest}) as app:
            app.action('resume')
            deadline = time.monotonic() + 5
            while not (app.root / 'pid').exists() and time.monotonic() < deadline:
                time.sleep(.01)
            pid = int((app.root / 'pid').read_text())
            busy = app.call('update', path='/input', value={'text': 'cannot race'})
            self.assertEqual(busy['error']['code'], 'busy')
            app.action('cancel')
            state = app.until(lambda s: s['runtime']['status'] == 'failed' and not s['runtime']['in_flight'])
            self.assertEqual(state['state']['cursor'], 0)
            self.assertTrue(state['runtime']['effects_may_be_incomplete'])
            with self.assertRaises(ProcessLookupError):
                os.kill(pid, 0)
            self.assertFalse((app.root / 'must-not-exist').exists())
            self.assertEqual(app.call('control', action='resume')['error']['code'], 'uncertain_effects')
            app.ok('update', path='/source', value='run printf with message "repaired"\nthen save to "repaired.txt"\n')
            app.action('retry')
            app.until(lambda s: s['runtime']['status'] == 'completed')
            self.assertEqual((app.root / 'repaired.txt').read_text(), 'repaired\n')

    def test_live_inbox_drives_listener_and_mutable_config(self):
        source = 'listen to stdin\nwhenever a line arrives as event\n  then tr with from from_letters and to to_letters\n'
        with Session(source, {'from_letters': 'a-z', 'to_letters': 'A-Z'}, paused=False) as app:
            app.ok('send', value={'text': 'first\n'})
            app.until(lambda s: s['runtime']['runs_completed'] == 1)
            self.assertEqual(app.read()['runtime']['last_output'], {'text': 'FIRST\n'})
            app.action('pause')
            app.until(lambda s: not s['runtime']['in_flight'] and not s['runtime']['running'])
            app.ok('patch', operations=[{'op': 'replace', 'path': '/bindings/from_letters', 'value': 'A-Z'}, {'op': 'replace', 'path': '/bindings/to_letters', 'value': 'a-z'}])
            app.ok('create', path='/inbox/-', value={'text': 'SECOND\n'})
            app.action('resume')
            state = app.until(lambda s: s['runtime']['runs_completed'] == 2)
            self.assertEqual(state['runtime']['last_output'], {'text': 'second\n'})

    def test_native_listener_producer_runs_once_and_is_reaped(self):
        code = 'import os,time;open("producer.pid","w").write(str(os.getpid()));print("hello",flush=True);time.sleep(30)'
        manifest = {'name': 'feed', 'binary': sys.executable, 'arguments': [{'literal': '-u'}, {'literal': '-c'}, {'literal': code}]}
        source = 'listen to feed\nwhenever a line arrives as event\n  then tr with from "a-z" and to "A-Z"\n'
        with Session(source, paused=False, manifests={'feed': manifest}) as app:
            state = app.until(lambda s: s['runtime']['runs_completed'] == 1)
            self.assertEqual(state['runtime']['last_output'], {'text': 'HELLO\n'})
            pid = int((app.root / 'producer.pid').read_text())
            app.action('stop')
            self.assertEqual(app.child.wait(timeout=5), 0)
            with self.assertRaises(ProcessLookupError):
                os.kill(pid, 0)
            self.assertFalse(app.socket.exists())

    def test_schedules_step_without_installing_os_jobs(self):
        for header in ['every 30 seconds', 'on schedule "* * * * *"']:
            with self.subTest(header=header), Session(header + '\n  run printf with message "snapshot"\n') as app:
                app.action('step')
                state = app.until(lambda s: s['runtime']['runs_completed'] == 1)
                self.assertEqual(state['runtime']['last_output'], {'text': 'snapshot\n'})
                self.assertFalse(state['runtime']['running'])

    def test_persistent_stream_pause_resume_cancel(self):
        code = 'import os,time;open("stream.pid","w").write(str(os.getpid()));print("first",flush=True);time.sleep(.25);print("second",flush=True);time.sleep(30)'
        manifest = {'name': 'feed', 'binary': sys.executable, 'arguments': [{'literal': '-u'}, {'literal': '-c'}, {'literal': code}]}
        source = 'stream from feed\nshow each line\n'
        with Session(source, manifests={'feed': manifest}) as app:
            app.action('step')
            app.until(lambda s: s['runtime']['last_output'] == {'text': 'first\n'})
            app.until(lambda s: s['runtime']['status'] == 'paused')
            app.action('resume')
            app.until(lambda s: s['runtime']['last_output'] == {'text': 'second\n'})
            app.action('cancel')
            app.until(lambda s: s['runtime']['status'] == 'failed' and not s['runtime']['in_flight'])
            pid = int((app.root / 'stream.pid').read_text())
            with self.assertRaises(ProcessLookupError):
                os.kill(pid, 0)

    def test_live_conditionals_are_atomic_nodes_and_reject_interior_cursors(self):
        source = 'run "false" and keep status as code\nif code is 1\n  run "printf" with arguments ["handled"]\notherwise\n  run "execute-never-launched"\n'
        with Session(source) as app:
            app.action('step')
            app.until(lambda s: s['state']['cursor'] == 1 and not s['runtime']['in_flight'])
            before = app.read()['state']
            self.assertFalse(app.call('update', path='/cursor', value=3)['ok'])
            self.assertEqual(app.read()['state'], before)
            app.action('step')
            state = app.until(lambda s: s['runtime']['last_output'] == {'text': 'handled'})
            self.assertEqual(state['runtime']['completed_native_launches'], 2)
            self.assertEqual(state['state']['cursor'], 6)

    def test_loops_are_atomic_live_nodes_with_iteration_local_bindings(self):
        source = ('run "printf" with arguments ["a"] as outer\n'
                  'repeat 2 times\n'
                  '  then "tr" with arguments ["a-z", "b-za"] as local\n'
                  'then "printf" with arguments ["%s", outer]\n')
        with Session(source) as app:
            app.action('step')
            app.until(lambda s: s['state']['cursor'] == 1 and not s['runtime']['in_flight'])
            before = app.read()['state']
            for interior in (2, 3):
                self.assertFalse(app.call('update', path='/cursor', value=interior)['ok'])
                self.assertEqual(app.read()['state'], before)
            app.action('step')
            state = app.until(lambda s: s['state']['cursor'] == 4 and not s['runtime']['in_flight'])
            self.assertEqual(state['state']['input'], {'text': 'c'})
            self.assertNotIn('local', state['state']['bindings'])
            self.assertEqual(state['runtime']['completed_native_launches'], 3)
            app.action('reset')
            self.assertEqual(app.read()['state']['bindings'], {})

    def test_cancel_loop_reaps_native_child_and_requires_whole_node_retry(self):
        code = 'import os,time;open("loop.pid","w").write(str(os.getpid()));time.sleep(30)'
        source = ('while true is true\n  run ' + json.dumps(sys.executable) +
                  ' with arguments ["-c", ' + json.dumps(code) + ']\n')
        with Session(source) as app:
            app.action('resume')
            app.until(lambda s: (app.root / 'loop.pid').exists() and bool((app.root / 'loop.pid').read_text()))
            pid = int((app.root / 'loop.pid').read_text())
            app.action('cancel')
            state = app.until(lambda s: s['runtime']['status'] == 'failed' and not s['runtime']['in_flight'])
            self.assertEqual(state['state']['cursor'], 0)
            self.assertTrue(state['runtime']['effects_may_be_incomplete'])
            with self.assertRaises(ProcessLookupError):
                os.kill(pid, 0)
            self.assertEqual(app.call('control', action='resume')['error']['code'], 'uncertain_effects')

    def test_reusable_calls_are_atomic_local_scopes_and_reset_preserves_external_names(self):
        source = ('define a program named render\n  accept name as text\n'
                  '  run "printf" with arguments ["%s", name] as private\n'
                  'accept name as text with default "caller"\n'
                  'run program render with name "child" as result\n'
                  'then "printf" with arguments ["%s:%s", name, result]\n')
        with Session(source, bindings={'private': 'external'}) as app:
            self.assertFalse(app.call('update', path='/cursor', value=1)['ok'])
            app.action('step')
            state = app.until(lambda s: s['state']['cursor'] == 3 and not s['runtime']['in_flight'])
            self.assertEqual(state['state']['bindings'], {'name': 'caller', 'private': 'external', 'result': 'child'})
            self.assertEqual(state['runtime']['completed_native_launches'], 1)
            app.ok('update', path='/bindings/result', value='edited')
            app.action('step')
            state = app.until(lambda s: s['runtime']['last_output'] == {'text': 'caller:edited'})
            app.action('reset')
            state = app.read()
            self.assertEqual(state['state']['bindings'], {'name': 'caller', 'private': 'external'})

    def test_reusable_listener_calls_follow_configuration_edits_and_keep_locals_private(self):
        source = ('define a program named render\n  accept prefix as text\n  read from stdin as message\n'
                  '  then "printf" with arguments ["%s:%s", prefix, message]\n'
                  'accept prefix as text with default "first"\nlisten to stdin\n'
                  'whenever a line arrives as event\n  then program render with prefix prefix\n')
        with Session(source) as app:
            for prefix, record in [('first', 'one'), ('next', 'two')]:
                app.ok('update', path='/bindings/prefix', value=prefix)
                before = app.read()['runtime']['runs_completed']
                app.ok('send', value={'text': record})
                app.action('step')
                state = app.until(lambda s: s['runtime']['runs_completed'] > before)
                self.assertEqual(state['runtime']['last_output'], {'text': prefix + ':' + record})
                self.assertNotIn('message', state['state']['bindings'])
            self.assertEqual(state['runtime']['completed_native_launches'], 2)

    def test_failed_reusable_call_can_be_repaired_at_its_boundary_with_explicit_retry(self):
        source = ('define a program named task\n  run "printf" with arguments ["before"]\n'
                  '  run "false"\nrun program task as result\n')
        with Session(source) as app:
            app.action('resume')
            state = app.until(lambda s: s['runtime']['status'] == 'failed')
            self.assertEqual(state['state']['cursor'], 0)
            self.assertNotIn('result', state['state']['bindings'])
            app.ok('update', path='/source', value=source.replace('run "false"', 'run "printf" with arguments ["repaired"]'))
            self.assertFalse(app.call('control', action='resume')['ok'])
            app.action('retry')
            state = app.until(lambda s: s['runtime']['status'] == 'completed')
            self.assertEqual(state['runtime']['last_output'], {'text': 'repaired'})
            self.assertEqual(state['state']['bindings']['result'], 'repaired')

    def test_cancel_reusable_call_reaps_its_native_child(self):
        code = 'import os,time;open("call.pid","w").write(str(os.getpid()));time.sleep(30)'
        source = ('define a program named wait\n  run ' + json.dumps(sys.executable) +
                  ' with arguments ["-c", ' + json.dumps(code) + ']\nrun program wait\n')
        with Session(source) as app:
            app.action('resume')
            app.until(lambda s: (app.root / 'call.pid').exists() and bool((app.root / 'call.pid').read_text()))
            pid = int((app.root / 'call.pid').read_text())
            app.action('cancel')
            state = app.until(lambda s: s['runtime']['status'] == 'failed' and not s['runtime']['in_flight'])
            self.assertEqual(state['state']['cursor'], 0)
            with self.assertRaises(ProcessLookupError):
                os.kill(pid, 0)

    def test_structured_bindings_are_editable_objects_and_selection_reads_latest_state(self):
        source = ('accept data as json\ntake integer at "/count" from data as count\n'
                  'then "printf" with arguments ["%s", count]\n')
        with Session(source, bindings={'data': {'count': 1, 'nested': [None, 1.25]}}) as app:
            app.ok('update', path='/bindings/data/count', value=7)
            app.action('step')
            state = app.until(lambda s: s['state']['cursor'] == 1 and not s['runtime']['in_flight'])
            self.assertEqual(state['state']['bindings']['count'], 7)
            self.assertEqual(state['state']['bindings']['data']['nested'], [None, 1.25])
            app.ok('update', path='/bindings/count', value=8)
            app.action('step')
            state = app.until(lambda s: s['runtime']['last_output'] == {'text': '8'})
            self.assertEqual(state['runtime']['completed_native_launches'], 1)
            app.action('reset')
            state = app.read()
            self.assertNotIn('count', state['state']['bindings'])
            self.assertEqual(state['state']['bindings']['data']['count'], 7)

    def test_live_json_parse_and_type_failure_can_be_repaired_without_native_replay(self):
        source = ('read "payload.json"\nthen parse json as data\ntake integer at "/count" from data as count\n'
                  'then "printf" with arguments ["%s", count]\n')
        with Session(source) as app:
            (app.root / 'payload.json').write_text('{"count":"wrong","nested":[null,1.5]}')
            app.action('resume')
            state = app.until(lambda s: s['runtime']['status'] == 'failed')
            self.assertEqual(state['state']['cursor'], 2)
            self.assertEqual(state['state']['bindings']['data'], {'count': 'wrong', 'nested': [None, 1.5]})
            self.assertEqual(state['runtime']['completed_native_launches'], 0)
            app.ok('update', path='/bindings/data/count', value=12)
            app.action('retry')
            state = app.until(lambda s: s['runtime']['status'] == 'completed')
            self.assertEqual(state['runtime']['last_output'], {'text': '12'})
            self.assertEqual(state['runtime']['completed_native_launches'], 1)

    def test_tui_native_json_operations_use_widget_values_and_real_commands(self):
        source = ('open a screen titled "JSON"\nplace panel on the left\ndefine a program named panel\n'
                  '  show a text field named payload with value "{\\"count\\":2}"\n'
                  '  show a button named run labelled "Read"\n  show a text panel named result\n'
                  '  whenever run is pressed\n    run "printf" with arguments ["%s", payload]\n'
                  '    then parse json as data\n    take integer at "/count" from data as count\n'
                  '    then "printf" with arguments ["%s", count]\n    then show the result in result\n')
        with Session(source, headless=True) as app:
            app.ok('update', path='/ui/programs/panel/widgets/payload/text', value='{"count":23}')
            app.ok('event', value={'type': 'press', 'program': 'panel', 'widget': 'run'})
            app.action('resume')
            state = app.until(lambda s: s['state']['ui']['programs']['panel']['widgets']['result']['text'] == '23')
            self.assertEqual(state['runtime']['ui_errors'], {})
            self.assertEqual(state['runtime']['completed_native_launches'], 2)

    def test_live_listener_branches_follow_typed_state_edits_between_records(self):
        source = ('accept enabled as boolean with default false\nlisten to stdin\nwhenever a line arrives as event\n'
                  '  if enabled is true\n    run "printf" with arguments ["enabled:%s", event]\n'
                  '  otherwise\n    run "printf" with arguments ["disabled:%s", event]\n')
        with Session(source) as app:
            for enabled, record in [(False, 'one'), (True, 'two')]:
                app.ok('update', path='/bindings/enabled', value=enabled)
                before = app.read()['runtime']['runs_completed']
                app.ok('send', value={'text': record})
                app.action('step')
                state = app.until(lambda s: s['runtime']['runs_completed'] > before)
                self.assertEqual(state['runtime']['last_output'], {'text': ('enabled:' if enabled else 'disabled:') + record})
            self.assertEqual(state['runtime']['completed_native_launches'], 2)

    def test_tui_native_branches_read_widget_state_and_publish_selected_output(self):
        source = ('open a screen titled "Branching"\nplace panel on the left\ndefine a program named panel\n'
                  '  show a text field named mode with value "safe"\n  show a button named run labelled "Run"\n'
                  '  show a text panel named result\n  whenever run is pressed\n'
                  '    if mode is "safe"\n      run "printf" with arguments ["safe"]\n'
                  '    otherwise\n      run "printf" with arguments ["changed"]\n'
                  '    then show the result in result\n')
        with Session(source, headless=True) as app:
            app.action('resume')
            for mode in ['safe', 'changed']:
                app.ok('update', path='/ui/programs/panel/widgets/mode/text', value=mode)
                app.ok('event', value={'type': 'press', 'program': 'panel', 'widget': 'run'})
                state = app.until(lambda s: s['state']['ui']['programs']['panel']['widgets']['result']['text'] == mode)
                self.assertEqual(state['runtime']['ui_errors'], {})
            self.assertEqual(state['runtime']['completed_native_launches'], 2)

    def test_native_status_capture_is_live_integer_and_resets_between_records(self):
        source = 'listen to stdin\nwhenever a line arrives as event\n  run "false" and keep status as code\n  then "printf" with arguments ["%s:%s", code, event]\n'
        with Session(source) as app:
            for record in ['one\n', 'two\n']:
                previous_nodes = app.read()['runtime']['nodes_completed']
                app.ok('send', value={'text': record})
                app.action('step')
                state = app.until(lambda s: s['runtime']['nodes_completed'] > previous_nodes and s['runtime']['status'] == 'paused' and not s['runtime']['in_flight'])
                self.assertEqual(state['state']['bindings']['code'], 1)
                self.assertIs(type(state['state']['bindings']['code']), int)
                app.action('step')
                state = app.until(lambda s: s['runtime']['last_output'] == {'text': '1:' + record})
            app.action('reset')
            self.assertNotIn('code', app.read()['state']['bindings'])

    def test_ui_handler_can_handle_native_nonzero_status(self):
        source = ('open a screen titled "Native status"\nplace panel on the left\n'
                  'define a program named panel\n  show a text panel named result\n'
                  '  whenever the program opens\n'
                  '    run "false" and keep status as code\n'
                  '    then "printf" with arguments ["native exit: %s", code]\n'
                  '    then show the result in result\n')
        with Session(source, headless=True, paused=False) as app:
            state = app.until(lambda s: s['state']['ui']['programs']['panel']['widgets']['result']['text'] == 'native exit: 1')
            self.assertEqual(state['runtime']['ui_errors'], {})

    def test_json_binding_types_survive_unrelated_native_nodes_and_reset(self):
        bindings = {'config': {'limit': 3}, 'flags': ['-c'], 'ratio': 1.25, 'enabled': True}
        with Session('run printf with message "result" as result\n', bindings) as app:
            app.action('resume')
            state = app.until(lambda s: s['runtime']['status'] == 'completed')
            for name, expected in bindings.items():
                self.assertEqual(state['state']['bindings'][name], expected)
            app.action('reset')
            self.assertEqual(app.read()['state']['bindings'], bindings)
            app.action('restart')
            state = app.until(lambda s: s['runtime']['status'] == 'completed')
            self.assertEqual(state['runtime']['runs_completed'], 2)

    def test_native_source_bindings_can_be_changed_then_explicitly_restarted(self):
        code = 'import sys,time;print(sys.argv[1],flush=True);time.sleep(30)'
        manifest = {'name': 'feed', 'binary': sys.executable, 'parameters': {'message': {'type': 'string', 'required': True}}, 'arguments': [{'literal': '-u'}, {'literal': '-c'}, {'literal': code}, {'parameter': 'message'}]}
        source = 'listen to feed with message greeting\nwhenever a line arrives as event\n  then tr with from "a-z" and to "A-Z"\n'
        with Session(source, {'greeting': 'one'}, paused=False, manifests={'feed': manifest}) as app:
            app.until(lambda s: s['runtime']['runs_completed'] == 1)
            app.action('pause')
            app.until(lambda s: not s['runtime']['in_flight'])
            app.ok('update', path='/bindings/greeting', value='two')
            app.action('restart-source')
            app.action('resume')
            state = app.until(lambda s: s['runtime']['runs_completed'] == 2)
            self.assertEqual(state['runtime']['last_output'], {'text': 'TWO\n'})

    def test_queue_monitor_program_uses_live_threshold_on_the_next_real_tick(self):
        source = (PROJECT / 'examples/live/queue-monitor.program').read_text()
        bindings = json.loads((PROJECT / 'examples/live/queue-state.json').read_text())['bindings']
        with Session(source, bindings) as app:
            (app.root / 'queue.json').write_bytes((PROJECT / 'examples/live/queue.json').read_bytes())
            app.action('resume')
            app.until(lambda s: s['runtime']['runs_completed'] == 1)
            self.assertEqual(json.loads((app.root / 'queue-report.json').read_text())['overdue'], 1)
            app.action('pause')
            app.until(lambda s: not s['runtime']['in_flight'])
            app.ok('update', path='/bindings/threshold', value='10')
            app.action('resume')
            app.until(lambda s: s['runtime']['runs_completed'] == 2)
            self.assertEqual(json.loads((app.root / 'queue-report.json').read_text())['overdue'], 2)

    def test_wait_does_not_block_other_clients_and_unknown_fields_are_rejected(self):
        with Session('run printf with message "probe"\n') as app:
            revision = app.ok('read')['revision']
            with concurrent.futures.ThreadPoolExecutor(max_workers=1) as pool:
                pending = pool.submit(app.call, 'wait', after=revision, timeout_ms=5000)
                app.ok('create', path='/user/wake', value=True)
                self.assertTrue(pending.result(timeout=2)['result']['changed'])
            self.assertFalse(app.call('read', value='ignored typo')['ok'])
            self.assertFalse(app.call('update', path='/user/wake', value=False, if_revision=None)['ok'])

    def test_handler_source_edits_preserve_the_live_native_producer(self):
        code = 'import os,time;open("starts","a").write("start\\n");open("pid","w").write(str(os.getpid()));print("first",flush=True)\nwhile not os.path.exists("release"): time.sleep(.005)\nprint("SECOND",flush=True);time.sleep(30)'
        manifest = {'name': 'feed', 'binary': sys.executable, 'arguments': [{'literal': '-u'}, {'literal': '-c'}, {'literal': code}]}
        source = 'listen to feed\nwhenever a line arrives as event\n  then tr with from "a-z" and to "A-Z"\n'
        with Session(source, paused=False, manifests={'feed': manifest}) as app:
            app.until(lambda s: s['runtime']['runs_completed'] == 1)
            app.action('pause')
            app.until(lambda s: not s['runtime']['in_flight'])
            pid = int((app.root / 'pid').read_text())
            app.ok('update', path='/source', value=source.replace('from "a-z" and to "A-Z"', 'from "A-Z" and to "a-z"'))
            (app.root / 'release').write_text('ready')
            app.action('resume')
            state = app.until(lambda s: s['runtime']['runs_completed'] == 2)
            self.assertEqual(state['runtime']['last_output'], {'text': 'second\n'})
            self.assertEqual(int((app.root / 'pid').read_text()), pid)
            self.assertEqual((app.root / 'starts').read_text(), 'start\n')

    def test_script_assigned_state_persists_between_ticks_and_reset_restores_seed(self):
        source = 'every 30 seconds\n  run printf with message counter\n  then jq with flags ["-c"] and filter "tonumber + 1" as counter\n'
        with Session(source, {'counter': '0'}) as app:
            for expected in ['1\n', '2\n']:
                app.action('step')
                app.until(lambda s: s['state']['cursor'] == 1 and not s['runtime']['in_flight'])
                app.action('step')
                state = app.until(lambda s: s['runtime']['last_output'] == {'text': expected})
                self.assertEqual(state['state']['bindings']['counter'], expected)
            app.action('reset')
            self.assertEqual(app.read()['state']['bindings']['counter'], '0')

    def test_writes_have_compact_acks_and_reads_support_precise_projection(self):
        with Session('run printf with message "probe"\n') as app:
            ack = app.ok('create', path='/user/large', value='x' * 500000)
            self.assertLess(len(json.dumps(ack)), 1024)
            self.assertEqual(app.ok('read', path='/runtime/status')['result'], 'paused')
            projected = app.ok('wait', path='/runtime/status', after=ack['revision'], timeout_ms=1)
            self.assertEqual(projected['result']['snapshot'], 'paused')
            self.assertEqual(app.ok('read', path='/user/large')['result'], 'x' * 500000)

    def test_ui_widget_crud_updates_real_frame_and_script_handlers(self):
        source = (PROJECT / 'examples/live/widget-console.program').read_text()
        with Session(source, headless=True) as app:
            app.until(lambda s: 'Operator console' in s['runtime']['frame'])
            app.ok('update', path='/ui/programs/console/widgets/message/text', value='changed by agent')
            app.until(lambda s: 'changed by agent' in s['runtime']['frame'])
            app.ok('create', path='/ui/programs/console/widgets/choices/items/-', value='custom')
            app.ok('update', path='/ui/programs/console/widgets/choices/selected', value=2)
            app.ok('event', value={'type': 'choose', 'program': 'console', 'widget': 'choices', 'index': 2})
            app.action('resume')
            app.until(lambda s: s['state']['ui']['programs']['console']['widgets']['message']['text'] == 'custom')
            app.action('pause')
            app.until(lambda s: not s['runtime']['in_flight'])
            app.ok('patch', operations=[{'op': 'replace', 'path': '/ui/programs/console/widgets/choices/selected', 'value': 1}, {'op': 'remove', 'path': '/ui/programs/console/widgets/choices/items/1'}])
            # Removing an item also requires maintaining a valid selection atomically.

    def test_ui_ports_rows_bars_and_graph_series_are_real_editable_state(self):
        source = (PROJECT / 'examples/live/widget-console.program').read_text()
        with Session(source, headless=True) as app:
            app.ok('patch', operations=[
                {'op': 'create', 'path': '/ui/programs/console/inputs/incoming', 'value': 'port payload'},
                {'op': 'add', 'path': '/ui/programs/dashboard/widgets/metrics/rows/-', 'value': {'name': 'build', 'value': 7}},
                {'op': 'add', 'path': '/ui/programs/dashboard/widgets/load/bars/-', 'value': {'label': 'worker', 'value': 42}},
                {'op': 'add', 'path': '/ui/programs/dashboard/widgets/history/series/jobs', 'value': [[0, 3], [1, 7]]},
                {'op': 'replace', 'path': '/ui/programs/dashboard/widgets/history/active_series', 'value': 'jobs'},
            ])
            app.until(lambda s: 'worker' in s['runtime']['frame'] and 'build' in s['runtime']['frame'])
            app.action('resume')
            app.until(lambda s: s['state']['ui']['programs']['console']['widgets']['status']['text'] == 'port payload')
            app.action('pause')
            app.until(lambda s: not s['runtime']['in_flight'])
            app.ok('delete', path='/ui/programs/console/inputs/incoming')
            app.ok('patch', operations=[
                {'op': 'remove', 'path': '/ui/programs/dashboard/widgets/metrics/rows/0'},
                {'op': 'remove', 'path': '/ui/programs/dashboard/widgets/history/series/jobs'},
                {'op': 'replace', 'path': '/ui/programs/dashboard/widgets/history/active_series', 'value': ''},
            ])
            self.assertEqual(app.ok('read', path='/ui/programs/dashboard/widgets/metrics/rows')['result'], [])
            self.assertEqual(app.ok('read', path='/ui/programs/dashboard/widgets/history/series')['result'], {})

    def test_ui_invalid_edits_roll_back_and_layout_source_can_add_and_remove_widgets(self):
        source = (PROJECT / 'examples/live/widget-console.program').read_text()
        with Session(source, headless=True) as app:
            app.until(lambda s: bool(s['runtime']['frame']))
            before = app.ok('read')['revision']
            bad = app.call('patch', operations=[
                {'op': 'add', 'path': '/user/should_rollback', 'value': True},
                {'op': 'replace', 'path': '/ui/programs/console/widgets/message/text', 'value': 'x' * 513},
            ])
            self.assertFalse(bad['ok'])
            self.assertEqual(app.ok('read')['revision'], before)
            self.assertFalse(app.call('create', path='/ui/programs/console/inputs/unknown', value='bad')['ok'])
            changed = source.replace('text "Operator console"', 'text "Changed title"').replace('  whenever message is submitted', '  show a badge named extra with text "added widget"\n\n  whenever message is submitted')
            app.ok('update', path='/source', value=changed)
            app.until(lambda s: 'Changed title' in s['runtime']['frame'] and 'added widget' in s['runtime']['frame'])
            app.ok('update', path='/source', value=source)
            app.until(lambda s: 'extra' not in s['state']['ui']['programs']['console']['widgets'])

    def test_ui_remote_submit_runs_native_applet_and_publishes_its_real_result(self):
        source = (PROJECT / 'examples/live/widget-console.program').read_text()
        with Session(source, headless=True) as app:
            app.ok('update', path='/ui/programs/console/widgets/message/text', value='native output')
            app.ok('event', value={'type': 'submit', 'program': 'console', 'widget': 'message'})
            app.action('resume')
            app.until(lambda s: s['state']['ui']['programs']['console']['widgets']['result']['text'] == 'native output\n')
            self.assertEqual(app.read()['runtime']['ui_errors'], {})

    def test_live_ui_cancel_reaps_worker_and_repaired_source_can_retry(self):
        code = 'import os,time;open("pid","w").write(str(os.getpid()));time.sleep(30)'
        manifest = {'name': 'hold', 'binary': sys.executable, 'arguments': [{'literal': '-c'}, {'literal': code}]}
        source = 'open a screen titled "Cancellation"\nplace panel on the left\ndefine a program named panel\n  show a text panel named result\n  whenever the program opens\n    run hold\n    then show the result in result\n'
        with Session(source, paused=False, manifests={'hold': manifest}, headless=True) as app:
            deadline = time.monotonic() + 5
            while not (app.root / 'pid').exists() and time.monotonic() < deadline:
                time.sleep(.01)
            pid = int((app.root / 'pid').read_text())
            self.assertEqual(app.call('update', path='/ui/programs/panel/widgets/result/text', value='racing')['error']['code'], 'busy')
            app.action('cancel')
            app.until(lambda s: not s['runtime']['in_flight'] and s['runtime']['effects_may_be_incomplete'])
            with self.assertRaises(ProcessLookupError):
                os.kill(pid, 0)
            self.assertEqual(app.call('control', action='resume')['error']['code'], 'uncertain_effects')
            app.ok('update', path='/source', value=source.replace('run hold', 'run printf with message "repaired"'))
            app.action('retry')
            app.until(lambda s: s['state']['ui']['programs']['panel']['widgets']['result']['text'] == 'repaired\n')

    def test_cli_edits_render_in_a_real_pty_and_terminal_is_restored(self):
        with tempfile.TemporaryDirectory(prefix='execute-live-pty-') as directory:
            root = Path(directory)
            script = root / 'console.program'
            script.write_bytes((PROJECT / 'examples/live/widget-console.program').read_bytes())
            address = root / 'ui.sock'
            master, slave = pty.openpty()
            before = termios.tcgetattr(slave)
            fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack('HHHH', 40, 100, 0, 0))
            data = bytearray()
            stopped = threading.Event()
            def drain():
                while not stopped.is_set():
                    if not select.select([master], [], [], .02)[0]:
                        continue
                    try:
                        chunk = os.read(master, 65536)
                    except OSError as error:
                        if error.errno == errno.EIO:
                            return
                        raise
                    data.extend(chunk)
                    if b'\x1b[6n' in chunk:
                        os.write(master, b'\x1b[1;1R')
            reader = threading.Thread(target=drain)
            reader.start()
            child = subprocess.Popen([str(BINARY), 'live', 'serve', str(script), '--socket', str(address), '--paused', '--manifest-dir', str(PROJECT / 'manifests')], cwd=root, stdin=slave, stdout=slave, stderr=slave, env={**os.environ, 'TERM': 'xterm-256color'})
            def cli(*args):
                result = subprocess.run([str(BINARY), 'live', *args, '--socket', str(address)], capture_output=True, timeout=5)
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                return json.loads(result.stdout)
            try:
                deadline = time.monotonic() + 8
                while (not address.exists() or b'\x1b[?1049h' not in data) and time.monotonic() < deadline:
                    time.sleep(.01)
                self.assertIsNone(child.poll(), bytes(data)[-2000:])
                cli('update', '/ui/programs/console/widgets/message/text', '--json', '"ZZZZZZZZZZZZZZZZ"')
                deadline = time.monotonic() + 5
                while b'ZZZZZZZZZZZZZZZZ' not in data and time.monotonic() < deadline:
                    time.sleep(.01)
                self.assertIn(b'ZZZZZZZZZZZZZZZZ', data)
                cli('control', 'resume')
                os.write(master, b'\r')
                deadline = time.monotonic() + 5
                while time.monotonic() < deadline:
                    if cli('read', '/ui/programs/console/widgets/result/text')['result'] == 'ZZZZZZZZZZZZZZZZ\n':
                        break
                    time.sleep(.01)
                else:
                    self.fail('terminal Enter did not invoke the native handler using the remotely edited text')
                cli('control', 'stop')
                self.assertEqual(child.wait(timeout=5), 0)
                time.sleep(.05)
                self.assertEqual(termios.tcgetattr(slave), before)
                self.assertIn(b'\x1b[?1049l', data)
                self.assertFalse(address.exists())
            finally:
                if child.poll() is None:
                    child.terminate()
                    try:
                        child.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        child.kill()
                        child.wait(timeout=2)
                stopped.set()
                reader.join(timeout=2)
                os.close(master)
                os.close(slave)

    def test_partial_request_progress_cannot_renew_read_deadline_or_mutate_state(self):
        with Session('then encode 42 as json\n') as app:
            before = app.read()['state']
            with socket.socket(socket.AF_UNIX) as client:
                client.settimeout(2)
                client.connect(str(app.socket))
                client.sendall(b'{"op":"create","path":"/user/late","value":"')
                deadline = time.monotonic() + 4
                next_byte = time.monotonic()
                response = None
                rejected = False
                while time.monotonic() < deadline:
                    try:
                        if select.select([client], [], [], .02)[0]:
                            with client.makefile('rb') as reader:
                                wire = reader.readline()
                            if wire:
                                response = json.loads(wire)
                            rejected = True
                            break
                        if time.monotonic() >= next_byte:
                            client.sendall(b'x')
                            next_byte = time.monotonic() + .1
                    except (BrokenPipeError, ConnectionResetError):
                        # Closing a peer with unread inbound bytes may reset it;
                        # the timeout error frame is best effort in this case.
                        rejected = True
                        break
                self.assertTrue(rejected, 'progress renewed the request deadline')
                if response is not None:
                    self.assertFalse(response['ok'])
                    self.assertIn('deadline', response['error']['message'])
            self.assertEqual(app.read()['state'], before)
            self.assertEqual(app.read()['runtime']['completed_native_launches'], 0)

    def test_stop_reaps_clients_even_while_partial_json_keeps_arriving(self):
        with Session('then encode 42 as json\n') as app:
            client = socket.socket(socket.AF_UNIX)
            client.settimeout(1)
            client.connect(str(app.socket))
            client.sendall(b'{"op":"read" ')
            quit = threading.Event()
            def drip():
                while not quit.wait(.1):
                    try:
                        client.sendall(b' ')
                    except OSError:
                        return
            sender = threading.Thread(target=drip)
            sender.start()
            try:
                app.action('stop')
                self.assertEqual(app.child.wait(timeout=4), 0)
                self.assertFalse(app.socket.exists())
            finally:
                quit.set()
                client.close()
                sender.join(timeout=2)
                self.assertFalse(sender.is_alive())

    def test_cli_wait_handler_has_a_separate_budget_from_wire_io(self):
        with Session('then encode 42 as json\n') as app:
            app.until(lambda state: state['runtime']['status'] == 'paused')
            revision = app.ok('read')['revision']
            response = subprocess.run([
                str(BINARY), 'live', 'wait', '/runtime/status', '--socket', str(app.socket),
                '--after', str(revision), '--timeout-ms', '2200',
            ], capture_output=True, timeout=8)
            self.assertEqual(response.returncode, 0, response.stderr)
            self.assertFalse(json.loads(response.stdout)['result']['changed'])

    def test_malformed_oversized_requests_wait_and_existing_socket_path(self):
        with Session('run printf with message "probe"\n') as app:
            revision = app.ok('read')['revision']
            response = app.ok('wait', after=revision, timeout_ms=10)
            self.assertFalse(response['result']['changed'])
            self.assertFalse(app.call('read', unknown=True)['ok'])
            for payload in [b'{broken}\n', b' ' * (1024 * 1024 + 1)]:
                with socket.socket(socket.AF_UNIX) as client:
                    client.settimeout(3)
                    client.connect(str(app.socket))
                    client.sendall(payload)
                    with client.makefile('rb') as reader:
                        response = json.loads(reader.readline())
                    self.assertFalse(response['ok'])
            original = app.socket.stat().st_ino
            run = subprocess.run([str(BINARY), 'live', 'serve', str(app.file), '--socket', str(app.socket), '--paused', '--manifest-dir', str(app.manifests)], capture_output=True, timeout=5)
            self.assertEqual(run.returncode, 1)
            self.assertFalse(json.loads(run.stdout)['ok'])
            self.assertEqual(app.socket.stat().st_ino, original)
            self.assertTrue(app.ok('read')['ok'])


    def test_concurrent_thousand_crud_requests_preserve_isolated_state(self):
        with Session('run printf with message "idle"\n') as app:
            def worker(index):
                for iteration in range(25):
                    path = f'/user/worker-{index}-{iteration}'
                    app.ok('create', path=path, value={'value': 0})
                    app.ok('update', path=path+'/value', value=1)
                    self.assertEqual(app.ok('read', path=path+'/value')['result'], 1)
                    app.ok('patch', operations=[
                        {'op': 'test', 'path': path+'/value', 'value': 1},
                        {'op': 'replace', 'path': path+'/value', 'value': 2}])
                    app.ok('delete', path=path)
            with concurrent.futures.ThreadPoolExecutor(max_workers=8) as executor:
                list(executor.map(worker, range(8)))
            self.assertEqual(app.ok('read', path='/user')['result'], {})
            self.assertEqual(app.read()['runtime']['completed_native_launches'], 0)

    def test_malformed_socket_matrix_cannot_poison_state_or_worker(self):
        with Session('run printf with message "still alive"\n') as app:
            revision = app.ok('read')['revision']
            payloads = [b'null', b'[]', b'0', b'"read"', b'\xff', b'{"op":7}',
                        b'{"op":"update","path":false,"value":0}',
                        b'{"op":"read","path":7}', b'{"op":"read","value":0}',
                        b'{"op":"control","action":7}',
                        b'{"op":"patch","operations":null}',
                        b'{"op":"patch","operations":[null]}',
                        b'{"op":"create","path":"/user/x"}',
                        b'{"op":"read","path":"/user/~2"}',
                        b'['*256+b'0'+b']'*256]
            for payload in payloads:
                with self.subTest(payload=payload[:80]), socket.socket(socket.AF_UNIX) as client:
                    client.settimeout(3)
                    client.connect(str(app.socket))
                    client.sendall(payload+b'\n')
                    with client.makefile('rb') as response:
                        self.assertFalse(json.loads(response.readline())['ok'])
            self.assertEqual(app.ok('read')['revision'], revision)
            app.action('resume')
            self.assertEqual(app.until(lambda s: s['runtime']['status']=='completed')['runtime']['last_output'], {'text':'still alive\n'})

    def test_partial_idle_clients_do_not_prevent_eventual_control_or_shutdown(self):
        with Session('run printf with message "idle"\n') as app:
            parked = []
            try:
                for _ in range(12):
                    client = socket.socket(socket.AF_UNIX)
                    client.settimeout(3)
                    client.connect(str(app.socket))
                    client.sendall(b'{')
                    parked.append(client)
                self.assertTrue(app.ok('read')['ok'])
                started = time.monotonic()
                app.action('stop')
                app.child.wait(timeout=4)
                self.assertLess(time.monotonic()-started, 4)
                self.assertFalse(app.socket.exists())
            finally:
                for client in parked:
                    client.close()

    def test_replaced_socket_path_is_not_deleted_by_shutdown(self):
        with Session('run printf with message "idle"\n') as app:
            app.socket.unlink()
            app.socket.write_text('new owner file')
            app.child.terminate()
            app.child.wait(timeout=4)
            self.assertEqual(app.child.returncode, 0)
            self.assertEqual(app.socket.read_text(), 'new owner file')


    def test_native_producer_backpressure_keeps_a_bounded_fifo_then_drains_to_eof(self):
        code = 'import os;open("producer.pid","w").write(str(os.getpid()));os.write(1,b"".join(str(i).encode()+b"\\n" for i in range(300)))'
        manifest = {'name':'feed','binary':sys.executable,'arguments':[{'literal':'-u'},{'literal':'-c'},{'literal':code}]}
        source = 'listen to feed\nwhenever a line arrives as event\n  then cat\n'
        with Session(source, paused=False, manifests={'feed':manifest}) as app:
            app.until(lambda s:s['runtime']['runs_completed']>=1)
            app.action('pause')
            paused = app.until(lambda s:s['runtime']['status']=='paused' and not s['runtime']['in_flight'] and s['state']['cursor']==0)
            queued = app.until(lambda s:len(s['state']['inbox'])==128)
            count = queued['runtime']['runs_completed']
            self.assertEqual(queued['state']['inbox'], [{'text':f'{i}\n'} for i in range(count,count+128)])
            self.assertFalse(queued['runtime']['producer_done'])
            app.action('resume')
            state = app.until(lambda s:s['runtime']['status']=='completed',timeout=12)
            self.assertEqual(state['runtime']['runs_completed'],300)
            self.assertEqual(state['runtime']['last_output'],{'text':'299\n'})
            self.assertEqual(state['runtime']['completed_native_launches'],301)
            self.assertEqual(state['state']['inbox'],[])
            with self.assertRaises(ProcessLookupError):os.kill(int((app.root/'producer.pid').read_text()),0)

    def test_native_producer_failure_is_reported_after_queued_record_and_can_be_repaired(self):
        code = 'print("before failure",flush=True);raise SystemExit(7)'
        manifest = {'name':'feed','binary':sys.executable,'arguments':[{'literal':'-u'},{'literal':'-c'},{'literal':code}]}
        with Session('listen to feed\nwhenever a line arrives as event\n  then cat\n',paused=False,manifests={'feed':manifest}) as app:
            state = app.until(lambda s:s['runtime']['status']=='failed')
            self.assertEqual(state['runtime']['runs_completed'],1)
            self.assertEqual(state['runtime']['last_output'],{'text':'before failure\n'})
            self.assertIn('7',state['runtime']['producer_error'])
            app.ok('update',path='/source',value='listen to printf with message "repaired"\nwhenever a line arrives as event\n  then cat\n')
            app.action('resume')
            state=app.until(lambda s:s['runtime']['status']=='completed')
            self.assertEqual(state['runtime']['last_output'],{'text':'repaired\n'})
            self.assertIsNone(state['runtime']['producer_error'])

    def test_invalid_utf8_listener_record_fails_before_native_execution_and_next_record_can_recover(self):
        with Session('listen to stdin\nwhenever a line arrives as event\n  then cat\n',paused=False) as app:
            app.ok('send',value={'bytes':[255,0,10]})
            state=app.until(lambda s:s['runtime']['status']=='failed')
            self.assertIn('UTF-8',state['runtime']['error'])
            self.assertEqual(state['runtime']['completed_native_launches'],0)
            app.ok('send',value={'text':'valid next record\n'})
            app.action('resume')
            state=app.until(lambda s:s['runtime']['runs_completed']==1)
            self.assertEqual(state['runtime']['last_output'],{'text':'valid next record\n'})


if __name__ == '__main__':
    unittest.main()
