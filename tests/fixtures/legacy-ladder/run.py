#!/usr/bin/env python3
"""Execute the numbered ladder in isolated fixtures, checking every outcome."""
import argparse
import hashlib
import json
import os
import signal
import select
import threading
from collections import Counter
import subprocess
import sys
import tempfile
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
PROJECT = HERE.parents[2]


def fixtures(cwd):
    texts = {'fruit.txt':'pear\napple\napple\nbanana\n', 'a.txt':'alpha\n', 'b.txt':'beta\n',
             'empty.txt':'', 'with space.txt':'space\n', '-dash.txt':'dash\n', '*.txt':'literal wildcard\n',
             'people.csv':'1,ada\n2,grace\n3,linus\n', 'events.txt':'pass\nfail\npass again\n',
             'sub/a.txt':'alpha sub\n', 'sub/b.txt':'beta sub\n', 'sub/filename.txt':'a.txt',
             'other/a.txt':'alpha sub\n'}
    for name, text in texts.items():
        path = cwd/name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)
    (cwd/'binary.bin').write_bytes(bytes(range(256))*256)
    (cwd/'large.txt').write_bytes(b'apple banana\n'*65536)


def invoke(binary, cwd, arguments, timeout=10, stdin=b''):
    started = time.perf_counter_ns()
    environment = dict(os.environ, LC_ALL='C')
    # New session is owned by this harness. On timeout, ask the interpreter to
    # cancel its applet groups before killing the isolated launcher session.
    process = subprocess.Popen([str(binary), *arguments], cwd=cwd, env=environment,
                               stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                               stderr=subprocess.PIPE, start_new_session=True)
    try:
        stdout, stderr = process.communicate(stdin, timeout=timeout)
    except BaseException as error:
        if process.poll() is None: process.send_signal(signal.SIGTERM)
        try:
            process.communicate(timeout=2)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGKILL)
            process.communicate()
        if isinstance(error, subprocess.TimeoutExpired):
            raise AssertionError(f'process exceeded {timeout}s: {arguments}') from error
        raise
    return {'code':process.returncode, 'stdout':stdout, 'stderr':stderr,
            'elapsed_ms':(time.perf_counter_ns()-started)/1e6}


def interactive(binary, cwd, arguments, case):
    started=time.perf_counter_ns()
    process=subprocess.Popen([str(binary),*arguments], cwd=cwd,
        env=dict(os.environ,LC_ALL='C'), stdin=subprocess.PIPE, stdout=subprocess.PIPE,
        stderr=subprocess.PIPE, start_new_session=True, bufsize=0)
    diagnostics=[]
    def drain():
        retained=0
        while True:
            chunk=process.stderr.read(16384)
            if not chunk: break
            # Continue draining even if diagnostic retention reaches its bound.
            if retained < 4*1024*1024:
                diagnostics.append(chunk); retained+=len(chunk)
    reader=threading.Thread(target=drain,daemon=True); reader.start()
    output=bytearray()
    try:
        requests=case['stdin'].encode().splitlines(keepends=True)
        responses=case['expected'].encode().splitlines(keepends=True)
        assert len(requests)==len(responses)
        for request,expected in zip(requests,responses):
            if case['mode']=='tail':
                with (cwd/'events.txt').open('ab') as target:
                    target.write(request); target.flush()
            else: process.stdin.write(request)
            deadline=time.monotonic()+5
            response=bytearray()
            while len(response)<len(expected):
                remaining=deadline-time.monotonic()
                assert remaining>0 and select.select([process.stdout],[],[],max(0,remaining))[0], 'no response while producer stays alive'
                chunk=os.read(process.stdout.fileno(),len(expected)-len(response))
                assert chunk, 'listener exited before its response'
                response.extend(chunk)
            assert bytes(response)==expected, f'wrong interactive response: {response!r}'
            output.extend(response)
        process.stdin.close(); process.stdin=None
        process.wait(timeout=10)
        output.extend(process.stdout.read())
        reader.join(timeout=2)
        assert not reader.is_alive(), 'listener retained its diagnostic pipe'
        return {'code':process.returncode,'stdout':bytes(output),'stderr':b''.join(diagnostics),
                'elapsed_ms':(time.perf_counter_ns()-started)/1e6,
                'records_before_eof':len(requests)}
    except BaseException:
        if process.poll() is None: process.send_signal(signal.SIGTERM)
        try: process.wait(timeout=2)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid,signal.SIGKILL); process.wait()
        raise
    finally:
        if process.stdin is not None: process.stdin.close()
        process.stdout.close()
        reader.join(timeout=2)
        if not reader.is_alive(): process.stderr.close()


