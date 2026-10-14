"""Run preserved release test executables serially in alternating order."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import statistics
import subprocess
from datetime import datetime, timezone

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--baseline', type=Path, required=True)
parser.add_argument('--candidate', type=Path, required=True)
parser.add_argument('--test', required=True)
parser.add_argument('--output', type=Path, required=True)
parser.add_argument('--rounds', type=int, default=3)
args = parser.parse_args()
assert args.rounds >= 3
args.output.mkdir(parents=True, exist_ok=True)
binaries = {name: getattr(args, name).resolve() for name in ['baseline', 'candidate']}
reports = {name: [] for name in binaries}
orders = []
for trial in range(args.rounds):
    order = ['baseline', 'candidate'] if trial % 2 == 0 else ['candidate', 'baseline']
    orders.append(order)
    for name in order:
        path = (args.output / f'{name}-{trial + 1}.json').resolve()
        result = subprocess.run([str(binaries[name]), '--ignored', '--exact', args.test, '--nocapture'], env={**os.environ, 'OPT_BENCH_REPORT': str(path)}, capture_output=True, timeout=120)
        assert result.returncode == 0, result.stderr.decode(errors='replace') + result.stdout.decode(errors='replace')
        report = json.loads(path.read_text())
        reports[name].append(report.get('results', report.get('operations')))
summary = {}
for case in reports['baseline'][0]:
    values = {name: [report[case]['p50_ns'] for report in variant] for name, variant in reports.items()}
    medians = {name: statistics.median(raw) for name, raw in values.items()}
    summary[case] = {'median_of_round_p50_ns': medians, 'round_p50_ns': values, 'candidate_change_percent': (medians['candidate']/medians['baseline']-1)*100}
output = {'recorded_at_utc': datetime.now(timezone.utc).isoformat(), 'platform': platform.platform(), 'harness_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest(), 'binary_sha256': {name: hashlib.sha256(path.read_bytes()).hexdigest() for name,path in binaries.items()}, 'test': args.test, 'order': orders, 'method': 'Serial preserved release test executables; alternating order. Each executable reports warmup and batched raw in-process samples. No concurrent builds/tests permitted. Round medians are descriptive, not a confidence interval or cold-start/OS-process measurement.', 'summary': summary}
(args.output / 'summary.json').write_text(json.dumps(output,indent=2)+'\n')
print(json.dumps(summary, indent=2))
