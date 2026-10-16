#!/usr/bin/env python3
"""Independent standard-library audit for the frozen text JSON decoder trial."""
from __future__ import annotations
import hashlib, json, os, statistics, subprocess, sys
from datetime import datetime
from pathlib import Path

C=Path('/Users/earlcameron/Documents/Codex/2026-10-15/users-earlcameron-documents-codex-2026-10/work/execute-thesis-2026-10-15')
E=C/'evidence/text-json-decoder'
B=C/'work/text-json-decoder-baseline-3be9fe'
K=C/'work/text-json-decoder-candidate-3be9fe'
R=E/'paired-run-attempt-1/paired-report.json'
P=E/'preregistration-attempt-1.json'
SB=E/'source-fingerprints.json'
BUILD=E/'build-attempt-1'
BASE='3be9fe7eac71c371d3ec5c5a5fadb3161a83ea6c'
PATCH='95f4d413f86f0905421500f182e372e55a3c30163dc3364810ca71386ca03f36'
HARNESS='2293bc1e04377fdf23b430655ec51bf41643723f9133c3cf181136edbc6c146d'
HPATCH='8b1b7ed84822cf364c84febf5c0f898c4d4b4bf3cce9f13c03cc6b416e6aa920'
DRIVER='2c25beda9297988a24a86af1dc411c588c29e9c2f0a3c55f8996d22db9c44c06'
RUNNER='6dd826353dff66d318ae57b9cddaf7e08eeb529210f82f6f421af47992e893eb'
MAP='3e414d06b9f1fa78f0dc70df25b958d439329636450fd67141d36bd7a140c57a'
EMPTY='e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855'
CASE='text_json_decode_10000'
METHOD=('Public flow::contracts::OutputSchema::decode API; 10,000 decodes per sample of one fixed 4,096-byte ASCII JSON string. '
        'Each returned Value::String is checked byte-for-byte and dropped before the next decode. Includes JSON parsing, decoded string ownership, byte-equality verification, and dropping each returned value within its iteration. '
        'Excludes schema/fixture construction, payload serialization, hashing, and report writing. No filesystem reads or child processes.')

def need(x,msg):
    if not x: raise AssertionError(msg)
def sha(b): return hashlib.sha256(b).hexdigest()
def shafile(p): return sha(Path(p).read_bytes())
def unique(pairs):
    out={}
    for k,v in pairs:
        need(k not in out,'duplicate JSON key '+repr(k)); out[k]=v
    return out
def load(p): return json.loads(Path(p).read_text(),object_pairs_hook=unique,parse_constant=lambda x: (_ for _ in ()).throw(ValueError(x)))
def canon(v): return json.dumps(v,sort_keys=True,separators=(',',':'),ensure_ascii=False,allow_nan=False).encode()
def git(root,*args): return subprocess.run(['git',*args],cwd=root,check=True,stdout=subprocess.PIPE,stderr=subprocess.PIPE).stdout
def source_tree(root):
    tracked=git(root,'ls-files','-z').split(b'\0')
    other=git(root,'ls-files','--others','--exclude-standard','-z').split(b'\0')
    paths=sorted({os.fsdecode(x) for x in tracked+other if x})
    files={}
    for rel in paths:
        p=root/rel
        data=b'symlink\0'+os.fsencode(os.readlink(p)) if p.is_symlink() else p.read_bytes()
        files[rel]=sha(data)
    return {'sha256':sha(canon(files)),'file_count':len(files),'files':files}
def iso(s): return datetime.fromisoformat(s.replace('Z','+00:00'))

