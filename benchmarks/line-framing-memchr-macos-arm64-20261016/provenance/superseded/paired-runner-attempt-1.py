#!/usr/bin/env python3
"""Strict preregistration and paired runner for streaming line framing.

The runner is intentionally separate from Cargo and from the benchmark test.
It validates immutable build receipts and source trees before it derives the
source revision passed to the identical baseline/candidate harness.
"""
from __future__ import annotations

import argparse
from functools import lru_cache
import hashlib
import json
import os
from pathlib import Path
import platform
import signal
import statistics
import subprocess
import sys
import time
from datetime import datetime, timezone
from typing import Any


HERE = Path(__file__).resolve().parent
CAMPAIGN = HERE.parents[2]
WORK = CAMPAIGN / "work"
BASELINE = WORK / "line-framing-baseline-a884"
CANDIDATE = WORK / "line-framing-candidate-a884"
RUN_PLAN_PATH = HERE / "run-plan.json"
BUILD_DRIVER_PATH = HERE / "build_line_framing_pair.py"
BASE_COMMIT = "a884f6561fd43f45e43b25a3cc2233be17ee3944"
BUILD_DRIVER_SHA256 = "91cc1f09f8e58206fe6358dacf922fb6222a7d252b02406e9d0272e451825d79"
PRODUCT_PATCH_SHA256 = "eeb7c5512fd78c0264d6d96a85b6959c8f1302ac481ab1e98047e3fb52145c94"
HARNESS_SHA256 = "7171a2cb59c6749d656ed8eb5202109c17810e22d5e41c4256daeeec926256ad"
HARNESS_NAME = "native_stream_status_bench"
TEST_NAME = "public_native_stream_status_polling"
HARNESS_FORMAT = "execute.native-stream-status-polling.raw/1"
STAGES = (1, 4, 8)
RECORDS = 131_072
MAX_CARGO_JSONL_BYTES = 64 * 1024 * 1024
WARMUP_PAIRS = 3
MEASURED_PAIRS = 48
INNER_WARMUPS = 0
INNER_SAMPLES = 3
TIMEOUT_SECONDS = 120
MAX_REPORT_BYTES = 2_000_000
SOURCE_ROOTS = ("src/", "vendor/", ".cargo/")
SOURCE_CONFIG_FILES = {
    "Cargo.toml", "Cargo.lock", "build.rs", "rust-toolchain",
    "rust-toolchain.toml", "rustfmt.toml",
}
EXPECTED_SOURCE_HASHES = {
    "baseline": {
        "Cargo.lock": "2be0aa74e5bfe3e4897ea9ca9121efce663a94a4df7850958dc2cf511c6f427c",
        "Cargo.toml": "2e2cd88356201ac07b3b8d68be1af9379d7c307bf2c5b67765a13d0f8e040874",
        "src/runtime/streaming.rs": "d7d18b92c9ad0ece03bdef875ca100734f36de5d19de1dad39fadcb63b9bfc3a",
    },
    "candidate": {
        "Cargo.lock": "364ea897bd0db1094c5f235f9431aa10dfcd0fdde317afd6b9da824e84f1dfad",
        "Cargo.toml": "dd1ebd61a09bac5972ca8365e3be618ad4d8326361aff879568f7cae039a2b59",
        "src/runtime/streaming.rs": "5ada8d51fd5e1dea60a70a6e067bb56694ca48d6de1ed10a87e729f0e5f5049f",
    },
}
CARGO_BIN = Path("/Users/earlcameron/Documents/Codex/2026-10-12/you/work/rust/cargo/bin")
CARGO_HOME = Path("/Users/earlcameron/Documents/Codex/2026-10-12/you/work/rust/cargo")
RUSTUP_HOME = Path("/Users/earlcameron/Documents/Codex/2026-10-12/you/work/rust/rustup")
TOOLCHAIN = "stable-aarch64-apple-darwin"
BUILD_JOBS = "4"
CARGO_BUILD_TARGET = "aarch64-apple-darwin"
BUILD_TARGETS = {
    "baseline": ["native_stream_status_bench"],
    "candidate": ["native_stream_status_bench", "line_framing_lib_test"],
}
ARTIFACT_SPECS = {
    "native_stream_status_bench": {
        "target_name": "native_stream_status_bench",
        "kind": "test",
        "crate_type": "bin",
        "source": "tests/native_stream_status_bench.rs",
    },
    "line_framing_lib_test": {
        "target_name": "flow",
        "kind": "lib",
        "crate_type": "lib",
        "source": "src/lib.rs",
    },
}
DIFFERENTIAL_TEST = "runtime::streaming::framing_evidence::borrowed_record_framing_matches_legacy_failures_and_bytes"
INHERITED_BUILD_ENV_KEYS = (
    "PATH", "HOME", "TMPDIR", "LANG", "LC_ALL", "TZ",
    "SDKROOT", "DEVELOPER_DIR", "MACOSX_DEPLOYMENT_TARGET",
)
FIXED_BUILD_ENV = {
    "CARGO_HOME": str(CARGO_HOME),
    "RUSTUP_HOME": str(RUSTUP_HOME),
    "RUSTUP_TOOLCHAIN": TOOLCHAIN,
    "CARGO_BUILD_JOBS": BUILD_JOBS,
    "CARGO_TARGET_DIR": str(WORK / "perf-target-candidate"),
    "CARGO_BUILD_TARGET": CARGO_BUILD_TARGET,
    "RUSTC": str(CARGO_BIN / "rustc"),
}
BUILD_ENVIRONMENT_SCHEMA = "execute.controlled-cargo-build-environment/1"
CONFIG_FINGERPRINT_SCHEMA = "execute.line-framing.cargo-configuration-fingerprint/1"
CONFIG_FILENAMES = ("config", "config.toml")
HOST_IDENTITY_POLICY = {
    "schema": "execute.native-observer-host-identity-policy/1",
    "stable_fields": [
        "platform",
        "uname.system",
        "uname.release",
        "uname.version",
        "uname.machine",
        "uname.processor",
    ],
    "recorded_only_fields": {
        "uname.node": "mutable DNS/network hostname recorded for audit; not a machine identity",
    },
}


class PairError(RuntimeError):
    """Invalid frozen inputs or benchmark contract."""


def utc_now() -> str:
    return datetime.now(timezone.utc).isoformat(timespec="milliseconds").replace("+00:00", "Z")


def host_snapshot() -> dict[str, Any]:
    """Record platform and uname values while excluding the mutable node name from identity."""
    return {
        "platform": platform.platform(),
        "uname": list(platform.uname()),
        "identity_policy": HOST_IDENTITY_POLICY,
    }


def host_identity_matches(expected: Any, observed: Any) -> bool:
    if (
        not isinstance(expected, dict)
        or not isinstance(observed, dict)
        or expected.get("identity_policy") != HOST_IDENTITY_POLICY
        or observed.get("identity_policy") != HOST_IDENTITY_POLICY
        or not isinstance(expected.get("platform"), str)
        or not isinstance(observed.get("platform"), str)
        or expected.get("platform") != observed.get("platform")
    ):
        return False
    expected_uname = expected.get("uname")
    observed_uname = observed.get("uname")
    if (
        not isinstance(expected_uname, list)
        or not isinstance(observed_uname, list)
        or len(expected_uname) != 6
        or len(observed_uname) != 6
        or any(not isinstance(value, str) for value in expected_uname + observed_uname)
    ):
        return False
    # uname_result order is system, node, release, version, machine, processor.
    stable_indices = (0, 2, 3, 4, 5)
    return all(expected_uname[index] == observed_uname[index] for index in stable_indices)


