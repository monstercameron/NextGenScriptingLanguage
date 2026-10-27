#!/usr/bin/env python3
"""Validate and run 150 useful .program utilities in owned fixture directories."""
from __future__ import annotations
import argparse
import base64
import copy
import hashlib
import json
import os
from pathlib import Path
import selectors
import shlex
import signal
import stat
import subprocess
import sys
import tempfile
import time
from collections import Counter

HERE = Path(__file__).resolve().parent
PROJECT = HERE.parents[1]
TOTAL = 150
MAX_FILE = 8 * 1024 * 1024
MAX_FIXTURES = 16 * 1024 * 1024
MAX_CAPTURE = 16 * 1024 * 1024
MAX_SOURCE = 64 * 1024
SPECIAL_INPUTS = {'stdin', 'working directory', 'working_directory', '.'}
ISOLATED_STATE_HOME = '.execute-state'
WHAT_IF_KEY = Path(ISOLATED_STATE_HOME) / 'execute' / 'what-if.key'


def relative_path(name):
    if not isinstance(name, str) or not name or '\0' in name:
        raise ValueError(f'invalid relative path: {name!r}')
    path = Path(name)
    if path.is_absolute() or '..' in path.parts or (path.parts and path.parts[0] == '.utility'):
        raise ValueError(f'path leaves the fixture directory: {name!r}')
    return path


def payload(value, encoded=False):
    if not isinstance(value, str):
        raise ValueError('fixture and expected values must be strings')
    result = base64.b64decode(value, validate=True) if encoded else value.encode('utf-8')
    if len(result) > MAX_FILE:
        raise ValueError('fixture or expected value exceeds 8 MiB')
    return result


def mapping_bytes(case, text_key, binary_key):
    result = {}
    for key, encoded in [(text_key, False), (binary_key, True)]:
        values = case.get(key, {})
        if not isinstance(values, dict):
            raise ValueError(f'{key} must be an object')
        for name, value in values.items():
            relative_path(name)
            if name in result:
                raise ValueError(f'duplicate text/binary path: {name}')
            result[name] = payload(value, encoded)
    return result


def expected_bytes(case, prefix=''):
    a, b = prefix + 'expected', prefix + 'expected_base64'
    if (a in case) == (b in case):
        raise ValueError(f'exactly one of {a}/{b} is required')
    return payload(case[a] if a in case else case[b], b in case)


def normalized_source(source):
    lines = []
    for line in source.splitlines():
        quoted = escaped = False
        end = len(line)
        for index, char in enumerate(line):
            if quoted:
                if escaped:
                    escaped = False
                elif char == '\\':
                    escaped = True
                elif char == '"':
                    quoted = False
            elif char == '"':
                quoted = True
            elif char == '#':
                end = index
                break
        code = line[:end].rstrip()
        if code.strip():
            lines.append(code)
    return '\n'.join(lines)


def merged_variant(case, variant):
    result = copy.deepcopy(case)
    result.pop('variants', None)
    result.pop('rejections', None)
    for key, value in variant.items():
        if key in {'fixtures', 'fixtures_base64', 'files', 'files_base64'}:
            opposite = key[:-7] if key.endswith('_base64') else key + '_base64'
            for name in value:
                result.get(opposite, {}).pop(name, None)
            result.setdefault(key, {}).update(value)
        else:
            result[key] = value
    if 'expected_base64' in variant:
        result.pop('expected', None)
    elif 'expected' in variant:
        result.pop('expected_base64', None)
    return result


