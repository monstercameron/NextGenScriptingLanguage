#!/usr/bin/env python3
"""Build provenance support for fixed hosted emit benchmark profiles."""
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


DEFAULT_PROFILE = "emit-sync"
BASE_COMMIT = "b78693294169c22892bb4f78539348a5755992ad"
BASELINE_TRANSPORT_SHA256 = "db5430bd3a8cd25ed5e20bac4395b56814ec1ad6e2189d4e89d2f152c180c17f"
CANDIDATE_TRANSPORT_SHA256 = "7cc3497231fd60d7c31ed7c98390281df4be0913398e7ccee1e6ffe75f4c48d3"
HARNESS_SHA256 = "098db9038e8e8cc684eb6533d3fb60aaa33184f3e16cc9e9a028ac2c860a48f3"
BASELINE_PATCH_SHA256 = "3c7b691f8b0b88c4393f58105fea8d29a566dbbdf958d4eb4e063d20f93eb60e"
BASELINE_PATCH_XZ_SHA256 = "f3583a3d18929b393ebfca82a5422d5b306e4954e33bc7e8b596161393879974"
CANDIDATE_PATCH_SHA256 = "2c9bcd8ec874fed6f4798224f9d94b4fa5b59f208db16e7d8f60b81192f8d877"
CANDIDATE_PATCH_XZ_SHA256 = "239b3f3a19fcb65b2a2be490b32e7e9fb00acdbcb9f62a562693baebb8b7f9a3"
DRIVER_SHA256 = "1a2fe69a2cd667c65c5e23b2e04e877bf10163261fea54721be383834a6eeba6"

PROFILE_CONFIGS = {
    DEFAULT_PROFILE: {
        "base_commit": BASE_COMMIT,
        "harness_sha256": HARNESS_SHA256,
        "sources": {
            "baseline": BASELINE_TRANSPORT_SHA256,
            "candidate": CANDIDATE_TRANSPORT_SHA256,
        },
        "patches": {
            "baseline": {
                "archive": "baseline-patch.xz",
                "sha256": BASELINE_PATCH_SHA256,
                "xz_sha256": BASELINE_PATCH_XZ_SHA256,
            },
            "candidate": {
                "archive": "candidate-patch.xz",
                "sha256": CANDIDATE_PATCH_SHA256,
                "xz_sha256": CANDIDATE_PATCH_XZ_SHA256,
            },
        },
    },
    "payload-size": {
        "base_commit": "69a9e74abcdf09efd8ecb83e4b7f1a8d5f30488a",
        "harness_sha256": HARNESS_SHA256,
        "sources": {
            "baseline": "36e59989fc8abe3afeeab0922792cfb4fff1110ef234b6e8946c9930c54dc056",
            "candidate": "5975e969cf68e8b5d7cc5c11548d0b2234477f397697da81760c499d3ccb3927",
        },
        "patches": {
            "baseline": {
                "archive": "baseline-payload-size-patch.xz",
                "sha256": "63aae53a633ed39de9b18bc10c47166463de521231ca81f45a4d1f1fd424ba26",
                "xz_sha256": "72fb1e5a68a7322d084c7610e48f2bdc8836a79a6550ef3afaf5475d14f696f7",
            },
            "candidate": {
                "archive": "candidate-payload-size-patch.xz",
                "sha256": "ba25e336def3d22920ecca23b070b959d4968a98098cd8fa0466c93acaf01c59",
                "xz_sha256": "494a34d2bdd181dce3d9671c89c9201d7a977020f01e28bad742e50540f7429e",
            },
        },
    },
}


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


def profile_config(profile: str) -> dict[str, Any]:
    try:
        return PROFILE_CONFIGS[profile]
    except KeyError as error:
        raise EvidenceError(f"unknown frozen experiment profile: {profile}") from error


def profile_environment_values(profile: str) -> dict[str, str]:
    config = profile_config(profile)
    baseline_patch = config["patches"]["baseline"]
    candidate_patch = config["patches"]["candidate"]
    return {
        "EXPERIMENT_PROFILE": profile,
        "BASE_COMMIT": config["base_commit"],
        "BASELINE_PATCH_ARCHIVE": baseline_patch["archive"],
        "BASELINE_PATCH_SHA256": baseline_patch["sha256"],
        "BASELINE_PATCH_XZ_SHA256": baseline_patch["xz_sha256"],
        "CANDIDATE_PATCH_ARCHIVE": candidate_patch["archive"],
        "CANDIDATE_PATCH_SHA256": candidate_patch["sha256"],
        "CANDIDATE_PATCH_XZ_SHA256": candidate_patch["xz_sha256"],
        "BASELINE_SOURCE_SHA256": config["sources"]["baseline"],
        "CANDIDATE_SOURCE_SHA256": config["sources"]["candidate"],
        "HARNESS_SHA256": config["harness_sha256"],
        "DRIVER_SHA256": DRIVER_SHA256,
        "INVOCATION_TIMEOUT_SECONDS": "60",
    }


def select_profile(
    profile: str,
    github_env: Path | None = None,
    github_output: Path | None = None,
) -> dict[str, str]:
    values = profile_environment_values(profile)
    if github_env is not None:
        with github_env.open("a", encoding="utf-8") as handle:
            for name, value in values.items():
                handle.write(f"{name}={value}\n")
    if github_output is not None:
        with github_output.open("a", encoding="utf-8") as handle:
            for name, value in values.items():
                handle.write(f"{name.lower()}={value}\n")
    return values


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


