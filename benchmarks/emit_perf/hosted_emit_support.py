#!/usr/bin/env python3
"""Build provenance support for the fixed hosted emit benchmark replication."""
from __future__ import annotations

import argparse
import hashlib
import json
import lzma
import os
from pathlib import Path
import shutil
import subprocess
import sys
from datetime import datetime, timezone
from typing import Any, Iterable


BASE_COMMIT = "b78693294169c22892bb4f78539348a5755992ad"
BASELINE_TRANSPORT_SHA256 = "db5430bd3a8cd25ed5e20bac4395b56814ec1ad6e2189d4e89d2f152c180c17f"
CANDIDATE_TRANSPORT_SHA256 = "7cc3497231fd60d7c31ed7c98390281df4be0913398e7ccee1e6ffe75f4c48d3"
HARNESS_SHA256 = "098db9038e8e8cc684eb6533d3fb60aaa33184f3e16cc9e9a028ac2c860a48f3"
BASELINE_PATCH_SHA256 = "3c7b691f8b0b88c4393f58105fea8d29a566dbbdf958d4eb4e063d20f93eb60e"
BASELINE_PATCH_XZ_SHA256 = "f3583a3d18929b393ebfca82a5422d5b306e4954e33bc7e8b596161393879974"
CANDIDATE_PATCH_SHA256 = "2c9bcd8ec874fed6f4798224f9d94b4fa5b59f208db16e7d8f60b81192f8d877"
CANDIDATE_PATCH_XZ_SHA256 = "239b3f3a19fcb65b2a2be490b32e7e9fb00acdbcb9f62a562693baebb8b7f9a3"


class EvidenceError(RuntimeError):
    pass


def sha256_bytes(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def write_json(path: Path, payload: Any) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(payload, sort_keys=True, indent=2, allow_nan=False) + "\n", encoding="utf-8")


def _git(root: Path, *args: str) -> str:
    result = subprocess.run(
        ["git", "-C", str(root), *args],
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
        timeout=20,
    )
    if result.returncode:
        raise EvidenceError(result.stderr.decode("utf-8", errors="replace").strip())
    return result.stdout.decode("utf-8").strip()


def extract_harness(transport: bytes, frozen_harness: bytes) -> bytes:
    if sha256_bytes(frozen_harness) != HARNESS_SHA256:
        raise EvidenceError("frozen benchmark harness reference has the wrong digest")
    if transport.count(frozen_harness) != 1:
        raise EvidenceError("complete frozen benchmark harness must occur exactly once")
    start = transport.index(frozen_harness)
    extracted = transport[start : start + len(frozen_harness)]
    if sha256_bytes(extracted) != HARNESS_SHA256:
        raise EvidenceError("extracted benchmark harness differs from the frozen source")
    return extracted


def unpack_frozen_patch(side: str, archive: Path, output: Path) -> dict[str, Any]:
    expected = {
        "baseline": (BASELINE_PATCH_XZ_SHA256, BASELINE_PATCH_SHA256),
        "candidate": (CANDIDATE_PATCH_XZ_SHA256, CANDIDATE_PATCH_SHA256),
    }
    if side not in expected:
        raise EvidenceError("patch side must be baseline or candidate")
    archive = archive.resolve(strict=True)
    compressed = archive.read_bytes()
    compressed_hash, source_hash = expected[side]
    if sha256_bytes(compressed) != compressed_hash:
        raise EvidenceError(f"{side} compressed patch digest differs from its frozen digest")
    try:
        source = lzma.decompress(compressed, format=lzma.FORMAT_XZ)
    except lzma.LZMAError as error:
        raise EvidenceError(f"{side} patch is not a valid frozen XZ stream: {error}") from error
    if sha256_bytes(source) != source_hash:
        raise EvidenceError(f"{side} decompressed patch digest differs from its frozen digest")
    output = output.expanduser().absolute()
    if output.exists() or output.is_symlink():
        raise EvidenceError(f"refusing to overwrite temporary patch: {output}")
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_bytes(source)
    return {
        "side": side,
        "archive": str(archive),
        "archive_sha256": compressed_hash,
        "patch": str(output),
        "patch_sha256": source_hash,
    }