def validate_case(case, program_dir):
    for key in ['number', 'file', 'title', 'category', 'purpose', 'inputs', 'outputs', 'applets']:
        if key not in case or not case[key]:
            raise ValueError(f'case {case.get("number")}: missing {key}')
    if case.get('failure') or case.get('invalid') or case.get('strip'):
        raise ValueError('numbered utilities must be valid successes with exact byte oracles')
    filename = relative_path(case['file'])
    if filename.parent != Path('.') or filename.suffix != '.program':
        raise ValueError('program filename must be a local .program file')
    if not filename.name.startswith(f'{case["number"]:03}-'):
        raise ValueError(f'filename must use its three-digit case number: {filename}')
    source = (program_dir / filename).read_bytes()
    if len(source) > MAX_SOURCE:
        raise ValueError('program exceeds source-size budget')
    source.decode('utf-8')
    for key in ['inputs', 'outputs']:
        if not isinstance(case[key], dict):
            raise ValueError(f'{key} must describe path/stream names')
        for name, description in case[key].items():
            if not isinstance(description, str) or not description.strip():
                raise ValueError(f'{key} needs a description for {name}')
            if name not in SPECIAL_INPUTS and name != 'stdout':
                relative_path(name)
    if not isinstance(case['applets'], list) or not all(isinstance(x, str) for x in case['applets']):
        raise ValueError('applets must list the actual native programs')
    for name, parameter in case.get('parameters', {}).items():
        if not name.isascii() or not name.isidentifier() or name in {'true', 'false'}:
            raise ValueError(f'invalid parameter name: {name}')
        if parameter.get('type') != 'text' or not isinstance(parameter.get('default'), str):
            raise ValueError('utility path parameters must have a text default')
        if parameter.get('input') not in case['inputs'] or parameter['default'] != parameter['input']:
            raise ValueError('path parameter must identify its documented default input')
    if case.get('mode', 'finite') not in {'finite', 'cache', 'interactive', 'listener', 'tail', 'tail_stream', 'interval', 'calendar', 'stream'}:
        raise ValueError('unsupported utility test mode')
    fixtures = mapping_bytes(case, 'fixtures', 'fixtures_base64')
    if sum(map(len, fixtures.values())) > MAX_FIXTURES:
        raise ValueError('combined fixtures exceed 16 MiB')
    expected_bytes(case)
    outputs = mapping_bytes(case, 'files', 'files_base64')
    for name in outputs:
        if name not in case['outputs']:
            raise ValueError(f'undeclared output file: {name}')
    for name in case['outputs']:
        if name != 'stdout' and name not in outputs:
            raise ValueError(f'output file needs an exact expected-byte oracle: {name}')
    if 'stdout' not in case['outputs']:
        raise ValueError('stdout must be documented, including when intentionally empty')
    if 'stdin' in case:
        payload(case['stdin'])
        if 'stdin' not in case['inputs'] and not case.get('append_path'):
            raise ValueError('stdin data must be declared as an input')
    if case.get('append_path'):
        relative_path(case['append_path'])
    if case.get('mode') in {'interactive', 'tail', 'tail_stream'}:
        records = len(case.get('stdin', '').encode().splitlines(keepends=True))
        if records != len(expected_bytes(case).splitlines(keepends=True)):
            raise ValueError('live fixtures require one output line per input record')
        if case.get('mode') == 'tail':
            args = case.get('args', [])
            if '--max-runs' not in args or args.index('--max-runs') + 1 >= len(args):
                raise ValueError('live tail handler tests need an explicit finite --max-runs')
            if int(args[args.index('--max-runs') + 1]) != records:
                raise ValueError('live tail test --max-runs must match its appended record count')
    if case.get('timeout', 10) <= 0 or case.get('timeout', 10) > 90:
        raise ValueError('case timeout must be within 0..90 seconds')
    if case.get('mode') == 'cache':
        expected_bytes(case, 'after_')
        mapping_bytes(case, 'after_files', 'after_files_base64')
        mapping_bytes({'fixtures': case.get('mutate', {})}, 'fixtures', 'fixtures_base64')
    for variant in case.get('variants', []):
        if not isinstance(variant.get('name'), str) or not variant['name']:
            raise ValueError('variants need names')
        if not any(key in variant for key in ['fixtures', 'fixtures_base64', 'stdin']):
            raise ValueError('variant must supply different input data')
        validate_case(merged_variant(case, variant), program_dir)
    for rejection in case.get('rejections', []):
        if not rejection.get('name') or not rejection.get('stderr_contains'):
            raise ValueError('rejection checks need names and expected diagnostics')
        validate_case(merged_variant(case, rejection), program_dir)
    return source.decode('utf-8')


