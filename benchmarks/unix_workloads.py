#!/usr/bin/env python3
"""Matched Unix workloads. Only owned temporary files/processes; no network or Git writes."""
import argparse
import datetime
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import random
import re
import select
import shlex
import shutil
import signal
import statistics
import subprocess
import sys
import tempfile
import time

ROOT = Path(__file__).resolve().parents[1]
STARTUP_HOOKS = {'BASH_ENV', 'ENV', 'SHELLOPTS', 'BASHOPTS', 'CDPATH', 'GLOBIGNORE', 'PYTHONPATH', 'PYTHONHOME', 'PYTHONSTARTUP'}


def child_environment():
    environment = {key:value for key,value in os.environ.items()
                   if key not in STARTUP_HOOKS and not key.startswith('BASH_FUNC_')
                   and not value.lstrip().startswith('() {')}
    environment['LC_ALL'] = 'C'
    return environment



def distribution(values):
    ordered = sorted(values)
    return dict(samples=len(values), p50=statistics.median(ordered),
                p95=ordered[math.ceil(len(ordered)*.95)-1],
                p99=ordered[math.ceil(len(ordered)*.99)-1], raw=values)


def digest_file(path):
    digest = hashlib.sha256()
    with Path(path).open('rb') as file:
        for chunk in iter(lambda: file.read(1024*1024), b''):
            digest.update(chunk)
    return digest.hexdigest()


def run(command, cwd, *, output=None, timeout=60):
    started = time.perf_counter_ns()
    child = subprocess.Popen(command, cwd=cwd, env=child_environment(),
                             stdout=output if output is not None else subprocess.PIPE,
                             stderr=subprocess.PIPE, start_new_session=True)
    try:
        stdout, stderr = child.communicate(timeout=timeout)
    except BaseException:
        if child.poll() is None:
            child.send_signal(signal.SIGTERM)
        try:
            child.communicate(timeout=2)
        except subprocess.TimeoutExpired:
            os.killpg(child.pid, signal.SIGKILL)
            child.communicate()
        raise
    return dict(code=child.returncode, stdout=stdout, stderr=stderr,
                elapsed_ms=(time.perf_counter_ns()-started)/1e6)


def read_exact(pipe, length, timeout=10):
    output = bytearray()
    deadline = time.monotonic()+timeout
    while len(output)<length:
        remaining = deadline-time.monotonic()
        if remaining<=0 or not select.select([pipe], [], [], remaining)[0]:
            raise TimeoutError('live listener response deadline')
        part = os.read(pipe.fileno(), length-len(output))
        if not part:
            raise AssertionError('listener exited before response')
        output.extend(part)
    return bytes(output)