def _validate_arm(root: Path, side: str, frozen_harness: bytes) -> tuple[Path, str, bytes]:
    root = root.resolve(strict=True)
    if _git(root, "rev-parse", "HEAD") != BASE_COMMIT:
        raise EvidenceError(f"{side} source checkout is not at the frozen common base")
    expected = {
        "baseline": BASELINE_TRANSPORT_SHA256,
        "candidate": CANDIDATE_TRANSPORT_SHA256,
    }[side]
    transport_path = root / "src/graph/transport.rs"
    transport = transport_path.read_bytes()
    actual = sha256_bytes(transport)
    if actual != expected:
        raise EvidenceError(f"{side} transport SHA-256 differs from its frozen arm")
    changed = _git(root, "diff", "--name-only").splitlines()
    staged = _git(root, "diff", "--cached", "--name-only").splitlines()
    untracked = [
        item
        for item in _git(root, "ls-files", "--others", "--exclude-standard").splitlines()
        if item
    ]
    if changed != ["src/graph/transport.rs"] or staged or untracked:
        raise EvidenceError(
            f"{side} checkout has unexpected changes: changed={changed}, staged={staged}, untracked={untracked}"
        )
    return transport_path, actual, extract_harness(transport, frozen_harness)


def freeze(args: argparse.Namespace) -> None:
    reference = Path(args.frozen_harness).resolve(strict=True).read_bytes()
    if sha256_bytes(reference) != HARNESS_SHA256:
        raise EvidenceError("checked-in frozen harness reference has the wrong digest")
    baseline_path, baseline_hash, baseline_harness = _validate_arm(
        Path(args.baseline_root), "baseline", reference
    )
    candidate_path, candidate_hash, candidate_harness = _validate_arm(
        Path(args.candidate_root), "candidate", reference
    )
    if baseline_hash == candidate_hash or baseline_harness != candidate_harness:
        raise EvidenceError("arms must differ in transport source and share the exact harness bytes")
    if sha256_bytes(reference) != HARNESS_SHA256 or reference != baseline_harness:
        raise EvidenceError("checked-in frozen harness reference differs from extracted source")
    output = Path(args.output_dir).resolve()
    output.mkdir(parents=True, exist_ok=True)
    extracted = output / "emit-perf-harness-extracted.rs"
    extracted.write_bytes(baseline_harness)
    source_freeze = {
        "recorded_at": datetime.now(timezone.utc).isoformat(timespec="seconds").replace("+00:00", "Z"),
        "base_commit": BASE_COMMIT,
        "identical_harness": True,
        "harness_sha256": HARNESS_SHA256,
        "harness_extracted_from_both_arms": True,
        "source_paths": {
            str(baseline_path.resolve()): baseline_hash,
            str(candidate_path.resolve()): candidate_hash,
        },
        "claim": "hosted Linux replication inputs; no performance conclusion is implied by this record",
    }
    freeze_path = output / "emit-source-freeze.json"
    write_json(freeze_path, source_freeze)
    print(json.dumps({
        "harness_source": str(extracted),
        "harness_sha256": sha256_file(extracted),
        "source_freeze": str(freeze_path),
        "source_freeze_sha256": sha256_file(freeze_path),
    }, sort_keys=True))


def _cargo_artifact_lines(path: Path) -> Iterable[dict[str, Any]]:
    try:
        raw_lines = path.read_text(encoding="utf-8").splitlines()
    except (OSError, UnicodeError) as error:
        raise EvidenceError(f"cannot read Cargo JSON output: {error}") from error
    for line_number, line in enumerate(raw_lines, 1):
        if not line:
            continue
        try:
            record = json.loads(line)
        except json.JSONDecodeError as error:
            raise EvidenceError(f"Cargo output line {line_number} is not JSON: {error}") from error
        if isinstance(record, dict) and record.get("reason") == "compiler-artifact":
            yield record