def load_catalog(path=HERE / 'catalog.json', program_dir=HERE, numbers=None):
    catalog = json.loads(path.read_text())
    cases = catalog['cases']
    wanted = list(range(1, TOTAL + 1)) if numbers is None else list(numbers)
    if [case['number'] for case in cases] != wanted:
        raise ValueError(f'catalog must contain exactly ordered case IDs {wanted[0]}..{wanted[-1]}')
    if {case['file'] for case in cases} != {p.name for p in program_dir.glob('*.program')}:
        raise ValueError('numbered program files and catalog do not match')
    seen = {}
    for case in cases:
        source = validate_case(case, program_dir)
        code = normalized_source(source)
        if code in seen:
            raise ValueError(f'duplicate program bodies: {seen[code]} and {case["number"]}')
        seen[code] = case['number']
    return cases


def environment(cwd):
    # Keep executable lookup, but remove shell startup hooks and imported code.
    env = {key: value for key, value in os.environ.items()
           if key not in {'BASH_ENV', 'ENV', 'PYTHONPATH', 'PYTHONHOME', 'PYTHONSTARTUP'}
           and not key.startswith('BASH_FUNC_')}
    env.update(LC_ALL='C', HOME=str(cwd), TMPDIR=str(cwd),
               XDG_STATE_HOME=str(cwd / ISOLATED_STATE_HOME))
    return env


class Runner:
    def __init__(self, binary, cwd, arguments, timeout, max_capture=None):
        self.deadline = time.monotonic() + timeout
        self.max_capture = MAX_CAPTURE if max_capture is None else max_capture
        self.started = time.perf_counter_ns()
        self.child = subprocess.Popen([str(binary), *map(str, arguments)], cwd=cwd,
            env=environment(cwd), stdin=subprocess.PIPE, stdout=subprocess.PIPE,
            stderr=subprocess.PIPE, start_new_session=True, bufsize=0)
        self.stdout = bytearray()
        self.stderr = bytearray()
        self.selector = selectors.DefaultSelector()
        self.stdin_open = True
        for stream in [self.child.stdin, self.child.stdout, self.child.stderr]:
            os.set_blocking(stream.fileno(), False)
        self.selector.register(self.child.stdout, selectors.EVENT_READ, self.stdout)
        self.selector.register(self.child.stderr, selectors.EVENT_READ, self.stderr)
        self.cleanup = 'normal runtime exit'

    def pump(self, deadline=None):
        deadline = min(self.deadline, deadline or self.deadline)
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise TimeoutError('owned runner exceeded deadline')
        for key, _ in self.selector.select(min(remaining, 0.1)):
            chunk = os.read(key.fd, 65536)
            if not chunk:
                self.selector.unregister(key.fileobj)
                key.fileobj.close()
            else:
                key.data.extend(chunk)
                if len(self.stdout) + len(self.stderr) > self.max_capture:
                    raise ValueError(f'combined stdout/stderr exceeds {self.max_capture} bytes')

    def send(self, data):
        view = memoryview(data)
        while view:
            if time.monotonic() >= self.deadline:
                raise TimeoutError('input pipe deadline exceeded')
            try:
                n = os.write(self.child.stdin.fileno(), view[:32768])
                if n == 0:
                    raise RuntimeError('stdin write made no progress')
                view = view[n:]
            except BrokenPipeError:
                return
            except BlockingIOError:
                self.pump()
            except InterruptedError:
                continue

    def close_input(self):
        if self.stdin_open:
            self.child.stdin.close()
            self.stdin_open = False

    def finish(self):
        self.close_input()
        while self.selector.get_map():
            self.pump()
        remaining = self.deadline - time.monotonic()
        if remaining <= 0:
            raise TimeoutError('runner exit deadline exceeded')
        self.child.wait(timeout=remaining)
        return {'code': self.child.returncode, 'stdout': bytes(self.stdout),
                'stderr': bytes(self.stderr), 'elapsed_ms': (time.perf_counter_ns() - self.started) / 1e6,
                'cleanup': self.cleanup}

    def close(self):
        if self.child.poll() is None:
            self.child.terminate()
            try:
                self.child.wait(timeout=1)
                self.cleanup = 'cooperative TERM; applet cleanup owned by runtime'
            except subprocess.TimeoutExpired:
                if self.child.poll() is None:
                    self.child.kill()
                try:
                    self.child.wait(timeout=2)
                except subprocess.TimeoutExpired:
                    pass
                self.cleanup = 'forced runner kill; descendant cleanup unverified'
        for stream in [self.child.stdin, self.child.stdout, self.child.stderr]:
            try:
                stream.close()
            except OSError:
                pass
        self.selector.close()


