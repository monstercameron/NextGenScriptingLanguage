#!/usr/bin/env python3
"""Controlled, read-only gh replacement. Used only under an owned temp PATH."""
import json, os, pathlib, sys, time
root=pathlib.Path(os.environ['GH_FIXTURE_DIR'])
args=sys.argv[1:]
assert len(args)==6 and args[:5]==['repo','view','--json','nameWithOwner,stargazerCount,forkCount,description,url,updatedAt','--'], args
log=root/'calls.jsonl'
with log.open('a') as f: f.write(json.dumps(args)+'\n')
n=len(log.read_text().splitlines())
mode=os.environ.get('GH_FIXTURE_MODE','normal')
repo=args[5]
if repo=='slow/repo' or mode=='slow':
    (root/'worker.pid').write_text(str(os.getpid()))
    time.sleep(100)
if repo=='fail/repo' or mode=='fail':
    print('fixture permission denied',file=sys.stderr); sys.exit(1)
if mode=='noisy':
    sys.stderr.write('x'*1048576); sys.stderr.flush(); sys.exit(1)
if mode=='invalid': print('not JSON'); sys.exit(0)
data=dict(nameWithOwner=repo,stargazerCount=100+n,forkCount=9,description='Fixture description',url='https://github.com/'+repo,updatedAt='2026-10-13T00:00:00Z')
if mode=='missing': del data['stargazerCount']
if mode=='negative': data['stargazerCount']=-1
print(json.dumps(data))
