#!/usr/bin/env python3
"""Safe, stratified Bash/Execute differential cases; no replacement score.

Only explicit system binaries, bounded fixtures and private output files. This
is a command/argv matrix, not 100 distinct tools or a held-out Bash-script study.
"""
from __future__ import annotations

import argparse
from collections import Counter
from dataclasses import dataclass
from datetime import datetime, timezone
import hashlib
import json
from pathlib import Path
import platform
import random
import shlex
import statistics
import tempfile

from runtime_differential import bounded_command

ROOT = Path(__file__).resolve().parents[1]


class ProofFailure(Exception):
    pass


def require(condition, message):
    if not condition:
        raise ProofFailure(message)


def digest(data):
    return hashlib.sha256(data).hexdigest()


def system_tool(name):
    # Do not inherit PATH aliases, user binaries, shell builtins or startup hooks.
    for parent in ('/usr/bin', '/bin'):
        path = Path(parent) / name
        if path.is_file():
            return str(path)
    raise ProofFailure(f'required system tool unavailable: {name}')


@dataclass(frozen=True)
class Case:
    name: str
    category: str
    stages: tuple
    expected: bytes | None = None
    code: int = 0
    difference: str | None = None
    execute_source: str | None = None
    bash_source: str | None = None
    files: tuple = ()
    inputs: tuple = ()


