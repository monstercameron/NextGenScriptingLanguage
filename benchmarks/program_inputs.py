#!/usr/bin/env python3
"""Paired CLI-input overhead with matching native argv and exact output checks."""
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
    p.add_argument('--baseline', required=True, type=Path)
    p.add_argument('--candidate', required=True, type=Path)
    p.add_argument('--report', required=True, type=Path)
    p.add_argument('--samples', type=int, default=80)
    args = p.parse_args()
    if args.samples < 20:
        p.error('--samples must be at least 20')
    binaries = {'baseline': args.baseline.resolve(), 'candidate': args.candidate.resolve()}
    sources = {
        'file_checksum': {
            'literal': 'run "cksum" with arguments ["fixture.bin"]\n',
            'defaults': 'accept path as text with default "fixture.bin"\nrun "cksum" with arguments [path]\n',
            'supplied': 'accept path as text\nrun "cksum" with arguments [path]\n',
        },
        'typed_head': {
            'literal': 'read "lines.txt"\npipe to "head" with arguments ["-n", "2"]\n',
            'defaults': 'accept path as text with default "lines.txt"\naccept count as integer with default 2\nread path\npipe to "head" with arguments ["-n", count]\n',
            'supplied': 'accept path as text\naccept count as integer\nread path\npipe to "head" with arguments ["-n", count]\n',
        },
    }
    results = {}
    native_argv = {}
    rng = random.Random(8082026)
    with tempfile.TemporaryDirectory(prefix='execute-input-bench-') as directory:
        root = Path(directory)
        (root / 'fixture.bin').write_bytes(bytes(range(256)) * 32)
        (root / 'lines.txt').write_bytes(b'one\ntwo\nthree\n')
        expected = {'file_checksum': subprocess.check_output(['cksum', 'fixture.bin'], cwd=root, timeout=5),
                    'typed_head': b'one\ntwo\n'}
        for case, texts in sources.items():
            commands = {}
            for variant in ['baseline_literal', 'candidate_literal', 'candidate_defaults', 'candidate_supplied']:
                binary, syntax = variant.split('_', 1)
                file = root / (variant + '.program')
                file.write_text(texts[syntax])
                commands[variant] = [str(binaries[binary]), str(file), '--manifest-dir', str(PROJECT / 'manifests')]
                if syntax == 'supplied':
                    commands[variant] += ['--input', 'path=' + ('fixture.bin' if case == 'file_checksum' else 'lines.txt')]
                    if case == 'typed_head':
                        commands[variant] += ['--input', 'count=2']
            raw = {variant: [] for variant in commands}
            plans = {}
            for variant, command in commands.items():
                plan = subprocess.check_output([command[0], 'explain', *command[1:]], cwd=root, timeout=10).decode()
                plans[variant] = [json.loads(line.split('argv=', 1)[1]) for line in plan.splitlines() if ' argv=' in line]
            assert all(value == plans['baseline_literal'] for value in plans.values()), (case, plans)
            native_argv[case] = plans
            for i in range(args.samples + 5):
                order = list(commands)
                rng.shuffle(order)
                for variant in order:
                    start = time.perf_counter_ns()
                    result = subprocess.run(commands[variant], cwd=root, capture_output=True, timeout=10)
                    elapsed = (time.perf_counter_ns() - start) / 1000
                    assert result.returncode == 0, (case, variant, result.stderr)
                    assert result.stdout == expected[case], (case, variant, 'output differs')
                    assert result.stderr == b'', result.stderr
                    if i >= 5:
                        raw[variant].append(elapsed)
            results[case] = {name: summary(values) for name, values in raw.items()}
    report = {'recorded_at_utc': datetime.now(timezone.utc).isoformat(), 'platform': platform.platform(),
              'machine': platform.machine(), 'binaries': {name: {'version': subprocess.check_output([str(path), '--version']).decode().strip(), 'sha256': hashlib.sha256(path.read_bytes()).hexdigest()} for name, path in binaries.items()},
              'harness_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
              'source_sha256': {case: {name: hashlib.sha256(source.encode()).hexdigest() for name, source in variants.items()} for case, variants in sources.items()},
              'native_argv_verified_by_explain': native_argv,
              'method': 'Warm filesystem/executable caches; five warmups and seeded shuffled paired rounds. Fresh CLI process per sample. Same explicit catalog and equivalent native argv; literal/default/explicit-input variants use the same data. Python process launch/capture included. Every output checked outside timing. 8 KiB CRC fixture; three text lines with a two-line head. Background activity uncontrolled. No cold-cache, native Linux, or general speedup claim.',
              'results': results}
    args.report.parent.mkdir(parents=True, exist_ok=True)
    args.report.write_text(json.dumps(report, indent=2) + '\n')
    print(json.dumps({case: {name: {key: value for key, value in item.items() if key != 'raw_us'} for name, item in variants.items()} for case, variants in results.items()}, indent=2))


if __name__ == '__main__':
    main()
