"""Build and retain exact release emit test binaries, sequentially."""
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess

C = Path(__file__).resolve().parent.parent
E = C / 'evidence/graph-named-recv'
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

def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()

for side, root in [('baseline', C / 'work/graph-named-recv-baseline-c6'), ('candidate', C / 'work/graph-named-recv-worktree')]:
    source = root / 'src/graph/transport.rs'
    source_hash = digest(source)
    with (E / f'{side}-release-clean-attempt-1.log').open('x') as log:
        subprocess.run(['cargo', 'clean', '--release', '-p', 'execute'], cwd=root, env=ENV,
                       stdout=log, stderr=subprocess.STDOUT, check=True)
    stdout = E / f'{side}-build-attempt-1.jsonl'
    with stdout.open('x') as out, (E / f'{side}-build-attempt-1.log').open('x') as err:
        subprocess.run(['cargo', 'test', '--locked', '--release', '--lib', '--no-run', '--message-format=json'],
                       cwd=root, env=ENV, stdout=out, stderr=err, check=True)
    rows = [json.loads(line) for line in stdout.read_text().splitlines()]
    artifacts = [row for row in rows if row.get('reason') == 'compiler-artifact'
                 and row.get('target', {}).get('name') == 'flow'
                 and row.get('profile', {}).get('test') and row.get('executable')]
    assert len(artifacts) == 1, artifacts
    artifact = artifacts[0]
    assert artifact['fresh'] is False
    assert Path(artifact['manifest_path']) == root / 'Cargo.toml'
    assert Path(artifact['target']['src_path']) == root / 'src/lib.rs'
    assert artifact['profile']['opt_level'] == '3'
    assert artifact['profile']['debug_assertions'] is False
    binary = Path(artifact['executable'])
    assert binary.is_relative_to(TARGET / 'release/deps')
    destination = E / f'{side}-test-binary'
    assert not destination.exists()
    shutil.copy2(binary, destination)
    listing = subprocess.check_output([str(destination), '--list'], text=True)
    assert 'graph::transport::input_cursor_tests::recv_named_preloaded_bench::recv_named_preloaded_bounded_queue_throughput: test' in listing
    if side == 'candidate':
        assert 'graph::transport::input_cursor_tests::cached_seed_names_and_edge_cursors_survive_mixed_receive_methods' in listing
    assert digest(source) == source_hash
    (E / f'{side}-test-list.txt').write_text(listing)
    receipt = {'artifact': artifact, 'saved_binary': str(destination), 'sha256': digest(destination),
               'source_sha256': source_hash, 'source_stable': True, 'attempt': 1}
    (E / f'{side}-binary-receipt.json').write_text(json.dumps(receipt, indent=2) + '\n')
    print(json.dumps({'side': side, 'saved_binary': str(destination), 'sha256': receipt['sha256']}), flush=True)
