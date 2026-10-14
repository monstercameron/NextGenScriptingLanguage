#!/usr/bin/env python3
"""Interleave old/new named calls and new direct calls with exact output oracles."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import random
import statistics
import subprocess
import tempfile
import time
from datetime import datetime, timezone

PROJECT = Path(__file__).resolve().parents[1]


def summary(values):
    ordered = sorted(values)
    return {
        'samples': len(values),
        'p50_us': round(statistics.median(values), 3),
        'p95_us': round(ordered[int(.95 * (len(ordered) - 1))], 3),
        'raw_us': values,
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--baseline', required=True, type=Path)
    parser.add_argument('--candidate', required=True, type=Path)
    parser.add_argument('--report', required=True, type=Path)
    parser.add_argument('--samples', type=int, default=40)
    args = parser.parse_args()
    if args.samples < 10:
        parser.error('--samples must be at least 10')
    binaries = {'baseline': args.baseline.resolve(), 'candidate': args.candidate.resolve()}
    versions = {name: subprocess.check_output([str(binary), '--version'], timeout=10).decode().strip()
                for name, binary in binaries.items()}
    named = {
        'check_500_calls': 'run pwd\n' * 500,
        'single_native_printf': 'run printf with message "owned benchmark payload"\n',
        'three_stage_pipeline': 'run cat with path "lines.txt"\npipe to uppercase\npipe to wc with flags ["-l"]\n',
        'two_mib_binary_pipeline': 'run cat with path "bytes.bin"\npipe to cat\n',
    }
    direct = {
        'check_500_calls': 'run "pwd"\n' * 500,
        'single_native_printf': 'run "printf" with arguments ' + json.dumps([json.loads((PROJECT / "manifests/printf.json").read_text())["arguments"][0]["literal"], "owned benchmark payload"]) + "\n",
        'three_stage_pipeline': 'run "cat" with arguments ["--", "lines.txt"]\npipe to "tr" with arguments ["[:lower:]", "[:upper:]"]\npipe to "wc" with arguments ["-l"]\n',
        'two_mib_binary_pipeline': 'run "cat" with arguments ["--", "bytes.bin"]\npipe to "cat" with arguments ["--"]\n',
    }
    result = {}
    rng = random.Random(20261014)
    with tempfile.TemporaryDirectory(prefix='execute-direct-bench-') as directory:
        root = Path(directory)
        binary_input = bytes(range(256)) * 8192
        (root / 'bytes.bin').write_bytes(binary_input)
        (root / 'lines.txt').write_bytes(b'owned line\n' * 8192)
        expected = {'check_500_calls': b'', 'single_native_printf': b'owned benchmark payload\n',
                    'three_stage_pipeline': b'8192', 'two_mib_binary_pipeline': binary_input}
        for case in ['startup_version', *named]:
            variants = {'baseline_named': ('baseline', False), 'candidate_named': ('candidate', False)}
            if case != 'startup_version':
                variants['candidate_direct'] = ('candidate', True)
            commands = {}
            for variant, (binary, is_direct) in variants.items():
                command = [str(binaries[binary])]
                if case == 'startup_version':
                    command.append('--version')
                else:
                    file = root / (variant + '.program')
                    file.write_text((direct if is_direct else named)[case])
                    if case.startswith('check_'):
                        command.append('check')
                    command += [str(file), '--manifest-dir', str(PROJECT / 'manifests')]
                commands[variant] = command
            samples = {variant: [] for variant in variants}
            for index in range(args.samples + 5):
                order = list(variants)
                rng.shuffle(order)
                for variant in order:
                    begin = time.perf_counter_ns()
                    output = subprocess.run(commands[variant], cwd=root, stdout=subprocess.PIPE,
                                            stderr=subprocess.PIPE, timeout=15)
                    elapsed = (time.perf_counter_ns() - begin) / 1000
                    assert output.returncode == 0, (case, variant, output.stderr)
                    actual = output.stdout.strip() if case == 'three_stage_pipeline' else output.stdout
                    oracle = (versions[variants[variant][0]] + '\n').encode() if case == 'startup_version' else expected[case]
                    assert actual == oracle, (case, variant, 'output mismatch')
                    assert not output.stderr, (case, variant, output.stderr)
                    if index >= 5:
                        samples[variant].append(elapsed)
            result[case] = {variant: summary(values) for variant, values in samples.items()}
    report = {
        'recorded_at_utc': datetime.now(timezone.utc).isoformat(),
        'platform': platform.platform(), 'machine': platform.machine(),
        'binaries': {name: {'version': versions[name], 'sha256': hashlib.sha256(path.read_bytes()).hexdigest()}
                     for name, path in binaries.items()},
        'harness_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
        'manifest_sha256': {path.name: hashlib.sha256(path.read_bytes()).hexdigest()
                            for path in sorted((PROJECT / 'manifests').glob('*.json'))},
        'method': 'Warm filesystem/executable caches. Five warmups per variant and case. Seeded shuffled order within each paired round. Fresh process per sample. Python process launch and stdout capture included; exact output checks follow the timed region. All variants load the same explicit catalog. Named/direct native argv match, including cat --. Binary pipeline includes transfer and capture of 2 MiB. Startup --version bypasses parsing/catalog and is a separate reference. No true cold-cache, native Linux, physical display, or universal Bash speedup claim. Background system activity is uncontrolled.',
        'environment': {'path': os.environ.get('PATH'), 'locale': os.environ.get('LC_ALL', os.environ.get('LANG'))},
        'output_oracles': 'Every invocation verified outside timing; binary input includes every byte value.',
        'results': result,
    }
    args.report.parent.mkdir(parents=True, exist_ok=True)
    args.report.write_text(json.dumps(report, indent=2) + '\n')
    print(json.dumps({case: {v: {k: val for k, val in stats.items() if k != 'raw_us'}
                            for v, stats in variants.items()} for case, variants in result.items()}, indent=2))


if __name__ == '__main__':
    main()