binding=load(SB); prereg=load(P); report=load(R)
need(shafile(SB)==MAP,'source-map pin mismatch')
need(shafile(E/'text_json_decoder_product.patch')==PATCH,'product patch hash mismatch')
need(shafile(E/'output_schema_text_json_decode_harness.patch')==HPATCH,'harness patch hash mismatch')
need(shafile(C/'work/build_text_json_decoder_pair.py')==DRIVER,'build-driver hash mismatch')
runner=C/'work/text-json-decoder-paired-runner/text_json_decoder_pair.py'
need(shafile(runner)==RUNNER,'runner hash mismatch')
need(binding['base_commit']==BASE and binding['product_patch_sha256']==PATCH and binding['harness_source_sha256']==HARNESS and binding['harness_patch_sha256']==HPATCH,'source binding constants mismatch')
need(sha(P.read_bytes())==report['preregistration_sha256'],'preregistration report pin mismatch')
need(prereg['schema']=='output-schema-text-json-decoder-paired-preregistration/1','prereg schema')
need(prereg['runner']['sha256']==RUNNER and Path(prereg['runner']['path'])==runner.resolve(),'prereg runner pin')
need(prereg['provenance']['source_bindings_sha256']==MAP and prereg['provenance']['build_driver_sha256']==DRIVER,'prereg build/source pins')
need(prereg['protocol']['warmup_pairs']==3 and prereg['protocol']['measured_pairs']==48 and prereg['protocol']['retry_policy'].startswith('none'),'prereg schedule')
need(iso(prereg['prepared_at']) < iso(report['created_at']) < iso(report['completed_at']),'prereg/run timestamps')
need(report['schema']=='output-schema-text-json-decoder-paired-report/1' and report['status']=='complete','report status/schema')
need(report['attempt_count']==102 and report['successful_attempt_count']==102 and report['failed_attempt_count']==0 and len(report['attempts'])==102,'attempt totals')
need(report['post_run_identity']=={'arms':report['post_run_identity']['arms'],'preregistration_stable':True,'provenance_stable':True,'runner_stable':True,'source_bindings_sha256':MAP},'post-run identity flags')

# Independently enumerate and hash all 5,917 source files in each arm.
need(binding['schema']=='output-schema-text-json-decoder-source-fingerprints/1','source binding schema')
need(binding['arms']['baseline']['head']==BASE and binding['arms']['candidate']['head']==BASE,'source HEADs')
actual={}
for side,root in [('baseline',B),('candidate',K)]:
    expected=binding['arms'][side]['tree']
    tree=source_tree(root)
    need(tree==expected,side+' full source map differs')
    need(git(root,'rev-parse','HEAD').decode().strip()==BASE,side+' HEAD drift')
    diff=sha(git(root,'diff','--binary'))
    staged=sha(git(root,'diff','--binary','--cached'))
    expected_diff=EMPTY if side=='baseline' else PATCH
    need(diff==expected_diff and staged==EMPTY,side+' tracked/staged diff mismatch')
    status=sorted(line[3:] for line in git(root,'status','--porcelain=v1','--untracked-files=all').decode().splitlines())
    expected_status=['tests/output_schema_text_json_decode.rs'] if side=='baseline' else ['src/contracts.rs','tests/output_schema_text_json_decode.rs']
    need(status==expected_status,side+' working tree status mismatch')
    need(tree['file_count']==5917,side+' file count')
    actual[side]=tree
changed=sorted(p for p in actual['baseline']['files'] if actual['baseline']['files'][p]!=actual['candidate']['files'][p])
need(changed==['src/contracts.rs'],'unexpected source changes '+repr(changed))
need(shafile(B/'src/contracts.rs')==actual['baseline']['files']['src/contracts.rs'] and shafile(K/'src/contracts.rs')==actual['candidate']['files']['src/contracts.rs'],'contracts.rs source hashes')
need(shafile(B/'tests/output_schema_text_json_decode.rs')==HARNESS and shafile(K/'tests/output_schema_text_json_decode.rs')==HARNESS,'harness source mismatch')