def cases():
    result = []

    def add(name, category, *stages, expected=None, code=0, **kwargs):
        result.append(Case(name, category, tuple(tuple(stage) for stage in stages), expected, code, **kwargs))

    # NUL-delimited native printf is an independent argv round-trip oracle.
    for label, values in {
        'empty': [''], 'spaces': ['a b', ' c '], 'unicode': ['café', '東京', '🐈'],
        'newlines': ['one\ntwo', 'trailing\n'], 'tabs': ['a\tb', '\r'],
        'quotes': ['"double"', "'single'"], 'backslashes': ['a\\b', '\\n'],
        'wildcards': ['*.txt', '?', '[ab]'], 'dollars': ['$HOME', '${x}', '$(touch forbidden)'],
        'operators': ['; touch forbidden', '| cat', '> forbidden', '&& false'],
        'comments': ['# a comment', 'a#b'], 'leading-options': ['--', '-n', '-e'],
    }.items():
        add('argv-' + label, 'literal-argv', ['printf', '%s\\0', *values],
            expected=b''.join(value.encode() + b'\0' for value in values))

    # Multiple operations per tool deliberately count as cases, not new tools.
    specs = [
        ('cat-text', ['cat', 'lines.txt'], b'beta\nalpha\nbeta\ngamma\n'),
        ('cat-binary', ['cat', 'binary.bin'], bytes(range(256)) * 64),
        ('cat-files', ['cat', 'part-a.txt', 'part-b.txt'], b'first\nsecond\n'),
        ('cat-space-unicode', ['cat', 'space 雪.txt'], 'snow 雪\n'.encode()),
        ('cat-dash', ['cat', './-dash.txt'], b'dash\n'),
        ('head-lines', ['head', '-n', '2', 'lines.txt'], b'beta\nalpha\n'),
        ('head-bytes', ['head', '-c', '5', 'binary.bin'], bytes(range(5))),
        ('tail-lines', ['tail', '-n', '2', 'lines.txt'], b'beta\ngamma\n'),
        ('tail-from', ['tail', '-n', '+3', 'lines.txt'], b'beta\ngamma\n'),
        ('sort-lines', ['sort', 'lines.txt'], b'alpha\nbeta\nbeta\ngamma\n'),
        ('sort-unique', ['sort', '-u', 'lines.txt'], b'alpha\nbeta\ngamma\n'),
        ('sort-reverse', ['sort', '-r', 'lines.txt'], b'gamma\nbeta\nbeta\nalpha\n'),
        ('sort-numeric', ['sort', '-n', 'numbers.txt'], b'-1\n2\n10\n'),
        ('uniq-lines', ['uniq', 'sorted.txt'], b'alpha\nbeta\ngamma\n'),
        ('uniq-duplicates', ['uniq', '-d', 'sorted.txt'], b'beta\n'),
        ('uniq-singletons', ['uniq', '-u', 'sorted.txt'], b'alpha\ngamma\n'),
        ('grep-match', ['grep', '-F', 'beta', 'lines.txt'], b'beta\nbeta\n'),
        ('grep-invert', ['grep', '-v', 'beta', 'lines.txt'], b'alpha\ngamma\n'),
        ('grep-number', ['grep', '-n', '^alpha$', 'lines.txt'], b'2:alpha\n'),
        ('grep-count', ['grep', '-c', 'beta', 'lines.txt'], b'2\n'),
        ('sed-replace', ['sed', 's/beta/BETA/g', 'lines.txt'], b'BETA\nalpha\nBETA\ngamma\n'),
        ('sed-select', ['sed', '-n', '2,3p', 'lines.txt'], b'alpha\nbeta\n'),
        ('sed-delete', ['sed', '/beta/d', 'lines.txt'], b'alpha\ngamma\n'),
        ('awk-fields', ['awk', '-F,', '{print $2}', 'table.csv'], b'10\n20\n'),
        ('awk-sum', ['awk', '-F,', '{sum += $2} END {print sum}', 'table.csv'], b'30\n'),
        ('cut-fields', ['cut', '-d,', '-f', '1', 'table.csv'], b'alice\nbob\n'),
        ('cut-bytes', ['cut', '-b', '1-2', 'lines.txt'], b'be\nal\nbe\nga\n'),
        ('paste-files', ['paste', 'part-a.txt', 'part-b.txt'], b'first\tsecond\n'),
        ('paste-serial', ['paste', '-s', '-d,', 'lines.txt'], b'beta,alpha,beta,gamma\n'),
        ('join-files', ['join', 'left.txt', 'right.txt'], b'a 1 x\nb 2 y\n'),
        ('comm-common', ['comm', '-12', 'left-names.txt', 'right-names.txt'], b'beta\n'),
        ('comm-left', ['comm', '-23', 'left-names.txt', 'right-names.txt'], b'alpha\n'),
        ('rev-lines', ['rev', 'part-a.txt'], b'tsrif\n'),
        ('basename', ['basename', 'folder/name.txt'], b'name.txt\n'),
        ('basename-suffix', ['basename', 'folder/name.txt', '.txt'], b'name\n'),
        ('dirname', ['dirname', 'folder/name.txt'], b'folder\n'),
        ('dirname-root', ['dirname', '/name'], b'/\n'),
        ('expr-add', ['expr', '12', '+', '7'], b'19\n'),
        ('expr-length', ['expr', 'hello', ':', '.*'], b'5\n'),
        ('true', ['true'], b''),
        ('test-regular', ['test', '-f', 'lines.txt'], b''),
        ('test-directory', ['test', '-d', 'sub'], b''),
        ('test-equality', ['test', 'a b', '=', 'a b'], b''),
        ('printf-format', ['printf', '%04d:%s\n', '7', 'ok'], b'0007:ok\n'),
        ('echo-literal', ['echo', 'ordinary', 'text'], b'ordinary text\n'),
        ('ls-files', ['ls', '-1', 'sub'], b'a.txt\nb.txt\n'),
        ('find-files', ['find', 'sub', '-type', 'f', '-name', '*.txt', '-print'], None),
        ('wc-lines', ['wc', '-l', 'lines.txt'], None),
        ('wc-bytes', ['wc', '-c', 'binary.bin'], None),
        ('wc-words', ['wc', '-w', 'words.txt'], None),
        ('cksum', ['cksum', 'binary.bin'], None),
        ('shasum', ['shasum', '-a', '256', 'binary.bin'], None),
        ('md5-openssl', ['openssl', 'dgst', '-sha256', 'binary.bin'], None),
        ('base64', ['base64', '-i', 'part-a.txt'], None),
        ('od-bytes', ['od', '-An', '-tx1', 'binary.bin'], None),
        ('hexdump', ['hexdump', '-C', 'part-a.txt'], None),
        ('xxd', ['xxd', '-p', 'part-a.txt'], b'66697273740a\n'),
        ('gzip', ['gzip', '-n', '-c', 'binary.bin'], None),
        ('bzip2', ['bzip2', '-c', 'binary.bin'], None),
        ('cmp-same', ['cmp', 'part-a.txt', 'part-a.txt'], b''),
        ('diff-same', ['diff', 'part-a.txt', 'part-a.txt'], b''),
        ('getconf', ['getconf', 'ARG_MAX'], None),
        ('uname-system', ['uname', '-s'], None),
        ('uname-machine', ['uname', '-m'], None),
        ('pwd', ['pwd', '-P'], None),
        ('du-owned', ['du', '-k', 'sub'], None),
    ]
    for name, argv, expected in specs:
        add(name, 'native-tool', argv, expected=expected)

    pipes = [
        ('uppercase', [['cat', 'lines.txt'], ['tr', '[:lower:]', '[:upper:]']], b'BETA\nALPHA\nBETA\nGAMMA\n'),
        ('sort-uniq', [['cat', 'lines.txt'], ['sort'], ['uniq']], b'alpha\nbeta\ngamma\n'),
        ('numeric-sort', [['cat', 'numbers.txt'], ['sort', '-n']], b'-1\n2\n10\n'),
        ('filter-count', [['cat', 'lines.txt'], ['grep', 'beta'], ['wc', '-l']], None),
        ('binary-checksum', [['cat', 'binary.bin'], ['cksum']], None),
        ('binary-identity', [['cat', 'binary.bin'], ['cat'], ['cat']], bytes(range(256)) * 64),
        ('binary-uppercase', [['cat', 'binary.bin'], ['tr', 'a-z', 'A-Z']], (bytes(range(256)) * 64).translate(bytes.maketrans(b'abcdefghijklmnopqrstuvwxyz', b'ABCDEFGHIJKLMNOPQRSTUVWXYZ'))),
        ('remove-cr', [['cat', 'crlf.txt'], ['tr', '-d', '\r']], b'one\ntwo\n'),
        ('squeeze-space', [['cat', 'words.txt'], ['tr', '-s', ' ']], b'one two three\n'),
        ('expand-tabs', [['cat', 'tabs.txt'], ['expand', '-t', '4']], b'a   b\n'),
        ('unexpand-tabs', [['cat', 'spaces.txt'], ['unexpand', '-a', '-t', '4']], b'\ta\n'),
        ('fold-width', [['printf', 'abcdef\n'], ['fold', '-w', '3']], b'abc\ndef\n'),
        ('format-paragraph', [['cat', 'paragraph.txt'], ['fmt', '-w', '12']], None),
        ('number-lines', [['cat', 'part-a.txt'], ['nl', '-ba']], None),
        ('csv-select-sort', [['cat', 'table.csv'], ['cut', '-d,', '-f', '1'], ['sort', '-r']], b'bob\nalice\n'),
        ('gzip-roundtrip', [['gzip', '-n', '-c', 'binary.bin'], ['gzip', '-d', '-c']], bytes(range(256)) * 64),
        ('base64-roundtrip', [['base64', '-i', 'binary.bin'], ['base64', '-d']], bytes(range(256)) * 64),
        ('bzip2-roundtrip', [['bzip2', '-c', 'binary.bin'], ['bzip2', '-d', '-c']], bytes(range(256)) * 64),
        ('tee-owned', [['cat', 'binary.bin'], ['tee', 'copy.bin'], ['cksum']], None),
    ]
    for name, stages, expected in pipes:
        add('pipe-' + name, 'native-pipeline', *stages, expected=expected,
            files=('copy.bin',) if name == 'tee-owned' else ())

    add('typed-path-and-count', 'cli-inputs', ['head', '-n', '2', 'lines.txt'], expected=b'beta\nalpha\n',
        execute_source='accept path as text\naccept count as integer\nrun ' + json.dumps(system_tool('head')) +
        ' with arguments ["-n", count, path]\n', inputs=('path=lines.txt', 'count=2'))
    add('typed-multiple-paths', 'cli-inputs', ['cat', 'part-a.txt', 'part-b.txt'], expected=b'first\nsecond\n',
        execute_source='accept paths as list of text\nrun ' + json.dumps(system_tool('cat')) +
        ' with arguments paths\n', inputs=('paths=["part-a.txt","part-b.txt"]',))
    add('typed-boolean', 'cli-inputs', ['printf', '%s', 'true'], expected=b'true',
        execute_source='accept enabled as boolean\nrun ' + json.dumps(system_tool('printf')) +
        ' with arguments ["%s", enabled]\n', inputs=('enabled=true',))

    # Explicitly classified contract differences; never folded into match counts.
    for name, argv, status in [('false', ['false'], 1), ('grep-missing', ['grep', 'absent', 'lines.txt'], 1),
                               ('cmp-different', ['cmp', '-s', 'part-a.txt', 'part-b.txt'], 1),
                               ('test-missing', ['test', '-f', 'missing'], 1), ('expr-zero', ['expr', '1', '-', '1'], 1)]:
        add('failure-' + name, 'failure-contract', argv, code=status, expected=b'0\n' if name == 'expr-zero' else b'',
            difference='native_failure_diagnostic_and_buffered_stdout')
    add('failure-code-two', 'failure-contract', ['grep', '[', 'lines.txt'], code=2, expected=b'',
        difference='native_failure_diagnostic_and_buffered_stdout')
    # NUL framing makes absence of expansion observable without invoking a shell.
    printf = shlex.quote(system_tool('printf'))
    add('glob-is-literal', 'intentional-language-difference', ['printf', '%s\\0', 'part-*.txt'],
        expected=b'part-*.txt\0', difference='no_implicit_glob',
        bash_source=f"{printf} '%s\\0' part-*.txt\n")
    add('failure-native-seven', 'failure-contract', ['awk', 'BEGIN {exit 7}'], code=7, expected=b'',
        difference='native_failure_diagnostic_and_buffered_stdout')
    add('failure-native-255', 'failure-contract', ['awk', 'BEGIN {exit 255}'], code=255, expected=b'',
        difference='native_failure_diagnostic_and_buffered_stdout')
    add('failure-middle-stage', 'failure-contract', ['cat', 'part-a.txt'], ['awk', '{exit 7}'], ['cat'],
        code=0, expected=b'', difference='strict_pipeline_failure')
    return result