def max_rss(command, cwd, output_path):
    # Separate observation: instrumentation is excluded from wall-time samples.
    if platform.system() == 'Darwin':
        wrapper = ['/usr/bin/time', '-l']
        pattern, scale = rb'(\d+)\s+maximum resident set size', 1
    else:
        wrapper = ['/usr/bin/time', '-v']
        pattern, scale = rb'Maximum resident set size \(kbytes\):\s*(\d+)', 1024
    with output_path.open('wb') as output:
        result = run(wrapper+command, cwd, output=output)
    assert result['code']==0, result['stderr']
    match = re.search(pattern, result['stderr'])
    if not match:
        raise AssertionError('time did not report max RSS')
    return int(match[1])*scale


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, default=ROOT/'bin/execute')
    parser.add_argument('--output', type=Path, default=ROOT/'benchmarks/unix-workloads.json')
    parser.add_argument('--startup-samples', type=int, default=1000)
    parser.add_argument('--launch-count', type=int, default=1000)
    parser.add_argument('--launch-samples', type=int, default=5)
    parser.add_argument('--bulk-samples', type=int, default=5)
    parser.add_argument('--line-count', type=int, default=1000000)
    parser.add_argument('--json-count', type=int, default=100000)
    parser.add_argument('--event-count', type=int, default=200)
    args = parser.parse_args()
    if any(value<=0 for value in [args.startup_samples,args.launch_count,args.launch_samples,args.bulk_samples,args.line_count,args.json_count,args.event_count]):
        parser.error('counts and samples must be positive')
    if args.line_count * len(b'alpha beta\n') > 16*1024*1024:
        parser.error('line count exceeds finite-output budget for this workload')
    binary = args.binary.resolve()
    bash = shutil.which('bash')
    commands = {name: shutil.which(name) for name in ['printf','true','cat','tr','jq','tail','mkfifo']}
    if not bash or not all(commands.values()):
        parser.error('Bash, printf, true, cat, tr, jq, tail and mkfifo are required')
    report = dict(version=subprocess.check_output([binary,'--version'],text=True).strip(),
                  observed_at=datetime.datetime.now(datetime.timezone.utc).isoformat(),
                  platform=platform.platform(), architecture=platform.machine(),
                  binary_sha256=digest_file(binary), harness_sha256=digest_file(__file__),
                  bash_version=subprocess.check_output([bash,'--version'],text=True).splitlines()[0],
                  applets=commands, applet_sha256={k:digest_file(v) for k,v in commands.items()},
                  nushell=dict(status='not measured', executable=shutil.which('nu'), reason='No matched Nushell adapter in this run; no installation performed'),
                  method=dict(startup='fresh interpreters with warm filesystem caches; not OS-cold starts',
                              wall_time='spawn through output delivery and exit; correctness verification and preparation excluded',
                              sampling='deterministically shuffled paired wall-time/cancellation order and RSS observation order; no concurrent benchmark phases',
                              child_environment='LC_ALL=C; Bash/POSIX/Python startup-hook vars and imported shell functions removed',
                              rss='separate /usr/bin/time observation, normalized to bytes; reported high-water RSS, not simultaneous summed process-tree memory',
                              safety='owned temp directories and process groups; no host configuration changes, API calls, Git writes, or filesystem cache flushing',
                              conditions='LC_ALL=C; uncontrolled background applications; no CPU affinity or scheduler tuning'),
                  cases={})
    rng = random.Random(20261014)
    def save():
        args.output.parent.mkdir(parents=True,exist_ok=True)
        args.output.write_text(json.dumps(report,indent=2)+'\n')
    def progress(message):
        print(message, file=sys.stderr, flush=True);save()
    with tempfile.TemporaryDirectory(prefix='execute-unix-bench-') as directory:
        cwd=Path(directory); catalog=cwd/'manifests';shutil.copytree(ROOT/'manifests',catalog)
        (catalog/'true.json').write_text(json.dumps(dict(name='true',binary=commands['true'],arguments=[])))
        failure=cwd/'fail7';failure.write_text('#!/bin/sh\ncat >/dev/null\nprintf "deliberate middle failure\\n" >&2\nexit 7\n');failure.chmod(0o700)
        (catalog/'fail7.json').write_text(json.dumps(dict(name='fail7',binary=str(failure),accepts_stdin=True,arguments=[])))
        report['manifest_sha256']={p.name:digest_file(p) for p in sorted(catalog.glob('*.json'))}
        def execute(path, extra=()):
            return [str(binary),str(path),'--manifest-dir',str(catalog),*extra]
        def shell(path):
            return [bash,'--noprofile','--norc',str(path)]
        def write_pair(name, program, source):
            a=cwd/(name+'.program');a.write_text(program)
            b=cwd/(name+'.bash');b.write_text(source)
            return dict(execute=execute(a),bash=shell(b))
        def repeated(cases, samples, check, warmups=2):
            values={key:[] for key in cases}
            for _ in range(warmups):
                for key, command in cases.items():check(run(command,cwd),key)
            keys=list(cases)
            for _ in range(samples):
                rng.shuffle(keys)
                for key in keys:
                    result=run(cases[key],cwd);check(result,key);values[key].append(result['elapsed_ms'])
            return {key:distribution(value) for key,value in values.items()}
        def check_print(result, _):
            assert result['code']==0 and result['stdout']==b'hello\n',result
        cases=write_pair('print','run printf with message "hello"\n',f"{shlex.quote(commands['printf'])} '%s\\n' hello\n")
        report['cases']['fresh_start']=dict(status='measured; OS-cold-cache variant pending',milliseconds=repeated(cases,args.startup_samples,check_print,10),external_commands_per_run=1)
        progress('fresh-start workload complete')
        cases=write_pair('launch','run true\n'*args.launch_count,f"for ((i=0;i<{args.launch_count};i++)); do {shlex.quote(commands['true'])}; done\n")
        def check_empty(result, _):assert result['code']==0 and result['stdout']==b'',result
        launches=repeated(cases,args.launch_samples,check_empty,1)
        trace=run(cases['execute']+['--trace'],cwd)
        trace_rows=[json.loads(line) for line in trace['stderr'].splitlines()]
        launch_count=sum(row.get('event')=='spawn' for row in trace_rows)
        assert trace['code']==0 and launch_count==args.launch_count
        report['cases']['external_launches']=dict(status='measured',commands_per_run=args.launch_count,milliseconds=launches,execute_launches_observed=launch_count,execute_source_bytes=(cwd/'launch.program').stat().st_size,bash_source_bytes=(cwd/'launch.bash').stat().st_size,note='Execute unrolled statements versus Bash loop; exact external true binary for both, no builtin substitution')
        progress('external-launch workload complete')
        fixture=cwd/'million.txt';line=b'alpha beta\n'
        with fixture.open('wb') as file:
            for start in range(0,args.line_count,10000):file.write(line*min(10000,args.line_count-start))
        expected=hashlib.sha256((line.upper())*args.line_count).hexdigest()
        cases=write_pair('transform','run cat with path "million.txt"\npipe to tr with from "[:lower:]" and to "[:upper:]"\n',f"{shlex.quote(commands['cat'])} -- million.txt | {shlex.quote(commands['tr'])} '[:lower:]' '[:upper:]'\n")
        # Output size is deliberately below the default 16 MiB finite buffer.
        timings={key:[] for key in cases};rss={}
        for index in range(args.bulk_samples+2):
            keys=list(cases);rng.shuffle(keys)
            for key in keys:
                output_path=cwd/(key+'.out')
                with output_path.open('wb') as output:result=run(cases[key],cwd,output=output)
                assert result['code']==0 and digest_file(output_path)==expected,result
                if index>=2:timings[key].append(result['elapsed_ms'])
        rss_order=list(cases);rng.shuffle(rss_order)
        for key in rss_order:
            command=cases[key]
            rss[key]=max_rss(command,cwd,cwd/(key+'.out'))
            assert digest_file(cwd/(key+'.out'))==expected
        report['cases']['million_line_transform']=dict(status='measured; finite final capture, not persistent listener streaming',lines=args.line_count,bytes=fixture.stat().st_size,milliseconds={k:distribution(v) for k,v in timings.items()},lines_per_second_p50={k:args.line_count/(statistics.median(v)/1000) for k,v in timings.items()},reported_max_rss_bytes=rss,output_sha256=expected,external_pipeline_stages=2,note='Same external cat and tr; Execute captures final stdout before delivery, Bash streams to file')
        progress('million-line transform workload complete')
        staged=cwd/'staged-producer'
        staged.write_text('#!/usr/bin/env python3\nimport os,pathlib,time\npathlib.Path("source.ready").write_text(str(os.getpid()))\nos.write(1,b"EARLY\\n")\nwhile not pathlib.Path("source.release").exists(): time.sleep(.005)\nos.write(1,b"LATE\\n")\n');staged.chmod(0o700)
        (catalog/'staged.json').write_text(json.dumps(dict(name='staged',binary=str(staged),arguments=[])))
        probes=write_pair('firstbyte','run staged\npipe to cat\n',f"./staged-producer | {shlex.quote(commands['cat'])}\n")
        first_byte={}
        for key,command in probes.items():
            (cwd/'source.ready').unlink(missing_ok=True);(cwd/'source.release').unlink(missing_ok=True)
            child=subprocess.Popen(command,cwd=cwd,env=child_environment(),stdout=subprocess.PIPE,stderr=subprocess.PIPE,start_new_session=True,bufsize=0)
            try:
                until=time.monotonic()+5
                while not (cwd/'source.ready').exists():
                    assert child.poll() is None and time.monotonic()<until,'staged producer did not start'
                    time.sleep(.005)
                arrived=bool(select.select([child.stdout],[],[],.2)[0])
                early=os.read(child.stdout.fileno(),6) if arrived else b''
                assert not arrived or early==b'EARLY\n'
                assert child.poll() is None,'source was released prematurely'
                (cwd/'source.release').write_text('release')
                rest,stderr=child.communicate(timeout=5)
                assert child.returncode==0 and early+rest==b'EARLY\nLATE\n',stderr
                first_byte[key]=dict(bytes_observed_before_producer_release=len(early),source_still_running_at_observation=True)
            finally:
                (cwd/'source.release').write_text('release')
                if child.poll() is None:
                    child.send_signal(signal.SIGTERM)
                    try:child.wait(timeout=2)
                    except subprocess.TimeoutExpired:os.killpg(child.pid,signal.SIGKILL);child.wait()
                child.stdout.close();child.stderr.close()
        assert first_byte['bash']['bytes_observed_before_producer_release']==6
        assert first_byte['execute']['bytes_observed_before_producer_release']==0
        report['cases']['final_consumer_streaming']=dict(status='verified finite capture gap',results=first_byte,method='Same owned producer and external cat; producer gates EOF on a file. Allow 200ms for early output, release producer, then check complete bytes. A correctness probe, not a latency distribution')
        progress('final-consumer streaming probe complete')
        records=[dict(id=i,score=(i*31)%10000,keep=i%3!=0) for i in range(args.json_count)]
        (cwd/'records.json').write_text(json.dumps(records,separators=(',',':')))
        wanted=sorted((r for r in records if r['keep']),key=lambda r:(-r['score'],r['id']))
        filter_text='map(select(.keep)) | sort_by([-.score, .id])'
        cases=write_pair('records','run cat with path "records.json"\npipe to jq with flags ["-c"] and filter '+json.dumps(filter_text)+'\n',f"{shlex.quote(commands['cat'])} -- records.json | {shlex.quote(commands['jq'])} -c -- {shlex.quote(filter_text)}\n")
        def check_json(result, _):assert result['code']==0 and json.loads(result['stdout'])==wanted
        timings=repeated(cases,args.bulk_samples,check_json,2)
        rss={}
        rss_order=list(cases);rng.shuffle(rss_order)
        for key in rss_order:
            command=cases[key]
            rss[key]=max_rss(command,cwd,cwd/(key+'.json.out'));assert json.loads((cwd/(key+'.json.out')).read_text())==wanted
        report['cases']['structured_records']=dict(status='measured through shared jq applet; not native language structured values',input_records=args.json_count,output_records=len(wanted),jq_filter=filter_text,milliseconds=timings,reported_max_rss_bytes=rss,oracle='independent Python predicate and sort by descending score, ascending id')
        progress('structured-data workload complete')
        cases=write_pair('fail','run printf with message "before"\npipe to fail7\npipe to cat\nthen save to "must-not-exist"\n',f"set -o pipefail\n{shlex.quote(commands['printf'])} '%s\\n' before | ./fail7 | {shlex.quote(commands['cat'])} && printf wrong >must-not-exist\n")
        errors={}
        for key,command in cases.items():
            result=run(command,cwd)
            assert result['code']!=0 and not (cwd/'must-not-exist').exists()
            assert b'deliberate middle failure' in result['stderr']
            errors[key]=dict(exit_code=result['code'],stderr=result['stderr'].decode(errors='replace'),stdout_bytes=len(result['stdout']),later_effect_prevented=True)
        report['cases']['middle_pipeline_failure']=dict(status='verified',results=errors,note='Bash uses pipefail; actual CLI and native diagnostic status codes are retained above. Source candidate0.9.0-dev preserves native status codes.')
        progress('middle-failure workload complete')
        listener=cwd/'events.program';listener.write_text('listen to stdin\nwhenever a line arrives as record\n  then tr with from "[:lower:]" and to "[:upper:]"\n')
        listener_bash=cwd/'events.bash'
        listener_bash.write_text(f"count=0\nwhile IFS= read -r record; do\n  printf '%s\\n' \"$record\" | {shlex.quote(commands['tr'])} '[:lower:]' '[:upper:]'\n  count=$((count+1))\n  if [ \"$count\" -eq {args.event_count} ]; then break; fi\ndone\n")
        listener_cases=dict(execute=execute(listener,['--max-runs',str(args.event_count)]),bash=shell(listener_bash))
        listener_results={};listener_order=list(listener_cases);rng.shuffle(listener_order)
        for key in listener_order:
            child=subprocess.Popen(listener_cases[key],cwd=cwd,env=child_environment(),stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,bufsize=0,start_new_session=True)
            times=[]
            try:
                for _ in range(args.event_count):
                    started=time.perf_counter_ns();child.stdin.write(b'alpha\n');assert read_exact(child.stdout,6)==b'ALPHA\n';times.append((time.perf_counter_ns()-started)/1000)
                child.stdin.close();child.stdin=None
                stdout,stderr=child.communicate(timeout=10)
                assert child.returncode==0 and stdout==b'',stderr
            finally:
                if child.poll() is None:
                    child.send_signal(signal.SIGTERM)
                    try:child.wait(timeout=2)
                    except subprocess.TimeoutExpired:os.killpg(child.pid,signal.SIGKILL);child.wait()
                for stream in [child.stdin,child.stdout,child.stderr]:
                    if stream is not None:stream.close()
            excluded=10 if len(times)>10 else 0
            listener_results[key]=dict(microseconds=distribution(times[excluded:]),warmup_events_excluded=excluded)
        # Separate traced run proves that per-record external spawning is real.
        traced=subprocess.run(execute(listener,['--max-runs',str(args.event_count),'--trace']),cwd=cwd,input=b'alpha\n'*args.event_count,capture_output=True,timeout=30)
        assert traced.returncode==0 and traced.stdout==b'ALPHA\n'*args.event_count,traced.stderr
        count=sum(json.loads(line).get('event')=='spawn' for line in traced.stderr.splitlines())
        assert count==args.event_count
        report['cases']['per_record_listener']=dict(status='measured architectural gap',events=args.event_count,results=listener_results,transform_launches=count,note='Live interpreter, same external tr per line; Bash uses builtin printf to feed tr, Execute sends framed bytes. Not comparable to one persistent tr. Requests acknowledged before stdin EOF; runtime block order shuffled')
        progress('per-record listener workload complete')
        report['manifest_sha256']={p.name:digest_file(p) for p in sorted(catalog.glob('*.json'))}
        report['cases']['producer_cancellation']=producer_cancellation(binary,catalog,bash,commands,cwd,args.bulk_samples,rng)
        progress('producer-cancellation workload complete')
        report['cases']['concurrent_100_commands']=dict(status='unsupported',reason='No .program parallel-command construct; launching 100 execute processes from Python would benchmark the harness, not language concurrency')
        report['cases']['reactive_10_sources']=dict(status='unsupported exact workload',reason='UI places at most two components; no ten-independent-source app declared or measured',available_evidence='tests/ui_pty.py verifies stale-worker cancellation and switching; existing UI refresh benchmark measures one mocked gh source')
        report['cases']['ai_100_scripts']=dict(status='pending',reason='Requires a held-out task corpus and actual model generations with token/repair accounting; generated fixture programs are not AI-generation evidence',acceptance=['first-pass syntax and execution correctness','prompt/context and output tokens','repair turns and tool calls','same tasks/model settings for Bash, Nushell and .program'])
    save()
    print(json.dumps({k:v.get('status') for k,v in report['cases'].items()},indent=2))


