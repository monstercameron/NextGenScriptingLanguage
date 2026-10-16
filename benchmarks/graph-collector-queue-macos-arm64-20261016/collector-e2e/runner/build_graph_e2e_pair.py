"""Build and retain exact release emit test binaries, sequentially."""
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess

C = Path(__file__).resolve().parent.parent
E = C / 'evidence/graph-transport-e2e'
TARGET = C / 'work/perf-target-candidate'
ENV = dict(os.environ)
ENV.update(
    PATH='/Users/earlcameron/Documents/Codex/2026-10-12/you/work/rust/cargo/bin:' + ENV['PATH'],
    CARGO_HOME='/Users/earlcameron/Documents/Codex/2026-10-12/you/work/rust/cargo',
    RUSTUP_HOME='/Users/earlcameron/Documents/Codex/2026-10-12/you/work/rust/rustup',
    RUSTUP_TOOLCHAIN='stable-aarch64-apple-darwin',
    CARGO_BUILD_JOBS='4',
    CARGO_TARGET_DIR=str(TARGET),
)

metadata = {
    'rustc_vv': subprocess.check_output(['rustc', '-Vv'], env=ENV, text=True),
    'cargo_version': subprocess.check_output(['cargo', '--version'], env=ENV, text=True),
    'selected_build_environment': {key: value for key, value in ENV.items() if key in ['PATH','CARGO_HOME','RUSTUP_HOME','RUSTUP_TOOLCHAIN','CARGO_TARGET_DIR','CARGO_BUILD_JOBS','RUSTFLAGS','CARGO_ENCODED_RUSTFLAGS','RUSTDOCFLAGS','CC','CXX','AR','SDKROOT','MACOSX_DEPLOYMENT_TARGET'] or key.startswith('CARGO_PROFILE_RELEASE_')},
    'captured_before_clean_and_build': True,
}
with (E / 'build-environment-attempt-1.json').open('x') as out:
    json.dump(metadata, out, indent=2)
    out.write('\n')

def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()

for side, root in [('baseline', C / 'work/graph-optimizations-integrated-81bf9ba'), ('candidate', C / 'work/graph-collector-sleep-candidate')]:
    source = root / 'src/graph/transport.rs'
    source_hash = digest(source)
    with (E / f'{side}-release-clean-attempt-1.log').open('x') as log:
        subprocess.run(['cargo', 'clean', '--release', '-p', 'execute'], cwd=root, env=ENV,
                       stdout=log, stderr=subprocess.STDOUT, check=True)
    stdout = E / f'{side}-build-attempt-1.jsonl'
    with stdout.open('x') as out, (E / f'{side}-build-attempt-1.log').open('x') as err:
        subprocess.run(['cargo', 'test', '--locked', '--release', '--test', 'graph_transport_e2e_bench', '--test', 'graph_runtime', '--test', 'graph_lifecycles', '--test', 'graph_result_events', '--lib', '--no-run', '--message-format=json'],
                       cwd=root, env=ENV, stdout=out, stderr=err, check=True)
    rows = [json.loads(line) for line in stdout.read_text().splitlines()]
    artifacts = [row for row in rows if row.get('reason') == 'compiler-artifact'
                 and row.get('target', {}).get('name') == 'graph_transport_e2e_bench'
                 and row.get('profile', {}).get('test') and row.get('executable')]
    assert len(artifacts) == 1, artifacts
    artifact = artifacts[0]
    assert artifact['fresh'] is False
    assert Path(artifact['manifest_path']) == root / 'Cargo.toml'
    assert Path(artifact['target']['src_path']) == root / 'tests/graph_transport_e2e_bench.rs'
    assert artifact['profile']['opt_level'] == '3'
    assert artifact['profile']['debug_assertions'] is False
    binary = Path(artifact['executable'])
    assert binary.is_relative_to(TARGET / 'release/deps')
    destination = E / f'{side}-test-binary'
    assert not destination.exists()
    shutil.copy2(binary, destination)
    listing = subprocess.check_output([str(destination), '--list'], text=True)
    assert 'bounded_native_stream_fanout_has_exact_end_to_end_samples: test' in listing
    assert digest(source) == source_hash
    (E / f'{side}-test-list.txt').write_text(listing)
    receipt = {'artifact': artifact, 'saved_binary': str(destination), 'sha256': digest(destination),
               'source_sha256': source_hash, 'source_stable': True, 'attempt': 1, 'build_metadata': metadata}
    for extra in rows:
        if extra.get('reason') == 'compiler-artifact' and extra.get('executable') and extra.get('profile', {}).get('test') and extra.get('target',{}).get('name') in ['flow','graph_runtime','graph_lifecycles','graph_result_events']:
            shutil.copy2(extra['executable'], E / f"{side}-{extra['target']['name']}-test-binary")
    (E / f'{side}-binary-receipt.json').write_text(json.dumps(receipt, indent=2) + '\n')
    print(json.dumps({'side': side, 'saved_binary': str(destination), 'sha256': receipt['sha256']}), flush=True)