# Fresh clean release test-artifact receipts, logs, config/environment, and saved immutable binaries.
pair_build=load(BUILD/'build-pair-receipt.json')
need(pair_build['status']=='success' and pair_build['tests_executed'] is False and pair_build['source_bindings_sha256']==MAP,'paired build receipt')
need(shafile(BUILD/'build-pair-receipt.json')==prereg['provenance']['build_pair_receipt_sha256'],'paired build receipt hash')
need(pair_build['cargo_configuration_stable'] is True,'Cargo config stability')
cmd=load(BUILD/'build-pair-command.json')
need(shafile(BUILD/'build-pair-command.json')==prereg['provenance']['build_pair_command_sha256'],'build command hash')
need(cmd['tests_executed'] is False and cmd['artifact_target']=='output_schema_text_json_decode','build command target')
tool=load(BUILD/'tool-environment.json')
need(shafile(BUILD/'tool-environment.json')==prereg['provenance']['tool_environment_sha256'],'tool env hash')
need(tool['captured_before_clean_or_build'] is True and tool['unlisted_inherited_variables_forwarded'] is False,'controlled build env capture')
need(tool['actual_environment']['RUSTUP_TOOLCHAIN']=='stable-aarch64-apple-darwin' and tool['actual_environment']['CARGO_BUILD_JOBS']=='4','pinned build toolchain/jobs')
need(report['python']==prereg['python'] and report['runtime_environment']==prereg['runtime_environment'],'runtime env/Python pin')
identities={}
for side,root in [('baseline',B),('candidate',K)]:
    rp=BUILD/side/'build-receipt.json'; rec=load(rp); art=rec['artifact']; row=art['cargo_json']; saved=Path(art['saved_binary'])
    need(rec['status']=='success' and rec['clean_return_code']==0 and rec['build_return_code']==0,'build status '+side)
    need(rec['tests_executed'] is False and rec['source_stable'] is True and rec['head']==BASE,'build receipt '+side)
    need(rec['source_fingerprint_sha256']==binding['arms'][side]['tree']['sha256'] and rec['source_fingerprint_file_count']==5917,'build source pin '+side)
    need(rec['source_fingerprint_after']=={'sha256':binding['arms'][side]['tree']['sha256'],'file_count':5917},'postbuild source '+side)
    need(rec['tracked_diff_sha256']==(EMPTY if side=='baseline' else PATCH) and rec['staged_diff_sha256']==EMPTY,'build diff '+side)
    need(rec['product_patch_sha256']==PATCH and rec['harness_source_sha256']==HARNESS and rec['harness_patch_sha256']==HPATCH and rec['build_driver_sha256']==DRIVER,'build exact source pins '+side)
    need(row['reason']=='compiler-artifact' and row['fresh'] is False,'not a fresh compiler artifact '+side)
    need(row['target']['name']=='output_schema_text_json_decode' and row['target']['kind']==['test'] and row['target']['test'] is True,'artifact target '+side)
    need(row['profile']['test'] is True and row['profile']['opt_level']=='3' and row['profile']['debug_assertions'] is False,'optimized release test profile '+side)
    need(Path(row['manifest_path'])==root/'Cargo.toml' and Path(row['target']['src_path'])==root/'tests/output_schema_text_json_decode.rs','artifact source path '+side)
    need(saved.is_file() and not saved.is_symlink() and sha(saved.read_bytes())==art['saved_binary_sha256'],'saved executable '+side)
    need(os.access(saved,os.X_OK) and (saved.stat().st_mode&0o777)==0o555,'saved executable mode '+side)
    for field,hf in [('clean_log','clean_log_sha256'),('clean_stderr_log','clean_stderr_sha256'),('build_stdout_jsonl','build_stdout_sha256'),('build_stderr_log','build_stderr_sha256')]:
        lp=Path(rec[field]); need(lp.is_file() and sha(lp.read_bytes())==rec[hf],side+' '+field)
    need(rec['cargo_configuration_stable'] is True and rec['cargo_configuration_fingerprint_before']==rec['cargo_configuration_fingerprint_after'],'Cargo config '+side)
    identities[side]={'binary':saved,'sha256':art['saved_binary_sha256'],'receipt_sha256':sha(rp.read_bytes()),'tree_sha256':binding['arms'][side]['tree']['sha256']}
need(identities['baseline']['sha256']!=identities['candidate']['sha256'],'test binaries identical')