def sha256_bytes(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for block in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


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
        raise PairError(f"Cargo configuration path is present but not a regular file: {path}")
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


def _cargo_configuration_fingerprint(
    source_roots: dict[str, Path], environment: dict[str, str]
) -> dict[str, Any]:
    home = Path(environment["HOME"]).expanduser().resolve(strict=True)
    cargo_home = Path(environment["CARGO_HOME"]).expanduser().absolute()
    if not home.is_dir() or not cargo_home.is_dir():
        raise PairError("HOME and CARGO_HOME must be existing directories for Cargo configuration validation")
    cargo_home_files = [_cargo_config_file_identity(cargo_home / name) for name in CONFIG_FILENAMES]
    roots: dict[str, Any] = {}
    for side, root in sorted(source_roots.items()):
        resolved_root = root.resolve(strict=True)
        ancestors, home_is_ancestor = _source_ancestor_directories(resolved_root, home)
        files = [
            _cargo_config_file_identity(directory / ".cargo" / name)
            for directory in ancestors
            for name in CONFIG_FILENAMES
        ]
        roots[side] = {
            "root": str(resolved_root),
            "home_is_ancestor": home_is_ancestor,
            "config_files": files,
        }
    body = {
        "schema": CONFIG_FINGERPRINT_SCHEMA,
        "cargo_home": {"path": str(cargo_home.resolve(strict=True)), "config_files": cargo_home_files},
        "source_roots": roots,
    }
    digest = sha256_bytes(json.dumps(body, sort_keys=True, separators=(",", ":"), ensure_ascii=False).encode("utf-8"))
    return {**body, "sha256": digest}


def _validate_build_environment(snapshot: Any) -> dict[str, str]:
    if not isinstance(snapshot, dict) or snapshot.get("schema") != BUILD_ENVIRONMENT_SCHEMA:
        raise PairError("pre-clean metadata lacks the controlled Cargo build environment schema")
    if snapshot.get("inherited_allowlist") != list(INHERITED_BUILD_ENV_KEYS):
        raise PairError("pre-clean inherited build environment allowlist differs")
    if snapshot.get("fixed_values") != FIXED_BUILD_ENV or snapshot.get("unlisted_inherited_variables_forwarded") is not False:
        raise PairError("pre-clean fixed build environment policy differs")
    environment = snapshot.get("effective_environment")
    allowed = set(INHERITED_BUILD_ENV_KEYS) | set(FIXED_BUILD_ENV)
    if (
        not isinstance(environment, dict)
        or any(not isinstance(key, str) or not isinstance(value, str) for key, value in environment.items())
        or not set(environment).issubset(allowed)
        or not set(FIXED_BUILD_ENV).issubset(environment)
        or not environment.get("HOME")
        or not environment.get("PATH", "").startswith(str(CARGO_BIN) + os.pathsep)
        or any(environment.get(key) != value for key, value in FIXED_BUILD_ENV.items())
    ):
        raise PairError("pre-clean effective build environment has unexpected, missing, or unfixed values")
    digest = sha256_bytes(json.dumps(environment, sort_keys=True, separators=(",", ":"), ensure_ascii=False).encode("utf-8"))
    if snapshot.get("effective_environment_sha256") != digest:
        raise PairError("pre-clean effective build environment hash differs")
    return environment


def _verify_cargo_configuration(expected: Any, environment: dict[str, str]) -> dict[str, Any]:
    if not isinstance(expected, dict):
        raise PairError("pre-clean metadata lacks Cargo configuration fingerprint")
    body = {key: value for key, value in expected.items() if key != "sha256"}
    digest = sha256_bytes(json.dumps(body, sort_keys=True, separators=(",", ":"), ensure_ascii=False).encode("utf-8"))
    if expected.get("schema") != CONFIG_FINGERPRINT_SCHEMA or expected.get("sha256") != digest:
        raise PairError("Cargo configuration fingerprint schema or digest is invalid")
    current = _cargo_configuration_fingerprint({"baseline": BASELINE, "candidate": CANDIDATE}, environment)
    if current != expected:
        raise PairError("Cargo configuration presence/path/symlink/content hash changed")
    return expected


def _validate_toolchain_metadata(metadata: dict[str, Any]) -> None:
    expected_tools = {
        "cargo_version_verbose": str(CARGO_BIN / "cargo"),
        "rustc_version_verbose": str(CARGO_BIN / "rustc"),
        "rustc_sysroot": str(CARGO_BIN / "rustc"),
        "rustup_active_toolchain": str(CARGO_BIN / "rustup"),
    }
    for key, expected_path in expected_tools.items():
        item = metadata.get(key)
        if (
            not isinstance(item, dict)
            or item.get("executable_path") != expected_path
            or not isinstance(item.get("argv"), list)
            or not item["argv"]
            or item["argv"][0] != expected_path
            or not isinstance(item.get("output"), str)
            or not item["output"].strip()
            or not isinstance(item.get("executable_sha256"), str)
            or len(item["executable_sha256"]) != 64
            or any(char not in "0123456789abcdef" for char in item["executable_sha256"])
        ):
            raise PairError(f"pre-clean metadata lacks a valid fixed-toolchain identity for {key}")
    if not metadata["rustup_active_toolchain"]["output"].startswith(TOOLCHAIN):
        raise PairError("pre-clean active rustup toolchain differs from the fixed toolchain")


def strict_json_bytes(payload: bytes, label: str) -> Any:
    if len(payload) > MAX_REPORT_BYTES:
        raise PairError(f"{label} exceeds {MAX_REPORT_BYTES} bytes")

    def unique_object(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
        result: dict[str, Any] = {}
        for key, value in pairs:
            if key in result:
                raise ValueError(f"duplicate JSON key: {key}")
            result[key] = value
        return result

    def reject_constant(value: str) -> None:
        raise ValueError(f"non-finite JSON constant: {value}")

    try:
        return json.loads(
            payload.decode("utf-8"),
            object_pairs_hook=unique_object,
            parse_constant=reject_constant,
        )
    except (UnicodeDecodeError, json.JSONDecodeError, ValueError) as error:
        raise PairError(f"{label} is not strict UTF-8 JSON: {error}") from error


def read_json(path: Path, label: str) -> Any:
    try:
        return strict_json_bytes(path.expanduser().resolve(strict=True).read_bytes(), label)
    except OSError as error:
        raise PairError(f"cannot read {label}: {error}") from error


def write_json(path: Path, value: Any, *, exclusive: bool = False) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    if exclusive and (path.exists() or path.is_symlink()):
        raise PairError(f"refusing to overwrite {path}")
    payload = json.dumps(value, sort_keys=True, indent=2, allow_nan=False).encode("utf-8") + b"\n"
    flags = os.O_WRONLY | os.O_CREAT | (os.O_EXCL if exclusive else os.O_TRUNC)
    descriptor = os.open(path, flags, 0o600)
    with os.fdopen(descriptor, "wb") as output:
        output.write(payload)
        output.flush()
        os.fsync(output.fileno())


def append_jsonl(path: Path, value: Any) -> None:
    payload = json.dumps(value, sort_keys=True, allow_nan=False).encode("utf-8") + b"\n"
    with path.open("ab", buffering=0) as output:
        output.write(payload)
        os.fsync(output.fileno())


def git(root: Path, *args: str) -> bytes:
    completed = subprocess.run(
        ["git", "-C", str(root), *args],
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        timeout=20,
        check=False,
    )
    if completed.returncode:
        raise PairError(completed.stderr.decode("utf-8", errors="replace"))
    return completed.stdout


def is_source_input(relative: str) -> bool:
    return relative.startswith(SOURCE_ROOTS) or relative in SOURCE_CONFIG_FILES


def source_fingerprint(root: Path) -> dict[str, Any]:
    tracked = git(root, "ls-files", "-z").split(b"\0")
    paths = sorted(
        item.decode("utf-8")
        for item in tracked
        if item and is_source_input(item.decode("utf-8"))
    )
    files: dict[str, str] = {}
    digest = hashlib.sha256()
    for relative in paths:
        source = root / relative
        if not source.is_file():
            raise PairError(f"tracked build input missing: {relative}")
        file_hash = sha256_file(source)
        files[relative] = file_hash
        digest.update(relative.encode("utf-8"))
        digest.update(b"\0")
        digest.update(file_hash.encode("ascii"))
        digest.update(b"\n")
    harness = root / "tests/native_stream_status_bench.rs"
    if not harness.is_file() or sha256_file(harness) != HARNESS_SHA256:
        raise PairError(f"{root} does not contain the pinned benchmark harness")
    files["tests/native_stream_status_bench.rs"] = sha256_file(harness)
    digest.update(b"tests/native_stream_status_bench.rs\0")
    digest.update(files["tests/native_stream_status_bench.rs"].encode("ascii"))
    digest.update(b"\n")
    return {"sha256": digest.hexdigest(), "files": files}


def source_identity(side: str, root: Path) -> dict[str, Any]:
    root = root.expanduser().resolve(strict=True)
    head = git(root, "rev-parse", "HEAD").decode("ascii").strip()
    if head != BASE_COMMIT:
        raise PairError(f"{side} source HEAD changed: {head}")
    expected = EXPECTED_SOURCE_HASHES[side]
    actual = {path: sha256_file(root / path) for path in expected}
    if actual != expected:
        raise PairError(f"{side} pinned source hashes changed: {actual}")
    staged = git(root, "diff", "--cached", "--name-only").decode("utf-8").splitlines()
    changed = git(root, "diff", "--name-only", "HEAD").decode("utf-8").splitlines()
    untracked = [
        item.decode("utf-8")
        for item in git(root, "ls-files", "--others", "--exclude-standard", "-z").split(b"\0")
        if item
    ]
    if staged:
        raise PairError(f"{side} has staged changes: {staged}")
    if side == "baseline":
        if changed or untracked != ["tests/native_stream_status_bench.rs"]:
            raise PairError(f"baseline source tree changed unexpectedly: {changed}, {untracked}")
        product_patch_hash = None
    else:
        patch = git(root, "diff", "--binary", "HEAD", "--", "Cargo.lock", "Cargo.toml", "src/runtime/streaming.rs")
        product_patch_hash = sha256_bytes(patch)
        if product_patch_hash != PRODUCT_PATCH_SHA256:
            raise PairError(f"candidate product patch changed: {product_patch_hash}")
        if changed != ["Cargo.lock", "Cargo.toml", "src/runtime/streaming.rs"] or untracked != ["tests/native_stream_status_bench.rs"]:
            raise PairError(f"candidate source tree changed unexpectedly: {changed}, {untracked}")
    fingerprint = source_fingerprint(root)
    return {
        "root": str(root),
        "commit": head,
        "product_patch_sha256": product_patch_hash,
        "fingerprint": fingerprint,
        "fingerprint_sha256": fingerprint["sha256"],
        "harness_sha256": HARNESS_SHA256,
    }


@lru_cache(maxsize=1)
def expected_fixture() -> tuple[int, str]:
    digest = hashlib.sha256()
    byte_count = 0
    for index in range(RECORDS):
        line = f'{{"n":{index},"value":"record-{index:08}"}}\n'.encode("ascii")
        digest.update(line)
        byte_count += len(line)
    return byte_count, digest.hexdigest()


def expected_report_metadata(source_revision: str, binary: Path, cat_path: Path, cat_hash: str) -> dict[str, Any]:
    input_bytes, input_hash = expected_fixture()
    return {
        "schema": HARNESS_FORMAT,
        "source_revision": source_revision,
        "test_binary": str(binary),
        "test_binary_sha256": sha256_file(binary),
        "stage_binary": str(cat_path),
        "stage_binary_sha256": cat_hash,
        "workload": {
            "api": "runtime::run_stream",
            "stages": list(STAGES),
            "records": RECORDS,
            "input_bytes": input_bytes,
            "input_sha256": input_hash,
            "expected_status": "all native stages exit successfully",
            "setup_outside_timing": "temporary JSONL fixture, program parsing, registry/options construction, and input/output hash setup",
            "timed_scope": "public run_stream call, native launches/pipeline/readiness/status supervision, and synchronous record callback hashing/counting",
        },
        "warmups_per_case": INNER_WARMUPS,
        "samples_per_case": INNER_SAMPLES,
        "interleaving_stage_order_by_round": [[1, 4, 8], [4, 8, 1], [8, 1, 4]],
    }


def expected_paired_plan() -> str:
    return (
        "Build baseline and candidate from the same harness bytes into fresh immutable test binaries; "
        "run 0 warmups and 3 measured rounds per binary, with stage-count order rotated as recorded. "
        "Pair baseline/candidate in preregistered AB/BA order per case on the same host; retain every "
        "sample, failure, timeout, and report without retries."
    )


def _cargo_artifact_path(value: Any, label: str, target_root: Path) -> Path:
    """Normalize a Cargo-reported path without requiring cargo output to survive clean."""
    if not isinstance(value, str) or not value:
        raise PairError(f"{label} must be a path string")
    raw = Path(value).expanduser()
    if not raw.is_absolute():
        raise PairError(f"{label} must be absolute: {value}")
    try:
        resolved = raw.resolve(strict=False)
        allowed_root = target_root.resolve(strict=False)
    except (OSError, RuntimeError) as error:
        raise PairError(f"{label} cannot be normalized: {value}: {error}") from error
    if not resolved.is_relative_to(allowed_root):
        raise PairError(f"{label} is outside the pinned Cargo target tree: {resolved}")
    return resolved


def _validate_cargo_artifact(artifact: dict[str, Any], root: Path, target_dir: Path, target_key: str) -> Path:
    target = artifact.get("target")
    profile = artifact.get("profile")
    if not isinstance(target, dict) or not isinstance(profile, dict):
        raise PairError("line-framing Cargo artifact lacks target/profile metadata")
    spec = ARTIFACT_SPECS[target_key]
    try:
        manifest = Path(str(artifact["manifest_path"])).resolve(strict=True)
        source = Path(str(target["src_path"])).resolve(strict=True)
    except (KeyError, OSError, RuntimeError, TypeError, ValueError) as error:
        raise PairError(f"line-framing Cargo source paths are invalid: {error}") from error
    expected_root = root.resolve(strict=True)
    package_id = artifact.get("package_id")
    if (
        artifact.get("reason") != "compiler-artifact"
        or artifact.get("fresh") is not False
        or target.get("name") != spec["target_name"]
        or target.get("test") is not True
        or not isinstance(target.get("kind"), list)
        or spec["kind"] not in target["kind"]
        or not isinstance(target.get("crate_types"), list)
        or spec["crate_type"] not in target["crate_types"]
        or profile.get("test") is not True
        or profile.get("opt_level") != "3"
        or profile.get("debug_assertions") is not False
        or manifest != expected_root / "Cargo.toml"
        or source != expected_root / spec["source"]
        or not isinstance(package_id, str)
        or not package_id.startswith(f"path+{expected_root.as_uri()}#execute@")
    ):
        raise PairError(f"line-framing Cargo artifact {target_key} differs from the pinned source/profile/fresh contract")
    target_root = target_dir.resolve(strict=False) / CARGO_BUILD_TARGET / "release" / "deps"
    executable = _cargo_artifact_path(artifact.get("executable"), f"line-framing {target_key} executable", target_root)
    filenames = artifact.get("filenames")
    if not isinstance(filenames, list) or executable not in [
        _cargo_artifact_path(value, "line-framing artifact filename", target_root) for value in filenames
    ]:
        raise PairError(f"line-framing {target_key} executable is absent from Cargo artifact filenames")
    return executable


def _validate_cargo_artifact_log(path: Path, expected: dict[str, Any], target_key: str) -> None:
    try:
        payload = path.read_bytes()
    except OSError as error:
        raise PairError(f"cannot read line-framing Cargo JSONL: {error}") from error
    if len(payload) > MAX_CARGO_JSONL_BYTES:
        raise PairError("line-framing Cargo JSONL exceeds the frozen size limit")
    matches = []
    for number, line in enumerate(payload.splitlines(), 1):
        if not line:
            continue
        row = strict_json_bytes(line, f"line-framing Cargo JSONL line {number}")
        if not isinstance(row, dict):
            raise PairError(f"line-framing Cargo JSONL line {number} is not an object")
        target = row.get("target")
        profile = row.get("profile")
        if (
            row.get("reason") == "compiler-artifact"
            and isinstance(target, dict)
            and target.get("name") == ARTIFACT_SPECS[target_key]["target_name"]
            and isinstance(profile, dict)
            and profile.get("test") is True
        ):
            matches.append(row)
    if len(matches) != 1 or matches[0] != expected:
        raise PairError(f"line-framing Cargo JSONL artifact {target_key} does not exactly match its build receipt")


def _validate_saved_binary_receipt(item: dict[str, Any], side: str, build_dir: Path, cargo_executable: Path, target_key: str) -> tuple[Path, str]:
    try:
        saved_lexical = Path(item["saved_binary"]).expanduser()
        build_root = build_dir.resolve(strict=True)
        expected_lexical = build_root / f"{side}-{target_key}-test-binary"
        saved = saved_lexical.resolve(strict=True)
        expected = expected_lexical.resolve(strict=True)
    except (KeyError, OSError, RuntimeError, TypeError, ValueError) as error:
        raise PairError(f"{side}/{target_key} saved binary receipt path is invalid: {error}") from error
    if (
        not saved_lexical.is_absolute()
        or saved_lexical != expected_lexical
        or saved != expected
        or not saved.is_relative_to(build_root)
        or saved == cargo_executable
        or not saved.is_file()
        or not os.access(saved, os.X_OK)
    ):
        raise PairError(f"{side}/{target_key} saved binary is not the prescribed executable evidence copy")
    target_root = (Path(FIXED_BUILD_ENV["CARGO_TARGET_DIR"]).resolve(strict=False)
                  / CARGO_BUILD_TARGET / "release" / "deps")
    if saved.is_relative_to(target_root):
        raise PairError(f"{side}/{target_key} saved binary aliases the shared Cargo target tree")
    saved_hash = item.get("saved_binary_sha256")
    if (
        not isinstance(saved_hash, str)
        or len(saved_hash) != 64
        or any(character not in "0123456789abcdef" for character in saved_hash)
        or sha256_file(saved) != saved_hash
    ):
        raise PairError(f"{side}/{target_key} saved binary hash differs from its exact receipt")
    return saved, saved_hash


def pair_schedule() -> list[tuple[int, str, tuple[str, str]]]:
    schedule = []
    for pair_index in range(WARMUP_PAIRS + MEASURED_PAIRS):
        phase = "warmup" if pair_index < WARMUP_PAIRS else "measured"
        order = ("baseline", "candidate") if pair_index % 2 == 0 else ("candidate", "baseline")
        schedule.append((pair_index, phase, order))
    return schedule


def validate_raw_report(payload: bytes, identity: dict[str, Any], expected_revision: str) -> dict[str, Any]:
    report = strict_json_bytes(payload, "native stream report")
    if not isinstance(report, dict):
        raise PairError("raw report is not an object")
    expected = expected_report_metadata(
        expected_revision,
        Path(identity["binary"]),
        Path(identity["stage_binary"]),
        identity["stage_binary_sha256"],
    )
    allowed = set(expected) | {"warmup_samples", "raw_samples", "paired_plan"}
    if set(report) != allowed:
        raise PairError(f"raw report top-level fields differ: extra={sorted(set(report)-allowed)}, missing={sorted(allowed-set(report))}")
    for key, value in expected.items():
        if json.dumps(report.get(key), sort_keys=True, separators=(",", ":")) != json.dumps(value, sort_keys=True, separators=(",", ":")):
            raise PairError(f"raw report field {key!r} differs from frozen contract")
    if report["paired_plan"] != expected_paired_plan():
        raise PairError("raw report paired_plan differs from frozen contract")
    if report["warmup_samples"] != []:
        raise PairError("inner harness warmup report must be empty")
    samples = report["raw_samples"]
    if not isinstance(samples, list) or len(samples) != len(STAGES) * INNER_SAMPLES:
        raise PairError("raw report must contain exactly nine measured samples")
    expected_sequence = [(round_index, stage) for round_index, order in enumerate(((1, 4, 8), (4, 8, 1), (8, 1, 4))) for stage in order]
    sample_fields = {
        "round", "stages", "elapsed_ns", "result_records", "callback_records",
        "launched_processes", "bytes", "output_sha256", "successful_statuses",
        "result_counts_match", "launch_count_matches", "callback_counts_match",
        "output_matches", "sample_valid", "error",
    }
    by_stage: dict[int, list[dict[str, Any]]] = {stage: [] for stage in STAGES}
    input_bytes, input_hash = expected_fixture()
    for sample, (round_index, stage) in zip(samples, expected_sequence):
        if not isinstance(sample, dict) or set(sample) != sample_fields:
            raise PairError("raw sample has unexpected fields")
        if type(sample["round"]) is not int or sample["round"] != round_index:
            raise PairError("raw sample round index/order mismatch")
        if type(sample["stages"]) is not int or sample["stages"] != stage:
            raise PairError("raw sample stage order mismatch")
        duration = sample["elapsed_ns"]
        if type(duration) is not int or not 0 < duration <= (1 << 64) - 1:
            raise PairError("raw sample elapsed_ns is invalid")
        exact_oracle = {
            "result_records": RECORDS,
            "callback_records": RECORDS,
            "launched_processes": stage,
            "bytes": input_bytes,
            "output_sha256": input_hash,
            "successful_statuses": True,
            "result_counts_match": True,
            "launch_count_matches": True,
            "callback_counts_match": True,
            "output_matches": True,
            "sample_valid": True,
            "error": None,
        }
        for key, value in exact_oracle.items():
            if sample.get(key) != value or type(sample.get(key)) is not type(value):
                raise PairError(f"sample {round_index}/{stage} oracle field {key!r} failed")
        by_stage[stage].append(sample)
    return {
        "source_revision": report["source_revision"],
        "workload": report["workload"],
        "interleaving_stage_order_by_round": report["interleaving_stage_order_by_round"],
        "raw_samples": samples,
        "samples_by_stage": {str(stage): by_stage[stage] for stage in STAGES},
        "median_elapsed_ns_by_stage": {
            str(stage): int(statistics.median(sample["elapsed_ns"] for sample in by_stage[stage]))
            for stage in STAGES
        },
    }


def _build_receipt_identity(side: str, receipt_path: Path, build_dir: Path, metadata: dict[str, Any], pair: dict[str, Any]) -> dict[str, Any]:
    receipt_path = receipt_path.expanduser().resolve(strict=True)
    receipt = read_json(receipt_path, f"{side} build receipt")
    if not isinstance(receipt, dict) or receipt.get("schema") != "execute.line-framing-build-receipt/1":
        raise PairError(f"{side} receipt schema is unexpected")
    if receipt.get("status") != "complete_build_only" or receipt.get("side") != side:
        raise PairError(f"{side} receipt is incomplete or mislabeled")
    if receipt.get("commit") != BASE_COMMIT or receipt.get("build_driver_sha256") != BUILD_DRIVER_SHA256:
        raise PairError(f"{side} receipt has incorrect commit or build-driver identity")
    if receipt.get("harness_sha256") != HARNESS_SHA256:
        raise PairError(f"{side} receipt harness hash differs")
    expected_patch = PRODUCT_PATCH_SHA256 if side == "candidate" else None
    if receipt.get("tracked_product_patch_sha256") != expected_patch:
        raise PairError(f"{side} receipt product patch identity differs")
    if receipt.get("test_execution") != "not run" or receipt.get("benchmark_timing") != "not run":
        raise PairError(f"{side} build receipt unexpectedly records execution/timing")
    if receipt.get("source_sha256_before") != receipt.get("source_sha256_after"):
        raise PairError(f"{side} source identity changed during its build")
    expected_source_map = EXPECTED_SOURCE_HASHES[side] | {"tests/native_stream_status_bench.rs": HARNESS_SHA256}
    if receipt.get("source_sha256_after") != expected_source_map:
        raise PairError(f"{side} receipt source hash map differs from pinned source files")
    build_environment = metadata.get("build_environment")
    cargo_configuration = metadata.get("cargo_configuration_before_build")
    if not isinstance(build_environment, dict) or not isinstance(cargo_configuration, dict):
        raise PairError("pre-clean metadata lacks controlled environment or Cargo configuration")
    if receipt.get("effective_build_environment_sha256") != build_environment.get("effective_environment_sha256"):
        raise PairError(f"{side} receipt build environment hash differs from pre-clean metadata")
    configuration_checks = receipt.get("cargo_configuration_checks")
    required_config_keys = {
        "before_clean", "after_clean", "before_build", "before_each_target", "after_each_target", "after_build"
    }
    if not isinstance(configuration_checks, dict) or set(configuration_checks) != required_config_keys:
        raise PairError(f"{side} receipt lacks before/after Cargo configuration checks")
    if any(configuration_checks.get(key) != cargo_configuration for key in ("before_clean", "after_clean", "before_build", "after_build")):
        raise PairError(f"{side} Cargo configuration changed during clean/build")
    target_keys = set(BUILD_TARGETS[side])
    before_each = configuration_checks.get("before_each_target")
    after_each = configuration_checks.get("after_each_target")
    if not isinstance(before_each, dict) or set(before_each) != target_keys or not isinstance(after_each, dict) or set(after_each) != target_keys:
        raise PairError(f"{side} Cargo configuration checks do not bracket the required targets")
    if any(snapshot != cargo_configuration for snapshot in [*before_each.values(), *after_each.values()]):
        raise PairError(f"{side} Cargo configuration changed during or between target builds")

    artifacts = receipt.get("artifacts")
    commands = receipt.get("commands")
    if not isinstance(artifacts, list) or not isinstance(commands, list):
        raise PairError(f"{side} receipt lacks artifact or command arrays")
    if {item.get("target") for item in artifacts if isinstance(item, dict)} != target_keys:
        raise PairError(f"{side} artifact set differs from required targets")
    target_dir = Path(pair["target_directory"]).resolve(strict=True)
    root = Path(metadata["worktrees"][side]).resolve(strict=True)
    validated: dict[str, dict[str, Any]] = {}
    for target_key in BUILD_TARGETS[side]:
        spec = ARTIFACT_SPECS[target_key]
        matches = [item for item in artifacts if isinstance(item, dict) and item.get("target") == target_key]
        if len(matches) != 1:
            raise PairError(f"{side} must have exactly one {target_key} artifact")
        item = matches[0]
        if item.get("target_spec") != spec:
            raise PairError(f"{side}/{target_key} target specification differs")
        artifact = item.get("artifact")
        if not isinstance(artifact, dict) or not isinstance(artifact.get("target"), dict) or not isinstance(artifact.get("profile"), dict):
            raise PairError(f"{side}/{target_key} artifact metadata is malformed")
        target = artifact["target"]
        profile = artifact["profile"]
        if (
            artifact.get("reason") != "compiler-artifact"
            or target.get("name") != spec["target_name"]
            or target.get("test") is not True
            or spec["kind"] not in target.get("kind", [])
            or spec["crate_type"] not in target.get("crate_types", [])
            or artifact.get("fresh") is not False
            or profile.get("test") is not True
            or profile.get("opt_level") != "3"
            or profile.get("debug_assertions") is not False
            or item.get("fresh") is not False
            or item.get("opt_level") != "3"
            or item.get("debug_assertions") is not False
        ):
            raise PairError(f"{side}/{target_key} is not a freshly compiled optimized release test")
        try:
            manifest = Path(artifact["manifest_path"]).resolve(strict=True)
            source_path = Path(target["src_path"]).resolve(strict=True)
        except (KeyError, OSError, TypeError, ValueError) as error:
            raise PairError(f"{side}/{target_key} artifact paths are invalid: {error}") from error
        if manifest != root / "Cargo.toml" or source_path != root / spec["source"]:
            raise PairError(f"{side}/{target_key} artifact paths do not match the pinned worktree/source")
        phase = "build_test_target" if spec["kind"] == "test" else "build_library_test_target"
        command_matches = [
            command for command in commands
            if isinstance(command, dict) and command.get("phase") == phase and command.get("target") == target_key
        ]
        if len(command_matches) != 1 or command_matches[0].get("exit_code") != 0:
            raise PairError(f"{side} receipt lacks exactly one successful {target_key} build command")
        command = command_matches[0]
        stdout_log = Path(command.get("stdout_jsonl", "")).resolve(strict=True)
        stderr_log = Path(command.get("stderr", "")).resolve(strict=True)
        if not stdout_log.is_relative_to(build_dir) or not stderr_log.is_relative_to(build_dir):
            raise PairError(f"{side}/{target_key} compiler logs escaped build evidence")
        if sha256_file(stdout_log) != item.get("cargo_stdout_jsonl_sha256") or sha256_file(stderr_log) != item.get("cargo_stderr_sha256"):
            raise PairError(f"{side}/{target_key} compiler log hashes differ from receipt")
        executable = _validate_cargo_artifact(artifact, root, target_dir, target_key)
        _validate_cargo_artifact_log(stdout_log, artifact, target_key)
        saved, saved_hash = _validate_saved_binary_receipt(item, side, build_dir, executable, target_key)
        validated[target_key] = {
            "binary": str(saved),
            "binary_sha256": saved_hash,
            "cargo_executable": str(executable),
            "cargo_artifact_fresh": artifact["fresh"],
            "opt_level": profile["opt_level"],
            "debug_assertions": profile["debug_assertions"],
        }
    source = source_identity(side, root)
    return {
        "side": side,
        "receipt_path": str(receipt_path),
        "receipt_sha256": sha256_file(receipt_path),
        "build_receipt_content": receipt,
        "root": str(root),
        "source": source,
        **validated[HARNESS_NAME],
        "lib_test": validated.get("line_framing_lib_test"),
        "build_driver_sha256": receipt["build_driver_sha256"],
        "effective_build_environment": build_environment["effective_environment"],
        "effective_build_environment_sha256": build_environment["effective_environment_sha256"],
        "cargo_configuration_before_build": cargo_configuration,
    }

def prepare(args: argparse.Namespace) -> None:
    build_dir = Path(args.build_dir).expanduser().resolve(strict=True)
    expected_build_root = (HERE.parent / "build").resolve(strict=True)
    if not build_dir.is_relative_to(expected_build_root):
        raise PairError(f"build evidence must remain under {expected_build_root}")
    if platform.system() != "Darwin" or platform.machine() not in {"arm64", "aarch64"}:
        raise PairError("line-framing pair is pinned to macOS arm64")
    plan = read_json(RUN_PLAN_PATH, "run plan")
    if (
        plan.get("schema") != "execute.line-framing-pair-plan/1"
        or plan.get("base_commit") != BASE_COMMIT
        or plan.get("candidate_product_patch_sha256") != PRODUCT_PATCH_SHA256
        or plan.get("benchmark_harness_sha256") != HARNESS_SHA256
        or plan.get("paired_runner_sha256") != sha256_file(Path(__file__).resolve())
        or plan.get("offline_test_file_sha256") != sha256_file(HERE / "test_line_framing_pair.py")
        or plan.get("worktrees") != {"baseline": str(BASELINE), "candidate": str(CANDIDATE)}
    ):
        raise PairError("pinned run-plan identity differs")
    build_workflow = plan.get("build_only_workflow")
    expected_environment_policy = {
        "schema": BUILD_ENVIRONMENT_SCHEMA,
        "inherited_allowlist": list(INHERITED_BUILD_ENV_KEYS),
        "fixed_values": FIXED_BUILD_ENV,
        "actual_effective_values_recorded": True,
        "unlisted_inherited_variables_forwarded": False,
        "cargo_config_capture": "SHA-256 presence/path/symlink-target/content identities for config and config.toml in CARGO_HOME and each .cargo directory from both source roots through filesystem root, recorded before and after every arm build and immediately before and after each target build, then checked unchanged",
    }
    if (
        not isinstance(build_workflow, dict)
        or build_workflow.get("script") != str(BUILD_DRIVER_PATH)
        or build_workflow.get("script_sha256") != BUILD_DRIVER_SHA256
        or build_workflow.get("target_directory_shared_by_arms") != str(Path(FIXED_BUILD_ENV["CARGO_TARGET_DIR"]))
        or build_workflow.get("build_target") != CARGO_BUILD_TARGET
        or build_workflow.get("build_environment_provenance") != expected_environment_policy
    ):
        raise PairError("run-plan build environment provenance contract differs")
    if sha256_file(BUILD_DRIVER_PATH) != BUILD_DRIVER_SHA256:
        raise PairError("build-driver source hash differs from plan")
    metadata_path = build_dir / "preclean-tool-metadata.json"
    pair_path = build_dir / "build-pair-receipt.json"
    metadata = read_json(metadata_path, "pre-clean build metadata")
    pair = read_json(pair_path, "build-pair receipt")
    _validate_toolchain_metadata(metadata)
    if metadata.get("build_driver_sha256") != BUILD_DRIVER_SHA256:
        raise PairError("pre-clean metadata does not bind the pinned build driver")
    effective_build_environment = _validate_build_environment(metadata.get("build_environment"))
    cargo_configuration = _verify_cargo_configuration(
        metadata.get("cargo_configuration_before_build"), effective_build_environment
    )
    if Path(metadata.get("target_directory_is_shared_across_arms", "")).resolve(strict=True) != Path(FIXED_BUILD_ENV["CARGO_TARGET_DIR"]).resolve(strict=True):
        raise PairError("pre-clean target directory differs from the controlled build environment")
    if metadata.get("baseline_commit") != BASE_COMMIT or metadata.get("candidate_commit") != BASE_COMMIT:
        raise PairError("pre-clean metadata commit pins differ")
    if metadata.get("candidate_product_patch_sha256") != PRODUCT_PATCH_SHA256 or metadata.get("benchmark_harness_sha256") != HARNESS_SHA256:
        raise PairError("pre-clean metadata source/harness pins differ")
    if pair.get("schema") != "execute.line-framing-build-pair/1" or pair.get("status") != "complete_build_only":
        raise PairError("build-pair receipt is not complete")
    for key, expected in {
        "base_commit": BASE_COMMIT,
        "candidate_product_patch_sha256": PRODUCT_PATCH_SHA256,
        "benchmark_harness_sha256": HARNESS_SHA256,
        "build_driver_sha256": BUILD_DRIVER_SHA256,
    }.items():
        if pair.get(key) != expected:
            raise PairError(f"build-pair receipt {key} differs")
    if (
        pair.get("effective_build_environment_sha256") != metadata["build_environment"]["effective_environment_sha256"]
        or pair.get("cargo_configuration_before_build_sha256") != cargo_configuration["sha256"]
        or pair.get("cargo_configuration_after_build") != cargo_configuration
    ):
        raise PairError("build-pair receipt does not show stable controlled environment/configuration provenance")
    if Path(pair.get("target_directory", "")).resolve(strict=True) != Path(metadata.get("target_directory_is_shared_across_arms", "")).resolve(strict=True):
        raise PairError("pair/pre-clean target-directory provenance differs")
    if Path(pair.get("baseline_receipt", "")).resolve(strict=True) != (build_dir / "baseline-build-receipt.json").resolve(strict=True):
        raise PairError("pair receipt baseline path differs from build directory")
    if Path(pair.get("candidate_receipt", "")).resolve(strict=True) != (build_dir / "candidate-build-receipt.json").resolve(strict=True):
        raise PairError("pair receipt candidate path differs from build directory")
    metadata_with_worktrees = dict(metadata)
    metadata_with_worktrees["worktrees"] = plan["worktrees"]
    baseline = _build_receipt_identity("baseline", build_dir / "baseline-build-receipt.json", build_dir, metadata_with_worktrees, pair)
    candidate = _build_receipt_identity("candidate", build_dir / "candidate-build-receipt.json", build_dir, metadata_with_worktrees, pair)
    if pair.get("baseline_receipt_sha256") != baseline["receipt_sha256"] or pair.get("candidate_receipt_sha256") != candidate["receipt_sha256"]:
        raise PairError("build-pair receipt arm digest differs from immutable build receipt")
    if baseline["root"] == candidate["root"] or baseline["binary"] == candidate["binary"]:
        raise PairError("baseline/candidate roots and saved binary paths must be distinct")
    if baseline["binary_sha256"] == candidate["binary_sha256"]:
        raise PairError("baseline/candidate executable hashes unexpectedly match")
    if candidate["lib_test"] is None or candidate["lib_test"]["binary"] == candidate["binary"]:
        raise PairError("candidate differential-test and benchmark binaries must be distinct")
    if baseline["source"]["fingerprint_sha256"] == candidate["source"]["fingerprint_sha256"]:
        raise PairError("baseline/candidate tracked source fingerprints unexpectedly match")
    cat_path = Path("/bin/cat").resolve(strict=True)
    cat_hash = sha256_file(cat_path)
    temp_root = Path(os.environ.get("TMPDIR") or "/tmp").expanduser().resolve(strict=True)
    if not temp_root.is_dir():
        raise PairError(f"runtime TMPDIR is not a directory: {temp_root}")
    prereg = {
            "schema": "execute.line-framing-pair-preregistration/1",
        "driver": {"path": str(Path(__file__).resolve()), "sha256": sha256_file(Path(__file__).resolve())},
        "protocol": {
            "warmup_pairs": WARMUP_PAIRS,
            "measured_pairs": MEASURED_PAIRS,
            "pair_order": "alternating AB/BA over 51 total pairs; warmups are AB,BA,AB and the first measured pair is BA; measured set is balanced 24 AB/24 BA",
            "inner_harness_warmups": INNER_WARMUPS,
            "inner_harness_samples": INNER_SAMPLES,
            "stage_cases": list(STAGES),
            "stage_order_by_inner_round": [[1, 4, 8], [4, 8, 1], [8, 1, 4]],
            "outer_invocation_timeout_seconds": TIMEOUT_SECONDS,
            "retries": 0,
            "loss_policy": "retain every timeout, nonzero exit, missing or malformed report, invalid sample, and incomplete pair; never retry or filter a loss",
            "metric": "per case and pair, median of all three raw elapsed_ns samples; candidate minus baseline (positive is slower); preserve every raw duration and pair difference",
            "comparison_scope": "same-host paired public run_stream calls from separately saved fresh release integration-test binaries",
        },
        "workload": {
            "api": "runtime::run_stream",
            "records_per_case_sample": RECORDS,
            "fixture_bytes": expected_fixture()[0],
            "fixture_sha256": expected_fixture()[1],
            "program": "one /bin/cat stage, four /bin/cat stages, or eight /bin/cat stages",
            "timed_scope": "public run_stream call, native launches/pipeline/readiness/status supervision, and synchronous record callback hashing/counting",
            "setup_outside_timing": "temporary JSONL fixture, program parsing, registry/options construction, and input/output hash setup",
        },
        "inputs": {
            "run_plan": {"path": str(RUN_PLAN_PATH), "sha256": sha256_file(RUN_PLAN_PATH)},
            "build_driver": {"path": str(BUILD_DRIVER_PATH), "sha256": sha256_file(BUILD_DRIVER_PATH)},
            "paired_runner": {"path": str(Path(__file__).resolve()), "sha256": sha256_file(Path(__file__).resolve())},
            "build_directory": str(build_dir),
            "preclean_metadata": {"path": str(metadata_path), "sha256": sha256_file(metadata_path)},
            "build_pair_receipt": {"path": str(pair_path), "sha256": sha256_file(pair_path)},
            "build_tool_metadata": metadata,
            "build_pair_receipt_content": pair,
            "candidate_product_patch": {"path": str(HERE.parent / "line-framing-memchr.patch"), "sha256": PRODUCT_PATCH_SHA256},
            "harness_sha256": HARNESS_SHA256,
            "baseline": baseline,
            "candidate": candidate,
            "stage_binary": {"path": str(cat_path), "sha256": cat_hash},
            "host": {**host_snapshot(), "python": sys.version},
            "runtime_environment": {
                "PATH": os.defpath,
                "TMPDIR": str(temp_root),
                "LC_ALL": "C",
                "TZ": "UTC",
            },
        },
        "prepared_at": utc_now(),
    }
    destination = Path(args.output).expanduser().absolute()
    write_json(destination, prereg, exclusive=True)
    print(json.dumps({"preregistration": str(destination), "sha256": sha256_file(destination)}, sort_keys=True))


def verify_runtime_identity(prereg: dict[str, Any]) -> None:
    inputs = prereg["inputs"]
    host = inputs.get("host")
    if not host_identity_matches(host, host_snapshot()):
        raise PairError("benchmark host differs from the preregistered host identity")
    runtime_environment = inputs.get("runtime_environment")
    if (
        not isinstance(runtime_environment, dict)
        or set(runtime_environment) != {"PATH", "TMPDIR", "LC_ALL", "TZ"}
        or runtime_environment.get("PATH") != os.defpath
        or not isinstance(runtime_environment.get("TMPDIR"), str)
        or runtime_environment.get("LC_ALL") != "C"
        or runtime_environment.get("TZ") != "UTC"
    ):
        raise PairError("runtime environment differs from preregistered controlled environment")
    temp_root = Path(runtime_environment["TMPDIR"]).resolve(strict=True)
    if not temp_root.is_dir():
        raise PairError("preregistered TMPDIR is no longer available")
    for key, expected_path_key in (("run_plan", "run-plan.json"), ("build_driver", "build_line_framing_pair.py")):
        identity = inputs[key]
        path = Path(identity["path"]).resolve(strict=True)
        if path.name != expected_path_key or sha256_file(path) != identity["sha256"]:
            raise PairError(f"frozen {key} identity changed")
    for key in ("preclean_metadata", "build_pair_receipt"):
        identity = inputs[key]
        path = Path(identity["path"]).resolve(strict=True)
        if sha256_file(path) != identity["sha256"]:
            raise PairError(f"frozen {key} changed")
    runner = inputs.get("paired_runner")
    if not isinstance(runner, dict):
        raise PairError("preregistration lacks paired runner identity")
    runner_path = Path(runner["path"]).resolve(strict=True)
    if runner_path != Path(__file__).resolve(strict=True) or sha256_file(runner_path) != runner["sha256"]:
        raise PairError("paired runner source changed since preregistration")
    patch_path = Path(inputs["candidate_product_patch"]["path"]).resolve(strict=True)
    if sha256_file(patch_path) != PRODUCT_PATCH_SHA256:
        raise PairError("candidate product patch changed")
    cat_path = Path(inputs["stage_binary"]["path"]).resolve(strict=True)
    if sha256_file(cat_path) != inputs["stage_binary"]["sha256"]:
        raise PairError("stage executable changed")
    for side in ("baseline", "candidate"):
        info = inputs[side]
        source = source_identity(side, Path(info["root"]))
        if source != info["source"]:
            raise PairError(f"{side} source identity changed since preregistration")
        binary = Path(info["binary"]).resolve(strict=True)
        if not os.access(binary, os.X_OK) or sha256_file(binary) != info["binary_sha256"]:
            raise PairError(f"{side} saved binary changed since preregistration")
        if side == "candidate":
            lib_test = info.get("lib_test")
            if not isinstance(lib_test, dict):
                raise PairError("candidate library-test binary receipt is missing")
            lib_binary = Path(lib_test["binary"]).resolve(strict=True)
            if not os.access(lib_binary, os.X_OK) or sha256_file(lib_binary) != lib_test.get("binary_sha256"):
                raise PairError("candidate saved library-test binary changed since preregistration")
        receipt = Path(info["receipt_path"]).resolve(strict=True)
        if sha256_file(receipt) != info["receipt_sha256"]:
            raise PairError(f"{side} build receipt changed since preregistration")


def arm_source_revision(side: str, prereg: dict[str, Any]) -> str:
    source = prereg["inputs"][side]["source"]
    return f"{side}:{source['commit']}:{source['fingerprint_sha256']}:{source['product_patch_sha256'] or 'clean'}"


def terminate_process_group(process: subprocess.Popen[bytes]) -> str | None:
    error = None
    try:
        os.killpg(process.pid, signal.SIGTERM)
    except ProcessLookupError:
        return None
    except OSError as caught:
        error = f"SIGTERM: {type(caught).__name__}: {caught}"
    try:
        process.communicate(timeout=1.0)
        return error
    except subprocess.TimeoutExpired:
        pass
    try:
        os.killpg(process.pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    except OSError as caught:
        error = f"{error}; SIGKILL: {type(caught).__name__}: {caught}" if error else f"SIGKILL: {type(caught).__name__}: {caught}"
    try:
        process.communicate(timeout=5.0)
    except subprocess.TimeoutExpired:
        error = f"{error}; process group did not close pipes" if error else "process group did not close pipes"
    return error


def differential_test_output_passes(stdout: bytes) -> bool:
    text = stdout.decode("utf-8", errors="replace")
    return (
        "running 1 test" in text
        and f"test {DIFFERENTIAL_TEST} ... ok" in text
        and "1 passed; 0 failed" in text
    )


def run_attempt(side: str, pair_index: int, phase: str, arm_position: int, prereg: dict[str, Any], run_dir: Path, journal: Path) -> dict[str, Any]:
    identity = prereg["inputs"][side]
    attempt_id = f"{phase}-{pair_index:03d}-{side}"
    attempt_dir = run_dir / "attempts" / attempt_id
    attempt_dir.mkdir(parents=True, exist_ok=False)
    raw_path = attempt_dir / "report.json"
    binary = Path(identity["binary"]).resolve(strict=True)
    binary_before = sha256_file(binary)
    source_revision = arm_source_revision(side, prereg)
    command = [str(binary), "--exact", TEST_NAME, "--ignored", "--nocapture", "--test-threads=1"]
    controlled = {
        "OPT_BENCH_WARMUPS": str(INNER_WARMUPS),
        "OPT_BENCH_SAMPLES": str(INNER_SAMPLES),
        "OPT_BENCH_REPORT": str(raw_path.resolve()),
        "OPT_BENCH_SOURCE_REV": source_revision,
    }
    env = dict(prereg["inputs"]["runtime_environment"])
    env.update(controlled)
    metadata = {
        "schema": "execute.line-framing-attempt/1",
        "attempt_id": attempt_id,
        "phase": phase,
        "pair_index": pair_index,
        "arm_position": arm_position,
        "side": side,
        "source_revision": source_revision,
        "source_identity": identity["source"],
        "binary": str(binary),
        "binary_sha256_expected": identity["binary_sha256"],
        "binary_sha256_before": binary_before,
        "command": command,
        "timeout_seconds": TIMEOUT_SECONDS,
        "raw_report_path": str(raw_path.resolve()),
        "environment": env,
        "started_at": utc_now(),
    }
    write_json(attempt_dir / "started.json", metadata, exclusive=True)
    append_jsonl(journal, {"event": "started", **metadata})
    start = time.monotonic_ns()
    timed_out = False
    spawn_error = None
    signal_error = None
    returncode: int | None = None
    stdout = b""
    stderr = b""
    process: subprocess.Popen[bytes] | None = None
    try:
        verify_runtime_identity(prereg)
        identity_before_error = None
    except Exception as error:
        identity_before_error = f"{type(error).__name__}: {error}"
    if identity_before_error:
        spawn_error = "frozen input identity failed before spawn"
    elif binary_before != identity["binary_sha256"]:
        spawn_error = "saved binary hash mismatch before spawn"
    else:
        try:
            process = subprocess.Popen(
                command,
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                env=env,
                start_new_session=True,
            )
            try:
                stdout, stderr = process.communicate(timeout=TIMEOUT_SECONDS)
            except subprocess.TimeoutExpired:
                timed_out = True
                signal_error = terminate_process_group(process)
                try:
                    stdout, stderr = process.communicate(timeout=5.0)
                except subprocess.TimeoutExpired as stuck:
                    spawn_error = "child process pipes remained open after process-group termination"
                    stdout = stuck.output or b""
                    stderr = stuck.stderr or b""
                    try:
                        os.killpg(process.pid, signal.SIGKILL)
                    except OSError:
                        pass
                    process.kill()
                    try:
                        more_stdout, more_stderr = process.communicate(timeout=2.0)
                        stdout, stderr = more_stdout, more_stderr
                    except subprocess.TimeoutExpired as still_stuck:
                        stdout = still_stuck.output or stdout
                        stderr = still_stuck.stderr or stderr
                        if process.stdout is not None:
                            process.stdout.close()
                        if process.stderr is not None:
                            process.stderr.close()
                        try:
                            process.wait(timeout=1.0)
                        except subprocess.TimeoutExpired:
                            spawn_error += "; process did not exit after forced termination"
            returncode = process.returncode
        except Exception as error:
            spawn_error = f"{type(error).__name__}: {error}"
            stderr = spawn_error.encode("utf-8", errors="replace")
            if process is not None and process.poll() is None:
                signal_error = terminate_process_group(process)
                try:
                    stdout, stderr = process.communicate(timeout=3.0)
                except subprocess.TimeoutExpired:
                    try:
                        os.killpg(process.pid, signal.SIGKILL)
                    except OSError:
                        pass
                        try:
                            process.kill()
                        except OSError:
                            pass
                    if process.stdout is not None:
                        process.stdout.close()
                    if process.stderr is not None:
                        process.stderr.close()
                    try:
                        process.wait(timeout=1.0)
                    except subprocess.TimeoutExpired:
                        spawn_error += "; process did not exit after forced termination"
    if process is not None:
        returncode = process.poll()
    elapsed = time.monotonic_ns() - start
    binary_after = sha256_file(binary) if binary.is_file() else None
    try:
        verify_runtime_identity(prereg)
        identity_error = None
    except Exception as error:  # retain this attempt, then stop schedule on input drift
        identity_error = f"{type(error).__name__}: {error}"
    (attempt_dir / "stdout.bin").write_bytes(stdout)
    (attempt_dir / "stderr.bin").write_bytes(stderr)
    report_payload = raw_path.read_bytes() if raw_path.is_file() else None
    report_sha = sha256_bytes(report_payload) if report_payload is not None else None
    parsed = None
    raw_document = None
    validation_error = None
    if report_payload is not None:
        try:
            raw_document = strict_json_bytes(report_payload, "native stream report")
            parsed = validate_raw_report(
                report_payload,
                {
                    "binary": str(binary),
                    "stage_binary": prereg["inputs"]["stage_binary"]["path"],
                    "stage_binary_sha256": prereg["inputs"]["stage_binary"]["sha256"],
                },
                source_revision,
            )
        except Exception as error:
            validation_error = f"{type(error).__name__}: {error}"
    losses = []
    if timed_out:
        losses.append("timeout")
    if returncode != 0:
        losses.append("nonzero_exit_or_no_exit")
    if spawn_error:
        losses.append("spawn_or_termination_error")
    if identity_before_error:
        losses.append("input_identity_drift_before_invocation")
    if identity_error:
        losses.append("input_identity_drift")
    if report_payload is None:
        losses.append("missing_report")
    if validation_error:
        losses.append("invalid_report_or_oracle")
    if binary_after != identity["binary_sha256"]:
        losses.append("binary_changed")
    result = {
        **metadata,
        "finished_at": utc_now(),
        "returncode": returncode,
        "timed_out": timed_out,
        "elapsed_wall_ns": elapsed,
        "signal_error": signal_error,
        "spawn_error": spawn_error,
        "input_identity_before_error": identity_before_error,
        "input_identity_error": identity_error,
        "binary_sha256_after": binary_after,
        "stdout_path": str((attempt_dir / "stdout.bin").resolve()),
        "stdout_sha256": sha256_bytes(stdout),
        "stderr_path": str((attempt_dir / "stderr.bin").resolve()),
        "stderr_sha256": sha256_bytes(stderr),
        "report_present": report_payload is not None,
        "report_path": str(raw_path.resolve()),
        "report_sha256": report_sha,
        "report_byte_length": len(report_payload) if report_payload is not None else None,
        "raw_report_document": raw_document,
        "validation_error": validation_error,
        "validated": parsed is not None and returncode == 0 and not timed_out and identity_error is None,
        "losses": losses,
        "validated_report": parsed,
    }
    write_json(attempt_dir / "finished.json", result, exclusive=True)
    append_jsonl(journal, {"event": "finished", "attempt_id": attempt_id, "finished_at": result["finished_at"], "returncode": returncode, "timed_out": timed_out, "losses": losses, "report_sha256": report_sha})
    return result


def pair_summary(pair_index: int, phase: str, order: tuple[str, str], attempts: dict[str, dict[str, Any]]) -> dict[str, Any]:
    base = attempts.get("baseline", {})
    cand = attempts.get("candidate", {})
    def all_raw_samples(attempt: dict[str, Any]) -> list[dict[str, Any]] | None:
        parsed = attempt.get("validated_report")
        if parsed is not None:
            return parsed["raw_samples"]
        raw = attempt.get("raw_report_document")
        samples = raw.get("raw_samples") if isinstance(raw, dict) else None
        return samples if isinstance(samples, list) else None

    baseline_raw = all_raw_samples(base)
    candidate_raw = all_raw_samples(cand)
    per_case = {}
    for stage in STAGES:
        base_result = base.get("validated_report")
        cand_result = cand.get("validated_report")
        base_samples = base_result["samples_by_stage"][str(stage)] if base_result else None
        cand_samples = cand_result["samples_by_stage"][str(stage)] if cand_result else None
        base_median = base_result["median_elapsed_ns_by_stage"][str(stage)] if base_result else None
        cand_median = cand_result["median_elapsed_ns_by_stage"][str(stage)] if cand_result else None
        base_durations = [row["elapsed_ns"] for row in base_samples] if base_samples else None
        cand_durations = [row["elapsed_ns"] for row in cand_samples] if cand_samples else None
        same_oracle = False
        if base_samples and cand_samples:
            same_oracle = all(
                left[key] == right[key]
                for left, right in zip(base_samples, cand_samples)
                for key in ("result_records", "callback_records", "launched_processes", "bytes", "output_sha256", "successful_statuses", "sample_valid")
            )
        per_case[str(stage)] = {
            "baseline_raw_samples": [sample for sample in baseline_raw if isinstance(sample, dict) and sample.get("stages") == stage] if baseline_raw is not None else None,
            "candidate_raw_samples": [sample for sample in candidate_raw if isinstance(sample, dict) and sample.get("stages") == stage] if candidate_raw is not None else None,
            "baseline_raw_elapsed_ns": base_durations,
            "candidate_raw_elapsed_ns": cand_durations,
            "baseline_median_ns": base_median,
            "candidate_median_ns": cand_median,
            "candidate_minus_baseline_ns": cand_median - base_median if base_median is not None and cand_median is not None else None,
            "cross_arm_sample_oracles_equal": same_oracle,
            "effect_valid": bool(base.get("validated") and cand.get("validated") and same_oracle),
        }
    return {
        "pair_index": pair_index,
        "phase": phase,
        "arm_order": list(order),
        "attempt_ids": {side: attempts.get(side, {}).get("attempt_id") for side in ("baseline", "candidate")},
        "attempts_valid": {side: bool(attempts.get(side, {}).get("validated")) for side in ("baseline", "candidate")},
        "losses_by_side": {side: attempts.get(side, {}).get("losses", ["attempt_not_run"]) for side in ("baseline", "candidate")},
        "arm_reports": {
            side: {
                "attempt_id": attempts.get(side, {}).get("attempt_id"),
                "source_revision": attempts.get(side, {}).get("source_revision"),
                "binary_sha256_expected": attempts.get(side, {}).get("binary_sha256_expected"),
                "report_sha256": attempts.get(side, {}).get("report_sha256"),
                "report_path": attempts.get(side, {}).get("report_path"),
                "report_byte_length": attempts.get(side, {}).get("report_byte_length"),
                "returncode": attempts.get(side, {}).get("returncode"),
                "timed_out": attempts.get(side, {}).get("timed_out"),
                "elapsed_wall_ns": attempts.get(side, {}).get("elapsed_wall_ns"),
                "stdout_sha256": attempts.get(side, {}).get("stdout_sha256"),
                "stderr_sha256": attempts.get(side, {}).get("stderr_sha256"),
                "validation_error": attempts.get(side, {}).get("validation_error"),
                "spawn_error": attempts.get(side, {}).get("spawn_error"),
                "input_identity_before_error": attempts.get(side, {}).get("input_identity_before_error"),
                "input_identity_error": attempts.get(side, {}).get("input_identity_error"),
                "raw_samples": (
                    all_raw_samples(attempts.get(side, {}))
                ),
            }
            for side in ("baseline", "candidate")
        },
        "per_case": per_case,
    }


def run_candidate_framing_regression(prereg: dict[str, Any], run_dir: Path, journal: Path) -> dict[str, Any]:
    identity = prereg["inputs"]["candidate"].get("lib_test")
    if not isinstance(identity, dict):
        raise PairError("candidate library-test binary is unavailable for the LineSink differential regression")
    binary = Path(identity["binary"]).resolve(strict=True)
    attempt_dir = run_dir / "preflight"
    attempt_dir.mkdir(parents=True, exist_ok=False)
    stdout_path = attempt_dir / "candidate-line-framing-differential.stdout"
    stderr_path = attempt_dir / "candidate-line-framing-differential.stderr"
    command = [str(binary), "--exact", DIFFERENTIAL_TEST, "--nocapture", "--test-threads=1"]
    result: dict[str, Any] = {
        "schema": "execute.line-framing-differential-preflight/1",
        "test": DIFFERENTIAL_TEST,
        "binary": str(binary),
        "binary_sha256": identity["binary_sha256"],
        "command": command,
        "timeout_seconds": TIMEOUT_SECONDS,
        "started_at": utc_now(),
    }
    append_jsonl(journal, {"event": "candidate_differential_preflight_started", **result})
    process: subprocess.Popen[bytes] | None = None
    timed_out = False
    termination_error = None
    spawn_error = None
    stdout = b""
    stderr = b""
    returncode: int | None = None
    try:
        verify_runtime_identity(prereg)
        if sha256_file(binary) != identity["binary_sha256"]:
            raise PairError("candidate library-test binary hash changed before differential preflight")
        process = subprocess.Popen(
            command,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env=dict(prereg["inputs"]["runtime_environment"]),
            start_new_session=True,
        )
        try:
            stdout, stderr = process.communicate(timeout=TIMEOUT_SECONDS)
            returncode = process.returncode
        except subprocess.TimeoutExpired as error:
            timed_out = True
            termination_error = terminate_process_group(process)
            try:
                stdout, stderr = process.communicate(timeout=5.0)
                returncode = process.returncode
            except subprocess.TimeoutExpired as stuck:
                spawn_error = "candidate differential-test pipes remained open after termination"
                stdout = stuck.output or error.output or b""
                stderr = stuck.stderr or error.stderr or b""
    except Exception as error:
        spawn_error = f"{type(error).__name__}: {error}"
        if process is not None and process.poll() is None:
            termination_error = terminate_process_group(process)
            try:
                stdout, stderr = process.communicate(timeout=5.0)
                returncode = process.returncode
            except subprocess.TimeoutExpired:
                spawn_error += "; child remained after process-group termination"
    for path, payload in ((stdout_path, stdout), (stderr_path, stderr)):
        with path.open("xb") as output:
            output.write(payload)
            output.flush()
            os.fsync(output.fileno())
    one_expected_test_ran = differential_test_output_passes(stdout)
    result.update({
        "finished_at": utc_now(),
        "returncode": returncode,
        "timed_out": timed_out,
        "termination_error": termination_error,
        "spawn_error": spawn_error,
        "stdout_path": str(stdout_path.resolve()),
        "stdout_sha256": sha256_bytes(stdout),
        "stdout_bytes": len(stdout),
        "stderr_path": str(stderr_path.resolve()),
        "stderr_sha256": sha256_bytes(stderr),
        "stderr_bytes": len(stderr),
        "one_expected_test_ran": one_expected_test_ran,
        "passed": returncode == 0 and not timed_out and spawn_error is None and termination_error is None and one_expected_test_ran,
    })
    write_json(attempt_dir / "result.json", result, exclusive=True)
    append_jsonl(journal, {"event": "candidate_differential_preflight_finished", **result})
    return result


def run(args: argparse.Namespace) -> None:
    prereg_path = Path(args.preregistration).expanduser().resolve(strict=True)
    prereg = read_json(prereg_path, "preregistration")
    if prereg.get("schema") != "execute.line-framing-pair-preregistration/1":
        raise PairError("unknown preregistration schema")
    driver = Path(__file__).resolve(strict=True)
    if prereg.get("driver") != {"path": str(driver), "sha256": sha256_file(driver)}:
        raise PairError("paired runner source differs from preregistration")
    verify_runtime_identity(prereg)
    run_dir = Path(args.output_dir).expanduser().absolute()
    run_dir.mkdir(parents=True, exist_ok=False)
    journal = run_dir / "journal.jsonl"
    journal.touch(exist_ok=False)
    append_jsonl(journal, {
        "event": "run_started",
        "at": utc_now(),
        "preregistration": str(prereg_path),
        "preregistration_sha256": sha256_file(prereg_path),
        "host": {**host_snapshot(), "python": sys.version},
    })
    preflight = run_candidate_framing_regression(prereg, run_dir, journal)
    schedule = pair_schedule()
    total = len(schedule)
    completed_pairs = []
    integrity_abort = None if preflight["passed"] else "candidate LineSink differential preflight failed; timing schedule skipped"
    for pair_index, phase, order in (schedule if preflight["passed"] else []):
        attempts: dict[str, dict[str, Any]] = {}
        for position, side in enumerate(order):
            try:
                attempt = run_attempt(side, pair_index, phase, position, prereg, run_dir, journal)
                attempts[side] = attempt
                if attempt.get("input_identity_error"):
                    integrity_abort = attempt["input_identity_error"]
                    break
            except Exception as error:
                # Do not hide runner failures: create a durable loss record and continue
                # the paired schedule unless the frozen inputs no longer validate.
                loss = {
                    "schema": "execute.line-framing-runner-loss/1",
                    "pair_index": pair_index,
                    "phase": phase,
                    "arm_position": position,
                    "side": side,
                    "error": f"{type(error).__name__}: {error}",
                    "at": utc_now(),
                }
                append_jsonl(journal, {"event": "runner_loss", **loss})
                attempts[side] = {**loss, "attempt_id": None, "validated": False, "losses": ["runner_error"]}
                try:
                    verify_runtime_identity(prereg)
                except Exception as identity_error:
                    integrity_abort = f"{type(identity_error).__name__}: {identity_error}"
                    break
        completed_pairs.append(pair_summary(pair_index, phase, order, attempts))
        append_jsonl(journal, {"event": "pair_finished", "pair": completed_pairs[-1], "at": utc_now()})
        if integrity_abort:
            break
    final = {
        "schema": "execute.line-framing-pair-results/1",
        "status": "complete" if integrity_abort is None and len(completed_pairs) == total else "incomplete_integrity_abort",
        "preregistration": str(prereg_path),
        "preregistration_sha256": sha256_file(prereg_path),
        "runner": {"path": str(driver), "sha256": sha256_file(driver)},
        "protocol": prereg["protocol"],
        "candidate_framing_preflight": preflight,
        "inputs": prereg["inputs"],
        "pair_count_planned": total,
        "pair_count_recorded": len(completed_pairs),
        "attempt_count_planned": total * 2,
        "attempt_count_recorded": sum(1 for pair in completed_pairs for side in ("baseline", "candidate") if pair["attempt_ids"][side] is not None),
        "integrity_abort": integrity_abort,
        "losses": [
            {"pair_index": pair["pair_index"], "phase": pair["phase"], "side": side, "valid": pair["attempts_valid"][side]}
            for pair in completed_pairs
            for side in ("baseline", "candidate")
            if not pair["attempts_valid"][side]
        ],
        "pairs": completed_pairs,
        "journal": str(journal.resolve()),
        "finished_at": utc_now(),
    }
    write_json(run_dir / "results.json", final, exclusive=True)
    append_jsonl(journal, {"event": "run_finished", "at": final["finished_at"], "status": final["status"], "pair_count_recorded": len(completed_pairs), "integrity_abort": integrity_abort})
    print(json.dumps({"results": str((run_dir / 'results.json').resolve()), "status": final["status"], "pairs": len(completed_pairs), "integrity_abort": integrity_abort}, sort_keys=True), flush=True)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)
    prepare_parser = subparsers.add_parser("prepare", help="validate build receipts and save an immutable preregistration")
    prepare_parser.add_argument("--build-dir", required=True)
    prepare_parser.add_argument("--output", required=True)
    run_parser = subparsers.add_parser("run", help="execute the frozen paired schedule; do not retry losses")
    run_parser.add_argument("--preregistration", required=True)
    run_parser.add_argument("--output-dir", required=True)
    args = parser.parse_args()
    try:
        if args.command == "prepare":
            prepare(args)
        else:
            run(args)
    except (PairError, OSError, KeyError, TypeError, ValueError) as error:
        parser.exit(2, f"line-framing pair: {type(error).__name__}: {error}\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