def fixtures(folder):
    data = {'lines.txt': b'beta\nalpha\nbeta\ngamma\n', 'sorted.txt': b'alpha\nbeta\nbeta\ngamma\n',
            'binary.bin': bytes(range(256)) * 64, 'part-a.txt': b'first\n', 'part-b.txt': b'second\n',
            'space 雪.txt': 'snow 雪\n'.encode(), '-dash.txt': b'dash\n', 'numbers.txt': b'10\n2\n-1\n',
            'table.csv': b'alice,10\nbob,20\n', 'left.txt': b'a 1\nb 2\n', 'right.txt': b'a x\nb y\n',
            'left-names.txt': b'alpha\nbeta\n', 'right-names.txt': b'beta\ngamma\n',
            'words.txt': b'one  two   three\n', 'tabs.txt': b'a\tb\n', 'spaces.txt': b'    a\n',
            'crlf.txt': b'one\r\ntwo\r\n', 'paragraph.txt': b'one two three four five six\n',
            'sub/a.txt': b'a\n', 'sub/b.txt': b'b\n'}
    for name, content in data.items():
        path = folder / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(content)
    return {name: digest(content) for name, content in data.items()}


def sources(case):
    stages = [[system_tool(stage[0]), *stage[1:]] for stage in case.stages]
    execute = ''.join(('run ' if index == 0 else 'pipe to ') + json.dumps(stage[0]) +
                      ' with arguments ' + json.dumps(stage[1:], ensure_ascii=False) + '\n'
                      for index, stage in enumerate(stages))
    bash = ' | '.join(shlex.join(stage) for stage in stages) + '\n'
    return stages, case.execute_source or execute, case.bash_source or bash


