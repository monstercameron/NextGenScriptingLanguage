#!/usr/bin/env python3
"""Reproducible release-CLI and native-pipe baselines; owned temporary files only."""
import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import random
import shutil
import statistics
import subprocess
import sys
import tempfile
import time


def percentiles(values):
    values = sorted(values)
    def percentile(p):
        return values[max(0, math.ceil(len(values) * p) - 1)]
    return {"samples": len(values), "p50": statistics.median(values),
            "p95": percentile(0.95), "p99": percentile(0.99), "min": values[0]}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, default=Path(__file__).resolve().parents[1] / 'bin/execute')
    parser.add_argument('--samples', type=int, default=200)
    parser.add_argument('--output', type=Path)
    args = parser.parse_args()
    if args.samples < 10:
        parser.error('--samples must be at least 10')
    binary = args.binary.resolve()
    project = Path(__file__).resolve().parents[1]
    manifests = project / 'manifests'
    commands = {name: shutil.which(name) for name in ('printf', 'tr', 'wc', 'cat', 'sh')}
    if not all(commands.values()):
        parser.error('printf, tr, wc, cat, and sh executables are required')
    cpu_model = platform.processor()
    if platform.system() == 'Darwin':
        cpu_model = subprocess.check_output(['/usr/sbin/sysctl', '-n', 'machdep.cpu.brand_string']).decode().strip()
    report = {
        'environment': {'os': platform.system(), 'os_version': platform.release(),
                        'architecture': platform.machine(), 'cpu_model': cpu_model, 'python': platform.python_version(),
                        'logical_cpus': os.cpu_count(), 'execute_version': subprocess.check_output([binary, '--version']).decode().strip(),
                        'build': 'release', 'applets': commands,
                        'binary_sha256': hashlib.sha256(binary.read_bytes()).hexdigest(),
                        'manifest_sha256': {p.name: hashlib.sha256(p.read_bytes()).hexdigest() for p in sorted(manifests.glob('*.json'))}},
        'method': {'timer': 'perf_counter_ns', 'warmup_runs_per_case': 10,
                   'samples_per_case': args.samples, 'ordering': 'deterministic shuffled cases each round',
                   'conditions': 'new CLI processes; warmed OS filesystem/executable caches; no cache flushing',
                   'cli_timing': 'spawn to exit, includes launcher/capture and complete stdout; preparation excluded',
                   'percentiles': 'nearest rank p95/p99; conventional median p50',
                   'safety': 'temporary fixture/cache directories; no Git execution or external service calls'},
        'cli_ms': {}, 'native_pipe_roundtrip_us': {}, 'first_byte_ms': {},
    }
    with tempfile.TemporaryDirectory(prefix='flow-benchmark-') as scratch:
        cwd = Path(scratch)
        small = b'hello\nflow\nunix\n'
        large = b'hello flow unix\n' * 65536
        (cwd / 'small.txt').write_bytes(small)
        (cwd / 'large.txt').write_bytes(large)
        scripts = {
            'print.program': 'run printf with message "hello latency"\n',
            'small.program': 'read "small.txt"\npipe to tr with from "[:lower:]" and to "[:upper:]"\npipe to wc with flags ["-l"]\n',
            'large.program': 'read "large.txt"\npipe to tr with from "[:lower:]" and to "[:upper:]"\npipe to wc with flags ["-l"]\n',
            'copy.program': 'read "large.txt"\npipe to tr with from "[:lower:]" and to "[:upper:]"\npipe to cat\n',
            'uncached.program': 'read "large.txt"\nthen tr with from "[:lower:]" and to "[:upper:]"\nthen wc with flags ["-l"]\n',
            'cached.program': 'read "large.txt"\nthen tr with from "[:lower:]" and to "[:upper:]" and cache\nthen wc with flags ["-l"]\n',
        }
        for name, source in scripts.items():
            (cwd / name).write_text(source)
        def execute(command, script, extra=()):
            return [str(binary), *([] if command == 'run' else [command]), script, '--manifest-dir', str(manifests), *extra]
        hit_cache, miss_cache = cwd / 'hit-cache', cwd / 'miss-cache'
        def remove_miss_cache():
            if miss_cache.exists():
                shutil.rmtree(miss_cache)
        # Shell source is fixed trusted text, never built from script parameters.
        # Applets resolve from the same inherited PATH as Execute.
        cases = {
            'shell_startup_only': ([commands['sh'], '-c', ':'], b'', False, None),
            'execute_check_no_execution': (execute('check', 'print.program'), b'', False, None),
            'raw_printf': ([commands['printf'], '%s\n', 'hello latency'], b'hello latency\n', False, None),
            'shell_builtin_printf': ([commands['sh'], '-c', "printf '%s\\n' 'hello latency'"], b'hello latency\n', False, None),
            'execute_printf': (execute('run', 'print.program'), b'hello latency\n', False, None),
            'shell_small_pipe': ([commands['sh'], '-c', "tr '[:lower:]' '[:upper:]' < small.txt | wc -l"], b'3', True, None),
            'execute_small_pipe': (execute('run', 'small.program'), b'3', True, None),
            'shell_1MiB_pipe_count': ([commands['sh'], '-c', "tr '[:lower:]' '[:upper:]' < large.txt | wc -l"], b'65536', True, None),
            'execute_1MiB_pipe_count': (execute('run', 'large.program'), b'65536', True, None),
            'shell_1MiB_pipe_copy': ([commands['sh'], '-c', "tr '[:lower:]' '[:upper:]' < large.txt | cat"], large.upper(), False, None),
            'execute_1MiB_pipe_copy': (execute('run', 'copy.program'), large.upper(), False, None),
            'execute_1MiB_finite_uncached': (execute('run', 'uncached.program'), b'65536', True, None),
            'execute_1MiB_finite_cache_miss': (execute('run', 'cached.program', ('--cache-dir', str(miss_cache))), b'65536', True, remove_miss_cache),
            'execute_1MiB_finite_cache_hit': (execute('run', 'cached.program', ('--cache-dir', str(hit_cache))), b'65536', True, None),
        }
        def measure(case):
            command, expected, strip, prepare = case
            if prepare:
                prepare()
            started = time.perf_counter_ns()
            result = subprocess.run(command, cwd=cwd, capture_output=True, timeout=10)
            elapsed = (time.perf_counter_ns() - started) / 1e6
            if result.returncode != 0:
                raise RuntimeError(f'{command}: {result.stderr.decode(errors="replace")}')
            actual = result.stdout.strip() if strip else result.stdout
            if actual != expected:
                raise AssertionError(f'Incorrect output for {command}')
            return elapsed
        for name, case in cases.items():
            for _ in range(10):
                measure(case)
        # Confirm the putative hit case really avoids launching tr.
        probe = subprocess.run(execute('run', 'cached.program', ('--cache-dir', str(hit_cache), '--trace')),
                               cwd=cwd, capture_output=True, timeout=10, check=True)
        if b'cache_hit' not in probe.stderr:
            raise AssertionError('cache hit benchmark is not hitting the cache')
        samples = {name: [] for name in cases}
        order = list(cases)
        rng = random.Random(20261013)
        for round_index in range(args.samples):
            rng.shuffle(order)
            for name in order:
                samples[name].append(measure(cases[name]))
            if (round_index + 1) % 50 == 0:
                print(f'benchmarks: {round_index + 1}/{args.samples} rounds', file=sys.stderr, flush=True)
        report['cli_ms'] = {name: percentiles(values) for name, values in samples.items()}
        report['raw_cli_ms'] = samples
        report['fixtures'] = {'small_bytes': len(small), 'large_bytes': len(large), 'large_lines': 65536}
        report['scripts'] = scripts
        if args.output:
            args.output.write_text(json.dumps(report, indent=2) + '\n')
        # A warmed persistent cat echoes one byte. This measures two pipe
        # transfers, cat's work, scheduling and Python calls; not DSL dispatch.
        echo = subprocess.Popen([commands['cat']], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                stderr=subprocess.DEVNULL, bufsize=0)
        try:
            roundtrips = []
            for index in range(2200):
                started = time.perf_counter_ns()
                echo.stdin.write(b'x')
                if echo.stdout.read(1) != b'x':
                    raise AssertionError('pipe echo failed')
                elapsed = (time.perf_counter_ns() - started) / 1000
                if index >= 200:
                    roundtrips.append(elapsed)
            report['native_pipe_roundtrip_us'] = percentiles(roundtrips)
            report['raw_native_pipe_roundtrip_us'] = roundtrips
            report['native_pipe_roundtrip_us']['includes'] = 'two OS pipes + persistent cat + Python read/write + scheduling; no startup'
        finally:
            echo.stdin.close()
            try:
                echo.wait(timeout=3)
            except subprocess.TimeoutExpired:
                echo.kill()
                echo.wait()
            echo.stdout.close()
        # Show the terminal-capture policy separately from end-to-end runtime.
        first_byte_cases = {'shell_1MiB_pipe_copy': cases['shell_1MiB_pipe_copy'][0],
                            'execute_1MiB_pipe_copy': cases['execute_1MiB_pipe_copy'][0]}
        for name, command in first_byte_cases.items():
            times = []
            for index in range(60):
                started = time.perf_counter_ns()
                child = subprocess.Popen(command, cwd=cwd, stdout=subprocess.PIPE, stderr=subprocess.PIPE, bufsize=0)
                try:
                    first = child.stdout.read(1)
                    elapsed = (time.perf_counter_ns() - started) / 1e6
                    rest, stderr = child.communicate(timeout=10)
                    if child.returncode or first + rest != large.upper():
                        raise AssertionError(f'first-byte case failed: {stderr!r}')
                    if index >= 10:
                        times.append(elapsed)
                finally:
                    if child.poll() is None:
                        child.kill()
                        child.wait()
                    child.stdout.close()
                    child.stderr.close()
            report['first_byte_ms'][name] = percentiles(times)
            report.setdefault('raw_first_byte_ms', {})[name] = times
        report['first_byte_ms']['note'] = 'Execute buffers finite terminal output until completion; shell streams it immediately. Not an event-worker benchmark.'
    serialized = json.dumps(report, indent=2)
    if args.output:
        args.output.write_text(serialized + '\n')
    print(serialized)


if __name__ == '__main__':
    main()
