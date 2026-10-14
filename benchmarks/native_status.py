#!/usr/bin/env python3
"""Paired warm CLI timings plus separately labelled native status-capture overhead."""
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


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--baseline', type=Path, required=True)
    parser.add_argument('--candidate', type=Path, required=True)
    parser.add_argument('--samples', type=int, default=60)
    parser.add_argument('--report', type=Path, required=True)
    args = parser.parse_args()
    assert args.samples >= 20
    binaries = {name: getattr(args, name).resolve() for name in ['baseline', 'candidate']}
    rng = random.Random(10142026)
    results = {}
    with tempfile.TemporaryDirectory(prefix='execute-status-bench-') as directory:
        root = Path(directory)
        cases = {
            'printf': ('run "printf" with arguments ["%s", "alpha"]\n', b'alpha', 'run', args.samples),
            'pipe3': ('run "printf" with arguments ["alpha"]\npipe to "cat"\npipe to "cat"\n', b'alpha', 'run', args.samples),
            'launch50': ('run "true"\n' * 50, b'', 'run', 20),
            'check1000': ('run "true"\n' * 1000, b'', 'check', args.samples),
        }
        for case, (source, output, verb, count) in cases.items():
            file = root / (case + '.program')
            file.write_text(source)
            variants = {name: (binary, file) for name, binary in binaries.items()}
            if case in ('printf', 'launch50'):
                captured = root / (case + '-capture.program')
                captured.write_text(''.join(line + f' and keep status as code_{index}\n'
                                           for index, line in enumerate(source.splitlines())))
                variants['candidate_capture'] = (binaries['candidate'], captured)
            raw = {name: [] for name in variants}
            for trial in range(count + 5):
                order = list(variants)
                rng.shuffle(order)
                for name in order:
                    binary, program = variants[name]
                    started = time.perf_counter_ns()
                    run = subprocess.run([str(binary), verb, str(program), '--manifest-dir', str(Path(__file__).resolve().parents[1] / 'manifests')],
                                         cwd=root, input=b'', capture_output=True, timeout=10)
                    elapsed = (time.perf_counter_ns() - started) / 1000
                    assert run.returncode == 0 and run.stdout == (output if verb == 'run' else b'') and not run.stderr, (case, name, run)
                    if trial >= 5:
                        raw[name].append(elapsed)
            results[case] = {'source_sha256': hashlib.sha256(source.encode()).hexdigest(),
                             'timings_us': {name: summary(values) for name, values in raw.items()}}
    report = {
        'at_utc': datetime.now(timezone.utc).isoformat(), 'platform': platform.platform(),
        'binaries': {name: {'path': str(binary), 'sha256': hashlib.sha256(binary.read_bytes()).hexdigest()} for name, binary in binaries.items()},
        'harness_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
        'method': 'Five warmups then deterministically shuffled interleaved release CLI trials. Baseline/candidate use identical source, native argv, the same existing project catalog and verified output/status. Candidate_capture is a DIFFERENT source that adds status bindings; native executable argv and stdout remain identical, so it measures feature overhead, not output-equivalent language semantics or a speedup. Capture cases use only success status 0. Warm filesystem, fresh CLI processes including startup/capture/exit. No concurrent builds/tests, cold-cache, peak-memory, native Linux or broad optimization claim.',
        'results': results,
    }
    args.report.write_text(json.dumps(report, indent=2) + '\n')
    print(json.dumps({case: {name: {key: value for key, value in timing.items() if key != 'raw_us'}
                            for name, timing in data['timings_us'].items()} for case, data in results.items()}, indent=2))


if __name__ == '__main__':
    main()