def select_test_binary(
    records: Iterable[dict[str, Any]],
    root: Path,
    target_dir: Path,
) -> tuple[dict[str, Any], Path]:
    root = root.resolve(strict=True)
    target_dir = target_dir.resolve(strict=True)
    manifest = (root / "Cargo.toml").resolve(strict=True)
    source = (root / "src/lib.rs").resolve(strict=True)
    release_deps = (target_dir / "release/deps").resolve(strict=True)
    matches: list[tuple[dict[str, Any], Path]] = []
    for artifact in records:
        target = artifact.get("target")
        profile = artifact.get("profile")
        if not isinstance(target, dict) or not isinstance(profile, dict):
            continue
        kinds = target.get("kind")
        try:
            artifact_manifest = Path(artifact["manifest_path"]).resolve(strict=True)
            target_source = Path(target["src_path"]).resolve(strict=True)
            executable = Path(artifact["executable"]).resolve(strict=True)
        except (KeyError, OSError, TypeError, ValueError):
            continue
        if (
            artifact.get("reason") == "compiler-artifact"
            and artifact.get("fresh") is False
            and artifact_manifest == manifest
            and target_source == source
            and target.get("name") == "flow"
            and target.get("test") is True
            and isinstance(kinds, list)
            and "lib" in kinds
            and profile.get("test") is True
            and profile.get("opt_level") == "3"
            and profile.get("debug_assertions") is False
            and executable.is_file()
            and os.access(executable, os.X_OK)
        ):
            try:
                executable.relative_to(release_deps)
            except ValueError:
                continue
            matches.append((artifact, executable))
    if len(matches) != 1:
        raise EvidenceError(f"expected exactly one fresh release library test binary, found {len(matches)}")
    return matches[0]


def record_build(args: argparse.Namespace) -> None:
    side = args.side
    if side not in ("baseline", "candidate"):
        raise EvidenceError("side must be baseline or candidate")
    root = Path(args.root).resolve(strict=True)
    expected_source = {
        "baseline": BASELINE_TRANSPORT_SHA256,
        "candidate": CANDIDATE_TRANSPORT_SHA256,
    }[side]
    source_path = root / "src/graph/transport.rs"
    actual_source = sha256_file(source_path)
    if actual_source != expected_source or _git(root, "rev-parse", "HEAD") != BASE_COMMIT:
        raise EvidenceError(f"{side} build source differs from the frozen source identity")
    target_dir = Path(args.target_dir).resolve(strict=True)
    if not args.fresh_target_dir_confirmed:
        raise EvidenceError("fresh target-directory check was not recorded")
    artifact, executable = select_test_binary(
        _cargo_artifact_lines(Path(args.cargo_json)),
        root,
        target_dir,
    )
    out = Path(args.output_dir).resolve()
    out.mkdir(parents=True, exist_ok=True)
    saved_binary = out / "test-binary"
    shutil.copyfile(executable, saved_binary)
    saved_binary.chmod(0o755)
    receipt = {
        "artifact": artifact,
        "saved_binary": str(saved_binary),
        "sha256": sha256_file(saved_binary),
        "source_sha256": actual_source,
        "source_stable": True,
        "fresh_target_dir_confirmed": True,
        "arm": side,
    }
    receipt_path = out / "build-receipt.json"
    write_json(receipt_path, receipt)
    print(json.dumps({
        "receipt": str(receipt_path),
        "receipt_sha256": sha256_file(receipt_path),
        "binary_sha256": receipt["sha256"],
    }, sort_keys=True))


def unpack_patch(args: argparse.Namespace) -> None:
    result = unpack_frozen_patch(args.side, Path(args.archive), Path(args.output))
    print(json.dumps(result, sort_keys=True))


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    subcommands = parser.add_subparsers(dest="command", required=True)
    freeze_parser = subcommands.add_parser("freeze")
    freeze_parser.add_argument("--baseline-root", required=True)
    freeze_parser.add_argument("--candidate-root", required=True)
    freeze_parser.add_argument("--frozen-harness", required=True)
    freeze_parser.add_argument("--output-dir", required=True)
    freeze_parser.set_defaults(function=freeze)
    unpack_parser = subcommands.add_parser("unpack-patch")
    unpack_parser.add_argument("--side", required=True)
    unpack_parser.add_argument("--archive", required=True)
    unpack_parser.add_argument("--output", required=True)
    unpack_parser.set_defaults(function=unpack_patch)
    receipt_parser = subcommands.add_parser("record-build")
    receipt_parser.add_argument("--side", required=True)
    receipt_parser.add_argument("--root", required=True)
    receipt_parser.add_argument("--target-dir", required=True)
    receipt_parser.add_argument("--cargo-json", required=True)
    receipt_parser.add_argument("--output-dir", required=True)
    receipt_parser.add_argument("--fresh-target-dir-confirmed", action="store_true")
    receipt_parser.set_defaults(function=record_build)
    return parser


def main() -> int:
    args = _parser().parse_args()
    try:
        args.function(args)
        return 0
    except (EvidenceError, OSError, KeyError, ValueError, TypeError, json.JSONDecodeError) as error:
        print(f"hosted emit evidence: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