# Strictly inspect every retained raw report and per-attempt record independently.
expected_workload=prereg['workload']
expected_hash=expected_workload['decoded_string_sha256']
by_pair={}
all_times=[]
per_side_samples={'baseline':[],'candidate':[]}
for i,a in enumerate(report['attempts']):
    side=a['side']; g=a['global_pair_index']; phase='warmup' if g<3 else 'measurement'; order=('baseline','candidate') if g%2==0 else ('candidate','baseline'); pos=order.index(side)+1
    # phase index is local to each phase; attempt id is generated from that index.
    phase_index=g+1 if g<3 else g-2
    expected_id=f"{phase}-{phase_index:02d}-{pos}-{side}"
    need(a['phase']==phase and a['phase_pair_index']==phase_index and a['position_in_pair']==pos and a['attempt_id']==expected_id,'attempt schedule identity')
    need(a['successful'] is True and a['returncode']==0 and a['timed_out'] is False and a.get('execution_error') is None and a.get('retry')==0,'attempt outcome')
    root=B if side=='baseline' else K
    ident=identities[side]
    need(a['binary_sha256']==ident['sha256'] and a['source_fingerprint_sha256']==ident['tree_sha256'] and a['build_receipt_sha256']==ident['receipt_sha256'],'attempt provenance')
    env=a['execution_environment']
    raw_path=Path(a['raw_report_path'])
    need(raw_path.is_absolute() and raw_path.is_file() and env=={'LC_ALL':'C','TZ':'UTC','OPT_BENCH_REPORT':str(raw_path)},'runtime environment/report path')
    attempt_dir=Path(a['attempt_dir'])
    started=load(attempt_dir/'started.json'); result=load(attempt_dir/'result.json')
    need(started['binary_sha256_before']==ident['sha256'] and started['execution_environment']==env and started['retry']==0,'pre-run binary/env/retry')
    need(result['successful'] is True and result['binary_sha256_after']==ident['sha256'] and result['raw_report_sha256']==sha(raw_path.read_bytes()),'result/raw/binary identity')
    stdout=Path(a['stdout_path']).read_bytes(); stderr=Path(a['stderr_path']).read_bytes()
    need(sha(stdout)==a['stdout_sha256'] and len(stdout)==a['stdout_bytes'] and sha(stderr)==a['stderr_sha256'] and len(stderr)==a['stderr_bytes'],'captured output hashes')
    need(stderr==b'' and b'test text_json_decode_10000 ... ok' in stdout and b'1 passed; 0 failed' in stdout,'one ignored benchmark test passes per invocation')
    raw=load(raw_path)
    need(set(raw)=={'schema','method','warmup_runs','measured_samples','workload','results'},'raw schema fields')
    need(raw['schema']=='output_schema_text_json_decode_bench/v1' and raw['method']==METHOD and raw['warmup_runs']==0 and raw['measured_samples']==3,'raw report method/count')
    need(raw['workload']==expected_workload,'raw workload mismatch')
    case=raw['results'][CASE]
    times=case['raw_sample_ns']; need(len(times)==3 and all(type(x) is int and x>0 for x in times),'raw sample durations')
    ordered=sorted(times); need(case['p50_ns']==ordered[1] and case['p95_ns']==ordered[2],'raw p50/p95 recomputation')
    need(len(case['semantic_oracles'])==3,'oracle count')
    for oracle in case['semantic_oracles']:
        need(oracle=={'decoded_values':10000,'exact_value_matches':10000,'errors':0,'decoded_string_bytes':4096,'decoded_string_sha256':expected_hash},'semantic oracle mismatch')
    need(a['raw_sample_ns']==times and a['median_duration_ns']==ordered[1] and a['semantic_oracles']==case['semantic_oracles'],'outer report raw-result mismatch')
    per_side_samples[side].extend(times)
    all_times.append((iso(a['started_at']),iso(a['finished_at'])))
    by_pair.setdefault(g,{})[side]={'median':ordered[1],'phase':phase,'order':order}
need(len(by_pair)==51 and all(set(v)=={'baseline','candidate'} for v in by_pair.values()),'pair completeness')
need(all(all_times[i][1] <= all_times[i+1][0] for i in range(len(all_times)-1)),'attempts overlap or order drift')
need(all(iso(a['started_at'])<=iso(a['finished_at']) for a in report['attempts']),'attempt start/end order')

# Recompute every pair, order stratum, loss, and aggregate without report summaries.
measurement=[]
for g in range(3,51):
    pair=by_pair[g]; order=pair['baseline']['order']
    b=pair['baseline']['median']; c=pair['candidate']['median']; delta=c-b
    recorded=report['measured_pairs'][g-3]
    need(recorded['global_pair_index']==g and recorded['order']==list(order),'paired order/index')
    need(recorded['baseline_median_ns']==b and recorded['candidate_median_ns']==c and recorded['candidate_minus_baseline_ns']==delta and recorded['valid_for_timing'] is True and recorded['workload_compatible'] is True,'paired row arithmetic')
    measurement.append({'global_pair_index':g,'order':'AB' if order==('baseline','candidate') else 'BA','baseline_ns':b,'candidate_ns':c,'delta_ns':delta,'ratio':c/b})
deltas=[x['delta_ns'] for x in measurement]
ratios=[x['ratio'] for x in measurement]
wins=sum(x<0 for x in deltas); losses=sum(x>0 for x in deltas); ties=sum(x==0 for x in deltas)
summary=report['paired_summary']
need(summary['raw_deltas_ns']==deltas and summary['valid_pairs']==48 and summary['wins']==wins and summary['losses']==losses and summary['ties']==ties,'summary recomputation')
need(summary['median_candidate_minus_baseline_ns']==statistics.median(deltas),'exact median delta')
orders={}
for tag in ('AB','BA'):
    values=[x for x in measurement if x['order']==tag]
    orders[tag]={'pairs':len(values),'wins':sum(x['delta_ns']<0 for x in values),'losses':sum(x['delta_ns']>0 for x in values),'ties':sum(x['delta_ns']==0 for x in values),'median_delta_ns':statistics.median(x['delta_ns'] for x in values),'median_paired_ratio':statistics.median(x['ratio'] for x in values)}
