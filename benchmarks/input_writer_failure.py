#!/usr/bin/env python3
"""Bounded owned fixture: child exits while a short-lived escaped reader holds stdin."""
import argparse
from datetime import datetime, timezone
import hashlib
import json
from pathlib import Path
import platform
import subprocess
import sys
import tempfile
import time

WORKER = '''import os,time
ready_r,ready_w=os.pipe()
pid=os.fork()
if pid==0:
    os.close(ready_r)
    os.setsid()
    os.close(1)
    os.close(2)
    os.write(ready_w,b"ready")
    os.close(ready_w)
    end=time.monotonic()+.8
    while time.monotonic()<end and not os.path.exists("release-reader"):
        time.sleep(.005)
    os.close(0)
    open("reader-ended","w").close()
    os._exit(0)
os.close(ready_w)
os.read(ready_r,5)
os.close(ready_r)
print("owner finished",flush=True)
'''


def probe(binary):
    with tempfile.TemporaryDirectory(prefix='execute-writer-failure-') as directory:
        root = Path(directory)
        (root / 'payload').write_bytes(b'x' * (1024 * 1024))
        (root / 'worker.py').write_text(WORKER)
        source = 'read "payload" as data\nthen ' + json.dumps(sys.executable) + ' with arguments ["worker.py"]\n'
        (root / 'case.program').write_text(source)
        started = time.perf_counter_ns()
        try:
            result = subprocess.run([str(binary), str(root / 'case.program'), '--command-timeout-ms', '150'],
                                    cwd=root, stdin=subprocess.DEVNULL, capture_output=True, timeout=5)
            elapsed_ms = (time.perf_counter_ns() - started) / 1_000_000
        finally:
            # This affects only the owned short-lived fixture. Its .8s self-limit
            # guarantees bounded lifetime even if the supervisor is regressed.
            (root / 'release-reader').touch()
            end = time.monotonic() + 2
            while not (root / 'reader-ended').exists() and time.monotonic() < end:
                time.sleep(.005)
        assert (root / 'reader-ended').exists(), 'owned fixture failed to close its reader'
        return {'elapsed_ms': elapsed_ms, 'status': result.returncode,
                'stdout': result.stdout.decode(), 'stderr': result.stderr.decode(),
                'reader_cleanup': 'owned reader closed stdin and ended its fixture'}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', required=True, type=Path)
    parser.add_argument('--samples', type=int, default=3)
    parser.add_argument('--report', required=True, type=Path)
    args = parser.parse_args()
    if not 1 <= args.samples <= 20:
        parser.error('--samples must be 1..20')
    binary = args.binary.resolve()
    results = [probe(binary) for _ in range(args.samples)]
    report = {'recorded_at_utc': datetime.now(timezone.utc).isoformat(), 'platform': platform.platform(),
              'binary_sha256': hashlib.sha256(binary.read_bytes()).hexdigest(),
              'harness_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
              'deadline_ms': 150, 'fixture_max_lifetime_ms': 800, 'results': results,
              'method': 'Functional deadline probe, not a successful-workload speed comparison. A bounded owned child deliberately changes only its own session and retains the generated stdin pipe after its owner exits. The fixture self-expires within .8 seconds or is released in finally; it closes stdout/stderr. Native process startup and CLI overhead included. No unrelated processes/signals or host configuration involved.'}
    args.report.write_text(json.dumps(report, indent=2) + '\n')
    print(json.dumps(results, indent=2))


if __name__ == '__main__':
    main()
