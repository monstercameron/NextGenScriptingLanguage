#!/usr/bin/env python3
"""Functional checkpoint symlink probes; every target belongs to this fixture."""
import argparse
from datetime import datetime,timezone
import hashlib
import json
from pathlib import Path
import stat
import subprocess
import tempfile


def probe(binary, case):
    with tempfile.TemporaryDirectory(prefix='execute-checkpoint-paths-') as directory:
        root=Path(directory)
        script=root/'probe.program';script.write_text('run "printf" with arguments ["owned fixture"]\n')
        target=root/'owned-target';target.mkdir(mode=0o755)
        target.chmod(0o755)
        (target/'sentinel').write_text('unchanged')
        checkpoint=root/'checkpoint'
        if case in ('directory', 'directory_trailing_slash'):
            checkpoint.symlink_to(target, target_is_directory=True)
        else:
            checkpoint.mkdir()
            (checkpoint/'blobs').symlink_to(target,target_is_directory=True)
        path=str(checkpoint)+('/' if case=='directory_trailing_slash' else '')
        result=subprocess.run([str(binary),str(script),'--checkpoint',path],cwd=root,input=b'',capture_output=True,timeout=5)
        return {'case':case,'returncode':result.returncode,'stdout':result.stdout.decode(),'stderr':result.stderr.decode(),
                'target_mode':oct(stat.S_IMODE(target.stat().st_mode)),
                'target_entries':sorted(p.name for p in target.iterdir()),'sentinel':(target/'sentinel').read_text()}


def main():
    p=argparse.ArgumentParser(description=__doc__);p.add_argument('--binary',type=Path,required=True);p.add_argument('--report',type=Path,required=True);a=p.parse_args();binary=a.binary.resolve()
    report={'at_utc':datetime.now(timezone.utc).isoformat(),'binary_sha256':hashlib.sha256(binary.read_bytes()).hexdigest(),
            'harness_sha256':hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
            'method':'Three real CLI runs in separate owned temporary workspaces. Symlink targets, sentinel files and permission modes all belong to fixture. No privileged action, unrelated path or process. Functional probe, not timing benchmark.',
            'results':[probe(binary,c) for c in ('directory','directory_trailing_slash','blobs')]}
    a.report.write_text(json.dumps(report,indent=2)+'\n');print(json.dumps(report['results'],indent=2))

if __name__=='__main__':main()
