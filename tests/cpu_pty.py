#!/usr/bin/env python3
"""Run the actual CPU .program in a PTY with deterministic ps and real jq."""
import importlib.util, json, os, pathlib, sys, time
project=pathlib.Path(__file__).resolve().parents[1]
spec=importlib.util.spec_from_file_location('ui_pty',project/'tests/ui_pty.py');m=importlib.util.module_from_spec(spec);spec.loader.exec_module(m)
source=(project/'examples/top-cpu.program').read_text()
# Inherit only the scoped fixture directory; the test runs in its own process.
with m.tempfile.TemporaryDirectory(prefix='execute-cpu-pty-') as directory:
    root=pathlib.Path(directory);ps=root/'ps';ps.write_text((project/'tests/fixtures/ps.py').read_text());ps.chmod(0o755)
    os.environ['CPU_FIXTURE_DIR']=directory;os.environ['CPU_FIXTURE_MODE']='normal'
    s=m.Session(directory,source_text=source)
    def calls():
        p=root/'ps-calls.jsonl';return len(p.read_text().splitlines()) if p.exists() else 0
    try:
        s.wait(lambda:calls()>=1 and b'Live.' in s.data)
        assert '█'.encode() in s.data and b'125.5%' in s.data
        s.send(b'\x12');s.wait(lambda:calls()>=2)
        # The real program's two-second event must cause the next query.
        s.wait(lambda:calls()>=3,seconds=5)
        s.finish()
        print('CPU PTY passed: real jq, bars, manual refresh, two-second timer, script quit, terminal restoration')
    finally:s.close()
