#!/usr/bin/env python3
"""Identical conditional sources isolate validation table-copy costs between release builds."""
import argparse
from datetime import datetime,timezone
import hashlib,json,platform,random,subprocess,tempfile,time
from pathlib import Path
from direct_commands import summary
PROJECT=Path(__file__).resolve().parents[1]


def main():
 p=argparse.ArgumentParser(description=__doc__)
 p.add_argument('--baseline',type=Path,required=True);p.add_argument('--candidate',type=Path,required=True)
 p.add_argument('--samples',type=int,default=80);p.add_argument('--report',type=Path,required=True)
 a=p.parse_args();assert a.samples>=20
 binaries={name:getattr(a,name).resolve() for name in ['baseline','candidate']}
 prefix=''.join(f'accept flag_{index} as boolean with default false\n' for index in range(100))
 named=''.join(f'if flag_0 is true\n  run "printf" with arguments ["yes"] as result_{index}\notherwise\n  run "printf" with arguments ["no"] as result_{index}\n' for index in range(64))
 cases={'no_inputs_1000_readonly': 'if true is true\n  run "true"\n'*1000,
        '100_inputs_1000_readonly':prefix+'if flag_0 is true\n  run "true"\notherwise\n  run "false"\n'*1000,
        '100_inputs_64_named_results':prefix+named+'run "printf" with arguments [result_63]\n',
        '100_inputs_nested32':prefix+''.join('  '*n+'if flag_0 is true\n' for n in range(32))+'  '*32+'run "true"\n'}
 rng=random.Random(1014202612);results={}
 with tempfile.TemporaryDirectory(prefix='execute-conditional-validation-') as directory:
  root=Path(directory)
  for case,source in cases.items():
   file=root/(case+'.program');file.write_text(source);raw={name:[] for name in binaries}
   for trial in range(a.samples+5):
    order=list(binaries);rng.shuffle(order)
    for name in order:
     start=time.perf_counter_ns();run=subprocess.run([str(binaries[name]),'check',str(file),'--manifest-dir',str(PROJECT/'manifests')],cwd=root,input=b'',capture_output=True,timeout=10)
     elapsed=(time.perf_counter_ns()-start)/1000
     assert run.returncode==0 and not run.stdout and not run.stderr,(case,name,run)
     if trial>=5:raw[name].append(elapsed)
   results[case]={'source_sha256':hashlib.sha256(source.encode()).hexdigest(),'timings_us':{name:summary(values) for name,values in raw.items()}}
 report={'at_utc':datetime.now(timezone.utc).isoformat(),'platform':platform.platform(),'binaries':{name:{'path':str(binary),'sha256':hashlib.sha256(binary.read_bytes()).hexdigest()} for name,binary in binaries.items()},'harness_sha256':hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),'catalog_sha256':{file.name:hashlib.sha256(file.read_bytes()).hexdigest() for file in sorted((PROJECT/'manifests').glob('*.json'))},'method':'Same conditional source, argv and catalog for both archived release binaries, with exact static success/empty-output checks. No native applets execute. Five warmups and shuffled interleaved fresh CLI trials, warm filesystem, no concurrent builds/tests. This includes startup, parsing and validation; no native execution, cold-cache or whole-runtime gain claim. Named-result case exercises the existing copying fallback; readonly cases exercise scope table reuse.','results':results}
 a.report.write_text(json.dumps(report,indent=2)+'\n');print(json.dumps({case:{name:{k:v for k,v in values.items() if k!='raw_us'} for name,values in data['timings_us'].items()} for case,data in results.items()},indent=2))

if __name__=='__main__':main()
