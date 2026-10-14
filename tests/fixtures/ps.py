#!/usr/bin/env python3
"""Deterministic ps fixture. Real jq performs parsing and ranking in UI tests."""
import json, os, pathlib, sys
root=pathlib.Path(os.environ['CPU_FIXTURE_DIR'])
assert sys.argv[1:]==['-A','-o','pid=','-o','pcpu=','-o','comm='],sys.argv
log=root/'ps-calls.jsonl'
with log.open('a') as f:f.write(json.dumps(sys.argv[1:])+'\n')
mode=os.environ.get('CPU_FIXTURE_MODE','normal')
if mode=='fail':print('fixture ps failure',file=sys.stderr);sys.exit(1)
if mode=='malformed':print('malformed process row');sys.exit(0)
if mode=='empty':sys.exit(0)
if mode=='large':
    for n in range(5000):print(f'{n+1} {n/10:.1f} /usr/bin/process-{n}')
    sys.exit(0)
# Includes >100%, decimals that expose lexical sorting, a comma decimal,
# equal CPU values, spaces, quotes, Unicode, and zero usage.
rows=[(22,'9.9','/Applications/Editor With Spaces.app/editor'),(11,'98.3','/usr/bin/server'),(55,'125.5','/bin/multicore'),(44,'0.0','/usr/bin/idle'),(99,'71,2','/bin/locale'),(66,'55.1','/tmp/quote"name'),(88,'9.9','/bin/tie'),(77,'19.7','/bin/東京'),(33,'9.1','/bin/nine'),(12,'7.4','/bin/seven'),(13,'3.1','/bin/three'),(14,'1.2','/bin/one')]
for pid,cpu,name in rows:print(f'  {pid} {cpu} {name}')
