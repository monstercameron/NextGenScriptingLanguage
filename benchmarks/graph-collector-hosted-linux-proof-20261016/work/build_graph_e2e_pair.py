#!/usr/bin/env python3
"""Prepare fixed Linux sources and build receipts for the graph E2E pair.

This helper never runs the ignored benchmark. The frozen paired runner owns the
three warmup / 48 measured-pair execution and evidence validation.
"""
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

CAMPAIGN = Path(__file__).resolve().parent.parent
PACKAGE = CAMPAIGN / "work/graph-transport-e2e-paired-runner"
sys.path.insert(0, str(PACKAGE))
import graph_transport_e2e_pair as pair  # noqa: E402

BASE_COMMIT = "81bf9ba766a775cccf059007b19f5def9b9bfb03"
HARNESS_RELATIVE = "tests/graph_transport_e2e_bench.rs"
HARNESS_SHA256 = "e3a8c29881fe904b6b68b9e4a97485704a919dcac03aff0be2cc1998456b0d1e"
RUNNER_SHA256 = "8d39ec34c761f5a186ea45d8978c9a8a66e0cadf37149d3d4bb8dbcb9cfbd1b8"
RUNNER_TEST_SHA256 = "62f3047bf52fb72c1e49dd526bdad14d6779c7c706f3794be21599429c5f8585"
CANDIDATE_TRANSPORT_SHA256 = "bfc03a7028b2871a4823ed0a130e3b735ca8dc535cac9c12cd39d876174dad9c"
BASELINE_TRANSPORT_SHA256 = "049b2b13a6c82ba0c98f20b57a44e76c420d631815f1b673ba70d7902f24610c"
CANDIDATE_PATCH = {
    "archive": "graph-collector-candidate.patch.xz",
    "archive_sha256": "ac1a15348ab47a7b102eb61be74042f265b8fcebf9b11278f84af1525aedcac0",
    "patch_sha256": "7d411b9b0fc3a8a37fbd589381720016916b6edabec43be06414e198cd39bca4",
}
BUILD_TIMEOUT_SECONDS = 2700
BUILD_JOBS = "4"
BASELINE_ROOT_RELATIVE = "work/graph-optimizations-integrated-81bf9ba"
CANDIDATE_ROOT_RELATIVE = "work/graph-collector-sleep-candidate"
EVIDENCE_RELATIVE = "evidence/graph-transport-e2e"
BUILD_COMMAND = [
    "cargo", "test", "--locked", "--release",
    "--test", "graph_transport_e2e_bench",
    "--test", "graph_runtime",
    "--test", "graph_lifecycles",
    "--test", "graph_result_events",
    "--lib", "--no-run", "--message-format=json",
]
INHERITED_BUILD_ENV_KEYS = ("HOME", "PATH", "TMPDIR", "LANG")
FIXED_BUILD_ENV_KEYS = {
    "CARGO_HOME", "RUSTUP_HOME", "RUSTUP_TOOLCHAIN", "CARGO_BUILD_JOBS",
    "CARGO_INCREMENTAL", "CARGO_TARGET_DIR", "LC_ALL", "TZ",
}
CONFIG_FINGERPRINT_ENV_KEY = "EXECUTE_CARGO_CONFIG_FINGERPRINT_SHA256"
SELECTED_ENV_KEYS = set(INHERITED_BUILD_ENV_KEYS) | FIXED_BUILD_ENV_KEYS | {CONFIG_FINGERPRINT_ENV_KEY}
CONFIG_FILENAMES = ("config", "config.toml")
CARGO_CONFIG_SCHEMA = "execute.graph.transport-e2e.cargo-configuration-fingerprint/1"


class EvidenceError(RuntimeError):
    """A pinned source, build, or receipt did not satisfy its contract."""


def utc_now() -> str:
    return datetime.now(timezone.utc).isoformat(timespec="seconds").replace("+00:00", "Z")


