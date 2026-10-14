#!/usr/bin/env python3
"""Paired completed-checkpoint validation for repeated native executables."""
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
    parser.add_argument('--baseline', type=Path, required=True)
    parser.add_argument('--candidate', type=Path, required=True)
    parser.add_argument('--samples', type=int, default=30)
    parser.add_argument('--report', type=Path, required=True)
    args = parser.parse_args()
    assert args.samples >= 5
    binaries = {'baseline': args.baseline.resolve(), 'candidate': args.candidate.resolve()}
    rng, results = random.Random(9092026), {}
    with tempfile.TemporaryDirectory(prefix='execute-resume-validation-') as directory:
        root = Path(directory)
        for nodes in (1, 32, 256):
            script = root/f'{nodes}.program'
            # Existing native applets; no wrappers, binary modification or application compilation.
            script.write_text('run "/bin/cat"\n' + 'then "/bin/cat"\n'*(nodes-1))
            checkpoint = root/f'checkpoint-{nodes}'
            setup = subprocess.run([str(binaries['baseline']), str(script), '--checkpoint', str(checkpoint)],
                                   cwd=root, input=b'', capture_output=True, timeout=30)
            assert setup.returncode == 0 and setup.stdout == b'', setup.stderr
            before = (checkpoint/'journal.json').read_bytes()
            raw = {name: [] for name in binaries}
            for index in range(args.samples+3):
                order = list(binaries)
                rng.shuffle(order)
                for name in order:
                    started = time.perf_counter_ns()
                    run = subprocess.run([str(binaries[name]), 'resume', str(script), '--checkpoint', str(checkpoint), '--trace'],
                                         cwd=root, input=b'', capture_output=True, timeout=10)
                    elapsed = (time.perf_counter_ns()-started)/1000
                    assert run.returncode == 0 and run.stdout == b'', run.stderr
                    events = [json.loads(line) for line in run.stderr.splitlines()]
                    assert not any(event['event'] == 'spawn' for event in events), events
                    assert any(event['event'] == 'checkpoint_resume' and event['restored_nodes'] == nodes for event in events)
                    assert (checkpoint/'journal.json').read_bytes() == before
                    if index >= 3:
                        raw[name].append(elapsed)
            results[str(nodes)] = {name: summary(values) for name,values in raw.items()}
    report = {'recorded_at_utc':datetime.now(timezone.utc).isoformat(),'platform':platform.platform(),
              'binaries':{name:hashlib.sha256(binary.read_bytes()).hexdigest() for name,binary in binaries.items()},
              'native_executable':{'path':'/bin/cat','size':Path('/bin/cat').stat().st_size,'sha256':hashlib.sha256(Path('/bin/cat').read_bytes()).hexdigest()},
              'harness_sha256':hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
              'method':'Completed baseline checkpoints for 1/32/256 identical native commands. Checkpoint setup excluded; same source/cwd/checkpoint/argv/environment for both variants. Three warmups, shuffled paired trials, wall time includes CLI startup and trace capture. Exact output, no native respawn, restored node count and unchanged journal verified outside timing. Warm caches, no concurrent builds/tests. Not fresh execution latency, a cold-cache result or native Linux execution.',
              'results':results}
    args.report.write_text(json.dumps(report,indent=2)+'\n')
    print(json.dumps({case:{name:{k:v for k,v in data.items() if k!='raw_us'} for name,data in variants.items()} for case,variants in results.items()},indent=2))

if __name__ == '__main__':
    main()