def plans(output):
    decoder = json.JSONDecoder()
    return [decoder.raw_decode(line.split(' argv=', 1)[1])[0]
            for line in output.decode().splitlines() if ' argv=' in line]


def run(command, folder, env):
    code, out, err, elapsed, error, cleanup = bounded_command(command, folder, env, 8, 1024 * 1024)
    require(error is None, f'{command}: {error}; cleanup={cleanup}')
    return dict(code=code, stdout=out, stderr=err, elapsed_ms=elapsed / 1e6)


def check(case, execute, bash):
    if case.difference in ('native_failure_diagnostic_and_buffered_stdout', 'strict_pipeline_failure'):
        require(bash['code'] == case.code, f'{case.name}: Bash status {bash["code"]} != {case.code}')
        require(execute['code'] == 1 and b'execute:' in execute['stderr'], f'{case.name}: missing Execute failure')
        require(execute['stdout'] == b'', f'{case.name}: failed finite output escaped')
        native_code = 7 if case.difference == 'strict_pipeline_failure' else case.code
        require(f'exit status: {native_code}'.encode() in execute['stderr'], f'{case.name}: wrong native failure status')
        require(bash['stderr'] in execute['stderr'], f'{case.name}: native stderr was lost')
        if case.expected is not None:
            require(bash['stdout'] == case.expected, f'{case.name}: native failure stdout oracle mismatch')
    elif case.difference == 'no_implicit_glob':
        require(execute['code'] == bash['code'] == 0, f'{case.name}: nonzero status')
        require(execute['stdout'] == case.expected and bash['stdout'] == b'part-a.txt\0part-b.txt\0', f'{case.name}: glob contract mismatch')
        require(execute['stderr'] == bash['stderr'] == b'', f'{case.name}: unexpected stderr')
    else:
        require(execute['code'] == bash['code'] == case.code, f'{case.name}: unexpected status')
        require(execute['stdout'] == bash['stdout'], f'{case.name}: different stdout')
        require(execute['stderr'] == bash['stderr'] == b'', f'{case.name}: different/nonempty stderr')
        if case.expected is not None:
            require(execute['stdout'] == case.expected, f'{case.name}: independent output oracle mismatch')


