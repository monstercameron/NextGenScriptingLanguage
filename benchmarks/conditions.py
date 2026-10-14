#!/usr/bin/env python3
"""Paired warm ordinary CLI timings and separately labelled conditional feature overhead."""
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


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--baseline', type=Path, required=True)
    p.add_argument('--candidate', type=Path, required=True)
    p.add_argument('--samples', type=int, default=100)
    p.add_argument('--report', type=Path, required=True)
    a = p.parse_args()
    assert a.samples >= 20
    binaries = {name: getattr(a, name).resolve() for name in ['baseline', 'candidate']}
    rng, results = random.Random(1014202611), {}
    with tempfile.TemporaryDirectory(prefix='execute-condition-bench-') as directory:
        root = Path(directory)
        cases = {
            'printf': ('run "printf" with arguments ["%s", "alpha"]\n', b'alpha', 'run', a.samples),
            'pipe3': ('run "printf" with arguments ["alpha"]\npipe to "cat"\npipe to "cat"\n', b'alpha', 'run', a.samples),
            'launch50': ('run "true"\n' * 50, b'', 'run', 30),
            'check1000': ('run "true"\n' * 1000, b'', 'check', a.samples),
            'empty_vs_skip1000': ('', b'', 'run', a.samples),
            'check1000inputs': (''.join(f'accept flag_{index} as boolean with default false\n' for index in range(100)) + 'run \"true\"\n' * 1000, b'', 'check', a.samples),
        }
        for case, (source, wanted, verb, count) in cases.items():
            file = root / (case + '.program')
            file.write_text(source)
            variants = {name: (binary, file) for name, binary in binaries.items()}
            conditional = root / (case + '-condition.program')
            if case == 'empty_vs_skip1000':
                conditional.write_text('if false is true\n  run "true"\n' * 1000)
            elif case == 'check1000inputs':
                prefix = ''.join(f'accept flag_{index} as boolean with default false\n' for index in range(100))
                conditional.write_text(prefix + 'if true is true\n' + '  run \"true\"\n' * 1000 + 'otherwise\n  run \"false\"\n')
                many = root / 'many-conditions.program'
                many.write_text(prefix + 'if true is true\n  run \"true\"\n' * 1000)
                variants['candidate_condition_many'] = (binaries['candidate'], many)
            else:
                conditional.write_text('if true is true\n' + ''.join('  ' + line + '\n' for line in source.splitlines())
                                       + 'otherwise\n  run "false"\n')
            variants['candidate_condition'] = (binaries['candidate'], conditional)
            raw = {name: [] for name in variants}
            for trial in range(count + 5):
                order = list(variants)
                rng.shuffle(order)
                for name in order:
                    binary, program = variants[name]
                    started = time.perf_counter_ns()
                    run = subprocess.run([str(binary), verb, str(program), '--manifest-dir', str(PROJECT / 'manifests')],
                                         cwd=root, input=b'', capture_output=True, timeout=10)
                    elapsed = (time.perf_counter_ns() - started) / 1000
                    assert run.returncode == 0 and run.stdout == (wanted if verb == 'run' else b'') and not run.stderr, (case, name, run)
                    if trial >= 5:
                        raw[name].append(elapsed)
            if verb == 'run':
                probe = subprocess.run([str(binaries['candidate']), str(conditional), '--manifest-dir', str(PROJECT / 'manifests'), '--trace'],
                                       cwd=root, input=b'', capture_output=True, timeout=10)
                assert probe.returncode == 0 and probe.stdout == wanted
                events = [json.loads(line) for line in probe.stderr.splitlines()]
                launches = sum(event['event'] == 'spawn' for event in events)
                assert launches == {'printf': 1, 'pipe3': 3, 'launch50': 50, 'empty_vs_skip1000': 0}[case]
            else:
                launches = None
            results[case] = {'source_sha256': hashlib.sha256(source.encode()).hexdigest(),
                             'conditional_source_sha256': hashlib.sha256(conditional.read_bytes()).hexdigest(),
                             'conditional_native_launches_verified_outside_timing': launches,
                             'many_condition_source_sha256': hashlib.sha256(many.read_bytes()).hexdigest() if case == 'check1000inputs' else None,
                             'timings_us': {name: summary(values) for name, values in raw.items()}}
    report = {
        'at_utc': datetime.now(timezone.utc).isoformat(), 'platform': platform.platform(),
        'binaries': {name: {'path': str(binary), 'sha256': hashlib.sha256(binary.read_bytes()).hexdigest()} for name, binary in binaries.items()},
        'harness_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
        'catalog_sha256': {file.name: hashlib.sha256(file.read_bytes()).hexdigest() for file in sorted((PROJECT / 'manifests').glob('*.json'))},
        'method': 'Five warmups then deterministically shuffled interleaved release CLI trials. Baseline/candidate use identical source, native argv, catalog and verified output/status. Candidate_condition is DIFFERENT source with a literal conditional; selected native executable argv/output remain identical. Empty_vs_skip1000 compares empty files against 1000 false branches with verified ZERO native launches; the additional cost includes parsing, validation and control, not an isolated per-branch latency. Trace probes verify launch counts outside timing. Warm filesystem, fresh CLI processes including startup/capture/exit. No concurrent builds/tests, cold-cache, peak-memory, native Linux or broad speedup claim.',
        'results': results,
    }
    a.report.write_text(json.dumps(report, indent=2) + '\n')
    print(json.dumps({case: {name: {key: value for key, value in timing.items() if key != 'raw_us'}
                            for name, timing in data['timings_us'].items()} for case, data in results.items()}, indent=2))


if __name__ == '__main__':
    main()
