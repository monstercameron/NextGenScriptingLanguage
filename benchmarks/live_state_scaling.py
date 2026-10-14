#!/usr/bin/env python3
"""Paired bounded-inbox RPC latency as unrelated live user state grows."""
import argparse
from datetime import datetime, timezone
import hashlib
import importlib.util
import json
from pathlib import Path
import platform
import random
import sys
import time
from direct_commands import summary

PROJECT = Path(__file__).resolve().parents[1]
# The integration helper owns and cleans up every temporary server.
saved = sys.argv
sys.argv = [str(PROJECT / 'tests/live_control.py')]
spec = importlib.util.spec_from_file_location('live_control', PROJECT / 'tests/live_control.py')
live = importlib.util.module_from_spec(spec)
spec.loader.exec_module(live)
sys.argv = saved


def trial(binary, padding, records=64):
    live.BINARY = binary
    with live.Session('listen to stdin\nwhenever a line arrives as event\n  then cat\n') as app:
        for index, start in enumerate(range(0, padding, 900000)):
            app.ok('create', path=f'/user/pad{index}', value='x' * min(900000, padding-start))
        payloads = [{'text': f'{index:04}:' + 'r' * 128 + '\n'} for index in range(records)]
        started = time.perf_counter_ns()
        for payload in payloads:
            app.ok('send', value=payload)
        elapsed = (time.perf_counter_ns() - started) / 1000
        state = app.read()
        assert state['state']['inbox'] == payloads
        assert not state['runtime']['running']
        assert len(json.dumps(state['state'], ensure_ascii=False, separators=(',', ':')).encode()) <= 8*1024*1024
        return elapsed


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--baseline', type=Path, required=True)
    parser.add_argument('--candidate', type=Path, required=True)
    parser.add_argument('--samples', type=int, default=12)
    parser.add_argument('--report', type=Path, required=True)
    args = parser.parse_args()
    assert args.samples >= 5
    binaries = {'baseline': args.baseline.resolve(), 'candidate': args.candidate.resolve()}
    rng, results = random.Random(89562026), {}
    for padding in (0, 1024*1024, 6*1024*1024):
        raw = {name: [] for name in binaries}
        for index in range(args.samples+2):
            order = list(binaries)
            rng.shuffle(order)
            for name in order:
                elapsed = trial(binaries[name], padding)
                if index >= 2:
                    raw[name].append(elapsed)
        results[str(padding)] = {name: summary(values) for name, values in raw.items()}
    report = {'recorded_at_utc': datetime.now(timezone.utc).isoformat(), 'platform': platform.platform(),
              'binaries': {name: hashlib.sha256(path.read_bytes()).hexdigest() for name,path in binaries.items()},
              'harness_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
              'helper_sha256': hashlib.sha256((PROJECT/'tests/live_control.py').read_bytes()).hexdigest(),
              'method': 'Fresh paused servers; padding/setup and final FIFO/document-size verification outside timing; 64 identical record send RPCs per trial. Two warmups, shuffled paired order, warm filesystem/executable caches. Python/socket/JSON transport included. No simultaneous builds or tests. No RSS, allocation-count, native Linux or whole-runtime claim.',
              'results': results}
    args.report.write_text(json.dumps(report, indent=2)+'\n')
    print(json.dumps({case:{name:{k:v for k,v in values.items() if k!='raw_us'} for name,values in variants.items()} for case,variants in results.items()},indent=2))

if __name__ == '__main__':
    main()
