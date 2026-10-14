#!/usr/bin/env python3
"""Time fresh one-shot CPU programs using real ps and jq, with checked output."""
import datetime, hashlib, json, math, pathlib, platform, statistics, subprocess, sys, tempfile, time
root=pathlib.Path(__file__).resolve().parents[1]
binary=pathlib.Path(sys.argv[1] if len(sys.argv)>1 else root/'bin/execute').resolve()
values=[]
with tempfile.TemporaryDirectory(prefix='execute-cpu-bench-') as directory:
    for n in range(60):
        start=time.perf_counter_ns()
        result=subprocess.run([str(binary),str(root/'examples/top-cpu.program'),'--headless','--manifest-dir',str(root/'manifests')],cwd=directory,capture_output=True,timeout=35)
        elapsed=(time.perf_counter_ns()-start)/1e6
        assert result.returncode==0,result.stderr.decode()
        data=json.loads(result.stdout)
        rows=data['programs']['details']['widgets']['records']['rows'];bars=data['programs']['monitor']['widgets']['usage']['bars']
        assert 0<len(rows)<=10 and len(rows)==len(bars)
        assert all(rows[i]['cpu']>=rows[i+1]['cpu'] for i in range(len(rows)-1))
        assert all(b['value']==r['cpu'] and b['label']==r['label'] for b,r in zip(bars,rows))
        if n>=10:values.append(elapsed)
a=sorted(values)
report=dict(version=subprocess.check_output([str(binary),'--version'],text=True).strip(),observed_at=datetime.datetime.now(datetime.timezone.utc).isoformat(),platform=platform.platform(),binary_sha256=hashlib.sha256(binary.read_bytes()).hexdigest(),program_sha256=hashlib.sha256((root/'examples/top-cpu.program').read_bytes()).hexdigest(),adapter_sha256=hashlib.sha256((root/'manifests/jq.ps.json').read_bytes()).hexdigest(),samples=50,warmups=10,p50_ms=statistics.median(a),p95_ms=a[math.ceil(len(a)*.95)-1],p99_ms=a[math.ceil(len(a)*.99)-1],raw_ms=values,method='Fresh execute process, real ps and jq, native pipe, JSON decoding, component routing, in-memory 100x30 rendering, JSON output capture, and exit. Excludes physical terminal writes. Dynamic rankings checked for each sample; no fixture data or CPU load generator. Warm filesystem caches; background processes uncontrolled.')
(root/'benchmarks/cpu-macos-arm64.json').write_text(json.dumps(report,indent=2)+'\n')
print(json.dumps({k:v for k,v in report.items() if k!='raw_ms'},indent=2))
