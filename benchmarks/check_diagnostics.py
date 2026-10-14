#!/usr/bin/env python3
"""Static check latency and structured diagnostic equivalence through the real CLI."""
import argparse
from datetime import datetime,timezone
import hashlib
import json
from pathlib import Path
import platform
import random
import subprocess
import tempfile
import time
from direct_commands import summary

PROJECT=Path(__file__).resolve().parents[1]


def main():
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument('--baseline',type=Path,required=True);p.add_argument('--candidate',type=Path,required=True)
    p.add_argument('--samples',type=int,default=100);p.add_argument('--report',type=Path,required=True)
    a=p.parse_args();assert a.samples>=20
    binaries={'baseline':a.baseline.resolve(),'candidate':a.candidate.resolve()}
    rng,results=random.Random(9302026),{}
    with tempfile.TemporaryDirectory(prefix='execute-diagnostics-bench-') as directory:
        root=Path(directory)
        sources={'valid':'accept path as text\nrun "execute-check-nonexistent-tool" with arguments [path]\nthen save to "forbidden"\n',
                 'parse_error':'read "unterminated\n','missing_file':None}
        for case,source in sources.items():
            file=root/(case+'.program')
            if source is not None:file.write_text(source)
            commands={name:[str(binary),'check',file.name,'--manifest-dir',str(PROJECT/'manifests')] for name,binary in binaries.items()}
            commands['candidate_json']=[*commands['candidate'],'--json']
            outputs={};raw={name:[] for name in commands}
            for index in range(a.samples+5):
                order=list(commands);rng.shuffle(order)
                for name in order:
                    started=time.perf_counter_ns()
                    run=subprocess.run(commands[name],cwd=root,input=b'',capture_output=True,timeout=5)
                    elapsed=(time.perf_counter_ns()-started)/1000
                    outputs[name]=run
                    assert run.returncode==(0 if case=='valid' else 1),(case,name,run.stderr)
                    if index>=5:raw[name].append(elapsed)
            assert outputs['baseline'].stdout==outputs['candidate'].stdout
            assert outputs['baseline'].stderr==outputs['candidate'].stderr
            structured=json.loads(outputs['candidate_json'].stdout)
            assert outputs['candidate_json'].stderr==b''
            assert structured['protocol']=='execute.check/1' and structured['ok']==(case=='valid')
            if case!='valid':
                assert outputs['baseline'].stderr==('execute: '+structured['diagnostics'][0]['message']+'\n').encode()
            else:assert structured['diagnostics']==[]
            assert not (root/'forbidden').exists()
            results[case]={name:summary(values) for name,values in raw.items()}
    report={'at_utc':datetime.now(timezone.utc).isoformat(),'platform':platform.platform(),
            'binaries':{name:hashlib.sha256(binary.read_bytes()).hexdigest() for name,binary in binaries.items()},
            'harness_sha256':hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
            'method':'Identical source/argv for baseline and candidate ordinary static check. Additional candidate_json format measured separately; it is not output-identical. Five warmups and shuffled interleaved trials; wall time includes CLI startup and capture. Status/error equivalence, JSON framing and absent side effects checked outside timing. Warm caches, no concurrent builds/tests. No native tool probing/execution, cold-cache or whole-runtime gain claim.',
            'results':results}
    a.report.write_text(json.dumps(report,indent=2)+'\n')
    print(json.dumps({case:{name:{k:v for k,v in data.items() if k!='raw_us'} for name,data in variants.items()} for case,variants in results.items()},indent=2))

if __name__=='__main__':main()