def events(result):
    records=[]
    for line in result['stderr'].splitlines():
        if line.startswith(b'{'):
            try:
                value=json.loads(line)
                if isinstance(value,dict) and 'event' in value: records.append(value)
            except ValueError:
                pass
    return records


def verify(result, cwd, case, phase='first'):
    failed = case.get('failure', False) or (case.get('mode')=='recover' and phase=='first')
    if phase=='after': failed=False
    assert (result['code'] != 0) == failed, result['stderr'].decode(errors='replace')
    key='after_expected' if phase=='after' else 'expected'
    expected=case.get(key)
    actual=result['stdout'].strip() if case.get('strip') else result['stdout']
    if expected is not None:
        assert actual==expected.encode(), f'expected {expected!r}, got {actual[:500]!r}'
    if phase=='first' and 'cwd_output' in case:
        assert actual.decode().strip()==str((cwd/case['cwd_output']).resolve())
    if phase=='first' and 'error' in case:
        assert case['error'].encode() in result['stderr'], result['stderr'].decode(errors='replace')
    for name in case.get('absent',[]):
        assert not (cwd/name).exists(), f'unexpected effect: {name}'
    for name, text in case.get('after_files' if phase=='after' else 'files',{}).items():
        assert (cwd/name).read_bytes()==text.encode(), f'wrong saved result: {name}'


def summary(result):
    trace=events(result)
    retained=trace if len(trace)<=100 else trace[:50]+trace[-50:]
    value={'exit_code':result['code'], 'elapsed_ms':round(result['elapsed_ms'],3),
            'stdout_bytes':len(result['stdout']), 'stdout_sha256':hashlib.sha256(result['stdout']).hexdigest(),
            'trace_event_counts':dict(Counter(event['event'] for event in trace)),
            'trace_events_truncated':len(trace)>100,
            'events':[{key:value for key,value in event.items()
                       if key in ('event','applet','restored_nodes','completed_nodes','next_step')}
                      for event in retained]}
    if 'records_before_eof' in result: value['records_before_eof']=result['records_before_eof']
    return value


