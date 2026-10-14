#!/usr/bin/env python3
"""Paired successful buffered-input pipelines, including prefilled and threaded paths."""
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
SOURCE = 'read "payload" as input\nthen "cat"\n'


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--baseline', type=Path, required=True)
    parser.add_argument('--candidate', type=Path, required=True)
    parser.add_argument('--samples', type=int, default=80)
    parser.add_argument('--report', type=Path, required=True)
    args = parser.parse_args()
    if args.samples < 20:
        parser.error('--samples must be at least 20')
    binaries = {'baseline': args.baseline.resolve(), 'candidate': args.candidate.resolve()}
    rng, results = random.Random(8372026), {}
    with tempfile.TemporaryDirectory(prefix='execute-writer-bench-') as directory:
        root = Path(directory)
        file = root / 'feed.program'
        file.write_text(SOURCE)
        commands = {name: [str(binary), str(file), '--manifest-dir', str(PROJECT / 'manifests')]
                    for name, binary in binaries.items()}
        for size in [4096, 65536, 1048576]:
            data = (b'0123456789abcdef\n' * (size // 17 + 1))[:size]
            (root / 'payload').write_bytes(data)
            raw = {name: [] for name in binaries}
            for trial in range(args.samples + 5):
                order = list(binaries)
                rng.shuffle(order)
                for name in order:
                    started = time.perf_counter_ns()
                    result = subprocess.run(commands[name], cwd=root, capture_output=True, timeout=10)
                    elapsed = (time.perf_counter_ns() - started) / 1000
                    assert result.returncode == 0 and result.stderr == b'', (name, size, result.stderr)
                    assert result.stdout == data, (name, size, 'payload mismatch')
                    if trial >= 5:
                        raw[name].append(elapsed)
            results[str(size)] = {'input_sha256': hashlib.sha256(data).hexdigest(),
                                  'variants': {name: summary(values) for name, values in raw.items()}}
    report = {'recorded_at_utc': datetime.now(timezone.utc).isoformat(), 'platform': platform.platform(),
              'binaries': {name: {'sha256': hashlib.sha256(binary.read_bytes()).hexdigest(),
                                 'version': subprocess.check_output([str(binary), '--version']).decode().strip()}
                           for name, binary in binaries.items()},
              'source': SOURCE, 'harness_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
              'method': 'Release binaries, identical .program source/native argv, fresh CLI processes, warm filesystem/executable caches, five warmups and seeded shuffled paired rounds. read-as materializes input before then cat, exercising the generated input pipe (prefill or thread). Exact stdout and status checked outside timing. Python startup/capture included. No builds/tests/stress concurrent. Background activity uncontrolled; no cold-cache, native Linux, hard realtime or universal speedup claim.',
              'results': results}
    args.report.write_text(json.dumps(report, indent=2) + '\n')
    print(json.dumps({size: {name: {key: value for key, value in stats.items() if key != 'raw_us'}
                             for name, stats in case['variants'].items()} for size, case in results.items()}, indent=2))


if __name__ == '__main__':
    main()