def observation(value):
    return {'code': value['code'], 'stdout_bytes': len(value['stdout']), 'stdout_sha256': digest(value['stdout']),
            'stderr_bytes': len(value['stderr']), 'stderr_sha256': digest(value['stderr']),
            'stderr': value['stderr'].decode(errors='replace'), 'elapsed_ms': value['elapsed_ms']}


def check_spawns(name, stages, events):
    require([event['binary'] for event in events] == [stage[0] for stage in stages],
            f'{name}: native spawn identities differ')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, required=True)
    parser.add_argument('--report', type=Path, required=True)
    parser.add_argument('--samples', type=int, default=1, help='paired correctness rounds, 1..100; not performance proof')
    parser.add_argument('--warmups', type=int, default=0, help='0..10 paired rounds excluded from timing summaries')
    args = parser.parse_args()
    if not 1 <= args.samples <= 100:
        parser.error('--samples must be 1..100')
    if not 0 <= args.warmups <= 10:
        parser.error('--warmups must be 0..10')
    binary = args.binary.resolve(strict=True)
    bash_binary = system_tool('bash')
    matrix = cases()
    require(len({case.name for case in matrix}) == len(matrix), 'duplicate case names')
    report = {'at_utc': datetime.now(timezone.utc).isoformat(), 'status': 'running',
              'platform': platform.platform(), 'binary': str(binary), 'binary_sha256': digest(binary.read_bytes()),
              'harness_sha256': digest(Path(__file__).read_bytes()), 'bash_sha256': digest(Path(bash_binary).read_bytes()),
              'runner_sha256': digest((ROOT / 'benchmarks/runtime_differential.py').read_bytes()),
              'case_count': len(matrix), 'categories': dict(Counter(case.category for case in matrix)),
              'distinct_native_tools': sorted({stage[0] for case in matrix for stage in case.stages}),
              'method': 'Explicit absolute system executables, same argv from a shared specification except the marked glob-expansion difference; Execute explain verifies argv and separate traces verify native spawn identities/count. Bash files use shlex quoting except that glob test, with no startup hooks or builtin applets. NUL-framed native printf independently checks adversarial argv transport. Exact exit/stdout/stderr per case, independent expected output where supplied; otherwise Bash is the byte oracle. Marked failure cases check distinct expected status/diagnostic/output contracts. Traces and explain are excluded from timed observations. Owned fixtures only, minimal environment, one-entry unused catalog (not the normal project catalog), 8s/1MiB per invocation. Sequential seeded interleaved pairs, warm filesystem, fresh processes; requested warmups excluded from summaries but retained. Timing includes Python spawn/capture/exit and runner overhead. Descriptive per-case observations only; no general speedup, isolated interpreter/pipe latency, normal-catalog startup, cold-cache, native Linux, 100 distinct commands, held-out script/model, full-replacement or score claim.',
              'samples': args.samples, 'warmups': args.warmups, 'cases': [], 'tools': {}}
    args.report.parent.mkdir(parents=True, exist_ok=True)

    def save():
        args.report.write_text(json.dumps(report, indent=2, ensure_ascii=False) + '\n')

    save()
    try:
        with tempfile.TemporaryDirectory(prefix='execute unix matrix ') as temporary:
            folder = Path(temporary)
            report['fixture_directory'] = str(folder)
            report['fixture_sha256'] = fixtures(folder)
            catalog = folder / 'minimal-catalog'
            catalog.mkdir()
            manifest = json.dumps({'name': 'unused', 'binary': system_tool('true'), 'arguments': []})
            (catalog / 'unused.json').write_text(manifest)
            report['unused_manifest_sha256'] = digest(manifest.encode())
            env = {'PATH': '/usr/bin:/bin', 'LC_ALL': 'C', 'LANG': 'C', 'HOME': str(folder), 'TMPDIR': str(folder)}
            rng = random.Random(20261014)
            for case in matrix:
                stages, execute_source, bash_source = sources(case)
                for stage in stages:
                    report['tools'].setdefault(stage[0], digest(Path(stage[0]).read_bytes()))
                program = folder / 'case.program'
                shell = folder / 'case.bash'
                program.write_text(execute_source)
                shell.write_text(bash_source)
                common = [str(program), '--manifest-dir', str(catalog)]
                for value in case.inputs:
                    common += ['--input', value]
                command = [str(binary), *common, '--command-timeout-ms', '5000']
                plan = run([str(binary), 'explain', *common], folder, env)
                require(plan['code'] == 0 and not plan['stderr'], f'{case.name}: plan failed {plan["stderr"]!r}')
                require(plans(plan['stdout']) == [stage[1:] for stage in stages], f'{case.name}: argv plan mismatch')
                for stage in stages:
                    require(json.dumps(stage[0]).encode() in plan['stdout'], f'{case.name}: executable plan mismatch')
                row = {'name': case.name, 'category': case.category, 'classification': 'intentional_difference' if case.difference else 'exact_match',
                       'difference': case.difference, 'native_argv': stages, 'program': execute_source, 'bash': bash_source,
                       'independent_output_oracle': case.expected is not None, 'inputs': case.inputs,
                       'observations': [], 'status': 'running'}
                report['cases'].append(row)
                for trial in range(args.samples + args.warmups):
                    commands = {'execute': command, 'bash': [bash_binary, '--noprofile', '--norc', str(shell)]}
                    order = list(commands)
                    rng.shuffle(order)
                    results = {}
                    outputs = {}
                    for side in order:
                        for name in case.files:
                            (folder / name).unlink(missing_ok=True)
                        results[side] = run(commands[side], folder, env)
                        outputs[side] = {name: digest((folder / name).read_bytes()) for name in case.files}
                        require(not (folder / 'forbidden').exists(), f'{case.name}: shell-looking argument executed')
                    row['observations'].append({'warmup': trial < args.warmups,
                                                **{side: observation(value) for side, value in results.items()}})
                    check(case, results['execute'], results['bash'])
                    require(outputs['execute'] == outputs['bash'], f'{case.name}: output files differ')
                    if case.files:
                        require(outputs['execute']['copy.bin'] == report['fixture_sha256']['binary.bin'], f'{case.name}: tee bytes wrong')
                trace = run(command + ['--trace'], folder, env)
                events = []
                for line in trace['stderr'].splitlines():
                    try:
                        event = json.loads(line)
                        if isinstance(event, dict) and event.get('event') == 'spawn':
                            events.append(event)
                    except (ValueError, UnicodeDecodeError):
                        pass
                check_spawns(case.name, stages, events)
                # A trace invocation must satisfy the same outcome, with trace rows
                # removed; the diagnostic is checked on the ordinary invocation.
                require(trace['code'] == results['execute']['code'] and trace['stdout'] == results['execute']['stdout'], f'{case.name}: trace outcome differs')
                row['native_spawns'] = [{'binary': event['binary'], 'line': event['line']} for event in events]
                row['timings_ms'] = {}
                for side in ('execute', 'bash'):
                    raw = [sample[side]['elapsed_ms'] for sample in row['observations'] if not sample['warmup']]
                    row['timings_ms'][side] = {'p50': statistics.median(raw),
                                               'p95': sorted(raw)[(95 * len(raw) + 99) // 100 - 1], 'raw': raw}
                row['status'] = 'passed'
                save()
            report['status'] = 'passed'
            report['summary'] = {'exact_matches': sum(not case.difference for case in matrix),
                                 'intentional_differences': sum(bool(case.difference) for case in matrix),
                                 'distinct_tools': len(report['distinct_native_tools']), 'paired_observations': len(matrix) * args.samples,
                                 'unexplained_mismatches': 0}
    except Exception as error:
        report['status'] = 'failed'
        report['error'] = f'{type(error).__name__}: {error}'
        raise
    finally:
        report['finished_at_utc'] = datetime.now(timezone.utc).isoformat()
        save()
    print(json.dumps(report['summary'], indent=2))


if __name__ == '__main__':
    main()
