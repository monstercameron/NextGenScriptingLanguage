#!/usr/bin/env python3
"""Latency of a live Execute listener and monotonic interval scheduling."""
import argparse
import json
import math
import os
from pathlib import Path
import select
import statistics
import subprocess
import tempfile
import time


def percentiles(values):
    values = sorted(values)
    return {'samples': len(values), 'p50': statistics.median(values),
            'p95': values[math.ceil(len(values) * .95) - 1],
            'p99': values[math.ceil(len(values) * .99) - 1]}


def read_exact(pipe, count, timeout=10):
    deadline = time.monotonic() + timeout
    output = bytearray()
    while len(output) < count:
        remaining = deadline - time.monotonic()
        if remaining <= 0 or not select.select([pipe], [], [], remaining)[0]:
            raise TimeoutError('listener response timed out')
        chunk = os.read(pipe.fileno(), count - len(output))
        if not chunk:
            raise RuntimeError('listener closed before its response')
        output.extend(chunk)
    return bytes(output)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    root = Path(__file__).resolve().parents[1]
    parser.add_argument('--binary', type=Path, default=root / 'bin/execute')
    parser.add_argument('--samples', type=int, default=200)
    parser.add_argument('--output', type=Path)
    args = parser.parse_args()
    if args.samples < 10:
        parser.error('--samples must be at least 10')
    binary = args.binary.resolve()
    report = {'version': subprocess.check_output([binary, '--version']).decode().strip(),
              'conditions': 'one warm listener process; sequential requests over pipes; emit before stdin EOF; no runner startup in per-event timing',
              'latency_includes': 'Python pipe write/read, event framing, named binding, finite body, stdout flush, scheduling',
              'listener_us': {}}
    with tempfile.TemporaryDirectory(prefix='execute-services-') as scratch:
        cwd = Path(scratch)
        fixture = b'hello\nflow\nunix\n'
        (cwd / 'small.txt').write_bytes(fixture)
        cases = {
            'read_fixture_no_applet_spawn': ('read "small.txt"', fixture),
            'tr_external_applet': ('then tr with from "[:lower:]" and to "[:upper:]"', b'HELLO\n'),
        }
        for name, (body, expected) in cases.items():
            program = cwd / (name + '.program')
            program.write_text('listen to stdin\nwhenever a line arrives as message\n  ' + body + '\n')
            child = subprocess.Popen([binary, program, '--manifest-dir', root / 'manifests'],
                                     cwd=cwd, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                     stderr=subprocess.PIPE, bufsize=0)
            try:
                times = []
                for index in range(args.samples + 20):
                    started = time.perf_counter_ns()
                    child.stdin.write(b'hello\n')
                    actual = read_exact(child.stdout, len(expected))
                    elapsed_us = (time.perf_counter_ns() - started) / 1000
                    if actual != expected:
                        raise AssertionError(f'{name}: incorrect response {actual!r}')
                    if index >= 20:
                        times.append(elapsed_us)
                child.stdin.close()
                child.wait(timeout=10)
                stderr = child.stderr.read()
                if child.returncode:
                    raise RuntimeError(stderr.decode(errors='replace'))
                report['listener_us'][name] = percentiles(times)
            finally:
                if child.poll() is None:
                    child.terminate()
                    try:
                        child.wait(timeout=3)
                    except subprocess.TimeoutExpired:
                        child.kill()
                        child.wait()
                for stream in (child.stdin, child.stdout, child.stderr):
                    if stream:
                        stream.close()
        program = cwd / 'interval.program'
        program.write_text('every 5 milliseconds\n  read "small.txt"\n')
        interval = subprocess.run([binary, program, '--manifest-dir', root / 'manifests',
                                   '--max-runs', str(args.samples + 20), '--trace'],
                                  cwd=cwd, capture_output=True, timeout=15, check=True)
        if interval.stdout != fixture * (args.samples + 20):
            raise AssertionError('incorrect interval outputs')
        trace = [json.loads(line) for line in interval.stderr.splitlines()]
        lateness = [row['lateness_us'] for row in trace if row['event'] == 'interval_tick'][20:]
        assert len(lateness) == args.samples
        report['interval_5ms_lateness_us'] = percentiles(lateness)
        report['interval_note'] = 'positive lateness relative to monotonic tick deadline; body reads 16-byte fixture; first 20 ticks excluded; missed ticks skipped'
    text = json.dumps(report, indent=2)
    if args.output:
        args.output.write_text(text + '\n')
    print(text)


if __name__ == '__main__':
    main()
