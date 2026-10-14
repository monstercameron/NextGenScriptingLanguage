#!/usr/bin/env python3
"""Paired, sequential measurements of two execute builds using owned local fixtures."""
import argparse, datetime, hashlib, json, os, platform, random, select, shutil, signal, statistics, subprocess, tempfile, time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]

def digest(p):
    return hashlib.sha256(Path(p).read_bytes()).hexdigest()

def distribution(values):
    s=sorted(values)
    return {'samples':len(s),'p50':statistics.median(s),'p95':s[min(len(s)-1,int(.95*(len(s)-1)))],'p99':s[min(len(s)-1,int(.99*(len(s)-1)))],'raw':values}

def environment():
    env={k:v for k,v in os.environ.items() if k not in {'BASH_ENV','ENV','SHELLOPTS','BASHOPTS','CDPATH','GLOBIGNORE'} and not k.startswith(('BASH_FUNC_','PYTHON'))}
    env['LC_ALL']='C';return env

def run(command,cwd):
    start=time.perf_counter_ns()
    child=subprocess.Popen(command,cwd=cwd,env=environment(),stdin=subprocess.DEVNULL,stdout=subprocess.PIPE,stderr=subprocess.PIPE,start_new_session=True)
    try:
        out,err=child.communicate(timeout=45)
    except BaseException:
        if child.poll() is None:
            os.killpg(child.pid,signal.SIGKILL)
        child.wait();raise
    ms=(time.perf_counter_ns()-start)/1e6
    if child.returncode:raise RuntimeError((command,child.returncode,err.decode(errors='replace')))
    return ms,out,err

def exact_read(stream,n):
    out=b'';deadline=time.monotonic()+10
    while len(out)<n:
        wait=deadline-time.monotonic()
        if wait<=0 or not select.select([stream],[],[],wait)[0]:raise TimeoutError('listener acknowledgement')
        part=os.read(stream.fileno(),n-len(out))
        if not part:raise RuntimeError('early listener EOF')
        out+=part
    return out

def main():
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument('--baseline',required=True,type=Path);p.add_argument('--candidate',required=True,type=Path);p.add_argument('--output',required=True,type=Path)
    p.add_argument('--scale',type=int,default=1)
    args=p.parse_args()
    if args.scale<1:p.error('scale must be positive')
    binaries={k:getattr(args,k).resolve() for k in ['baseline','candidate']}
    report={'observed_at':datetime.datetime.now(datetime.timezone.utc).isoformat(),'platform':platform.platform(),'binaries':{k:{'path':str(v),'sha256':digest(v)} for k,v in binaries.items()},'harness_sha256':digest(__file__),'method':'Fresh process wall ms including launch/output/exit; deterministic shuffled pairs; warm filesystem; 10 warmups for startup, 2 elsewhere; listener ack us excludes first 10 records per block. All fixtures local. No concurrent benchmark phases, CPU affinity or cache flushing. Raw samples retained.','cases':{}}
    rng=random.Random(1495)
    def save():args.output.parent.mkdir(parents=True,exist_ok=True);args.output.write_text(json.dumps(report,indent=2)+'\n')
    with tempfile.TemporaryDirectory(prefix='execute-opt-') as tmp:
        cwd=Path(tmp);catalog=cwd/'manifests';shutil.copytree(ROOT/'manifests',catalog)
        (catalog/'true.json').write_text(json.dumps({'name':'true','binary':shutil.which('true'),'arguments':[]}))
        sources={
            'check_1000':('run true\n'*1000,b'',200*args.scale,'check'),
            'fresh_printf':('run printf with message "alpha"\n',b'alpha\n',200*args.scale,'run'),
            'launch_100':('run true\n'*100,b'',12*args.scale,'run'),
            'pipe_three':('run printf with message "alpha"\npipe to tr with from "[:lower:]" and to "[:upper:]"\npipe to cat\n',b'ALPHA\n',100*args.scale,'run'),
            'then_three':('run printf with message "alpha"\nthen tr with from "[:lower:]" and to "[:upper:]"\nthen cat\n',b'ALPHA\n',100*args.scale,'run'),
            'cache_hit':('run printf with message "alpha"\nthen tr with from "[:lower:]" and to "[:upper:]" and cache\n',b'ALPHA\n',50*args.scale,'run'),
            'cache_miss':('run printf with message "alpha"\nthen tr with from "[:lower:]" and to "[:upper:]" and cache\n',b'ALPHA\n',30*args.scale,'run'),
            'checkpoint_20':('run true\n'*20,b'',12*args.scale,'run'),
        }
        for name,(source,wanted,count,verb) in sources.items():
            path=cwd/(name+'.program');path.write_text(source);samples={key:[] for key in binaries};keys=list(binaries)
            for i in range(count+(10 if name.startswith(('fresh','check')) else 2)):
                rng.shuffle(keys)
                for key in keys:
                    command=[str(binaries[key]),verb,str(path),'--manifest-dir',str(catalog)]
                    if verb=='run':command+=['--command-timeout-ms','3000']
                    if name.startswith('cache_'):
                        cache=cwd/(key+'-cache')
                        if name=='cache_miss':shutil.rmtree(cache,ignore_errors=True)
                        command+=['--cache-dir',str(cache)]
                    if name.startswith('checkpoint'):
                        journal=cwd/(key+'-journal');shutil.rmtree(journal,ignore_errors=True);command+=['--checkpoint',str(journal)]
                    elapsed,out,err=run(command,cwd)
                    if name=='check_1000':assert out==b'',(out,err)
                    else:assert out==wanted,(name,key,out,err)
                    if i>=(10 if name.startswith(('fresh','check')) else 2):samples[key].append(elapsed)
            report['cases'][name]={'milliseconds':{k:distribution(v) for k,v in samples.items()}}
            save();print(name,{k:round(statistics.median(v),3) for k,v in samples.items()},flush=True)
        path=cwd/'listener.program';path.write_text('listen to stdin\nwhenever a line arrives as record\n  then tr with from "[:lower:]" and to "[:upper:]"\n')
        samples={k:[] for k in binaries}
        for block in range(3*args.scale):
            keys=list(binaries);rng.shuffle(keys)
            for key in keys:
                child=subprocess.Popen([str(binaries[key]),str(path),'--manifest-dir',str(catalog),'--max-runs','210','--command-timeout-ms','3000'],cwd=cwd,env=environment(),stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,bufsize=0,start_new_session=True)
                try:
                    for i in range(210):
                        start=time.perf_counter_ns();child.stdin.write(b'alpha\n');got=exact_read(child.stdout,6);elapsed=(time.perf_counter_ns()-start)/1000
                        assert got==b'ALPHA\n',got
                        if i>=10:samples[key].append(elapsed)
                    child.stdin.close();child.stdin=None;out,err=child.communicate(timeout=10);assert child.returncode==0 and not out,(out,err)
                finally:
                    if child.poll() is None:os.killpg(child.pid,signal.SIGKILL);child.wait()
                    for stream in [child.stdin,child.stdout,child.stderr]:
                        if stream is not None:stream.close()
        report['cases']['listener_record']={'microseconds':{k:distribution(v) for k,v in samples.items()},'warmups_excluded_per_block':10};save()
        print('listener_record',{k:round(statistics.median(v),3) for k,v in samples.items()},flush=True)
        report['manifest_sha256']={f.name:digest(f) for f in sorted(catalog.glob('*.json'))};save()

if __name__=='__main__':main()
