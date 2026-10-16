#!/usr/bin/env python3
"""Verify the published graph benchmark bundle without extracting raw archives."""
from __future__ import annotations
import hashlib
import json
import re
from pathlib import Path, PurePosixPath
import sys

ROOT = Path(__file__).resolve().parent
MANIFEST = Path('bundle-manifest.json')
SUMS = Path('bundle-sha256sums.txt')
SELF = Path('verify_bundle.py')

def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open('rb') as f:
        for chunk in iter(lambda: f.read(1024 * 1024), b''):
            digest.update(chunk)
    return digest.hexdigest()

def safe_file(rel: str) -> Path:
    p = PurePosixPath(rel)
    if p.is_absolute() or not p.parts or any(part in ('', '.', '..') for part in p.parts):
        raise ValueError(f'unsafe path in manifest: {rel!r}')
    path = ROOT.joinpath(*p.parts)
    if path.is_symlink() or not path.is_file():
        raise ValueError(f'missing or symlinked bundle file: {rel}')
    if ROOT not in path.resolve().parents:
        raise ValueError(f'path escapes bundle: {rel}')
    return path

def validate_source_map(expected: dict[str, tuple[int, str]]) -> None:
    rel = 'source-map.json'
    if rel not in expected:
        raise ValueError('source-map.json is missing from the bundle manifest')
    source_map = json.loads(safe_file(rel).read_text(encoding='utf-8'))

    def validate_digest_value(value: object, path: str) -> None:
        if isinstance(value, str):
            if re.fullmatch(r'[0-9a-f]{64}', value) is None:
                raise ValueError(f'invalid SHA-256 digest at source map {path}')
        elif isinstance(value, dict):
            for key, child in value.items():
                validate_digest_value(child, f'{path}.{key}')
        else:
            raise ValueError(f'invalid SHA-256 digest at source map {path}')

    def validate_digest_fields(value: object, path: str = '$') -> None:
        if isinstance(value, dict):
            for key, child in value.items():
                child_path = f'{path}.{key}'
                if key == 'sha256' or key.endswith('_sha256'):
                    validate_digest_value(child, child_path)
                else:
                    validate_digest_fields(child, child_path)
        elif isinstance(value, list):
            for index, child in enumerate(value):
                validate_digest_fields(child, f'{path}[{index}]')

    validate_digest_fields(source_map)
    snapshots = source_map.get('snapshots')
    if not isinstance(snapshots, list):
        raise ValueError('source-map snapshots must be an array')
    snapshot_hashes: dict[str, str] = {}
    for snapshot in snapshots:
        if not isinstance(snapshot, dict) or not isinstance(snapshot.get('path'), str):
            raise ValueError('invalid source-map snapshot entry')
        path = snapshot['path']
        digest = snapshot.get('sha256')
        if path in snapshot_hashes:
            raise ValueError(f'duplicate source-map snapshot path: {path}')
        if path not in expected:
            raise ValueError(f'source-map snapshot is not listed in the bundle manifest: {path}')
        if digest != expected[path][1]:
            raise ValueError(f'source-map snapshot digest does not match bundled bytes: {path}')
        snapshot_hashes[path] = digest

    def require_transport(label: str, declared: object, snapshot_path: str) -> None:
        if snapshot_path not in snapshot_hashes or declared != snapshot_hashes[snapshot_path]:
            raise ValueError(f'source-map {label} does not match snapshot {snapshot_path}')

    shared_path = 'shared-source/transport-snapshots/shared-collector-bfc03a.rs'
    original_path = 'shared-source/transport-snapshots/original-baseline-049b2b.rs'
    queue_path = 'shared-source/transport-snapshots/queue-candidate-ac5465.rs'
    require_transport('shared collector transport hash',
                      source_map.get('shared_collector_progress', {}).get('transport_sha256'), shared_path)
    require_transport('recv_any candidate patch transport hash',
                      source_map.get('recv_any_candidate_only', {}).get('transport_sha256'), queue_path)
    trials = source_map.get('trials')
    if not isinstance(trials, dict):
        raise ValueError('source-map trials must be an object')
    require_transport('collector baseline transport hash',
                      trials.get('collector-cpu-diagnostic', {}).get('baseline_transport_sha256'), original_path)
    require_transport('collector CPU candidate transport hash',
                      trials.get('collector-cpu-diagnostic', {}).get('candidate_transport_sha256'), shared_path)
    require_transport('recv_any baseline transport hash',
                      trials.get('recv-any-isolated', {}).get('baseline_transport_sha256'), shared_path)
    require_transport('recv_any candidate transport hash',
                      trials.get('recv-any-isolated', {}).get('candidate_transport_sha256'), queue_path)

