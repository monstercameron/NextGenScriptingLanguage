#!/usr/bin/env python3
"""Paired managed interval completion latency through the real local control API."""
import argparse
from datetime import datetime, timezone
import hashlib
import json
from pathlib import Path
import platform
import random
import select
import socket
import subprocess
import tempfile
import time
from direct_commands import summary

PROJECT = Path(__file__).resolve().parents[1]
CASES = {
    'builtin_1ms_20_ticks': (1, 20, 'run cd with path "."'),
    'native_5ms_10_ticks': (5, 10, 'run "printf" with arguments ["tick\\n"]'),
    'native_50ms_5_ticks': (50, 5, 'run "printf" with arguments ["tick\\n"]'),
}


def trial(binary, interval, target, action):
    with tempfile.TemporaryDirectory(prefix='execute-reactive-') as directory:
        root = Path(directory)
        script, address = root / 'timer.program', root / 'control.sock'
        script.write_text(f'every {interval} milliseconds\n  {action}\n')
        child = subprocess.Popen([str(binary), 'live', 'serve', str(script), '--socket', str(address),
                                  '--manifest-dir', str(PROJECT / 'manifests'), '--paused'],
                                 cwd=root, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE)

        def rpc(op, **values):
            with socket.socket(socket.AF_UNIX) as stream:
                stream.settimeout(5)
                stream.connect(str(address))
                stream.sendall(json.dumps({'op': op, **values}).encode() + b'\n')
                with stream.makefile('rb') as reader:
                    response = json.loads(reader.readline(1024 * 1024))
            assert response['ok'], response
            return response

        try:
            assert select.select([child.stdout], [], [], 5)[0], 'startup timeout'
            assert json.loads(child.stdout.readline())['event'] == 'ready'
            started = time.perf_counter_ns()
            revision = rpc('control', action='resume')['revision']
            deadline = time.monotonic() + 5
            while True:
                response = rpc('wait', path='/runtime/runs_completed', after=revision, timeout_ms=1000)
                revision = response['revision']
                if response['result']['snapshot'] >= target:
                    elapsed_us = (time.perf_counter_ns() - started) / 1000
                    observed = response['result']['snapshot']
                    break
                assert time.monotonic() < deadline, 'timer did not complete requested runs'
            rpc('control', action='pause')
            deadline = time.monotonic() + 5
            while True:
                state = rpc('read', path='/runtime')['result']
                if not state['in_flight']:
                    break
                assert time.monotonic() < deadline, 'pause did not reach a boundary'
                time.sleep(.001)
            assert state['error'] is None, state
            if action.startswith('run "printf"'):
                assert state['completed_native_launches'] >= target, state
                assert state['last_output'] == {'text': 'tick\n'}, state
            else:
                assert state['completed_native_launches'] == 0, state
            rpc('control', action='stop')
            assert child.wait(timeout=5) == 0
            assert not address.exists()
            return elapsed_us, observed
        finally:
            if child.poll() is None:
                child.terminate()
                try:
                    child.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    child.kill()
                    child.wait(timeout=2)
            child.stdout.close()
            child.stderr.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--baseline', type=Path, required=True)
    parser.add_argument('--candidate', type=Path, required=True)
    parser.add_argument('--samples', type=int, default=20)
    parser.add_argument('--report', type=Path, required=True)
    args = parser.parse_args()
    if args.samples < 5:
        parser.error('--samples must be at least 5')
    binaries = {'baseline': args.baseline.resolve(), 'candidate': args.candidate.resolve()}
    rng, results = random.Random(8232026), {}
    for case, (interval, ticks, action) in CASES.items():
        raw = {name: [] for name in binaries}
        observed = {name: [] for name in binaries}
        for index in range(args.samples + 2):
            order = list(binaries)
            rng.shuffle(order)
            for name in order:
                elapsed, completed = trial(binaries[name], interval, ticks, action)
                if index >= 2:
                    raw[name].append(elapsed)
                    observed[name].append(completed)
        results[case] = {'interval_ms': interval, 'target_ticks': ticks, 'action': action,
                        'variants': {name: summary(values) for name, values in raw.items()},
                        'observed_runs_at_timed_boundary': observed}
    report = {
        'recorded_at_utc': datetime.now(timezone.utc).isoformat(), 'platform': platform.platform(),
        'binaries': {name: {'sha256': hashlib.sha256(binary.read_bytes()).hexdigest(),
                           'version': subprocess.check_output([str(binary), '--version']).decode().strip()}
                     for name, binary in binaries.items()},
        'harness_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
        'method': 'Fresh local managed servers; startup excluded via ready handshake; two warmups per case and shuffled paired trials. Timed resume request through observation of target completed runs via revision long-poll RPC. Python/socket/JSON observation included; overshoot recorded. Programs own interval control flow and invoke native printf or generic cd builtin. Native launch/output and cleanup verified outside timing. Warm executable/filesystem caches; uncontrolled background activity. Not a hard realtime guarantee, native Linux result or general runtime speedup.',
        'results': results,
    }
    args.report.parent.mkdir(parents=True, exist_ok=True)
    args.report.write_text(json.dumps(report, indent=2) + '\n')
    print(json.dumps({case: {name: {key: value for key, value in data.items() if key != 'raw_us'}
                             for name, data in entry['variants'].items()} for case, entry in results.items()}, indent=2))


if __name__ == '__main__':
    main()