def invoke(binary, cwd, arguments, timeout=10, stdin=b'', max_capture=None):
    runner = Runner(binary, cwd, arguments, timeout, max_capture)
    try:
        runner.send(stdin)
        return runner.finish()
    finally:
        runner.close()


def events(result):
    records = []
    for line in result['stderr'].splitlines():
        if line.startswith(b'{'):
            try:
                record = json.loads(line)
                if isinstance(record, dict) and 'event' in record:
                    records.append(record)
            except ValueError:
                pass
    return records


def interactive(binary, cwd, arguments, case):
    runner = Runner(binary, cwd, arguments, case.get('timeout', 15))
    try:
        requests = case['stdin'].encode().splitlines(keepends=True)
        replies = expected_bytes(case).splitlines(keepends=True)
        if len(requests) != len(replies):
            raise ValueError('interactive fixtures require one output line per input record')
        cursor = 0
        for request, reply in zip(requests, replies):
            if case.get('append_path'):
                with (cwd / relative_path(case['append_path'])).open('ab') as target:
                    target.write(request)
            else:
                runner.send(request)
            deadline = min(runner.deadline, time.monotonic() + 5)
            while len(runner.stdout) < cursor + len(reply):
                runner.pump(deadline)
                if runner.child.poll() is not None and not runner.selector.get_map():
                    raise AssertionError('service ended before its expected response')
            assert runner.stdout[cursor:cursor + len(reply)] == reply, 'wrong live response'
            cursor += len(reply)
        result = runner.finish()
        result['records_before_eof'] = len(requests)
        return result
    finally:
        runner.close()


def materialize(cwd, case, program_dir=HERE):
    fixtures = mapping_bytes(case, 'fixtures', 'fixtures_base64')
    for name in case.get('directories', []):
        (cwd / relative_path(name)).mkdir(parents=True, exist_ok=True)
    for name, data in fixtures.items():
        path = cwd / relative_path(name)
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(data)
    path = cwd / case['file']
    if path.exists():
        raise ValueError('program file collides with an input fixture')
    path.write_bytes((program_dir / case['file']).read_bytes())
    return path


def verify(result, cwd, case, phase='first'):
    assert result['code'] == 0, result['stderr'][:4096].decode(errors='replace')
    expected = expected_bytes(case, 'after_' if phase == 'after' else '')
    assert result['stdout'] == expected, f'expected {expected[:500]!r}; got {result["stdout"][:500]!r}'
    files = mapping_bytes(case, 'after_files', 'after_files_base64') if phase == 'after' else mapping_bytes(case, 'files', 'files_base64')
    for name, expected_file in files.items():
        assert (cwd / relative_path(name)).read_bytes() == expected_file, f'wrong output file: {name}'
    for name in case.get('absent', []):
        assert not (cwd / relative_path(name)).exists(), f'unexpected effect: {name}'
    trace = events(result)
    if phase == 'first' and 'spawn_counts' in case:
        actual = Counter(e.get('applet') for e in trace if e['event'] == 'spawn')
        assert dict(actual) == case['spawn_counts'], f'native launch counts: {dict(actual)}'


def summarize(result, cwd, case, phase='first'):
    trace = events(result)
    files = mapping_bytes(case, 'after_files', 'after_files_base64') if phase == 'after' else mapping_bytes(case, 'files', 'files_base64')
    report = {'exit_code': result['code'], 'elapsed_ms': round(result['elapsed_ms'], 3),
              'stdout_bytes': len(result['stdout']), 'stdout_sha256': hashlib.sha256(result['stdout']).hexdigest(),
              'trace_event_counts': dict(Counter(e['event'] for e in trace)),
              'native_applet_launches': dict(Counter(e.get('applet') for e in trace if e['event'] == 'spawn')),
              'output_files': {name: {'bytes': len((cwd / name).read_bytes()), 'sha256': hashlib.sha256((cwd / name).read_bytes()).hexdigest()} for name in files}}
    if 'records_before_eof' in result:
        report['records_before_eof'] = result['records_before_eof']
    return report