def extract_harness(
    transport: bytes, frozen_harness: bytes, expected_sha256: str = HARNESS_SHA256
) -> bytes:
    if sha256_bytes(frozen_harness) != expected_sha256:
        raise EvidenceError("frozen benchmark harness reference has the wrong digest")
    if transport.count(frozen_harness) != 1:
        raise EvidenceError("complete frozen benchmark harness must occur exactly once")
    start = transport.index(frozen_harness)
    extracted = transport[start : start + len(frozen_harness)]
    if sha256_bytes(extracted) != expected_sha256:
        raise EvidenceError("extracted benchmark harness differs from the frozen source")
    return extracted


def unpack_frozen_patch(
    side: str, archive: Path, output: Path, profile: str = DEFAULT_PROFILE
) -> dict[str, Any]:
    if side not in ("baseline", "candidate"):
        raise EvidenceError("patch side must be baseline or candidate")
    patch_config = profile_config(profile)["patches"][side]
    archive = archive.resolve(strict=True)
    compressed = archive.read_bytes()
    compressed_hash, source_hash = patch_config["xz_sha256"], patch_config["sha256"]
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
        "profile": profile,
        "archive": str(archive),
        "archive_sha256": compressed_hash,
        "patch": str(output),
        "patch_sha256": source_hash,
    }


def _validate_arm(
    root: Path, side: str, frozen_harness: bytes, profile: str = DEFAULT_PROFILE
) -> tuple[Path, str, bytes]:
    root = root.resolve(strict=True)
    config = profile_config(profile)
    if _git(root, "rev-parse", "HEAD") != config["base_commit"]:
        raise EvidenceError(f"{side} source checkout is not at the frozen common base")
    expected = config["sources"][side]
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
    return transport_path, actual, extract_harness(
        transport, frozen_harness, config["harness_sha256"]
    )


def freeze(args: argparse.Namespace) -> None:
    config = profile_config(args.profile)
    reference = Path(args.frozen_harness).resolve(strict=True).read_bytes()
    if sha256_bytes(reference) != config["harness_sha256"]:
        raise EvidenceError("checked-in frozen harness reference has the wrong digest")
    baseline_path, baseline_hash, baseline_harness = _validate_arm(
        Path(args.baseline_root), "baseline", reference, args.profile
    )
    candidate_path, candidate_hash, candidate_harness = _validate_arm(
        Path(args.candidate_root), "candidate", reference, args.profile
    )
    if baseline_hash == candidate_hash or baseline_harness != candidate_harness:
        raise EvidenceError("arms must differ in transport source and share the exact harness bytes")
    if sha256_bytes(reference) != config["harness_sha256"] or reference != baseline_harness:
        raise EvidenceError("checked-in frozen harness reference differs from extracted source")
    output = Path(args.output_dir).resolve()
    output.mkdir(parents=True, exist_ok=True)
    extracted = output / "emit-perf-harness-extracted.rs"
    extracted.write_bytes(baseline_harness)
    source_freeze = {
        "recorded_at": datetime.now(timezone.utc).isoformat(timespec="seconds").replace("+00:00", "Z"),
        "profile": args.profile,
        "base_commit": config["base_commit"],
        "identical_harness": True,
        "harness_sha256": config["harness_sha256"],
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
    config = profile_config(args.profile)
    root = Path(args.root).resolve(strict=True)
    expected_source = config["sources"][side]
    source_path = root / "src/graph/transport.rs"
    actual_source = sha256_file(source_path)
    if actual_source != expected_source or _git(root, "rev-parse", "HEAD") != config["base_commit"]:
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
        "profile": args.profile,
        "base_commit": config["base_commit"],
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
    result = unpack_frozen_patch(
        args.side, Path(args.archive), Path(args.output), args.profile
    )
    print(json.dumps(result, sort_keys=True))


def choose_profile(args: argparse.Namespace) -> None:
    values = select_profile(
        args.profile,
        Path(args.github_env) if args.github_env else None,
        Path(args.github_output) if args.github_output else None,
    )
    print(json.dumps(values, sort_keys=True))


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    subcommands = parser.add_subparsers(dest="command", required=True)
    profile_parser = subcommands.add_parser("select-profile")
    profile_parser.add_argument("--profile", required=True)
    profile_parser.add_argument("--github-env")
    profile_parser.add_argument("--github-output")
    profile_parser.set_defaults(function=choose_profile)
    freeze_parser = subcommands.add_parser("freeze")
    freeze_parser.add_argument("--profile", default=DEFAULT_PROFILE)
    freeze_parser.add_argument("--baseline-root", required=True)
    freeze_parser.add_argument("--candidate-root", required=True)
    freeze_parser.add_argument("--frozen-harness", required=True)
    freeze_parser.add_argument("--output-dir", required=True)
    freeze_parser.set_defaults(function=freeze)
    unpack_parser = subcommands.add_parser("unpack-patch")
    unpack_parser.add_argument("--profile", default=DEFAULT_PROFILE)
    unpack_parser.add_argument("--side", required=True)
    unpack_parser.add_argument("--archive", required=True)
    unpack_parser.add_argument("--output", required=True)
    unpack_parser.set_defaults(function=unpack_patch)
    receipt_parser = subcommands.add_parser("record-build")
    receipt_parser.add_argument("--profile", default=DEFAULT_PROFILE)
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
