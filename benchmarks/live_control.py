#!/usr/bin/env python3
"""Measure a paused, real live server: local RPC and fresh CLI latency."""
import argparse
import hashlib
import json
from pathlib import Path
import platform
import random
import select
import socket
import statistics
import subprocess
import tempfile
import time

PROJECT = Path(__file__).resolve().parents[1]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, default=PROJECT / 'bin/execute')
    parser.add_argument('--rpc-samples', type=int, default=200)
    parser.add_argument('--cli-samples', type=int, default=20)
    parser.add_argument('--report', type=Path, required=True)
    args = parser.parse_args()
    if not 10 <= args.rpc_samples <= 10000 or not 10 <= args.cli_samples <= 1000:
        parser.error('use 10..10000 RPC samples and 10..1000 CLI samples')
    binary = args.binary.resolve()
    records = {key: [] for key in ['rpc_read', 'rpc_update', 'cli_read', 'cli_update', 'cli_version']}
    with tempfile.TemporaryDirectory(prefix='execute-control-bench-') as directory:
        root = Path(directory)
        script = root / 'report.program'
        script.write_text('run printf with message message\nthen save to "result.txt"\n')
        address = root / 'live.sock'
        child = subprocess.Popen([str(binary), 'live', 'serve', str(script), '--socket', str(address), '--paused', '--bindings', '{"message":"initial"}', '--manifest-dir', str(PROJECT / 'manifests')], cwd=root, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE)

        def rpc(op, **values):
            with socket.socket(socket.AF_UNIX) as stream:
                stream.settimeout(3)
                stream.connect(str(address))
                stream.sendall(json.dumps({'op': op, **values}).encode() + b'\n')
                with stream.makefile('rb') as reader:
                    result = json.loads(reader.readline(2 * 1024 * 1024))
            assert result['ok'], result
            return result

        def cli(*parts):
            run = subprocess.run([str(binary), *parts], cwd=root, capture_output=True, timeout=5)
            assert run.returncode == 0, run.stderr
            return run.stdout

        try:
            assert select.select([child.stdout], [], [], 10)[0], 'server startup timeout'
            assert json.loads(child.stdout.readline())['event'] == 'ready'
            for _ in range(10):
                rpc('read', path='/bindings/message')
                rpc('update', path='/bindings/message', value='warmup')
                cli('live', 'read', '/bindings/message', '--socket', str(address))
                cli('--version')
            trials = [('rpc_read', i) for i in range(args.rpc_samples)] + [('rpc_update', i) for i in range(args.rpc_samples)] + [(key, i) for key in ['cli_read', 'cli_update', 'cli_version'] for i in range(args.cli_samples)]
            random.Random(481).shuffle(trials)
            for key, index in trials:
                started = time.perf_counter_ns()
                if key == 'rpc_read':
                    result = rpc('read', path='/bindings/message')
                elif key == 'rpc_update':
                    result = rpc('update', path='/bindings/message', value=f'input-{index}')
                elif key == 'cli_read':
                    result = cli('live', 'read', '/bindings/message', '--socket', str(address))
                elif key == 'cli_update':
                    result = cli('live', 'update', '/bindings/message', '--json', json.dumps(f'cli-{index}'), '--socket', str(address))
                else:
                    result = cli('--version')
                records[key].append((time.perf_counter_ns() - started) / 1000)
                if key in {'cli_read', 'cli_update'}:
                    assert json.loads(result)['ok']
            # Functional proof outside the timed region: the edited binding reaches native argv/save.
            rpc('update', path='/bindings/message', value='agent-final')
            rpc('control', action='resume')
            deadline = time.monotonic() + 5
            while time.monotonic() < deadline:
                if rpc('read')['result']['runtime']['status'] == 'completed':
                    break
                time.sleep(.01)
            assert (root / 'result.txt').read_bytes() == b'agent-final\n'
            rpc('control', action='stop')
            assert child.wait(timeout=5) == 0
            assert not address.exists()
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
    records['rpc_ui_update_ack'] = []
    records['ui_update_to_applied'] = []
    with tempfile.TemporaryDirectory(prefix='execute-ui-bench-') as directory:
        root = Path(directory)
        script = root / 'ui.program'
        script.write_text('open a screen titled "Live control latency"\nplace panel on the left\ndefine a program named panel\n  show a text field named message with value "initial"\n')
        address = root / 'ui.sock'
        child = subprocess.Popen([str(binary), 'live', 'serve', str(script), '--socket', str(address), '--paused', '--headless', '--manifest-dir', str(PROJECT / 'manifests')], cwd=root, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        try:
            assert select.select([child.stdout], [], [], 10)[0]
            assert json.loads(child.stdout.readline())['event'] == 'ready'
            for index in range(60):
                text = f'agent-edit-{index}'
                started = time.perf_counter_ns()
                changed = rpc('update', path='/ui/programs/panel/widgets/message/text', value=text)
                ack_us = (time.perf_counter_ns() - started) / 1000
                deadline = time.monotonic() + 3
                while time.monotonic() < deadline:
                    observed = rpc('read', path='/runtime/ui_applied_revision')
                    if observed['result'] >= changed['revision']:
                        break
                    rpc('wait', path='/runtime/ui_applied_revision', after=observed['revision'], timeout_ms=100)
                else:
                    raise AssertionError('UI update did not reach the renderer')
                applied_us = (time.perf_counter_ns() - started) / 1000
                if index >= 10:
                    records['rpc_ui_update_ack'].append(ack_us)
                    records['ui_update_to_applied'].append(applied_us)
                assert text in rpc('read', path='/runtime/frame')['result']
            rpc('control', action='stop')
            assert child.wait(timeout=5) == 0
            assert not address.exists()
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
    def summarize(values):
        ordered = sorted(values)
        return {'samples': len(values), 'p50_us': round(statistics.median(values), 3), 'p95_us': round(ordered[min(len(ordered)-1, int(.95*(len(ordered)-1)))], 3), 'p99_us': round(ordered[min(len(ordered)-1, int(.99*(len(ordered)-1)))], 3), 'raw_us': values}
    report = {'execute_version': subprocess.check_output([str(binary), '--version']).decode().strip(), 'binary_sha256': hashlib.sha256(binary.read_bytes()).hexdigest(), 'harness_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest(), 'platform': platform.platform(), 'architecture': platform.machine(), 'method': 'Warm filesystem/executable caches. Ten warmups; shuffled/interleaved local trials against one paused server with no native jobs during measurements. RPC includes Python JSON/client overhead and a new Unix socket connection. CLI includes fresh process startup and JSON handling; version is a startup reference. UI phase: 10 warmups then 50 sequential edits; applied latency includes polling/IPC until the actual headless renderer publishes the edit, with frame content checked outside timing. Small payloads only. Background activity uncontrolled; low-sample CLI tails are descriptive. No Linux runtime claim or hard realtime guarantee.', 'functional_binding_to_native_output': 'passed outside timed region', 'functional_ui_frame_edits': 'all 60 actual renderer updates verified', 'results': {key: summarize(values) for key, values in records.items()}}
    args.report.write_text(json.dumps(report, indent=2) + '\n')
    print(json.dumps({key: {k: v for k, v in summary.items() if k != 'raw_us'} for key, summary in report['results'].items()}, indent=2))


if __name__ == '__main__':
    main()
