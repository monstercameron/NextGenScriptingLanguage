#!/usr/bin/env python3
"""Observe a release UI with a deterministic gh replacement; no API/network costs."""
import hashlib, importlib.util, json, math, os, pathlib, platform, statistics, subprocess, sys, tempfile, time
root=pathlib.Path(__file__).resolve().parents[1]
binary=pathlib.Path(sys.argv[1] if len(sys.argv)>1 else root/'bin/execute').resolve()
sys.argv=[sys.argv[0],str(binary)]
spec=importlib.util.spec_from_file_location('ui_pty',root/'tests/ui_pty.py'); module=importlib.util.module_from_spec(spec);spec.loader.exec_module(module)
def stats(values):
    a=sorted(values)
    return dict(samples=len(a),p50_ms=statistics.median(a),p95_ms=a[math.ceil(len(a)*.95)-1],p99_ms=a[math.ceil(len(a)*.99)-1],raw_ms=values)
refresh=[]; startup=[]
with tempfile.TemporaryDirectory(prefix='execute-ui-bench-') as directory:
    s=module.Session(directory)
    try:
        s.wait(lambda:len(s.calls())>=1 and b'Live data.' in s.data)
        for n in range(55):
            offset=len(s.data); count=len(s.calls())
            # Capture the first observation, before wait()'s convenience drain.
            observed=[]
            def complete():
                if len(s.calls())>count and b'Live data.' in s.data[offset:]:
                    if not observed:observed.append(time.perf_counter_ns())
                    return True
                return False
            start=time.perf_counter_ns();s.send(b'\x12');s.wait(complete)
            if n>=5:refresh.append((observed[0]-start)/1e6)
        s.finish()
        env=dict(os.environ,PATH=directory+':'+os.environ['PATH'],GH_FIXTURE_DIR=directory,GH_FIXTURE_MODE='normal')
        for n in range(55):
            start=time.perf_counter_ns()
            result=subprocess.run([str(binary),str(root/'examples/github-stars.program'),'--headless','--manifest-dir',str(root/'manifests')],cwd=directory,env=env,capture_output=True,timeout=35)
            elapsed=(time.perf_counter_ns()-start)/1e6
            assert result.returncode==0,result.stderr.decode()
            data=json.loads(result.stdout); assert data['completed_jobs']==1
            if n>=5:startup.append(elapsed)
    finally:s.close()
report=dict(version=subprocess.check_output([str(binary),'--version'],text=True).strip(),platform=platform.platform(),architecture=platform.machine(),binary_sha256=hashlib.sha256(binary.read_bytes()).hexdigest(),manifest_sha256=hashlib.sha256((root/'manifests/gh.repo.json').read_bytes()).hexdigest(),fixture_sha256=hashlib.sha256((root/'tests/fixtures/gh.py').read_bytes()).hexdigest(),ui_refresh_with_mock_gh=stats(refresh),headless_startup_with_mock_gh=stats(startup),method='50 samples after 5 warmups; PTY refresh includes key dispatch, worker/thread/process startup, Python fixture execution, JSON projection, rendering and observation. Headless includes fresh interpreter, output and exit. No network or real gh in these timings. Terminal observer uses a 20ms select deadline; rendering is measured in memory separately. Background applications uncontrolled; filesystem caches warm.')
path=root/'benchmarks/ui-macos-arm64.json';path.write_text(json.dumps(report,indent=2)+'\n')
print(json.dumps({k:v for k,v in report.items() if k not in ('ui_refresh_with_mock_gh','headless_startup_with_mock_gh')},indent=2))
print(json.dumps({k:{a:b for a,b in v.items() if a!='raw_ms'} for k,v in report.items() if isinstance(v,dict)},indent=2))