def verify_isolated_state(cwd, expected):
    """Require only the private checkpoint key inside this case's state home."""
    state_home = cwd / ISOLATED_STATE_HOME
    if not expected:
        if state_home.exists() or state_home.is_symlink():
            raise AssertionError('unexpected per-user state was created')
        return None

    state_dir = state_home / 'execute'
    key = cwd / WHAT_IF_KEY
    for directory in (state_home, state_dir):
        try:
            metadata = directory.lstat()
        except FileNotFoundError as error:
            raise AssertionError(f'missing isolated state directory: {directory.relative_to(cwd)}') from error
        if not stat.S_ISDIR(metadata.st_mode):
            raise AssertionError(f'isolated state path is not a directory: {directory.relative_to(cwd)}')
        if os.name == 'posix' and stat.S_IMODE(metadata.st_mode) != 0o700:
            raise AssertionError(f'isolated state directory is not mode 0700: {directory.relative_to(cwd)}')

    try:
        metadata = key.lstat()
    except FileNotFoundError as error:
        raise AssertionError(f'missing generated checkpoint key: {WHAT_IF_KEY.as_posix()}') from error
    if not stat.S_ISREG(metadata.st_mode):
        raise AssertionError('generated checkpoint key is not a regular file')
    if os.name == 'posix' and stat.S_IMODE(metadata.st_mode) != 0o600:
        raise AssertionError('generated checkpoint key is not mode 0600')
    contents = key.read_bytes()
    if len(contents) != 32:
        raise AssertionError('generated checkpoint key is not exactly 32 bytes')

    actual = {path.relative_to(state_home).as_posix() for path in state_home.rglob('*')}
    expected_paths = {'execute', 'execute/what-if.key'}
    if actual != expected_paths:
        raise AssertionError(f'unexpected isolated state entries: {sorted(actual - expected_paths)}')
    return contents


def audit_outputs(cwd, allowed, allowed_prefixes=()):
    """Reject every output outside the case's exact oracle and allowances."""
    for path in cwd.rglob('*'):
        name = path.relative_to(cwd).as_posix()
        if path.is_file() and name not in allowed and not any(name.startswith(prefix) for prefix in allowed_prefixes):
            raise AssertionError(f'undeclared output file: {name}')


def run_case(binary, case, program_dir=HERE, manifests=PROJECT / 'manifests'):
    with tempfile.TemporaryDirectory(prefix=f'execute-utility-{case["number"]:03}-') as tmp:
        cwd = Path(tmp)
        script = materialize(cwd, case, program_dir)
        catalog = ['--manifest-dir', str(manifests)]
        for command in ['check', 'explain']:
            supplied = [item for name, spec in case.get('parameters', {}).items() for item in ['--input', f'{name}={spec["default"]}']]
            checked = invoke(binary, cwd, [command, script, *catalog, *supplied])
            assert checked['code'] == 0, checked['stderr'][:4096].decode(errors='replace')
        args = [script, *catalog, *case.get('args', []), '--trace']
        if case.get('mode') == 'cache':
            args += ['--cache-dir', 'cache']
        first = interactive(binary, cwd, args, case) if case.get('mode') in {'interactive', 'tail', 'tail_stream'} else invoke(binary, cwd, args, case.get('timeout', 10), case.get('stdin', '').encode())
        verify(first, cwd, case)
        key_contents = verify_isolated_state(cwd, case.get('mode') == 'cache')
        for name, data in mapping_bytes(case, 'fixtures', 'fixtures_base64').items():
            if name not in case['outputs']:
                expected_input = data + case.get('stdin', '').encode() if name == case.get('append_path') else data
                assert (cwd / name).read_bytes() == expected_input, f'undeclared mutation of input: {name}'
        allowed = set(mapping_bytes(case, 'fixtures', 'fixtures_base64')) | set(case['outputs']) | {case['file']}
        if key_contents is not None:
            allowed.add(WHAT_IF_KEY.as_posix())
        cache_prefixes = ('cache/',) if case.get('mode') == 'cache' else ()
        audit_outputs(cwd, allowed, cache_prefixes)
        phases = {'first': summarize(first, cwd, case)}
        if case.get('mode') == 'cache':
            for name, text in case.get('mutate', {}).items():
                (cwd / relative_path(name)).write_bytes(text.encode())
            after = invoke(binary, cwd, args, case.get('timeout', 10), case.get('stdin', '').encode())
            verify(after, cwd, case, 'after')
            if verify_isolated_state(cwd, True) != key_contents:
                raise AssertionError('checkpoint key changed during the cache reuse phase')
            audit_outputs(cwd, allowed, cache_prefixes)
            traces = events(after)
            if case.get('after_event'):
                assert any(e['event'] == case['after_event'] for e in traces), f'missing {case["after_event"]}'
                if case['after_event'] == 'cache_store':
                    assert not any(e['event'] == 'cache_hit' for e in traces)
            phases['after'] = summarize(after, cwd, case, 'after')
        return phases


