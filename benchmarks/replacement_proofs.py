#!/usr/bin/env python3
"""Bounded, local streaming/lifecycle evidence for the replacement challenge.

This is not the 100-command Unix matrix, an Omarchy port suite, or an AI eval.
No network, package installation, user-file changes, or shell execution.
"""
import argparse
import datetime
import hashlib
import json
import os
from pathlib import Path
import platform
import select
import shutil
import signal
import subprocess
import sys
import tempfile
import time

ROOT = Path(__file__).resolve().parents[1]
# Every byte value, with newline occurring exactly at the record boundary.
RECORD = bytes(value for value in range(256) if value != 10) + b'\n'


def digest(path):
    result = hashlib.sha256()
    with Path(path).open('rb') as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b''):
            result.update(chunk)
    return result.hexdigest()


def environment():
    hooks = {'BASH_ENV', 'ENV', 'PYTHONPATH', 'PYTHONHOME', 'PYTHONSTARTUP'}
    result = {key: value for key, value in os.environ.items()
              if key not in hooks and not key.startswith('BASH_FUNC_')}
    result['LC_ALL'] = 'C'
    return result


def trace_events(path):
    events = []
    for line in path.read_bytes().splitlines():
        try:
            event = json.loads(line)
        except (ValueError, UnicodeDecodeError):
            continue
        if isinstance(event, dict):
            events.append(event)
    return events


def alive(pid):
    try:
        os.kill(pid, 0)
        return True
    except ProcessLookupError:
        return False


def owned_cleanup(child, trace, folder):
    """Ask the supervisor to clean up; only force-stop uniquely identified fixtures."""
    if child.returncode is None:
        child.send_signal(signal.SIGTERM)
        try:
            child.wait(timeout=2)
        except subprocess.TimeoutExpired:
            child.kill()
            child.wait(timeout=2)
    # No signals to unverified remembered descendant PIDs. A fixture's absolute
    # unique path and its group identity must still match before fallback cleanup.
    for event in trace_events(trace):
        if event.get('event') != 'spawn' or not alive(event['pid']):
            continue
        pid = event['pid']
        command = subprocess.run(['ps', '-ww', '-p', str(pid), '-o', 'command='],
                                 capture_output=True, timeout=2).stdout.decode(errors='replace')
        try:
            if str(folder) in command and os.getpgid(pid) == pid:
                os.killpg(pid, signal.SIGKILL)
        except ProcessLookupError:
            pass


def run_case(binary, folder, program, *, expected_code, expected_bytes=None,
             expected_hash=None, expected_size=None, expected_spawns=2,
             stop_signal=None, stalled=False, command_timeout_ms=5000,
             stderr_contains=None, timeout=30):
    path = folder/'probe.program'
    trace = folder/'trace.log'
    path.write_text(program)
    command = [str(binary), str(path), '--trace', '--manifest-dir', str(folder/'manifests'),
               '--command-timeout-ms', str(command_timeout_ms)]
    sha = hashlib.sha256()
    total = 0
    prefix = bytearray()
    started = time.monotonic()
    deadline = started + timeout
    sent = False
    ready_at = None
    child = None
    usage = None
    try:
        with trace.open('wb') as diagnostics:
            child = subprocess.Popen(command, cwd=folder, env=environment(),
                                     stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                                     stderr=diagnostics, start_new_session=True)
            while True:
                now = time.monotonic()
                if now >= deadline:
                    raise TimeoutError('probe exceeded its owned-process deadline')
                if stop_signal is not None and not sent:
                    spawns = [e for e in trace_events(trace) if e.get('event') == 'spawn']
                    if len(spawns) == expected_spawns:
                        if ready_at is None:
                            ready_at = now
                        if now - ready_at >= 0.05:
                            child.send_signal(stop_signal)
                            sent = True
                # Withhold all stdout reads until after the cancellation request.
                # This fills native output pipes without an unbounded Python queue.
                if stalled and not sent:
                    time.sleep(.005)
                    continue
                if not select.select([child.stdout], [], [], .01)[0]:
                    continue
                chunk = os.read(child.stdout.fileno(), 65536)
                if not chunk:
                    break
                total += len(chunk)
                sha.update(chunk)
                if len(prefix) < 65536:
                    prefix.extend(chunk[:65536-len(prefix)])
            # wait4 obtains this invocation's resource usage, rather than the
            # cumulative RUSAGE_CHILDREN high-water mark of earlier probes.
            while True:
                waited, status, usage = os.wait4(child.pid, os.WNOHANG)
                if waited:
                    child.returncode = os.waitstatus_to_exitcode(status)
                    break
                if time.monotonic() >= deadline:
                    raise TimeoutError('probe did not exit after stdout EOF')
                time.sleep(.001)
        events = trace_events(trace)
        spawns = [e for e in events if e.get('event') == 'spawn']
        assert len(spawns) == expected_spawns, ('native spawn count', spawns)
        assert child.returncode == expected_code, (child.returncode, trace.read_text(errors='replace'))
        if stop_signal is not None:
            assert sent, 'probe exited before its signal was sent'
        if expected_bytes is not None:
            assert total == len(expected_bytes) and bytes(prefix) == expected_bytes, ('output mismatch', total)
        if expected_size is not None:
            assert total == expected_size, ('byte count mismatch', total, expected_size)
        if expected_hash is not None:
            assert sha.hexdigest() == expected_hash, 'stream SHA-256 mismatch'
        if stderr_contains is not None:
            assert stderr_contains in trace.read_bytes(), ('expected diagnostic missing', trace.read_text(errors='replace')[-4096:])
        for event in spawns:
            assert not alive(event['pid']), f'owned native child {event["pid"]} survived runner exit'
        high_water = usage.ru_maxrss * (1 if platform.system() == 'Darwin' else 1024)
        return {'status': 'passed', 'bytes': total, 'sha256': sha.hexdigest(),
                'native_launches': len(spawns), 'exit_code': child.returncode,
                'command_high_water_rss_bytes': high_water,
                'wall_ms': round((time.monotonic()-started)*1000, 3)}
    finally:
        if child is not None:
            owned_cleanup(child, trace, folder)
            child.stdout.close()