def sha256_bytes(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def write_json(path: Path, value: Any, *, exclusive: bool = False) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    encoded = (json.dumps(value, sort_keys=True, indent=2, allow_nan=False) + "\n").encode("utf-8")
    mode = "xb" if exclusive else "wb"
    with path.open(mode) as stream:
        stream.write(encoded)
        stream.flush()
        os.fsync(stream.fileno())


def _git(root: Path, *args: str, input_bytes: bytes | None = None, timeout: int = 30) -> bytes:
    try:
        result = subprocess.run(
            ["git", "-C", str(root), *args],
            input=input_bytes,
            stdin=subprocess.DEVNULL if input_bytes is None else None,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
            timeout=timeout,
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        raise EvidenceError(f"git command failed in {root}: {error}") from error
    if result.returncode:
        detail = result.stderr.decode("utf-8", errors="replace").strip()
        raise EvidenceError(f"git command failed ({result.returncode}): {detail}")
    return result.stdout


def _patch_bytes() -> bytes:
    archive = PACKAGE / CANDIDATE_PATCH["archive"]
    if not archive.is_file() or sha256_file(archive) != CANDIDATE_PATCH["archive_sha256"]:
        raise EvidenceError("compressed collector patch is missing or has the wrong digest")
    try:
        patch_bytes = lzma.decompress(archive.read_bytes(), format=lzma.FORMAT_XZ)
    except lzma.LZMAError as error:
        raise EvidenceError(f"collector patch is not a valid XZ archive: {error}") from error
    if sha256_bytes(patch_bytes) != CANDIDATE_PATCH["patch_sha256"]:
        raise EvidenceError("decompressed collector patch digest does not match its frozen pin")
    return patch_bytes


def verify_inputs(evidence_root: Path) -> dict[str, Any]:
    inputs = {
        "graph_transport_e2e_pair.py": RUNNER_SHA256,
        "test_graph_transport_e2e_pair.py": RUNNER_TEST_SHA256,
        "graph_transport_e2e_bench.rs": HARNESS_SHA256,
        CANDIDATE_PATCH["archive"]: CANDIDATE_PATCH["archive_sha256"],
    }
    hashes: dict[str, str] = {}
    for name, expected in inputs.items():
        path = PACKAGE / name
        if not path.is_file():
            raise EvidenceError(f"required pinned input is missing: {path}")
        observed = sha256_file(path)
        if observed != expected:
            raise EvidenceError(f"SHA-256 mismatch for {name}: {observed}")
        hashes[name] = observed
    patch_bytes = _patch_bytes()
    harness = (PACKAGE / "graph_transport_e2e_bench.rs").read_bytes()
    if pair.HARNESS_SHA256 != HARNESS_SHA256 or sha256_bytes(harness) != HARNESS_SHA256:
        raise EvidenceError("paired runner harness constant and canonical harness bytes disagree")
    evidence_root = evidence_root.expanduser().absolute()
    evidence_root.mkdir(parents=True, exist_ok=True)
    (evidence_root / "build").mkdir(parents=True, exist_ok=True)
    canonical_harness = evidence_root / "graph_transport_e2e_bench.rs"
    if canonical_harness.exists() or canonical_harness.is_symlink():
        raise EvidenceError(f"refusing to overwrite canonical harness copy: {canonical_harness}")
    canonical_harness.write_bytes(harness)
    receipt = {
        "schema": "execute.graph.transport-e2e.hosted-inputs/1",
        "verified_at": utc_now(),
        "base_commit": BASE_COMMIT,
        "hashes": hashes,
        "decoded_patch_hashes": {CANDIDATE_PATCH["archive"]: sha256_bytes(patch_bytes)},
        "canonical_harness_copy": str(canonical_harness),
        "canonical_harness_sha256": sha256_file(canonical_harness),
        "claim": "static input identity only; no build or timed invocation",
    }
    write_json(evidence_root / "input-verification.json", receipt, exclusive=True)
    return receipt


def _require_clean_base(root: Path, label: str) -> None:
    root = root.resolve(strict=True)
    commit = _git(root, "rev-parse", "HEAD").decode("ascii").strip()
    if commit != BASE_COMMIT:
        raise EvidenceError(f"{label} checkout has {commit}, expected frozen base {BASE_COMMIT}")
    status = _git(root, "status", "--porcelain=v1", "--untracked-files=all").decode("utf-8").strip()
    if status:
        raise EvidenceError(f"{label} checkout must be clean before staging: {status}")


def _apply_patch(root: Path, patch_bytes: bytes, log_path: Path) -> None:
    commands = [
        ["git", "-C", str(root), "apply", "--check", "--whitespace=error"],
        ["git", "-C", str(root), "apply", "--whitespace=error"],
    ]
    log = bytearray()
    for command in commands:
        try:
            result = subprocess.run(
                command,
                input=patch_bytes,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                check=False,
                timeout=60,
            )
        except (OSError, subprocess.TimeoutExpired) as error:
            raise EvidenceError(f"could not apply frozen patch: {error}") from error
        log.extend(("$ " + " ".join(command) + "\n").encode("utf-8"))
        log.extend(result.stdout)
        log.extend(result.stderr)
        if result.returncode:
            log_path.parent.mkdir(parents=True, exist_ok=True)
            log_path.write_bytes(bytes(log))
            raise EvidenceError(f"frozen source patch failed with status {result.returncode}")
    log_path.parent.mkdir(parents=True, exist_ok=True)
    log_path.write_bytes(bytes(log))


def stage_sources(campaign_root: Path, evidence_root: Path) -> dict[str, Any]:
    campaign_root = campaign_root.expanduser().resolve(strict=True)
    evidence_root = evidence_root.expanduser().absolute()
    patch_bytes = _patch_bytes()
    harness = (PACKAGE / "graph_transport_e2e_bench.rs").read_bytes()
    if sha256_bytes(harness) != HARNESS_SHA256:
        raise EvidenceError("canonical harness changed after input verification")
    baseline_root = (campaign_root / BASELINE_ROOT_RELATIVE).resolve(strict=True)
    candidate_root = (campaign_root / CANDIDATE_ROOT_RELATIVE).resolve(strict=True)
    _require_clean_base(baseline_root, "baseline")
    _require_clean_base(candidate_root, "candidate")
    _apply_patch(candidate_root, patch_bytes, evidence_root / "patch-apply-candidate.log")
    for side, root in (("baseline", baseline_root), ("candidate", candidate_root)):
        harness_path = root / HARNESS_RELATIVE
        if harness_path.exists() or harness_path.is_symlink():
            raise EvidenceError(f"refusing to overwrite {side} harness: {harness_path}")
        harness_path.parent.mkdir(parents=True, exist_ok=True)
        harness_path.write_bytes(harness)
        if sha256_file(harness_path) != HARNESS_SHA256:
            raise EvidenceError(f"{side} copied harness digest changed")
    summaries = {
        "baseline": pair._inspect_source(baseline_root, "baseline", harness),
        "candidate": pair._inspect_source(candidate_root, "candidate", harness),
    }
    if summaries["baseline"]["transport_sha256"] != BASELINE_TRANSPORT_SHA256:
        raise EvidenceError("baseline transport bytes differ from the frozen source pin")
    if summaries["candidate"]["transport_sha256"] != CANDIDATE_TRANSPORT_SHA256:
        raise EvidenceError("candidate transport bytes differ from the frozen collector source pin")
    check = subprocess.run(
        ["git", "-C", str(candidate_root), "diff", "--check"],
        stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        check=False, timeout=30,
    )
    if check.returncode:
        raise EvidenceError(
            "candidate git diff --check failed: "
            + check.stdout.decode("utf-8", errors="replace")
            + check.stderr.decode("utf-8", errors="replace")
        )
    base_files = summaries["baseline"]["source_fingerprint"]["files"]
    candidate_files = summaries["candidate"]["source_fingerprint"]["files"]
    if base_files.keys() != candidate_files.keys():
        raise EvidenceError("source arms have different tracked build-input path sets")
    differences = sorted(path for path in base_files if base_files[path] != candidate_files[path])
    if differences != ["src/graph/transport.rs"]:
        raise EvidenceError(f"source arms differ outside the collector transport file: {differences}")
    output = {
        "schema": "execute.graph.transport-e2e.hosted-source-freeze/1",
        "recorded_at": utc_now(),
        "base_commit": BASE_COMMIT,
        "candidate_patch_archive_sha256": CANDIDATE_PATCH["archive_sha256"],
        "candidate_patch_sha256": CANDIDATE_PATCH["patch_sha256"],
        "shared_harness_sha256": HARNESS_SHA256,
        "arms": summaries,
        "allowed_arm_differences": differences,
        "claim": "source-tree identity only; no compilation or timed invocation",
    }
    write_json(evidence_root / "source-freeze.json", output, exclusive=True)
    return output


def _tool_output(command: list[str], env: dict[str, str], timeout: int = 30) -> str:
    try:
        result = subprocess.run(
            command, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            env=env, check=False, timeout=timeout,
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        raise EvidenceError(f"tool query failed for {command!r}: {error}") from error
    if result.returncode:
        raise EvidenceError(f"tool query failed for {command!r}: {result.stderr.decode(errors='replace')}")
    return result.stdout.decode("utf-8", errors="strict")


def _base_build_environment(target_dir: Path, host_environment: dict[str, str] | None = None) -> dict[str, str]:
    """Build with a recorded allowlist, dropping inherited compiler overrides."""
    host = os.environ if host_environment is None else host_environment
    if not host.get("PATH") or not host.get("HOME"):
        raise EvidenceError("controlled build environment requires inherited PATH and HOME")
    home_path = Path(host["HOME"]).expanduser().resolve(strict=True)
    environment = {key: host[key] for key in INHERITED_BUILD_ENV_KEYS if key in host}
    environment.update({
        "HOME": str(home_path),
        "CARGO_HOME": str((home_path / ".cargo").resolve()),
        "RUSTUP_HOME": str((home_path / ".rustup").resolve()),
        "RUSTUP_TOOLCHAIN": "stable",
        "CARGO_BUILD_JOBS": BUILD_JOBS,
        "CARGO_INCREMENTAL": "0",
        "CARGO_TARGET_DIR": str(target_dir.expanduser().absolute()),
        "LC_ALL": "C",
        "TZ": "UTC",
    })
    return dict(sorted(environment.items()))


def selected_build_environment(environment: dict[str, str]) -> dict[str, str]:
    return {key: environment[key] for key in sorted(SELECTED_ENV_KEYS) if key in environment}


def _cargo_config_file_identity(path: Path) -> dict[str, Any]:
    path = path.absolute()
    present = path.exists() or path.is_symlink()
    identity: dict[str, Any] = {
        "path": str(path),
        "present": present,
        "resolved_path": None,
        "sha256": None,
        "symlink_target": None,
    }
    if not present:
        return identity
    if not path.is_file():
        raise EvidenceError(f"Cargo configuration path is present but not a regular file: {path}")
    identity["resolved_path"] = str(path.resolve(strict=True))
    identity["sha256"] = sha256_file(path)
    if path.is_symlink():
        identity["symlink_target"] = os.readlink(path)
    return identity


def _source_ancestor_directories(root: Path, home: Path) -> tuple[list[Path], bool]:
    root = root.resolve(strict=True)
    home = home.resolve(strict=True)
    directories: list[Path] = []
    current = root
    home_is_ancestor = False
    while True:
        directories.append(current)
        if current == home:
            home_is_ancestor = True
        if current == current.parent:
            return directories, home_is_ancestor
        current = current.parent


def cargo_configuration_fingerprint(source_roots: dict[str, Path], environment: dict[str, str]) -> dict[str, Any]:
    """Hash Cargo home and ancestor .cargo config presence/content, never contents."""
    home = Path(environment["HOME"]).expanduser().resolve(strict=True)
    cargo_home = Path(environment["CARGO_HOME"]).expanduser().absolute()
    if not home.is_dir() or not cargo_home.is_dir():
        raise EvidenceError("HOME and CARGO_HOME must exist for Cargo configuration capture")
    cargo_home_files = [
        _cargo_config_file_identity(cargo_home / filename)
        for filename in CONFIG_FILENAMES
    ]
    roots: dict[str, Any] = {}
    for side, root in sorted(source_roots.items()):
        resolved_root = root.resolve(strict=True)
        ancestors, home_is_ancestor = _source_ancestor_directories(resolved_root, home)
        files = [
            _cargo_config_file_identity(directory / ".cargo" / filename)
            for directory in ancestors
            for filename in CONFIG_FILENAMES
        ]
        roots[side] = {
            "root": str(resolved_root),
            "home_is_ancestor": home_is_ancestor,
            "config_files": files,
        }
    body = {
        "schema": CARGO_CONFIG_SCHEMA,
        "cargo_home": {"path": str(cargo_home), "config_files": cargo_home_files},
        "source_roots": roots,
    }
    encoded = json.dumps(body, sort_keys=True, separators=(",", ":"), ensure_ascii=False, allow_nan=False).encode("utf-8")
    return {**body, "sha256": sha256_bytes(encoded)}


def paired_cargo_configuration_sha256(fingerprint: dict[str, Any]) -> str:
    """Return one common tag after checking both roots see identical config bytes."""
    cargo_home = fingerprint["cargo_home"]
    cargo_path = Path(cargo_home["path"])
    cargo_files = [
        {
            "relative_path": os.path.relpath(item["path"], cargo_path),
            "present": item["present"],
            "sha256": item["sha256"],
            "symlink_target": item["symlink_target"],
        }
        for item in cargo_home["config_files"]
    ]
    normalized_roots: dict[str, list[dict[str, Any]]] = {}
    for side, root in sorted(fingerprint["source_roots"].items()):
        root_path = Path(root["root"])
        normalized_roots[side] = [
            {
                "relative_path": os.path.relpath(item["path"], root_path),
                "present": item["present"],
                "sha256": item["sha256"],
                "symlink_target": item["symlink_target"],
            }
            for item in root["config_files"]
        ]
    root_values = list(normalized_roots.values())
    if len(root_values) != 2 or root_values[0] != root_values[1]:
        raise EvidenceError("baseline and candidate Cargo ancestor configuration differ")
    common = {
        "schema": CARGO_CONFIG_SCHEMA,
        "cargo_home_configs": cargo_files,
        "source_ancestor_configs": root_values[0],
    }
    encoded = json.dumps(common, sort_keys=True, separators=(",", ":"), ensure_ascii=False, allow_nan=False).encode("utf-8")
    return sha256_bytes(encoded)


def verify_cargo_configuration_fingerprint(
    expected: dict[str, Any], source_roots: dict[str, Path], environment: dict[str, str], label: str,
) -> None:
    if cargo_configuration_fingerprint(source_roots, environment) != expected:
        raise EvidenceError(f"Cargo configuration inputs changed {label} the sequential build")


def build_environment_receipt_path(evidence_root: Path) -> Path:
    """Use the exact environment receipt namespace frozen in the paired runner."""
    receipt_relative = Path(pair.BUILD_ENVIRONMENT_RELATIVE)
    evidence_relative = Path(EVIDENCE_RELATIVE)
    try:
        relative_to_evidence = receipt_relative.relative_to(evidence_relative)
    except ValueError as error:
        raise EvidenceError("paired runner build-environment path is outside EVIDENCE_RELATIVE") from error
    return evidence_root / relative_to_evidence


def write_build_environment_receipt(evidence_root: Path, metadata: dict[str, Any]) -> Path:
    path = build_environment_receipt_path(evidence_root)
    write_json(path, metadata, exclusive=True)
    return path


def build_metadata(environment: dict[str, str]) -> dict[str, Any]:
    return {
        "rustc_vv": _tool_output(["rustc", "-Vv"], environment),
        "cargo_version": _tool_output(["cargo", "--version"], environment),
        "selected_build_environment": selected_build_environment(environment),
        "captured_before_clean_and_build": True,
    }


def validate_build_metadata_pair(baseline: dict[str, Any], candidate: dict[str, Any]) -> None:
    """Exercise the unchanged runner's real build-receipt validation contract."""
    pair._validate_build_metadata_pair(baseline, candidate, baseline)


def select_test_artifact(
    records: Iterable[dict[str, Any]], root: Path, target_dir: Path,
) -> tuple[dict[str, Any], Path]:
    root = root.resolve(strict=True)
    target_dir = target_dir.resolve(strict=True)
    manifest = (root / "Cargo.toml").resolve(strict=True)
    source = (root / HARNESS_RELATIVE).resolve(strict=True)
    release_deps = (target_dir / "release/deps").resolve(strict=True)
    matches: list[tuple[dict[str, Any], Path]] = []
    for row in records:
        target = row.get("target")
        profile = row.get("profile")
        if not isinstance(target, dict) or not isinstance(profile, dict):
            continue
        try:
            row_manifest = Path(row["manifest_path"]).resolve(strict=True)
            row_source = Path(target["src_path"]).resolve(strict=True)
            raw_executable = row.get("executable")
            executable = Path(raw_executable).resolve(strict=True) if isinstance(raw_executable, str) else None
        except (KeyError, OSError, TypeError, ValueError):
            continue
        kinds = target.get("kind")
        if (
            row.get("reason") == "compiler-artifact"
            and row.get("fresh") is False
            and row_manifest == manifest
            and row_source == source
            and target.get("name") == "graph_transport_e2e_bench"
            and target.get("test") is True
            and isinstance(kinds, list)
            and "test" in kinds
            and profile.get("test") is True
            and profile.get("opt_level") == "3"
            and profile.get("debug_assertions") is False
            and executable is not None
            and executable.parent == release_deps
            and os.access(executable, os.X_OK)
            and isinstance(row.get("filenames"), list)
            and raw_executable in row["filenames"]
        ):
            matches.append((row, executable))
    if len(matches) != 1:
        raise EvidenceError(f"expected one fresh optimized graph E2E test artifact, found {len(matches)}")
    return matches[0]


def _record_tool_versions(environment: dict[str, str], output: Path) -> None:
    python_version = _tool_output([sys.executable, "--version"], environment)
    rustc_vv = _tool_output(["rustc", "-Vv"], environment)
    cargo_version = _tool_output(["cargo", "--version"], environment)
    output.write_text(
        f"python: {python_version}cargo: {cargo_version}rustc:\n{rustc_vv}",
        encoding="utf-8",
    )


def _run_build(command: list[str], root: Path, environment: dict[str, str], stdout_path: Path, stderr_path: Path) -> tuple[int, bool]:
    try:
        with stdout_path.open("xb") as stdout, stderr_path.open("xb") as stderr:
            completed = subprocess.run(
                command, cwd=root, env=environment, stdin=subprocess.DEVNULL,
                stdout=stdout, stderr=stderr, check=False, timeout=BUILD_TIMEOUT_SECONDS,
            )
            return completed.returncode, False
    except subprocess.TimeoutExpired:
        return 124, True
    except OSError as error:
        stderr_path.write_text(f"{type(error).__name__}: {error}\n", encoding="utf-8")
        return 127, False


def build_pair(campaign_root: Path, evidence_root: Path, target_parent: Path) -> dict[str, Any]:
    campaign_root = campaign_root.expanduser().resolve(strict=True)
    evidence_root = evidence_root.expanduser().absolute()
    target_parent = target_parent.expanduser().absolute()
    evidence_root.mkdir(parents=True, exist_ok=True)
    target_parent.mkdir(parents=True, exist_ok=True)
    source_freeze_path = evidence_root / "source-freeze.json"
    if not source_freeze_path.is_file():
        raise EvidenceError("source freeze is missing; stage both clean source trees first")
    freeze = json.loads(source_freeze_path.read_text(encoding="utf-8"))
    if freeze.get("base_commit") != BASE_COMMIT or set(freeze.get("arms", {})) != {"baseline", "candidate"}:
        raise EvidenceError("source freeze does not identify the exact paired source roots")

    roots = {
        "baseline": (campaign_root / BASELINE_ROOT_RELATIVE).resolve(strict=True),
        "candidate": (campaign_root / CANDIDATE_ROOT_RELATIVE).resolve(strict=True),
    }
    target_dirs = {side: (target_parent / f"graph-transport-e2e-{side}").absolute() for side in roots}
    for side, target in target_dirs.items():
        if target.exists() or target.is_symlink():
            raise EvidenceError(f"{side} Cargo target must be fresh: {target}")
    environments = {side: _base_build_environment(target_dirs[side]) for side in roots}
    cargo_config = cargo_configuration_fingerprint(roots, environments["baseline"])
    common_config_sha256 = paired_cargo_configuration_sha256(cargo_config)
    for environment in environments.values():
        environment[CONFIG_FINGERPRINT_ENV_KEY] = common_config_sha256
    metadata: dict[str, dict[str, Any]] = {}
    build_dir = evidence_root / "build"
    build_dir.mkdir(parents=True, exist_ok=True)
    write_json(
        build_dir / "cargo-configuration-fingerprint.json",
        {
            "captured_at": utc_now(),
            "fingerprint": cargo_config,
            "shared_environment_sha256": common_config_sha256,
            "claim": "presence, path, symlink-target, and SHA-256 only; configuration contents are not recorded",
        },
        exclusive=True,
    )
    results: dict[str, Any] = {}
    failure_sides: list[str] = []

    # Record the common toolchain and first arm environment before any build.
    metadata["baseline"] = build_metadata(environments["baseline"])
    if not pair._validated_build_metadata(metadata["baseline"], "baseline").get("captured_before_clean_and_build"):
        raise EvidenceError("baseline metadata was not captured before the build")
    write_build_environment_receipt(evidence_root, metadata["baseline"])
    host_info = {
        "captured_at": utc_now(),
        "python": _tool_output([sys.executable, "--version"], environments["baseline"]),
        "uname": _tool_output(["uname", "-a"], environments["baseline"]),
        "runner_os": os.environ.get("RUNNER_OS", ""),
        "runner_arch": os.environ.get("RUNNER_ARCH", ""),
    }
    write_json(build_dir / "host-info.json", host_info, exclusive=True)

    for side in ("baseline", "candidate"):
        root = roots[side]
        out = evidence_root / f"{side}"
        out.mkdir(parents=True, exist_ok=False)
        target = target_dirs[side]
        environment = environments[side]
        if side == "candidate":
            metadata[side] = build_metadata(environment)
            validate_build_metadata_pair(metadata["baseline"], metadata[side])
        _record_tool_versions(environment, out / "tool-versions-before-build.txt")
        (evidence_root / f"{side}-release-clean-attempt-1.log").write_text(
            f"No cargo clean invoked. This arm uses a dedicated target that did not exist before this build: {target}\n",
            encoding="utf-8",
        )
        fresh = not target.exists() and not target.is_symlink()
        write_json(out / "target-freshness.json", {"path": str(target), "existed_before_build": not fresh, "confirmed_fresh": fresh}, exclusive=True)
        before = pair._inspect_source(root, side, (PACKAGE / "graph_transport_e2e_bench.rs").read_bytes())
        if before != freeze["arms"][side]:
            raise EvidenceError(f"{side} source changed after source freeze")
        verify_cargo_configuration_fingerprint(
            cargo_config, roots, environment, f"before {side} build"
        )
        command = BUILD_COMMAND.copy()
        command_record = {
            "side": side,
            "attempt": 1,
            "cwd": str(root),
            "argv": command,
            "timeout_seconds": BUILD_TIMEOUT_SECONDS,
            "build_metadata": metadata[side],
        }
        write_json(out / "build-command.json", command_record, exclusive=True)
        target.mkdir(parents=True)
        environment["CARGO_TARGET_DIR"] = str(target.resolve())
        exit_status, timed_out = _run_build(
            command, root, environment,
            evidence_root / f"{side}-build-attempt-1.jsonl",
            evidence_root / f"{side}-build-attempt-1.log",
        )
        verify_cargo_configuration_fingerprint(
            cargo_config, roots, environment, f"during {side} build"
        )
        (out / "cargo-exit-status.txt").write_text(f"{exit_status}\n", encoding="utf-8")
        after = pair._inspect_source(root, side, (PACKAGE / "graph_transport_e2e_bench.rs").read_bytes())
        stable = before == after and before == freeze["arms"][side]
        write_json(out / "source-stability.json", {"before": before, "after": after, "stable": stable}, exclusive=True)
        artifact = None
        receipt = None
        error = None
        if exit_status == 0 and stable:
            try:
                rows = []
                with (evidence_root / f"{side}-build-attempt-1.jsonl").open("rb") as stream:
                    for line_number, line in enumerate(stream, 1):
                        if not line.strip():
                            continue
                        try:
                            row = json.loads(line.decode("utf-8"))
                        except (UnicodeDecodeError, json.JSONDecodeError) as parse_error:
                            raise EvidenceError(f"Cargo output line {line_number} is not UTF-8 JSON: {parse_error}") from parse_error
                        if isinstance(row, dict):
                            rows.append(row)
                artifact, compiler_binary = select_test_artifact(rows, root, target)
                saved_binary = evidence_root / f"{side}-test-binary"
                if saved_binary.exists() or saved_binary.is_symlink():
                    raise EvidenceError(f"refusing to overwrite saved binary: {saved_binary}")
                shutil.copy2(compiler_binary, saved_binary)
                saved_binary.chmod(saved_binary.stat().st_mode | 0o111)
                binary_digest = sha256_file(saved_binary)
                listing = subprocess.run(
                    [str(saved_binary), "--list"], stdin=subprocess.DEVNULL,
                    stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=environment,
                    check=False, timeout=30,
                )
                (evidence_root / f"{side}-test-list.txt").write_bytes(listing.stdout)
                (out / "test-list-stderr.log").write_bytes(listing.stderr)
                if listing.returncode != 0 or not any(
                    line.strip() == f"{pair.TEST_NAME}: test" for line in listing.stdout.decode("utf-8", errors="strict").splitlines()
                ):
                    raise EvidenceError(f"{side} binary did not list the frozen ignored E2E test")
                receipt = {
                    "artifact": artifact,
                    "saved_binary": str(saved_binary.resolve(strict=True)),
                    "sha256": binary_digest,
                    "source_sha256": before["transport_sha256"],
                    "source_stable": stable,
                    "attempt": 1,
                    "build_metadata": metadata[side],
                }
                write_json(evidence_root / f"{side}-binary-receipt.json", receipt, exclusive=True)
            except (EvidenceError, OSError, ValueError, KeyError, subprocess.TimeoutExpired) as exception:
                error = f"{type(exception).__name__}: {exception}"
        elif exit_status == 124:
            error = f"Cargo build exceeded {BUILD_TIMEOUT_SECONDS}s; no retry"
        elif exit_status != 0:
            error = f"Cargo build failed with status {exit_status}; no retry"
        elif not stable:
            error = "source tree changed during build"
        result = {
            "side": side,
            "cargo_exit_status": exit_status,
            "timed_out": timed_out,
            "target_directory_fresh": fresh,
            "source_stable": stable,
            "artifact_selected": artifact is not None,
            "receipt_written": receipt is not None,
            "error": error,
        }
        write_json(out / "build-result.json", result, exclusive=True)
        results[side] = result
        if not (exit_status == 0 and stable and receipt is not None):
            failure_sides.append(side)
    summary = {
        "schema": "execute.graph.transport-e2e.hosted-build-pair/1",
        "completed_at": utc_now(),
        "attempt": 1,
        "sides": results,
        "failed_sides": failure_sides,
        "no_retries": True,
    }
    write_json(build_dir / "build-pair-result.json", summary, exclusive=True)
    return summary


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)
    verify = sub.add_parser("verify-inputs")
    verify.add_argument("--evidence-root", required=True)
    verify.set_defaults(function=lambda args: verify_inputs(Path(args.evidence_root)))
    stage = sub.add_parser("stage-sources")
    stage.add_argument("--campaign-root", required=True)
    stage.add_argument("--evidence-root", required=True)
    stage.set_defaults(function=lambda args: stage_sources(Path(args.campaign_root), Path(args.evidence_root)))
    build = sub.add_parser("build-pair")
    build.add_argument("--campaign-root", required=True)
    build.add_argument("--evidence-root", required=True)
    build.add_argument("--target-parent", required=True)
    build.set_defaults(function=lambda args: build_pair(Path(args.campaign_root), Path(args.evidence_root), Path(args.target_parent)))
    return parser


def main(argv: list[str] | None = None) -> int:
    args = _parser().parse_args(argv)
    try:
        value = args.function(args)
        if isinstance(value, dict):
            print(json.dumps(value, sort_keys=True))
        if args.command == "build-pair" and value.get("failed_sides"):
            return 1
        return 0
    except (EvidenceError, OSError, subprocess.TimeoutExpired, KeyError, ValueError) as error:
        print(f"graph E2E hosted preparation: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
