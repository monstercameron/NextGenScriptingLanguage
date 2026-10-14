#!/usr/bin/env python3
"""Paired live-widget update-to-render latency with increasing unrelated user state."""
import argparse
from datetime import datetime,timezone
import hashlib
import json
from pathlib import Path
import platform
import random
import time
from direct_commands import summary
from live_state_scaling import live

PROJECT=Path(__file__).resolve().parents[1]
SOURCE='open a screen titled "State scaling"\nplace panel on the left\ndefine a program named panel\n  show a text field named message with value "initial"\n'


def trial(binary,padding,edits=8):
    live.BINARY=binary
    with live.Session(SOURCE,headless=True) as app:
        app.until(lambda s:'initial' in s['runtime']['frame'])
        for index,start in enumerate(range(0,padding,900000)):
            app.ok('create',path=f'/user/pad{index}',value='x'*min(900000,padding-start))
        ack_total,applied_total=0,0
        for index in range(edits):
            text=f'edit-{index:04}'
            started=time.perf_counter_ns()
            ack=app.ok('update',path='/ui/programs/panel/widgets/message/text',value=text)
            ack_total+=(time.perf_counter_ns()-started)/1000
            revision=ack['revision'];deadline=time.monotonic()+5
            while True:
                result=app.ok('read',path='/runtime/ui_applied_revision')
                if result['result']>=ack['revision']:break
                app.ok('wait',path='/runtime/ui_applied_revision',after=result['revision'],timeout_ms=100)
                assert time.monotonic()<deadline,'UI update not applied'
            applied_total+=(time.perf_counter_ns()-started)/1000
            assert text in app.ok('read',path='/runtime/frame')['result']
        user=app.ok('read',path='/user')['result']
        assert sum(map(len,user.values()))==padding and all(set(value)=={'x'} for value in user.values())
        assert app.ok('read',path='/runtime/ui_errors')['result']=={}
        return ack_total,applied_total


def main():
    p=argparse.ArgumentParser(description=__doc__);p.add_argument('--baseline',type=Path,required=True);p.add_argument('--candidate',type=Path,required=True);p.add_argument('--samples',type=int,default=20);p.add_argument('--report',type=Path,required=True);a=p.parse_args();assert a.samples>=5
    binaries={'baseline':a.baseline.resolve(),'candidate':a.candidate.resolve()};rng=random.Random(9412026);results={}
    for padding in (0,1024*1024,6*1024*1024):
        raw={name:{'ack':[],'applied':[]} for name in binaries}
        for index in range(a.samples+2):
            order=list(binaries);rng.shuffle(order)
            for name in order:
                ack,applied=trial(binaries[name],padding)
                if index>=2:raw[name]['ack'].append(ack);raw[name]['applied'].append(applied)
        results[str(padding)]={name:{metric:summary(values) for metric,values in metrics.items()} for name,metrics in raw.items()}
    report={'at_utc':datetime.now(timezone.utc).isoformat(),'platform':platform.platform(),'binaries':{name:hashlib.sha256(binary.read_bytes()).hexdigest() for name,binary in binaries.items()},'harness_sha256':hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),'helper_sha256':hashlib.sha256((PROJECT/'tests/live_control.py').read_bytes()).hexdigest(),
            'method':'Fresh paused headless real renderers. Startup and 0/1/6 MiB user padding excluded. Each trial sums eight individual edit-to-ack and edit-to-applied segments; eight actual frame checks and retained padding/error checks outside timing. Applied observation uses projected RPC/long-poll and includes Python/JSON/socket costs. Two warmups, shuffled paired order, warm caches, no concurrent builds/tests. Not event-to-photon, cold startup, RSS or native Linux performance.',
            'edits_per_trial':8,'results':results}
    a.report.write_text(json.dumps(report,indent=2)+'\n');print(json.dumps({case:{name:{metric:{k:v for k,v in data.items() if k!='raw_us'} for metric,data in metrics.items()} for name,metrics in variants.items()} for case,variants in results.items()},indent=2))

if __name__=='__main__':main()