def run_rejection(binary, case, rejection):
    bad_input = merged_variant(case, rejection)
    with tempfile.TemporaryDirectory(prefix='execute-rejected-input-') as tmp:
        cwd = Path(tmp)
        script = materialize(cwd, bad_input)
        result = invoke(binary, cwd, [script, '--manifest-dir', PROJECT / 'manifests', '--trace'],
                        case.get('timeout', 10), bad_input.get('stdin', '').encode())
        assert result['code'] != 0, 'invalid input was accepted'
        assert result['stdout'] == expected_bytes(bad_input), 'unexpected partial output on rejected input'
        assert rejection['stderr_contains'].encode() in result['stderr'], 'missing rejection diagnostic'
        for name in rejection.get('absent_outputs', [n for n in case['outputs'] if n != 'stdout']):
            assert not (cwd / relative_path(name)).exists(), f'rejected input created {name}'
        return {'name': rejection['name'], 'exit_code': result['code'],
                'stdout_bytes': len(result['stdout']), 'expected_diagnostic': rejection['stderr_contains']}


def prepare(cwd, case, program_dir=HERE):
    if cwd.exists() and any(cwd.iterdir()):
        raise ValueError('--prepare requires a new or empty destination directory')
    cwd.mkdir(parents=True, exist_ok=True)
    materialize(cwd, case, program_dir)
    metadata = cwd / '.utility'
    metadata.mkdir()
    if 'stdin' in case:
        (metadata / 'sample-stdin.txt').write_bytes(case['stdin'].encode())
    contract = {k: case[k] for k in ['number', 'title', 'purpose', 'inputs', 'outputs', 'applets']}
    contract['parameters'] = case.get('parameters', {})
    (metadata / 'contract.json').write_text(json.dumps(contract, indent=2, ensure_ascii=False) + '\n')
    expected_root = metadata / 'expected'
    expected_root.mkdir()
    (expected_root / 'stdout').write_bytes(expected_bytes(case))
    for name, data in mapping_bytes(case, 'files', 'files_base64').items():
        target = expected_root / 'files' / name
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_bytes(data)
    command = f'execute {case["file"]}'
    if 'stdin' in case and not case.get('append_path'):
        command = 'cat .utility/sample-stdin.txt | ' + command
    details = f'# {case["title"]}\n\n{case["purpose"]}\n\nRun from this directory:\n\n```sh\n{command}\n```\n\nReplace the sample input files with your own data. File paths are relative to the calling directory. Sample expected stdout/files are in `.utility/expected/`.\n\n'
    if case.get('parameters'):
        details += 'Path inputs can also be changed without editing the program:\n\n'
        for name, spec in case['parameters'].items():
            details += f'- `{name}` defaults to `{spec["default"]}`.\n'
        name = next(iter(case['parameters']))
        details += '\n```sh\n' + shlex.join(['execute', case['file'], '--input', f'{name}=path/to/your/input']) + '\n```\n\nOutput paths retain the destinations documented below.\n\n'
    if case.get('append_path'):
        details += f'Start the program, then append .utility/sample-stdin.txt to {case["append_path"]} from another terminal.\n\n'
    if case.get('mode') in {'interval', 'calendar', 'listener'}:
        details += 'This is a foreground service; Ctrl-C stops it. The catalog test uses bounded runner options.\n\n'
    for key in ['inputs', 'outputs']:
        details += key.capitalize() + ':\n\n' + ''.join(f'- `{name}`: {description}\n' for name, description in case[key].items()) + '\n'
    (metadata / 'README.md').write_text(details)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, default=PROJECT / 'bin/execute')
    parser.add_argument('--case', type=int, action='append', help='Run only these IDs (1..150); repeatable')
    parser.add_argument('--category', help='Run one category')
    parser.add_argument('--report', type=Path)
    parser.add_argument('--list', action='store_true', help='List useful tasks without running them')
    parser.add_argument('--check-catalog', action='store_true')
    parser.add_argument('--prepare', type=Path, help='Export one selected utility with editable input fixtures')
    args = parser.parse_args()
    try:
        cases = load_catalog()
        if args.case and any(number not in range(1, TOTAL + 1) for number in args.case):
            parser.error('--case must be between 1 and 150')
        selected = [c for c in cases if (not args.case or c['number'] in args.case) and (not args.category or c['category'] == args.category)]
        if not selected:
            parser.error('no utilities match the selection')
        if args.list or args.check_catalog:
            if args.list:
                for c in selected:
                    print(f'{c["number"]:03} {c["category"]}: {c["title"]}')
            print(f'{len(cases)} program contracts validated')
            return 0
        if args.prepare:
            if len(selected) != 1:
                parser.error('--prepare requires exactly one selected --case')
            prepare(args.prepare.resolve(), selected[0])
            print(f'Prepared {selected[0]["file"]} in {args.prepare.resolve()}; see .utility/README.md')
            return 0
        results = []
        for case in selected:
            item = {k: case[k] for k in ['number', 'file', 'title', 'category']}
            try:
                item['phases'] = run_case(args.binary.resolve(), case)
                item['variants'] = []
                for variant in case.get('variants', []):
                    item['variants'].append({'name': variant['name'], 'phases': run_case(args.binary.resolve(), merged_variant(case, variant))})
                item['rejections'] = [run_rejection(args.binary.resolve(), case, rejection) for rejection in case.get('rejections', [])]
                item['passed'] = True
                print(f'{case["number"]:03} PASS {case["title"]}', flush=True)
            except Exception as error:
                item.update(passed=False, error=str(error)[:4096])
                print(f'{case["number"]:03} FAIL {case["title"]}: {str(error)[:4096]}', flush=True)
            results.append(item)
        with tempfile.TemporaryDirectory(prefix='execute-version-') as directory:
            version = invoke(args.binary.resolve(), Path(directory), ['--version'], 5, max_capture=4096)
        assert version['code'] == 0, 'version command failed'
        report = {'execute_version': version['stdout'].decode().strip(),
                  'binary_sha256': hashlib.sha256(args.binary.read_bytes()).hexdigest(), 'platform': sys.platform,
                  'cases_run': len(results), 'passed': sum(r['passed'] for r in results),
                  'input_variants_run': sum(len(r.get('variants', [])) for r in results),
                  'invalid_inputs_rejected': sum(len(r.get('rejections', [])) for r in results),
                  'method': 'owned temp directory per case/variant; bounded bytes and process deadlines; independent exact stdout/file oracles; check/explain before effects; single duration observations, not latency benchmarks',
                  'results': results}
        if args.report:
            args.report.write_text(json.dumps(report, indent=2) + '\n')
        print(f'{report["passed"]}/{len(results)} passed; {report["input_variants_run"]} alternate inputs checked', flush=True)
        return 0 if report['passed'] == len(results) else 1
    except (ValueError, OSError) as error:
        parser.error(str(error))


if __name__ == '__main__':
    raise SystemExit(main())
