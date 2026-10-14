#!/usr/bin/env python3
"""Paired finite stdin syntax versus the existing /dev/stdin file workaround."""
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
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--baseline', required=True, type=Path)
    parser.add_argument('--candidate', required=True, type=Path)
    parser.add_argument('--report', required=True, type=Path)
    parser.add_argument('--samples', type=int, default=80)
    args = parser.parse_args()
    if args.samples < 20:
        parser.error('--samples must be at least 20')
    binaries = {'baseline': args.baseline.resolve(), 'candidate': args.candidate.resolve()}
    cases = {
        'count_8k': (bytes(range(256)) * 32, 'pipe to "wc" with arguments ["-c"]\n'),
        'count_8m': (bytes(range(256)) * 32768, 'pipe to "wc" with arguments ["-c"]\n'),
        'capture_128k': (bytes(range(256)) * 512, ''),
    }
    sources, results = {}, {}
    rng = random.Random(8142026)
    with tempfile.TemporaryDirectory(prefix='execute-stdin-bench-') as directory:
        root = Path(directory)
        for case, (data, tail) in cases.items():
            input_path = root / 'input.bin'
            input_path.write_bytes(data)
            sources[case] = {'baseline': 'read "/dev/stdin"\n' + tail,
                             'candidate': 'read from stdin\n' + tail}
            commands = {}
            plans = {}
            for name, binary in binaries.items():
                file = root / (name + '.program')
                file.write_text(sources[case][name])
                commands[name] = [str(binary), str(file), '--manifest-dir', str(PROJECT / 'manifests')]
                plan = subprocess.check_output([str(binary), 'explain', *commands[name][1:]], timeout=10).decode()
                plans[name] = [json.loads(line.split('argv=', 1)[1]) for line in plan.splitlines() if ' argv=' in line]
            assert plans['baseline'] == plans['candidate'], plans
            raw = {name: [] for name in binaries}
            expected = data if not tail else str(len(data)).encode()
            for trial in range(args.samples + 5):
                order = list(binaries)
                rng.shuffle(order)
                for name in order:
                    with input_path.open('rb') as incoming:
                        start = time.perf_counter_ns()
                        result = subprocess.run(commands[name], stdin=incoming, capture_output=True, timeout=10)
                        elapsed_us = (time.perf_counter_ns() - start) / 1000
                    assert result.returncode == 0 and result.stderr == b'', (case, name, result.stderr)
                    assert (result.stdout if not tail else result.stdout.strip()) == expected, (case, name, 'output mismatch')
                    if trial >= 5:
                        raw[name].append(elapsed_us)
            results[case] = {'input_bytes': len(data), 'input_sha256': hashlib.sha256(data).hexdigest(),
                             'native_argv': plans, 'variants': {name: summary(values) for name, values in raw.items()}}
    report = {
        'recorded_at_utc': datetime.now(timezone.utc).isoformat(), 'platform': platform.platform(),
        'binaries': {name: {'sha256': hashlib.sha256(binary.read_bytes()).hexdigest(),
                           'version': subprocess.check_output([str(binary), '--version']).decode().strip()}
                     for name, binary in binaries.items()},
        'harness_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest(), 'sources': sources,
        'method': 'Fresh CLI processes; five warmups, seeded shuffled paired rounds, release binaries, warm caches. Input descriptor opened before timing; Python launch/capture included. Exact binary output or stripped native wc count checked outside timing, with equal native argv verified by explain. Baseline uses existing /dev/stdin file workaround; candidate uses explicit stdin. No claim of cold-cache or native Linux results, universal throughput or a speedup from noise. Background activity uncontrolled.',
        'results': results,
    }
    args.report.parent.mkdir(parents=True, exist_ok=True)
    args.report.write_text(json.dumps(report, indent=2) + '\n')
    print(json.dumps({case: {name: {key: value for key, value in stats.items() if key != 'raw_us'}
                             for name, stats in result['variants'].items()} for case, result in results.items()}, indent=2))


if __name__ == '__main__':
    main()
