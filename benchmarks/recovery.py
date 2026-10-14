#!/usr/bin/env python3
"""Measure opt-in durable checkpoints against ordinary execution in temp dirs."""
import argparse, hashlib, json, math, os, platform, random, select, shutil, statistics, subprocess, tempfile, time
from pathlib import Path

def stats(values):
    ordered = sorted(values)
    return {'samples':len(values), 'p50':statistics.median(values),
            'p95':ordered[math.ceil(len(values)*.95)-1], 'p99':ordered[math.ceil(len(values)*.99)-1]}

def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary',type=Path,default=Path(__file__).resolve().parents[1]/'bin/execute')
    parser.add_argument('--samples',type=int,default=200)
    parser.add_argument('--output',type=Path)
    args=parser.parse_args()
    if args.samples < 10: parser.error('at least 10 samples required')
    binary=args.binary.resolve(); project=Path(__file__).resolve().parents[1]
    report={'environment':{'os':platform.system(),'release':platform.release(),'architecture':platform.machine(),
            'version':subprocess.check_output([binary,'--version']).decode().strip(),
            'binary_sha256':hashlib.sha256(binary.read_bytes()).hexdigest()},
            'method':{'samples_per_case':args.samples,'warmups':10,'timer':'perf_counter_ns',
            'cli':'new process, warmed filesystem caches; fixture/directory preparation excluded',
            'listener':'one process, sequential stdin request to complete handler output; startup excluded',
            'durability':'local filesystem fsync + rename + directory sync; not a power-loss simulation',
            'ordering':'shuffled CLI cases; separate sequential listener batches',
            'safety':'temporary files only; no external API or Git operations'},'cli_ms':{},'listener_us':{}}
    with tempfile.TemporaryDirectory(prefix='execute-recovery-bench-') as tmp:
        cwd=Path(tmp); fixture=b'checkpoint fixture\n'; (cwd/'input.txt').write_bytes(fixture)
        scripts={'read.program':'read "input.txt"\n',
                 'pipeline.program':'run printf with message "checkpoint fixture"\nthen tr with from "[:lower:]" and to "[:upper:]"\nthen save to file "output.txt"\n',
                 'listener.program':'listen to stdin\nwhenever a line arrives as record\n  read "input.txt"\n'}
        report['scripts']=scripts
        for name, source in scripts.items(): (cwd/name).write_text(source)
        def command(script,extra=()): return [str(binary),script,'--manifest-dir',str(project/'manifests'),*extra]
        stored=cwd/'stored'
        subprocess.run(command('read.program',('--checkpoint',str(stored))),cwd=cwd,capture_output=True,check=True,timeout=10)
        cases={'normal_read':('read.program',(),fixture), 'checkpoint_read':('read.program',('--checkpoint',str(cwd/'fresh')),fixture),
               'completed_resume':('read.program',('--checkpoint',str(stored)),fixture),
               'normal_pipeline_save':('pipeline.program',(),fixture.upper()),
               'checkpoint_pipeline_save':('pipeline.program',('--checkpoint',str(cwd/'fresh')),fixture.upper())}
        samples={key:[] for key in cases}; rng=random.Random(20261013)
        for iteration in range(args.samples+10):
            order=list(cases); rng.shuffle(order)
            for name in order:
                if (cwd/'fresh').exists(): shutil.rmtree(cwd/'fresh')
                script,extra,expected=cases[name]; cmd=command(script,extra)
                if name=='completed_resume': cmd.insert(1,'resume')
                started=time.perf_counter_ns(); result=subprocess.run(cmd,cwd=cwd,capture_output=True,timeout=10)
                elapsed=(time.perf_counter_ns()-started)/1e6
                if result.returncode or result.stdout!=expected: raise RuntimeError((name,result.stderr,result.stdout))
                if iteration>=10: samples[name].append(elapsed)
        report['cli_ms']={name:stats(values) for name,values in samples.items()}
        for durable in (False,True):
            extra=('--checkpoint',str(cwd/'listener-state')) if durable else ()
            process=subprocess.Popen(command('listener.program',extra),cwd=cwd,stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,bufsize=0)
            values=[]
            try:
                for iteration in range(args.samples+10):
                    started=time.perf_counter_ns(); process.stdin.write(b'tick\n')
                    received=b''
                    while len(received)<len(fixture):
                        if not select.select([process.stdout],[],[],10)[0]: raise TimeoutError('listener failed to respond')
                        chunk=os.read(process.stdout.fileno(),len(fixture)-len(received))
                        if not chunk: raise RuntimeError(process.stderr.read().decode())
                        received+=chunk
                    elapsed=(time.perf_counter_ns()-started)/1000
                    if received!=fixture: raise RuntimeError('wrong listener result')
                    if iteration>=10: values.append(elapsed)
                process.stdin.close(); process.wait(timeout=10)
                if process.returncode: raise RuntimeError(process.stderr.read().decode())
            finally:
                if process.poll() is None: process.kill(); process.wait()
                process.stdout.close(); process.stderr.close()
            report['listener_us']['checkpoint_read_handler' if durable else 'normal_read_handler']=stats(values)
    encoded=json.dumps(report,indent=2)+'\n'
    if args.output: args.output.write_text(encoded)
    print(encoded)

if __name__=='__main__': main()
