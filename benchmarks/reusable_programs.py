#!/usr/bin/env python3
"""Measure same-file scope overhead against equivalent explicit inlined programs."""
import argparse
from datetime import datetime, timezone
import hashlib
import json
from pathlib import Path
import platform
import random
import subprocess
import tempfile
import time
from direct_commands import summary

PROJECT = Path(__file__).resolve().parents[1]


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--baseline', type=Path, required=True)
    parser.add_argument('--candidate', type=Path, required=True)
    parser.add_argument('--samples', type=int, default=80)
    parser.add_argument('--report', type=Path, required=True)
    parser.add_argument('--baseline-programs', action='store_true', help='Compare identical reusable sources on two function-capable builds')
    args = parser.parse_args()
    assert args.samples >= 20
    binaries = {name: getattr(args, name).resolve() for name in ['baseline', 'candidate']}
    rng = random.Random(1014202614)
    cases = {
        'native_printf_control': ('run "printf" with arguments ["%s", "owned"]\n', None, b'owned', [], 1),
        'native_pipe_control': ('run "printf" with arguments ["%s", "owned"]\npipe to "cat"\npipe to "cat"\n', None, b'owned', [], 3),
        'check_1000_control': ('run "true"\n' * 1000, None, b'', [], None),
        'native_calls_50': ('run "printf" with arguments ["%s", "owned"]\n' * 50,
                            'define a program named emit\n  accept message as text\n  run "printf" with arguments ["%s", message]\n' + 'run program emit with message "owned"\n' * 50,
                            b'owned', [], 50),
        'pure_calls_1000': ('then encode 7 as json\n' * 1000,
                            'define a program named emit\n  accept value as integer\n  then encode value as json\n' + 'run program emit with value 7\n' * 1000,
                            b'7', [], 0),
        'json_64k_inputs_100_calls': ('accept data as json\n' + ''.join(f'take integer at "/count" from data as count_{n}\nthen encode count_{n} as json\n' for n in range(100)),
                                     'define a program named select\n  accept data as json\n  take integer at "/count" from data as count\n  then encode count as json\naccept data as json\n' + 'run program select with data data\n' * 100,
                                     b'42', ['--input', 'data=' + json.dumps({'count': 42, 'padding': 'x' * 65536})], 0),
    }
    results = {}
    with tempfile.TemporaryDirectory(prefix='execute-reuse-bench-') as directory:
        root = Path(directory)
        for case, (inline, reused, oracle, supplied, expected_launches) in cases.items():
            baseline_key = 'baseline_programs' if args.baseline_programs and reused is not None else 'baseline_inline'
            sources = {baseline_key: reused if baseline_key == 'baseline_programs' else inline, 'candidate_inline': inline}
            if reused is not None:
                sources['candidate_programs'] = reused
            launches = {}
            raw = {name: [] for name in sources}
            commands = {}
            for name, source in sources.items():
                file = root / (name + '.program')
                file.write_text(source)
                binary = binaries['baseline' if name.startswith('baseline') else 'candidate']
                commands[name] = [str(binary)] + (['check'] if case.startswith('check_') else []) + [str(file), '--manifest-dir', str(PROJECT / 'manifests')] + supplied
                if expected_launches is not None:
                    probe = subprocess.run(commands[name] + ['--trace'], cwd=root, input=b'', capture_output=True, timeout=20)
                    assert probe.returncode == 0 and probe.stdout == oracle, (case, name, probe)
                    launches[name] = sum(json.loads(line).get('event') == 'spawn' for line in probe.stderr.splitlines())
                    assert launches[name] == expected_launches, (case, name, launches)
            for trial in range(args.samples + 5):
                order = list(sources)
                rng.shuffle(order)
                for name in order:
                    started = time.perf_counter_ns()
                    result = subprocess.run(commands[name], cwd=root, input=b'', capture_output=True, timeout=20)
                    elapsed = (time.perf_counter_ns() - started) / 1000
                    assert result.returncode == 0 and result.stdout == oracle and not result.stderr, (case, name, result)
                    if trial >= 5:
                        raw[name].append(elapsed)
            results[case] = {'sources': sources, 'source_sha256': {name: hashlib.sha256(source.encode()).hexdigest() for name, source in sources.items()}, 'supplied_input_sha256': hashlib.sha256(json.dumps(supplied).encode()).hexdigest(), 'oracle_hex': oracle.hex(), 'native_launches_verified_outside_timing': launches, 'timings_us': {name: summary(values) for name, values in raw.items()}}
    report = {'baseline_uses_identical_program_source': args.baseline_programs, 'at_utc': datetime.now(timezone.utc).isoformat(), 'platform': platform.platform(), 'binaries': {name: {'path': str(path), 'sha256': digest(path)} for name, path in binaries.items()}, 'harness_sha256': digest(Path(__file__)), 'catalog_sha256': {p.name: digest(p) for p in sorted((PROJECT / 'manifests').glob('*.json'))}, 'method': 'Fresh CLI runs with five warmups and shuffled interleaved samples, serial, warm filesystem, no concurrent builds/tests. Exact byte/status/stderr checks. Native launch counts verified separately with trace. Baseline/candidate inline sources are identical controls. With baseline_uses_identical_program_source=true, baseline_programs and candidate_programs are identical reusable sources; otherwise candidate_programs uses different source with equivalent explicit native/output behavior and typed local scopes. Includes startup, parsing/expansion, validation and execution. Large JSON call arguments use isolated value copies; do not infer an isolated dispatch cost, memory bound, arbitrary-workload speedup or native Linux results.', 'results': results}
    args.report.write_text(json.dumps(report, indent=2) + '\n')
    print(json.dumps({case: {name: {key: value for key, value in values.items() if key != 'raw_us'} for name, values in data['timings_us'].items()} for case, data in results.items()}, indent=2))


if __name__ == '__main__':
    main()