def make_fixture(path, count):
    chunk = RECORD * 4096
    expected = hashlib.sha256()
    with path.open('wb') as output:
        remaining = count
        while remaining:
            part = chunk[:min(remaining, len(chunk))]
            output.write(part)
            expected.update(part)
            remaining -= len(part)
    return expected.hexdigest()


def atomic_report(path, report):
    temporary = path.with_suffix(path.suffix+'.tmp')
    temporary.write_text(json.dumps(report, indent=2)+'\n')
    temporary.replace(path)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, default=ROOT/'bin/execute')
    parser.add_argument('--report', type=Path, required=True)
    parser.add_argument('--stream-bytes', type=int, default=1_000_000_000)
    parser.add_argument('--lifecycle-repetitions', type=int, default=10,
                        help='cycles of all seven lifecycle cases; challenge target: 10000')
    args = parser.parse_args()
    if not __debug__:
        parser.error('proof verification requires Python without -O')
    if not 1 <= args.stream_bytes <= 2_000_000_000:
        parser.error('--stream-bytes must be between 1 and 2,000,000,000')
    if not 1 <= args.lifecycle_repetitions <= 10000:
        parser.error('--lifecycle-repetitions must be between 1 and 10000')
    binary = args.binary.resolve(strict=True)
    report_path = args.report.resolve()
    report_path.parent.mkdir(parents=True, exist_ok=True)
    manifests = sorted((ROOT/'manifests').glob('*.json'))
    report = {'schema_version': 1, 'status': 'running',
              'created_utc': datetime.datetime.now(datetime.timezone.utc).isoformat(),
              'platform': platform.platform(), 'machine': platform.machine(),
              'binary_sha256': digest(binary), 'harness_sha256': digest(__file__),
              'catalog_sha256': {p.name: digest(p) for p in manifests},
              'registered_applets': len(manifests),
              'requested': {'stream_bytes': args.stream_bytes,
                            'lifecycle_cycles': args.lifecycle_repetitions},
              'bulk': [], 'lifecycle': [],
              'limits': ['Local Mac/Linux host only; not Omarchy or full Unix parity.',
                         'One observed run per bulk size; wall times include tracing, hashing, and observation.',
                         'wait4 ru_maxrss is command high-water usage; not summed simultaneous process-tree RSS.',
                         'No cold-cache claim, hard realtime claim, or proof of arbitrary input sizes.',
                         'No SIGKILL/crash containment claim; escaped groups remain outside supervision.',
                         'Fixture generation and expected hash calculation occur before the measured invocation.']}
    atomic_report(report_path, report)
    try:
        with tempfile.TemporaryDirectory(prefix='execute replacement proofs ') as directory:
            folder = Path(directory)
            shutil.copytree(ROOT/'manifests', folder/'manifests')
            fixture = folder/'binary records with spaces.bin'
            for size in sorted({min(8*1024*1024, args.stream_bytes), args.stream_bytes}):
                expected = make_fixture(fixture, size)
                source = (f'stream from cat with path {json.dumps(str(fixture))}\n'
                          'pipe to cat\nshow each line\n')
                result = run_case(binary, folder, source, expected_code=0,
                                  expected_hash=expected, expected_size=size,
                                  command_timeout_ms=90000, timeout=95)
                result['input_bytes'] = size
                report['bulk'].append(result)
                atomic_report(report_path, report)
                print(f'bulk: {size:,} bytes, exact SHA-256, two native launches', flush=True)
            events = folder/'events with spaces.txt'
            events.write_bytes(b'one\x00\n\xfftwo\nthree\nfour\n')
            oversized = folder/'oversized record.bin'
            oversized.write_bytes(b'x'*65537+b'\n')
            fault = folder/'failing fixture.py'
            fault.write_text('import os\nos.write(1,b"partial\\n")\nos.write(2,b"fixture-status-7\\n")\nraise SystemExit(7)\n')
            (folder/'manifests/fault.json').write_text(json.dumps({
                'name': 'fault', 'binary': sys.executable,
                'arguments': [{'literal': '-I'}, {'literal': str(fault)}]}))
            (folder/'manifests/fail-stage.json').write_text(json.dumps({
                'name': 'fail-stage', 'binary': shutil.which('false'),
                'accepts_stdin': True, 'arguments': []}))
            tail = f'stream from tail with flags ["-n", "+1", "-f"] and path {json.dumps(str(events))}\npipe to cat\n'
            blocked_fixture = fixture
            if fixture.stat().st_size < 4*1024*1024:
                blocked_fixture = folder/'blocked consumer fixture.bin'
                make_fixture(blocked_fixture, 4*1024*1024)
            blocked = f'stream from cat with path {json.dumps(str(blocked_fixture))}\npipe to cat\nshow each line\n'
            cases = [
                ('tail-three-binary-records', tail+'take the first 3 lines\nshow each line\n',
                 dict(expected_code=0, expected_bytes=b'one\x00\n\xfftwo\nthree\n')),
                ('tail-ctrl-c', tail+'show each line\n', dict(expected_code=1, stop_signal=signal.SIGINT)),
                ('tail-timeout', tail+'show each line\n',
                 dict(expected_code=1, command_timeout_ms=100, stderr_contains=b'timed out')),
                ('blocked-consumer-cancel', blocked,
                 dict(expected_code=1, stalled=True, stop_signal=signal.SIGTERM)),
                ('producer-exit-seven', 'stream from fault\npipe to cat\nshow each line\n',
                 dict(expected_code=1, stderr_contains=b'fixture-status-7')),
                ('middle-stage-failure', f'stream from cat with path {json.dumps(str(events))}\npipe to fail-stage\npipe to cat\nshow each line\n',
                 dict(expected_code=1, expected_spawns=3)),
                ('oversized-record-rejected', f'stream from cat with path {json.dumps(str(oversized))}\nshow each line\n',
                 dict(expected_code=1, expected_spawns=1, stderr_contains=b'record exceeds')),
            ]
            progress_every = max(10, args.lifecycle_repetitions // 20)
            for cycle in range(args.lifecycle_repetitions):
                for name, source, options in cases:
                    report['active_case'] = {'case': name, 'cycle': cycle+1}
                    result = run_case(binary, folder, source, **options)
                    result.update(case=name, cycle=cycle+1)
                    report['lifecycle'].append(result)
                if (cycle+1) % progress_every == 0 or cycle+1 == args.lifecycle_repetitions:
                    atomic_report(report_path, report)
                    print(f'lifecycle: {cycle+1}/{args.lifecycle_repetitions} cycles, {len(report["lifecycle"])} probes', flush=True)
        report.pop('active_case', None)
        report['status'] = 'passed'
    except BaseException as error:
        report['status'] = 'failed'
        report['failure'] = f'{type(error).__name__}: {error}'
        raise
    finally:
        atomic_report(report_path, report)
    print(f'report: {report_path}', flush=True)


if __name__ == '__main__':
    main()