def main() -> int:
    try:
        manifest = json.loads((ROOT / MANIFEST).read_text(encoding='utf-8'))
        if manifest.get('format') != 'graph-benchmark-publication-bundle.v1':
            raise ValueError('unsupported bundle manifest format')
        entries = manifest.get('files')
        if not isinstance(entries, list):
            raise ValueError('manifest files must be an array')
        expected: dict[str, tuple[int, str]] = {}
        for entry in entries:
            if not isinstance(entry, dict):
                raise ValueError('invalid manifest file entry')
            rel = entry.get('path')
            size = entry.get('bytes')
            digest = entry.get('sha256')
            if not isinstance(rel, str) or not isinstance(size, int) or not isinstance(digest, str):
                raise ValueError('invalid manifest path/size/hash')
            if rel in expected or rel in (str(MANIFEST), str(SUMS)):
                raise ValueError(f'duplicate or reserved manifest path: {rel}')
            expected[rel] = (size, digest)
        for reserved in (str(MANIFEST), str(SUMS)):
            if reserved in expected:
                raise ValueError(f'reserved path must not appear in manifest: {reserved}')
        validate_source_map(expected)
        for rel, (size, digest) in expected.items():
            path = safe_file(rel)
            if path.stat().st_size != size:
                raise ValueError(f'size mismatch: {rel}')
            if sha256(path) != digest:
                raise ValueError(f'SHA-256 mismatch: {rel}')
        sum_lines = (ROOT / SUMS).read_text(encoding='ascii').splitlines()
        sums: dict[str, str] = {}
        for line in sum_lines:
            parts = line.split('  ', 1)
            if len(parts) != 2 or len(parts[0]) != 64:
                raise ValueError(f'malformed checksum line: {line!r}')
            digest, rel = parts
            if rel in sums:
                raise ValueError(f'duplicate checksum path: {rel}')
            sums[rel] = digest
        checksum_paths = set(expected) | {str(MANIFEST), str(SELF)}
        if set(sums) != checksum_paths:
            missing = sorted(checksum_paths - set(sums))
            extra = sorted(set(sums) - checksum_paths)
            raise ValueError(f'checksum list path mismatch; missing={missing}, extra={extra}')
        for rel, digest in sums.items():
            if sha256(safe_file(rel)) != digest:
                raise ValueError(f'checksum list mismatch: {rel}')
        actual = set()
        for p in ROOT.rglob('*'):
            if p.is_symlink():
                raise ValueError(f'symlink found in bundle: {p.relative_to(ROOT)}')
            if p.is_file():
                actual.add(p.relative_to(ROOT).as_posix())
        allowed = checksum_paths | {str(SUMS)}
        if actual != allowed:
            raise ValueError(f'bundle file set mismatch; extra={sorted(actual-allowed)}, missing={sorted(allowed-actual)}')
        print(f"OK: {len(actual)} files verified; archives were hashed but not extracted")
        print(f"Payload bytes (excluding manifest/checksum list): {sum(size for size, _ in expected.values()):,}")
        return 0
    except Exception as exc:  # verifier should report concise integrity failures
        print(f'ERROR: {exc}', file=sys.stderr)
        return 1

if __name__ == '__main__':
    raise SystemExit(main())