arm_medians={s:statistics.median(per_side_samples[s]) for s in per_side_samples}
loss_rows=[{k:v for k,v in x.items() if k!='ratio'} for x in measurement if x['delta_ns']>0]
need(wins==42 and losses==6 and ties==0,'expected result counts differ')

out={
 'schema':'independent_text_json_decoder_trial_audit/1','status':'pass','reviewed_at_utc':datetime.now().astimezone().isoformat(),
 'report_sha256':sha(R.read_bytes()),'preregistration_sha256':sha(P.read_bytes()),'runner_sha256':sha(runner.read_bytes()),'source_map_sha256':sha(SB.read_bytes()),
 'provenance':{'base_commit':BASE,'baseline_tree_sha256':actual['baseline']['sha256'],'candidate_tree_sha256':actual['candidate']['sha256'],'source_file_count_each':5917,'only_changed_source_file':changed[0],'product_patch_sha256':PATCH,'harness_source_sha256':HARNESS,'harness_patch_sha256':HPATCH,'build_driver_sha256':DRIVER,'test_binaries':{k:{'sha256':v['sha256'],'build_receipt_sha256':v['receipt_sha256']} for k,v in identities.items()}},
 'build_and_run':{'build_pair_status':pair_build['status'],'clean_builds_successful':True,'optimized_release_test_artifacts_fresh_false':True,'saved_binaries_immutable_and_distinct':True,'test_executions':102,'test_passes':102,'builds_or_retries_during_trial':0,'report_window_utc':{'start':report['created_at'],'end':report['completed_at']},'attempt_first_start':report['attempts'][0]['started_at'],'attempt_last_finish':report['attempts'][-1]['finished_at'],'runtime_env_per_child':'exact LC_ALL=C, TZ=UTC, unique absolute report path'},
 'oracles':{'per_invocation_raw_reports':102,'samples_per_invocation':3,'raw_duration_samples':306,'all_sample_duration_positive':True,'all_sample_p50_p95_recomputed':True,'exact_decoded_string_checks':102*3*10000,'exact_output_oracles':102*3,'decoder_errors':0,'stderr_bytes':sum(a['stderr_bytes'] for a in report['attempts']),'all_stdout_test_pass_markers':True},
 'schedule':{'warmup_pairs':3,'warmup_orders':[p['order'] for p in report['warmup_pairs']],'measured_pairs':48,'measured_order_counts':{'AB':sum(x['order']=='AB' for x in measurement),'BA':sum(x['order']=='BA' for x in measurement)},'retries':0},
 'recomputed_results':{'pairs':48,'wins':wins,'losses':losses,'ties':ties,'median_candidate_minus_baseline_ns':statistics.median(deltas),'median_paired_ratio':statistics.median(ratios),'baseline_median_of_144_sample_durations_ns':arm_medians['baseline'],'candidate_median_of_144_sample_durations_ns':arm_medians['candidate'],'arm_median_ratio':arm_medians['candidate']/arm_medians['baseline'],'order_strata':orders,'all_loss_pairs':loss_rows,'minimum_pair_ratio':min(ratios),'maximum_pair_ratio':max(ratios)},
 'interpretation':'One narrow ARM64 macOS in-process public OutputSchema::decode workload: 10,000 decodes/sample of a 4 KiB ASCII JSON string, 3 samples/invocation. This does not establish CLI startup, whole-runtime latency, Bash superiority, broad decoder workloads, or end-to-end performance.',
 'retained_failure_history':'The earlier smoke validator rejection was caused by a descriptive fixture-string mismatch; it is preserved separately and excluded from timed pairs. The first smoke attempt passed only the baseline arm and candidate was not attempted. Two later smoke checks passed before the fixed paired trial.'
}
outpath=E/'independent-raw-audit-attempt-1/audit.json'
outpath.write_text(json.dumps(out,indent=2,ensure_ascii=False)+'\n')
print(json.dumps({'status':out['status'],'audit':str(outpath),'report_sha256':out['report_sha256'],'summary':out['recomputed_results']},indent=2))