def producer_cancellation(binary,catalog,bash,commands,cwd,samples,rng):
    observations={key:dict(latencies=[],total=[],launches=[]) for key in ['execute','bash']}
    program=cwd/'tail.program';program.write_text('listen to tail with flags ["-n", "+1", "-f"] and path "events.txt"\nwhenever a line arrives as record\n  then tr with from "[:lower:]" and to "[:upper:]"\n')
    bash_path=cwd/'tail.bash'
    bash_path.write_text(f'''set -o pipefail
{shlex.quote(commands['mkfifo'])} source.pipe
{shlex.quote(commands['tail'])} -n +1 -f events.txt >source.pipe &
producer=$!
printf '%s' "$producer" >producer.pid
trap 'kill "$producer" 2>/dev/null || :; wait "$producer" 2>/dev/null || :' EXIT
count=0
while IFS= read -r record; do
  printf '%s\\n' "$record" | {shlex.quote(commands['tr'])} '[:lower:]' '[:upper:]'
  count=$((count+1))
  if [ "$count" -eq 3 ]; then break; fi
done <source.pipe
''')
    for _ in range(samples):
        order=['execute','bash'];rng.shuffle(order)
        for key in order:
            latencies=observations[key]['latencies'];total=observations[key]['total'];launches=observations[key]['launches']
            for path in ['source.pipe','producer.pid']:
                (cwd/path).unlink(missing_ok=True)
            (cwd/'events.txt').write_bytes(b'')
            command=[str(binary),str(program),'--manifest-dir',str(catalog),'--max-runs','3','--trace'] if key=='execute' else [bash,'--noprofile','--norc',str(bash_path)]
            stderr_path=cwd/'tail.stderr'
            with stderr_path.open('wb') as diagnostics:
                child=subprocess.Popen(command,cwd=cwd,env=child_environment(),stdout=subprocess.PIPE,stderr=diagnostics,start_new_session=True,bufsize=0)
                started=time.perf_counter_ns();pid=None
                try:
                    until=time.monotonic()+5
                    while pid is None:
                        if key=='execute':
                            for line in stderr_path.read_bytes().splitlines():
                                try:entry=json.loads(line)
                                except ValueError:continue
                                if entry.get('event')=='listener_spawn':pid=entry['pid']
                        elif (cwd/'producer.pid').exists():
                            text=(cwd/'producer.pid').read_text()
                            if text:pid=int(text)
                        if pid is not None:break
                        if time.monotonic()>until or child.poll() is not None:raise AssertionError('producer did not start')
                        time.sleep(.005)
                    with (cwd/'events.txt').open('ab') as file:file.write(b'one\ntwo\nthree\nfour\n');file.flush()
                    assert read_exact(child.stdout,len(b'ONE\nTWO\nTHREE\n'))==b'ONE\nTWO\nTHREE\n'
                    cutoff=time.perf_counter_ns()
                    remaining,_=child.communicate(timeout=5)
                    latencies.append((time.perf_counter_ns()-cutoff)/1e6)
                    total.append((time.perf_counter_ns()-started)/1e6)
                    assert child.returncode==0 and remaining==b''
                    try:os.kill(pid,0)
                    except ProcessLookupError:pass
                    else:raise AssertionError(f'owned producer {pid} survived')
                    if key=='execute':
                        trace=[json.loads(line) for line in stderr_path.read_bytes().splitlines()]
                        transform_count=sum(v.get('event')=='spawn' for v in trace)
                        assert transform_count==3
                        launches.append(transform_count)
                finally:
                    if child.poll() is None:
                        child.send_signal(signal.SIGTERM)
                        try:child.wait(timeout=2)
                        except subprocess.TimeoutExpired:os.killpg(child.pid,signal.SIGKILL);child.wait()
                    # Cleanup signals only the owned live Popen session.
                    # Never send a signal to a remembered descendant PID.
                    child.stdout.close()
    results={}
    for key, observation in observations.items():
        latencies=observation['latencies'];total=observation['total'];launches=observation['launches']
        results[key]=dict(after_third_output_to_exit_ms=distribution(latencies),total_follow_ms=distribution(total),exact_three_outputs=True,owned_producer_reaped=True,transform_launches_per_sample=launches if launches else 3)
    return dict(status='measured with CLI limit; script-level cutoff pending',results=results,note='Both start one tail and new tr per line. Four source lines; exact first three transformed. Execute cutoff from --max-runs, Bash counter from script. Trace overhead included in Execute cancellation observations; tail watch implementation contributes to total latency; randomized paired trials, no warmup exclusion; low-sample distributions are descriptive')


if __name__=='__main__':
    main()