def run_case(binary, case):
    with tempfile.TemporaryDirectory(prefix=f'execute-ladder-{case["number"]:02}-') as tmp:
        cwd=Path(tmp); fixtures(cwd)
        for name,text in case.get('before_files',{}).items(): (cwd/name).write_text(text)
        script=cwd/case['file']; script.write_bytes((HERE/case['file']).read_bytes())
        manifest=['--manifest-dir',str(PROJECT/'manifests')]
        checked=invoke(binary,cwd,['check',str(script),*manifest])
        assert (checked['code'] != 0)==case.get('invalid',False), checked['stderr'].decode(errors='replace')
        if not case.get('invalid'):
            explained=invoke(binary,cwd,['explain',str(script),*manifest])
            assert explained['code']==0, explained['stderr'].decode(errors='replace')
        args=[str(script),*manifest,*case.get('args',[]),'--trace']
        mode=case.get('mode')
        if mode in ('cache','recover'): args+=['--cache-dir','cache']
        if mode=='recover': args+=['--checkpoint','state']
        first=interactive(binary,cwd,args,case) if mode in ('interactive','tail') else invoke(binary,cwd,args,case.get('timeout',10),case.get('stdin','').encode())
        verify(first,cwd,case)
        phases={'first':summary(first)}
        if mode in ('cache','recover'):
            for name,text in case.get('mutate',{}).items(): (cwd/name).write_text(text)
            if 'repair' in case: script.write_text(case['repair']+'\n')
            after_args=['resume',str(script),*manifest,'--checkpoint','state','--cache-dir','cache','--trace',*case.get('after_args',[])] if mode=='recover' else args
            after=invoke(binary,cwd,after_args)
            verify(after,cwd,case,'after'); phases['after']=summary(after)
            traces=events(after)
            if 'after_event' in case:
                assert any(e['event']==case['after_event'] for e in traces), f'missing {case["after_event"]}'
                if case['after_event']=='cache_store': assert not any(e['event']=='cache_hit' for e in traces)
            if 'after_spawns' in case:
                assert [e['applet'] for e in traces if e['event']=='spawn']==case['after_spawns']
            if 'restored' in case:
                assert next(e for e in traces if e['event']=='checkpoint_resume')['restored_nodes']==case['restored']
            if case.get('no_restarted_producer'):
                assert not any(e['event']=='listener_spawn' for e in traces)
            if case.get('gc'):
                journal=json.loads((cwd/'state/journal.json').read_text())['data']
                snapshot=journal['snapshot']; retained={snapshot['output']['sha256']}
                retained.update(value['sha256'] for value in snapshot['bindings'].values())
                actual={p.name for p in (cwd/'state/blobs').iterdir() if len(p.name)==64}
                assert actual==retained, 'completed service left old event blobs'
            if mode=='recover' and not case.get('no_restarted_producer'):
                repeated=invoke(binary,cwd,after_args)
                verify(repeated,cwd,case,'after')
                assert not any(e['event']=='spawn' for e in events(repeated)), 'completed effects replayed'
                phases['completed_resume']=summary(repeated)
        return phases


def main():
    def cancel(signum, frame):
        raise KeyboardInterrupt(f'ladder cancelled by signal {signum}')
    signal.signal(signal.SIGTERM, cancel)
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary',type=Path,default=PROJECT/'bin/execute')
    parser.add_argument('--case',type=int,action='append',help='Run only these numbered cases; repeatable')
    parser.add_argument('--report',type=Path,help='Write verification evidence as JSON')
    args=parser.parse_args(); binary=args.binary.resolve()
    catalog=json.loads((HERE/'catalog.json').read_text())['cases']
    assert [case['number'] for case in catalog]==list(range(42,51)), 'catalog must contain exactly the nine retained negative/recovery cases'
    assert {case['file'] for case in catalog}=={path.name for path in HERE.glob('*.program')}
    if args.case and any(number not in range(42,51) for number in args.case): parser.error('--case must be between 42 and 50')
    results=[]
    for case in catalog:
        if args.case and case['number'] not in args.case: continue
        item={'number':case['number'],'file':case['file'],'title':case['title']}
        try:
            item['phases']=run_case(binary,case); item['passed']=True
            print(f'{case["number"]:02} PASS {case["title"]}',flush=True)
        except Exception as error:
            item['passed']=False; item['error']=str(error)
            print(f'{case["number"]:02} FAIL {case["title"]}: {error}',flush=True)
        results.append(item)
    report={'execute_version':subprocess.check_output([binary,'--version']).decode().strip(),
            'binary_sha256':hashlib.sha256(binary.read_bytes()).hexdigest(),
            'platform':sys.platform,'cases_run':len(results),'passed':sum(item['passed'] for item in results),
            'method':'fresh temp directory per case; bounded processes; exact output, files, exit codes and traces checked; cron fires at a real minute boundary; durations are single observations, not latency benchmarks',
            'results':results}
    if args.report: args.report.write_text(json.dumps(report,indent=2)+'\n')
    print(f'{report["passed"]}/{len(results)} passed',flush=True)
    return 0 if all(item['passed'] for item in results) else 1

if __name__=='__main__': sys.exit(main())
