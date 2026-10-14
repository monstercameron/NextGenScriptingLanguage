#!/usr/bin/env python3
"""Compare explicit native JSON extraction with equivalent jq workflows and unchanged controls."""
import argparse
from datetime import datetime, timezone
import hashlib
import json
from pathlib import Path
import platform
import random
import shutil
import subprocess
import tempfile
import time
from direct_commands import summary

PROJECT = Path(__file__).resolve().parents[1]


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--baseline', type=Path, required=True)
    parser.add_argument('--candidate', type=Path, required=True)
    parser.add_argument('--samples', type=int, default=80)
    parser.add_argument('--report', type=Path, required=True)
    args = parser.parse_args()
    assert args.samples >= 20
    binaries = {name: getattr(args, name).resolve() for name in ['baseline', 'candidate']}
    jq = Path(shutil.which('jq')).resolve()
    rng = random.Random(1014202613)
    results = {}
    with tempfile.TemporaryDirectory(prefix='execute-json-bench-') as directory:
        root = Path(directory)
        payloads = {}
        for size in [1024, 1024 * 1024, 3 * 1024 * 1024]:
            payload = json.dumps({'padding': 'x' * size, 'result': {'name': 'snow 雪', 'count': 42}}, ensure_ascii=False).encode()
            (root / f'input-{size}.json').write_bytes(payload)
            payloads[size] = payload
        for case in ['native_printf_control', 'native_pipe_control', 'check_1000_control', 'json_1024', 'json_1048576', 'json_3145728']:
            if case.startswith('json_'):
                size = int(case.split('_')[1])
                ordinary = f'read "input-{size}.json"\npipe to {json.dumps(str(jq))} with arguments ["-cj", ".result"]\n'
                structured = f'read "input-{size}.json"\nthen parse json as document\ntake json at "/result" from document as selected\nthen encode selected as json\n'
                sources = {'baseline_jq': ordinary, 'candidate_jq_control': ordinary, 'candidate_structured': structured}
                oracle = b'{"count":42,"name":"snow \xe9\x9b\xaa"}'
                # jq preserves input object order while the native encoder sorts keys.
                # Use sorted input object keys so this is an exact byte oracle, not just JSON equality.
                payload = json.dumps({'padding': 'x' * size, 'result': {'count': 42, 'name': 'snow 雪'}}, ensure_ascii=False).encode()
                (root / f'input-{size}.json').write_bytes(payload)
                payloads[size] = payload
            else:
                source, oracle = {
                    'native_printf_control': ('run "printf" with arguments ["%s", "owned payload"]\n', b'owned payload'),
                    'native_pipe_control': ('run "printf" with arguments ["%s", "owned payload"]\npipe to "cat"\npipe to "cat"\n', b'owned payload'),
                    'check_1000_control': ('run "true"\n' * 1000, b''),
                }[case]
                sources = {'baseline': source, 'candidate': source}
            raw = {name: [] for name in sources}
            launches = {}
            for name, source in sources.items():
                (root / (name + '.program')).write_text(source)
                if 'check_' not in case:
                    binary = binaries['baseline' if name.startswith('baseline') else 'candidate']
                    trace = subprocess.run([str(binary), str(root / (name + '.program')), '--trace', '--manifest-dir', str(PROJECT / 'manifests')], cwd=root, input=b'', capture_output=True, timeout=10)
                    assert trace.returncode == 0 and trace.stdout == oracle, (case, name, trace)
                    launches[name] = sum(json.loads(line).get('event') == 'spawn' for line in trace.stderr.splitlines())
            for trial in range(args.samples + 5):
                order = list(sources)
                rng.shuffle(order)
                for name in order:
                    binary = binaries['baseline' if name.startswith('baseline') else 'candidate']
                    command = [str(binary)] + (['check'] if case.startswith('check_') else []) + [str(root / (name + '.program')), '--manifest-dir', str(PROJECT / 'manifests')]
                    started = time.perf_counter_ns()
                    result = subprocess.run(command, cwd=root, input=b'', capture_output=True, timeout=10)
                    elapsed = (time.perf_counter_ns() - started) / 1000
                    assert result.returncode == 0 and result.stdout == oracle and not result.stderr, (case, name, result)
                    if trial >= 5:
                        raw[name].append(elapsed)
            results[case] = {'sources': sources, 'source_sha256': {name: hashlib.sha256(source.encode()).hexdigest() for name, source in sources.items()}, 'oracle_hex': oracle.hex(), 'native_launches_verified_outside_timing': launches, 'timings_us': {name: summary(values) for name, values in raw.items()}}
    report = {'at_utc': datetime.now(timezone.utc).isoformat(), 'platform': platform.platform(), 'binaries': {name: {'path': str(path), 'sha256': digest(path)} for name, path in binaries.items()}, 'jq': {'path': str(jq), 'sha256': digest(jq), 'version': subprocess.check_output([str(jq), '--version']).decode().strip()}, 'harness_sha256': digest(Path(__file__)), 'catalog_sha256': {p.name: digest(p) for p in sorted((PROJECT / 'manifests').glob('*.json'))}, 'inputs_sha256': {str(size): hashlib.sha256(payload).hexdigest() for size, payload in payloads.items()}, 'method': 'Fresh CLI runs, five warmups and shuffled interleaved trials. Exact stdout/status/stderr oracles; trace launch counts outside timing. Warm filesystem, serial samples, no concurrent builds/tests. Unchanged native/check controls use identical source and argv across binaries. JSON variants compare different explicit implementations of the same selected result: existing native jq versus bounded interpreter parse/select/encode. This measures complete task latency, including startup and avoiding jq launch; not isolated parser latency, whole-runtime speedup, RSS or native Linux execution. Both JSON implementations materialize finite input; native encoding orders object keys.', 'results': results}
    args.report.write_text(json.dumps(report, indent=2) + '\n')
    print(json.dumps({case: {name: {key: value for key, value in values.items() if key != 'raw_us'} for name, values in data['timings_us'].items()} for case, data in results.items()}, indent=2))


if __name__ == '__main__':
    main()
