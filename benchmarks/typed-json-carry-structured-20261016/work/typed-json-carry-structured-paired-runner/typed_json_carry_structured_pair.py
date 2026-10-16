#!/usr/bin/env python3
"""Fixed-schedule paired runner for typed JSON carry's structured-output path.

The runner never builds. It validates full source maps and immutable release
test binaries, then runs exactly three warmup pairs and 48 AB/BA measurement
pairs without retries. Every invocation's status, streams, raw report, and
identity are retained in a new absolute evidence directory.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import stat
import statistics
import subprocess
import sys
import time
from datetime import datetime, timezone
from typing import Any, Mapping, Sequence

FORMAT_VERSION = 1
CASE = "structured_csv_parse_1000"
TEST_NAME = CASE
TARGET_NAME = "typed_json_carry_structured"
HARNESS_RELATIVE = "tests/typed_json_carry_structured.rs"
BASE_COMMIT = "e5fa5162b9bdca608f7f3c9d0d273db40a93c17b"
PRODUCT_PATCH_SHA256 = "99650ce4d9c2a2d4c8418e52bc3dbbda2f99a2a745e54c48332f9dc6a1958bb9"
HARNESS_SOURCE_SHA256 = "bac676768a6bf6e748f3cd0590c200ccaa58e1afe712ab99679f35ff29d5ddf3"
HARNESS_PATCH_SHA256 = "732a1926e71033ce5d73ed070e99a58c6a60db2a2b9ea316f5f65fd84a44b2d0"
SOURCE_BINDINGS_SHA256 = "dc9fc282efbcf2cb791eb2e1fba5a5e4b72d2c638bd97ef0eee5e9c300a04dd6"
BUILD_DRIVER_SHA256 = "72b67e06ca7d658bf01b5166f2d6312cdbd6c21e62ae982f4b97b23ccc05ebaa"
EMPTY_DIFF_SHA256 = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
WARMUP_PAIRS = 3
MEASURED_PAIRS = 48
INNER_WARMUPS = 0
INNER_SAMPLES = 3
INVOCATION_TIMEOUT_SECONDS = 120
SHA256_RE = re.compile(r"^[0-9a-f]{64}$")
EXPECTED_METHOD = (
    "Public runtime::run API; 1,000 uncached in-process csv.parse calls over one fixed 64-row, four-column CSV input. "
    "The language requires distinct result names, so calls bind table0000 through table0999 and all 1,000 tables remain "
    "live; the final table is encoded as compact JSON. Includes repeated in-process parsing, typed output validation/carry, "
    "binding retention, and runtime result construction; excludes registry/program/fixture setup, hashing, report writing, "
    "and RunResult destruction. No child processes."
)

C = Path(__file__).resolve().parents[2]
CAMPAIGN = C
EVIDENCE = C / "evidence/typed-json-carry-structured"
SOURCE_BINDINGS = EVIDENCE / "source-fingerprints.json"
BUILD_DRIVER = C / "work/build_typed_json_carry_structured_pair.py"
BUILD_RUN = EVIDENCE / "build-attempt-1"
BASELINE_ROOT = C / "work/typed-json-carry-structured-baseline-e5fa516"
CANDIDATE_ROOT = C / "work/typed-json-carry-structured-candidate-e5fa516"
TARGET = C / "work/perf-target-candidate"
PINNED_CARGO_BIN = Path("/Users/earlcameron/Documents/Codex/2026-10-12/you/work/rust/cargo/bin")
PINNED_CARGO_HOME = Path("/Users/earlcameron/Documents/Codex/2026-10-12/you/work/rust/cargo")
PINNED_RUSTUP_HOME = Path("/Users/earlcameron/Documents/Codex/2026-10-12/you/work/rust/rustup")
INHERITED_BUILD_ENV_KEYS = (
    "PATH", "HOME", "TMPDIR", "LANG", "LC_ALL", "TZ", "SDKROOT", "DEVELOPER_DIR",
    "MACOSX_DEPLOYMENT_TARGET",
)
FIXED_BUILD_ENVIRONMENT = {
    "CARGO_HOME": str(PINNED_CARGO_HOME),
    "RUSTUP_HOME": str(PINNED_RUSTUP_HOME),
    "RUSTUP_TOOLCHAIN": "stable-aarch64-apple-darwin",
    "CARGO_TARGET_DIR": str(TARGET.resolve()),
    "CARGO_BUILD_JOBS": "4",
}
RUNTIME_ENVIRONMENT_BASE = {"LC_ALL": "C", "TZ": "UTC"}
CLEAN_ARGS = ["cargo", "clean", "--release", "-p", "execute"]
BUILD_ARGS = [
    "cargo", "test", "--locked", "--release", "-p", "execute",
    "--test", TARGET_NAME, "--no-run", "--message-format=json",
]
CARGO_CONFIG_FILENAMES = ("config", "config.toml")
CARGO_CONFIG_SCHEMA = "execute.graph.cargo_configuration_fingerprint/1"


class PairError(RuntimeError):
    """A frozen-input, build-provenance, or benchmark-contract failure."""


def utc_now() -> str:
    return datetime.now(timezone.utc).isoformat(timespec="milliseconds").replace("+00:00", "Z")


def sha_bytes(payload: bytes) -> str:
    return hashlib.sha256(payload).hexdigest()


def sha_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def canonical_json_sha256(value: Any) -> str:
    payload = json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=False, allow_nan=False).encode()
    return sha_bytes(payload)


def python_identity() -> dict[str, Any]:
    return {
        "executable": sys.executable,
        "version": sys.version,
        "flags": repr(sys.flags),
        "optimize": sys.flags.optimize,
    }


def runtime_environment_contract() -> dict[str, Any]:
    invocation_template = {**RUNTIME_ENVIRONMENT_BASE, "OPT_BENCH_REPORT": "<fresh-absolute-path>"}
    return {
        "base": RUNTIME_ENVIRONMENT_BASE,
        "base_sha256": canonical_json_sha256(RUNTIME_ENVIRONMENT_BASE),
        "invocation_template": invocation_template,
        "invocation_template_sha256": canonical_json_sha256(invocation_template),
        "policy": "child receives exactly LC_ALL=C, TZ=UTC and a fresh absolute OPT_BENCH_REPORT; no host environment is inherited",
    }


def invocation_environment(raw_path: Path) -> tuple[dict[str, str], str]:
    _expect(raw_path.is_absolute(), "raw report path must be absolute")
    environment = {**RUNTIME_ENVIRONMENT_BASE, "OPT_BENCH_REPORT": str(raw_path)}
    return environment, canonical_json_sha256(environment)


def _unique_object(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    output: dict[str, Any] = {}
    for key, value in pairs:
        if key in output:
            raise PairError(f"duplicate JSON object key {key!r}")
        output[key] = value
    return output


def _reject_constant(value: str) -> None:
    raise PairError(f"non-finite JSON constant {value!r} is forbidden")


def strict_json_bytes(payload: bytes, label: str) -> Any:
    try:
        return json.loads(payload.decode("utf-8", errors="strict"), object_pairs_hook=_unique_object, parse_constant=_reject_constant)
    except (UnicodeDecodeError, json.JSONDecodeError, PairError) as error:
        raise PairError(f"{label} is not strict UTF-8 JSON: {error}") from error


def load_json(path: Path, label: str) -> Any:
    return strict_json_bytes(path.read_bytes(), label)


def write_json(path: Path, value: Any, *, exclusive: bool = False) -> None:
    payload = (json.dumps(value, indent=2, sort_keys=True, ensure_ascii=False, allow_nan=False) + "\n").encode()
    path.parent.mkdir(parents=True, exist_ok=True)
    mode = "xb" if exclusive else "wb"
    with path.open(mode) as stream:
        stream.write(payload)
        stream.flush()
        os.fsync(stream.fileno())


def git(root: Path, *arguments: str) -> bytes:
    completed = subprocess.run(["git", *arguments], cwd=root, check=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    return completed.stdout


def listed_paths(root: Path) -> list[str]:
    tracked = git(root, "ls-files", "-z").split(b"\0")
    untracked = git(root, "ls-files", "--others", "--exclude-standard", "-z").split(b"\0")
    return sorted({os.fsdecode(item) for item in tracked + untracked if item})


def source_fingerprint(root: Path) -> dict[str, Any]:
    files: dict[str, str] = {}
    for relative in listed_paths(root):
        path = root / relative
        content = b"symlink\0" + os.fsencode(os.readlink(path)) if path.is_symlink() else path.read_bytes()
        files[relative] = sha_bytes(content)
    canonical = json.dumps(files, sort_keys=True, separators=(",", ":")).encode()
    return {"sha256": sha_bytes(canonical), "file_count": len(files), "files": files}


def diff_sha256(root: Path, *, staged: bool = False) -> str:
    args = ["diff", "--binary"] + (["--cached"] if staged else [])
    return sha_bytes(git(root, *args))


def status_paths(root: Path) -> list[str]:
    raw = git(root, "status", "--porcelain=v1", "--untracked-files=all").decode()
    return sorted(line[3:] for line in raw.splitlines())


def _expect(condition: bool, message: str) -> None:
    if not condition:
        raise PairError(message)


def cargo_config_file_identity(path: Path) -> dict[str, Any]:
    path = path.absolute()
    present = path.exists() or path.is_symlink()
    result: dict[str, Any] = {"path": str(path), "present": present, "resolved_path": None, "sha256": None, "symlink_target": None}
    if not present:
        return result
    _expect(path.is_file(), f"Cargo config path is not a regular file: {path}")
    result["resolved_path"] = str(path.resolve(strict=True))
    result["sha256"] = sha_file(path)
    if path.is_symlink():
        result["symlink_target"] = os.readlink(path)
    return result


def cargo_configuration_fingerprint(roots: Mapping[str, Path], environment: Mapping[str, str]) -> dict[str, Any]:
    home = Path(environment["HOME"]).expanduser().resolve(strict=True)
    cargo_home = Path(environment["CARGO_HOME"]).expanduser().absolute()
    _expect(home.is_dir() and cargo_home.is_dir(), "HOME/CARGO_HOME must exist for config provenance")
    body: dict[str, Any] = {
        "schema": CARGO_CONFIG_SCHEMA,
        "cargo_home": {"path": str(cargo_home), "config_files": [cargo_config_file_identity(cargo_home / name) for name in CARGO_CONFIG_FILENAMES]},
        "source_roots": {},
    }
    for side, root in sorted(roots.items()):
        resolved = root.resolve(strict=True)
        ancestors: list[Path] = []
        current = resolved
        while True:
            ancestors.append(current)
            if current == current.parent:
                break
            current = current.parent
        body["source_roots"][side] = {
            "root": str(resolved),
            "home_is_ancestor": resolved == home or home in resolved.parents,
            "config_files": [cargo_config_file_identity(parent / ".cargo" / name) for parent in ancestors for name in CARGO_CONFIG_FILENAMES],
        }
    encoded = json.dumps(body, sort_keys=True, separators=(",", ":"), ensure_ascii=False, allow_nan=False).encode()
    return {**body, "sha256": sha_bytes(encoded)}


def validate_build_environment(value: Any) -> dict[str, str]:
    allowed = set(INHERITED_BUILD_ENV_KEYS) | set(FIXED_BUILD_ENVIRONMENT)
    _expect(isinstance(value, dict), "captured build environment must be an object")
    _expect(all(isinstance(key, str) and isinstance(item, str) for key, item in value.items()), "captured environment must map strings to strings")
    _expect(not (set(value) - allowed), "captured build environment has unallowlisted keys")
    _expect({"PATH", "HOME", *FIXED_BUILD_ENVIRONMENT}.issubset(value), "captured build environment lacks required values")
    for key, expected in FIXED_BUILD_ENVIRONMENT.items():
        _expect(value[key] == expected, f"captured environment has wrong {key}")
    _expect(value["PATH"].startswith(str(PINNED_CARGO_BIN) + os.pathsep), "captured PATH does not prioritize pinned Cargo")
    return value


def validate_cargo_configuration(value: Any, label: str) -> dict[str, Any]:
    _expect(isinstance(value, dict) and set(value) == {"schema", "cargo_home", "source_roots", "sha256"}, f"{label} config schema mismatch")
    _expect(value["schema"] == CARGO_CONFIG_SCHEMA, f"{label} config schema version mismatch")
    canonical = {key: value[key] for key in ("schema", "cargo_home", "source_roots")}
    digest = sha_bytes(json.dumps(canonical, sort_keys=True, separators=(",", ":"), ensure_ascii=False, allow_nan=False).encode())
    _expect(value["sha256"] == digest, f"{label} config hash mismatch")
    return value


def validate_artifact_row(row: Any, *, side: str, root: Path) -> Path:
    _expect(isinstance(row, dict), f"{side} Cargo artifact row must be an object")
    target = row.get("target")
    profile = row.get("profile")
    _expect(row.get("reason") == "compiler-artifact" and row.get("fresh") is False, f"{side} artifact is not a fresh release compile")
    _expect(isinstance(target, dict) and isinstance(profile, dict), f"{side} artifact lacks target/profile")
    _expect(target.get("name") == TARGET_NAME and target.get("kind") == ["test"] and target.get("test") is True, f"{side} target identity mismatch")
    _expect(profile.get("test") is True and profile.get("opt_level") == "3" and profile.get("debug_assertions") is False, f"{side} artifact is not optimized release test profile")
    _expect(Path(row.get("manifest_path", "")).resolve() == (root / "Cargo.toml").resolve(), f"{side} manifest path mismatch")
    _expect(Path(target.get("src_path", "")).resolve() == (root / HARNESS_RELATIVE).resolve(), f"{side} benchmark source path mismatch")
    package = row.get("package_id")
    _expect(isinstance(package, str) and package.startswith(f"path+{root.as_uri()}#execute@"), f"{side} package identity mismatch")
    executable = row.get("executable")
    _expect(isinstance(executable, str) and executable in row.get("filenames", []), f"{side} Cargo executable missing from filenames")
    binary = Path(executable).absolute()
    _expect(binary.is_relative_to(TARGET.absolute() / "release" / "deps"), f"{side} executable is outside shared release/deps")
    return binary


def validate_build_log_artifact(payload: bytes, *, side: str, root: Path, receipt_row: Any) -> dict[str, Any]:
    rows: list[dict[str, Any]] = []
    for line_number, line in enumerate(payload.splitlines(), start=1):
        if not line.strip():
            continue
        item = strict_json_bytes(line, f"{side} Cargo build JSONL line {line_number}")
        _expect(isinstance(item, dict), f"{side} Cargo build JSONL line {line_number} is not an object")
        target = item.get("target")
        if item.get("reason") == "compiler-artifact" and isinstance(target, dict) and target.get("name") == TARGET_NAME:
            rows.append(item)
    _expect(len(rows) == 1, f"{side} build log must contain exactly one {TARGET_NAME} compiler-artifact row; got {len(rows)}")
    validate_artifact_row(receipt_row, side=side, root=root)
    _expect(rows[0] == receipt_row, f"{side} Cargo artifact row differs from the hashed build log")
    return rows[0]


def validate_source_bindings() -> dict[str, Any]:
    _expect(sha_file(SOURCE_BINDINGS) == SOURCE_BINDINGS_SHA256, "structured source-fingerprints.json hash changed")
    binding = load_json(SOURCE_BINDINGS, "structured full source bindings")
    _expect(binding.get("schema") == "typed-json-carry-structured-source-fingerprints/1", "source binding schema mismatch")
    _expect(binding.get("base_commit") == BASE_COMMIT, "source base commit mismatch")
    _expect(binding.get("product_patch_sha256") == PRODUCT_PATCH_SHA256, "source product patch mismatch")
    _expect(binding.get("harness_source_sha256") == HARNESS_SOURCE_SHA256, "source harness hash mismatch")
    _expect(binding.get("harness_patch_sha256") == HARNESS_PATCH_SHA256, "source harness patch hash mismatch")
    _expect(sha_file(EVIDENCE / "typed_json_carry_product_v3.patch") == PRODUCT_PATCH_SHA256, "product patch copy hash mismatch")
    _expect(sha_file(EVIDENCE / "typed_json_carry_structured_harness.patch") == HARNESS_PATCH_SHA256, "harness patch hash mismatch")
    _expect(sha_file(BUILD_DRIVER) == BUILD_DRIVER_SHA256, "structured build driver changed")
    for side, expected_root in (("baseline", BASELINE_ROOT), ("candidate", CANDIDATE_ROOT)):
        frozen = binding["arms"][side]
        root = Path(frozen.get("root", "")).resolve(strict=True)
        _expect(root == expected_root.resolve(strict=True), f"{side} source root mismatch")
        _expect(frozen.get("head") == BASE_COMMIT, f"{side} base commit mismatch")
        tree = frozen.get("tree")
        _expect(isinstance(tree, dict) and tree.get("file_count") == 2197, f"{side} must bind all 2,197 files")
        _expect(isinstance(tree.get("files"), dict) and len(tree["files"]) == 2197, f"{side} source path map count mismatch")
        canonical = json.dumps(tree["files"], sort_keys=True, separators=(",", ":")).encode()
        _expect(tree.get("sha256") == sha_bytes(canonical), f"{side} tree hash mismatch")
        live = source_fingerprint(root)
        _expect(live == tree, f"{side} full source fingerprint drift")
        _expect(git(root, "rev-parse", "HEAD").decode().strip() == BASE_COMMIT, f"{side} HEAD drift")
        expected_diff = EMPTY_DIFF_SHA256 if side == "baseline" else PRODUCT_PATCH_SHA256
        _expect(frozen.get("tracked_diff_sha256") == expected_diff and diff_sha256(root) == expected_diff, f"{side} tracked diff mismatch")
        _expect(frozen.get("staged_diff_sha256") == EMPTY_DIFF_SHA256 and diff_sha256(root, staged=True) == EMPTY_DIFF_SHA256, f"{side} staged diff mismatch")
        expected_status = [HARNESS_RELATIVE] if side == "baseline" else sorted([
            HARNESS_RELATIVE, "src/contracts.rs", "src/manifest.rs", "src/runtime.rs",
            "src/runtime/program_backend.rs", "src/runtime/windows_process.rs", "tests/typed_contracts.rs",
        ])
        _expect(status_paths(root) == expected_status, f"{side} working tree status changed")
    base_files = binding["arms"]["baseline"]["tree"]["files"]
    candidate_files = binding["arms"]["candidate"]["tree"]["files"]
    _expect(base_files.keys() == candidate_files.keys(), "source arm file paths differ")
    changed = {key for key in base_files if base_files[key] != candidate_files[key]}
    _expect(changed == {
        "src/contracts.rs", "src/manifest.rs", "src/runtime.rs",
        "src/runtime/program_backend.rs", "src/runtime/windows_process.rs", "tests/typed_contracts.rs",
    }, f"product delta differs from the exact six-file patch: {sorted(changed)}")
    return binding


def validate_arm(side: str, binding: Mapping[str, Any], environment: Mapping[str, str], config: Mapping[str, Any], tool_sha: str, binding_sha: str, driver_sha: str) -> dict[str, Any]:
    root = BASELINE_ROOT if side == "baseline" else CANDIDATE_ROOT
    receipt_path = BUILD_RUN / side / "build-receipt.json"
    receipt = load_json(receipt_path, f"{side} build receipt")
    _expect(receipt.get("schema") == "typed_json_carry_structured_build_receipt/1" and receipt.get("status") == "success", f"{side} build receipt status/schema mismatch")
    _expect(receipt.get("tests_executed") is False and receipt.get("source_stable") is True, f"{side} build receipt claims test execution/source drift")
    _expect(receipt.get("side") == side and receipt.get("worktree") == str(root.resolve()), f"{side} receipt worktree mismatch")
    _expect(receipt.get("head") == BASE_COMMIT, f"{side} receipt HEAD mismatch")
    _expect(receipt.get("clean") == CLEAN_ARGS and receipt.get("build") == BUILD_ARGS, f"{side} receipt commands mismatch")
    _expect(receipt.get("target_directory") == str(TARGET.resolve()), f"{side} receipt target mismatch")
    frozen = binding["arms"][side]
    tree = frozen["tree"]
    _expect(receipt.get("source_fingerprint_sha256") == tree["sha256"], f"{side} build source hash mismatch")
    _expect(receipt.get("source_fingerprint_file_count") == 2197, f"{side} build source file count mismatch")
    _expect(receipt.get("source_fingerprint_after") == {"sha256": tree["sha256"], "file_count": 2197}, f"{side} postbuild source mismatch")
    _expect(receipt.get("tracked_diff_sha256") == frozen["tracked_diff_sha256"] and receipt.get("staged_diff_sha256") == EMPTY_DIFF_SHA256, f"{side} build diff mismatch")
    _expect(receipt.get("product_patch_sha256") == PRODUCT_PATCH_SHA256, f"{side} product hash mismatch")
    _expect(receipt.get("harness_source_sha256") == HARNESS_SOURCE_SHA256 and receipt.get("harness_patch_sha256") == HARNESS_PATCH_SHA256, f"{side} harness hashes mismatch")
    _expect(receipt.get("source_bindings_sha256") == binding_sha, f"{side} source bindings hash mismatch")
    _expect(receipt.get("build_driver_sha256") == driver_sha == BUILD_DRIVER_SHA256, f"{side} build driver hash mismatch")
    _expect(receipt.get("tool_environment_sha256") == tool_sha, f"{side} tool environment hash mismatch")
    _expect(receipt.get("clean_return_code") == 0 and receipt.get("build_return_code") == 0, f"{side} build command failed")
    config_before = validate_cargo_configuration(receipt.get("cargo_configuration_fingerprint_before"), f"{side} before build")
    config_after = validate_cargo_configuration(receipt.get("cargo_configuration_fingerprint_after"), f"{side} after build")
    _expect(config_before == config == config_after and receipt.get("cargo_configuration_stable") is True, f"{side} Cargo config changed during build")
    expected_logs = {
        "clean_log": (BUILD_RUN / side / "clean.log", "clean_log_sha256"),
        "clean_stderr_log": (BUILD_RUN / side / "clean.stderr.log", "clean_stderr_sha256"),
        "build_stdout_jsonl": (BUILD_RUN / side / "build.jsonl", "build_stdout_sha256"),
        "build_stderr_log": (BUILD_RUN / side / "build.stderr.log", "build_stderr_sha256"),
    }
    for field, (expected_path, hash_field) in expected_logs.items():
        logged = Path(receipt.get(field, "")).resolve(strict=True)
        _expect(logged == expected_path.resolve(strict=True), f"{side} {field} path mismatch")
        _expect(sha_file(logged) == receipt.get(hash_field), f"{side} {field} hash mismatch")
    artifact = receipt.get("artifact")
    _expect(isinstance(artifact, dict), f"{side} artifact receipt missing")
    build_log = BUILD_RUN / side / "build.jsonl"
    logged_artifact = validate_build_log_artifact(
        build_log.read_bytes(), side=side, root=root, receipt_row=artifact.get("cargo_json")
    )
    cargo_binary = validate_artifact_row(logged_artifact, side=side, root=root)
    _expect(artifact.get("cargo_binary_path_lexical") == str(cargo_binary), f"{side} Cargo binary path mismatch")
    saved = BUILD_RUN / side / "binaries" / TARGET_NAME
    _expect(Path(artifact.get("saved_binary", "")).resolve(strict=True) == saved.resolve(strict=True), f"{side} saved binary path mismatch")
    _expect(not saved.is_symlink(), f"{side} saved binary is a symlink")
    binary_sha = artifact.get("saved_binary_sha256")
    _expect(isinstance(binary_sha, str) and SHA256_RE.fullmatch(binary_sha) is not None, f"{side} saved binary hash malformed")
    _expect(sha_file(saved) == binary_sha and os.access(saved, os.X_OK), f"{side} saved binary hash/execute mismatch")
    _expect(stat.S_IMODE(saved.stat().st_mode) == 0o555 and artifact.get("saved_binary_mode") == "0o555", f"{side} saved binary mode mismatch")
    return {
        "side": side, "root": str(root.resolve()), "head": BASE_COMMIT,
        "source_fingerprint": tree, "tracked_diff_sha256": frozen["tracked_diff_sha256"],
        "staged_diff_sha256": frozen["staged_diff_sha256"], "receipt": str(receipt_path.resolve(strict=True)),
        "receipt_sha256": sha_file(receipt_path), "build_driver_sha256": driver_sha,
        "product_patch_sha256": PRODUCT_PATCH_SHA256, "harness_source_sha256": HARNESS_SOURCE_SHA256,
        "harness_patch_sha256": HARNESS_PATCH_SHA256,
        "test_binary": {"binary": str(saved.resolve(strict=True)), "binary_sha256": binary_sha, "cargo_path_lexical": str(cargo_binary), "artifact": artifact["cargo_json"]},
    }


def load_build_provenance() -> tuple[dict[str, Any], dict[str, Any]]:
    binding = validate_source_bindings()
    binding_sha = sha_file(SOURCE_BINDINGS)
    driver_sha = sha_file(BUILD_DRIVER)
    run_path = BUILD_RUN.resolve(strict=True)
    tool_path = BUILD_RUN / "tool-environment.json"
    tool = load_json(tool_path, "build tool environment")
    _expect(tool.get("schema") == "typed_json_carry_structured_tool_environment/1", "tool environment schema mismatch")
    _expect(tool.get("captured_before_clean_or_build") is True, "tool environment was captured late")
    environment = validate_build_environment(tool.get("actual_environment"))
    _expect(tool.get("inherited_allowlist") == list(INHERITED_BUILD_ENV_KEYS), "build env allowlist mismatch")
    _expect(tool.get("fixed_environment") == FIXED_BUILD_ENVIRONMENT, "fixed build environment mismatch")
    _expect(tool.get("unlisted_inherited_variables_forwarded") is False, "unlisted host variables were forwarded")
    _expect(Path(tool.get("target_directory", "")).resolve() == TARGET.resolve(), "tool environment target mismatch")
    _expect(isinstance(tool.get("python_executable"), str) and bool(tool["python_executable"]), "build Python executable identity missing")
    _expect(isinstance(tool.get("python_version"), str) and bool(tool["python_version"]), "build Python version missing")
    _expect(isinstance(tool.get("python_flags"), str) and bool(tool["python_flags"]), "build Python flags missing")
    _expect(type(tool.get("python_optimize")) is int and tool["python_optimize"] == 0, "build driver used optimized Python")
    config = validate_cargo_configuration(tool.get("cargo_configuration_fingerprint"), "captured")
    _expect(cargo_configuration_fingerprint({"baseline": BASELINE_ROOT, "candidate": CANDIDATE_ROOT}, environment) == config, "Cargo config changed since build")
    tool_sha = sha_file(tool_path)
    command_path = BUILD_RUN / "build-pair-command.json"
    command = load_json(command_path, "paired build command")
    _expect(command.get("schema") == "typed_json_carry_structured_build_pair_command/1", "build command schema mismatch")
    _expect(command.get("build_driver_sha256") == driver_sha and command.get("source_bindings_sha256") == binding_sha, "build command source hash mismatch")
    _expect(command.get("tool_environment_sha256") == tool_sha, "build command tool hash mismatch")
    _expect(command.get("cargo_configuration_fingerprint_before_sha256") == config["sha256"], "build command Cargo config hash mismatch")
    _expect(command.get("shared_target_directory") == str(TARGET.resolve()), "build command shared target mismatch")
    _expect(command.get("arms") == {"baseline": str(BASELINE_ROOT.resolve()), "candidate": str(CANDIDATE_ROOT.resolve())}, "build command source roots mismatch")
    _expect(command.get("clean_command_per_arm") == CLEAN_ARGS and command.get("build_command_per_arm") == BUILD_ARGS, "build command differs from fixed target-only plan")
    _expect(command.get("tests_executed") is False and command.get("artifact_target") == TARGET_NAME, "build command test/target identity mismatch")
    pair_path = BUILD_RUN / "build-pair-receipt.json"
    pair = load_json(pair_path, "paired build receipt")
    _expect(pair.get("schema") == "typed_json_carry_structured_build_pair_receipt/1" and pair.get("status") == "success", "build pair receipt status/schema mismatch")
    _expect(pair.get("tests_executed") is False and pair.get("build_driver_sha256") == driver_sha, "paired receipt test/driver provenance mismatch")
    _expect(pair.get("source_bindings_sha256") == binding_sha and pair.get("tool_environment_sha256") == tool_sha, "paired receipt source/tool hash mismatch")
    _expect(pair.get("cargo_configuration_fingerprint_before") == config, "paired receipt prebuild Cargo config mismatch")
    _expect(pair.get("cargo_configuration_fingerprint_after") == config and pair.get("cargo_configuration_stable") is True, "paired receipt Cargo config changed")
    _expect(pair.get("shared_target_directory") == str(TARGET.resolve()), "paired receipt shared target mismatch")
    _expect(pair.get("arms") == {side: str(BUILD_RUN / side / "build-receipt.json") for side in ("baseline", "candidate")}, "paired receipt paths mismatch")
    identities = {
        side: validate_arm(side, binding, environment, config, tool_sha, binding_sha, driver_sha)
        for side in ("baseline", "candidate")
    }
    _expect(identities["baseline"]["test_binary"]["binary"] != identities["candidate"]["test_binary"]["binary"], "paired binaries share a path")
    _expect(identities["baseline"]["test_binary"]["binary_sha256"] != identities["candidate"]["test_binary"]["binary_sha256"], "baseline and candidate binaries are byte-identical")
    metadata = {
        "source_bindings_path": str(SOURCE_BINDINGS.resolve(strict=True)),
        "source_bindings_sha256": binding_sha,
        "build_driver_path": str(BUILD_DRIVER.resolve(strict=True)), "build_driver_sha256": driver_sha,
        "build_pair_command_path": str(command_path.resolve(strict=True)), "build_pair_command_sha256": sha_file(command_path),
        "build_pair_receipt_path": str(pair_path.resolve(strict=True)), "build_pair_receipt_sha256": sha_file(pair_path),
        "tool_environment_path": str(tool_path.resolve(strict=True)), "tool_environment_sha256": tool_sha,
        "product_patch_sha256": PRODUCT_PATCH_SHA256, "harness_patch_sha256": HARNESS_PATCH_SHA256,
        "harness_source_sha256": HARNESS_SOURCE_SHA256, "base_commit": BASE_COMMIT, "build_run": str(run_path),
    }
    return metadata, identities


def workload_inputs() -> dict[str, Any]:
    headers = ["identifier", "label", "category", "note"]
    rows = [
        [f"record-{index:03}", f"label-{index:03}", f"group-{index % 8:02}", f"note-{index:03}-fixed-structured-output"]
        for index in range(64)
    ]
    csv_text = ",".join(headers) + "\n" + "".join(",".join(row) + "\n" for row in rows)
    source = "accept csv as text\n" + "".join(
        f"run csv.parse with text csv and has_headers true as table{index:04}\n" for index in range(1000)
    ) + "then encode table0999 as json\n"
    expected_value = {"headers": headers, "rows": rows}
    expected_output = json.dumps(expected_value, ensure_ascii=False, separators=(",", ":"), allow_nan=False).encode()
    return {
        "calls": 1000, "applet": "csv.parse",
        "binding_pattern": "table0000 through table0999 (distinct bindings required by the DSL)",
        "retained_parsed_tables": 1000, "rows": 64, "columns": 4,
        "input_bytes": len(csv_text.encode()), "input_sha256": sha_bytes(csv_text.encode()),
        "source_bytes": len(source.encode()), "source_sha256": sha_bytes(source.encode()),
        "expected_headers": headers, "expected_rows": rows,
        "expected_output_hex": expected_output.hex(), "expected_output_bytes": len(expected_output),
        "expected_output_sha256": sha_bytes(expected_output),
        "expected_per_call_stdout_bytes_including_lf": len(expected_output) + 1,
    }


def protocol() -> dict[str, Any]:
    return {
        "case": CASE, "test_name": TEST_NAME,
        "warmup_pairs": WARMUP_PAIRS, "measured_pairs": MEASURED_PAIRS,
        "pair_order": "global pairs alternate AB/BA beginning AB; after three warmup pairs, measurements begin BA and contain 24 pairs of each order",
        "invocation_samples": INNER_SAMPLES, "invocation_warmups": INNER_WARMUPS,
        "invocation_timeout_seconds": INVOCATION_TIMEOUT_SECONDS,
        "retry_policy": "none; retain each outcome and continue the fixed schedule",
        "invocation": ["--exact", TEST_NAME, "--ignored", "--nocapture", "--test-threads=1"],
        "environment": {"OPT_BENCH_REPORT": "fresh absolute path per invocation", "LC_ALL": "C", "TZ": "UTC"},
        "measured_metric": "median of three in-process runtime::run durations per invocation; paired candidate-minus-baseline delta across 48 measured pairs",
        "correctness": "strict raw report schema, exact independent 64-row CSV/table JSON bytes and hashes, 1,000 calls, zero child/cache, 256 successful status-0 retained records with exact per-call stdout bytes, 744 dropped execution records and stages, identical catalog/workload across arms",
        "interpretation_limit": "mechanistic public-runtime structured-output case; all 1,000 result tables remain bound as required by the DSL. This is not CLI startup or a general runtime-performance claim.",
    }


def validate_raw_report(payload: bytes) -> dict[str, Any]:
    report = strict_json_bytes(payload, "raw structured benchmark report")
    expected_fields = {"schema", "method", "warmup_runs", "measured_samples", "workload", "results"}
    _expect(isinstance(report, dict) and set(report) == expected_fields, "raw report has unexpected top-level fields")
    _expect(report["schema"] == "typed_json_carry_public_runtime_structured_bench/v1", "raw report schema mismatch")
    _expect(report["method"] == EXPECTED_METHOD, "raw report method mismatch")
    _expect(type(report["warmup_runs"]) is int and report["warmup_runs"] == 0, "raw report inner warmup count mismatch")
    _expect(type(report["measured_samples"]) is int and report["measured_samples"] == 3, "raw report sample count mismatch")
    workload = report["workload"]
    expected = workload_inputs()
    expected_workload_fields = set(expected) | {"catalog_sha256"}
    _expect(isinstance(workload, dict) and set(workload) == expected_workload_fields, "raw workload fields mismatch")
    for key, value in expected.items():
        _expect(type(workload.get(key)) is type(value) and workload[key] == value, f"raw workload {key} mismatch")
    _expect(isinstance(workload.get("catalog_sha256"), str) and SHA256_RE.fullmatch(workload["catalog_sha256"]) is not None, "catalog digest malformed")
    results = report["results"]
    _expect(isinstance(results, dict) and set(results) == {CASE}, "raw result case mismatch")
    case = results[CASE]
    _expect(isinstance(case, dict) and set(case) == {"p50_ns", "p95_ns", "raw_sample_ns", "semantic_oracles"}, "raw case fields mismatch")
    raw = case["raw_sample_ns"]
    _expect(isinstance(raw, list) and len(raw) == 3 and all(type(value) is int and 0 < value < (1 << 64) for value in raw), "raw durations malformed")
    ordered = sorted(raw)
    _expect(type(case["p50_ns"]) is int and case["p50_ns"] == ordered[1], "reported p50 does not match raw samples")
    _expect(type(case["p95_ns"]) is int and case["p95_ns"] == ordered[2], "reported p95 does not match raw samples")
    oracle_fields = {
        "output_sha256": expected["expected_output_sha256"],
        "output_bytes": expected["expected_output_bytes"],
        "launched_processes": 0, "cache_hits": 0,
        "retained_execution_records": 256, "retained_all_success_status_0": True,
        "retained_stdout_bytes_per_call": expected["expected_per_call_stdout_bytes_including_lf"],
        "dropped_execution_records": 744, "dropped_stages": 744,
    }
    oracles = case["semantic_oracles"]
    _expect(isinstance(oracles, list) and len(oracles) == 3, "raw report must contain one oracle per sample")
    for index, oracle in enumerate(oracles):
        _expect(isinstance(oracle, dict) and set(oracle) == set(oracle_fields), f"semantic oracle {index} schema mismatch")
        _expect(all(type(oracle[key]) is type(value) and oracle[key] == value for key, value in oracle_fields.items()), f"semantic oracle {index} mismatch")
    return {"workload": workload, "raw_sample_ns": raw, "median_duration_ns": ordered[1], "semantic_oracles": oracles}


def prepare(args: argparse.Namespace) -> None:
    metadata, arms = load_build_provenance()
    workload = workload_inputs()
    runner = Path(__file__).resolve(strict=True)
    prereg = {
        "schema": "typed-json-carry-structured-paired-preregistration/1",
        "protocol": protocol(), "provenance": metadata, "inputs": arms,
        "runner": {"path": str(runner), "sha256": sha_file(runner)},
        "python": python_identity(),
        "runtime_environment": runtime_environment_contract(),
        "workload": workload, "prepared_at": utc_now(),
    }
    destination = Path(args.preregistration).expanduser().absolute()
    _expect(not destination.exists() and not destination.is_symlink(), f"refusing to overwrite preregistration {destination}")
    write_json(destination, prereg, exclusive=True)
    print(json.dumps({"preregistration": str(destination), "sha256": sha_file(destination)}, sort_keys=True))


def verify_preregistration(path: Path) -> dict[str, Any]:
    prereg = load_json(path, "paired preregistration")
    _expect(isinstance(prereg, dict) and prereg.get("schema") == "typed-json-carry-structured-paired-preregistration/1", "preregistration schema mismatch")
    _expect(prereg.get("protocol") == protocol(), "paired protocol changed after preregistration")
    _expect(prereg.get("workload") == workload_inputs(), "workload changed after preregistration")
    _expect(prereg.get("python") == python_identity(), "Python interpreter identity/flags changed after preregistration")
    _expect(prereg.get("runtime_environment") == runtime_environment_contract(), "runtime environment contract changed after preregistration")
    runner = prereg.get("runner")
    current_runner = Path(__file__).resolve(strict=True)
    _expect(isinstance(runner, dict) and runner.get("path") == str(current_runner) and runner.get("sha256") == sha_file(current_runner), "runner changed after preregistration")
    metadata, arms = load_build_provenance()
    _expect(prereg.get("provenance") == metadata and prereg.get("inputs") == arms, "build/source provenance changed after preregistration")
    return prereg


def interleaved_order(global_pair_index: int) -> tuple[str, str]:
    return ("baseline", "candidate") if global_pair_index % 2 == 0 else ("candidate", "baseline")


def _normalize_output(value: bytes | str | None) -> bytes:
    if value is None:
        return b""
    return value.encode("utf-8", errors="replace") if isinstance(value, str) else value


def run_attempt(identity: Mapping[str, Any], attempt_dir: Path, metadata: Mapping[str, Any]) -> dict[str, Any]:
    attempt_dir.mkdir(parents=True, exist_ok=False)
    raw_path = attempt_dir / "raw.json"
    _expect(raw_path.is_absolute() and not raw_path.exists() and not raw_path.is_symlink(), "raw report path must be fresh and absolute")
    binary = Path(identity["test_binary"]["binary"])
    expected_hash = identity["test_binary"]["binary_sha256"]
    before = sha_file(binary)
    _expect(before == expected_hash and os.access(binary, os.X_OK), "saved immutable test binary hash/execute check failed")
    command = [str(binary), "--exact", TEST_NAME, "--ignored", "--nocapture", "--test-threads=1"]
    environment, environment_sha256 = invocation_environment(raw_path)
    started_record = {
        **dict(metadata), "started_at": utc_now(), "command": command,
        "cwd": identity["root"], "timeout_seconds": INVOCATION_TIMEOUT_SECONDS,
        "binary_sha256_before": before, "raw_report_path": str(raw_path),
        "execution_environment": environment,
        "execution_environment_sha256": environment_sha256,
        "retry": 0,
    }
    write_json(attempt_dir / "started.json", started_record, exclusive=True)
    stdout = stderr = b""
    returncode: int | None = None
    timed_out = False
    execution_error: str | None = None
    start = time.monotonic_ns()
    try:
        completed = subprocess.run(command, cwd=identity["root"], env=environment,
                                   stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                                   stderr=subprocess.PIPE, timeout=INVOCATION_TIMEOUT_SECONDS, check=False)
        returncode, stdout, stderr = completed.returncode, completed.stdout, completed.stderr
    except subprocess.TimeoutExpired as error:
        timed_out = True
        stdout, stderr = _normalize_output(error.stdout), _normalize_output(error.stderr)
        execution_error = f"TimeoutExpired: exceeded {INVOCATION_TIMEOUT_SECONDS}s"
    except OSError as error:
        execution_error = f"{type(error).__name__}: {error}"
    elapsed_ns = time.monotonic_ns() - start
    for name, payload in (("stdout.log", stdout), ("stderr.log", stderr)):
        with (attempt_dir / name).open("xb") as stream:
            stream.write(payload); stream.flush(); os.fsync(stream.fileno())
        (attempt_dir / name).chmod(0o444)
    report: dict[str, Any] = {
        **dict(metadata), "started_at": started_record["started_at"], "finished_at": utc_now(),
        "command": command, "cwd": identity["root"], "returncode": returncode,
        "timed_out": timed_out, "execution_error": execution_error,
        "invocation_wall_ns_not_benchmark_metric": elapsed_ns,
        "stdout_path": str((attempt_dir / "stdout.log").resolve()), "stdout_sha256": sha_bytes(stdout), "stdout_bytes": len(stdout),
        "stderr_path": str((attempt_dir / "stderr.log").resolve()), "stderr_sha256": sha_bytes(stderr), "stderr_bytes": len(stderr),
        "raw_report_path": str(raw_path), "retry": 0, "attempt_dir": str(attempt_dir.resolve()),
        "execution_environment": environment,
        "execution_environment_sha256": environment_sha256,
    }
    raw_result = None
    if raw_path.is_file():
        payload = raw_path.read_bytes()
        report["raw_report_sha256"] = sha_bytes(payload)
        report["raw_report_bytes"] = len(payload)
        try:
            raw_result = validate_raw_report(payload)
            report["workload"] = raw_result["workload"]
            report["raw_sample_ns"] = raw_result["raw_sample_ns"]
            report["median_duration_ns"] = raw_result["median_duration_ns"]
            report["semantic_oracles"] = raw_result["semantic_oracles"]
        except PairError as error:
            report["raw_report_error"] = str(error)
    else:
        report["raw_report_error"] = "test executable did not create its raw report"
    after = sha_file(binary) if binary.is_file() else None
    report["binary_sha256_after"] = after
    report["successful"] = (
        returncode == 0 and not timed_out and execution_error is None and raw_result is not None
        and after == expected_hash
    )
    if after != expected_hash:
        report["binary_identity_error"] = "saved test executable changed during invocation"
    write_json(attempt_dir / "result.json", report, exclusive=True)
    return report


def workload_compatible(values: Sequence[Mapping[str, Any]]) -> bool:
    if not values:
        return False
    first = values[0]
    return all(value == first for value in values[1:])


def pair_result(global_index: int, phase: str, phase_index: int, order: Sequence[str], attempts: Mapping[str, Mapping[str, Any]]) -> dict[str, Any]:
    baseline = attempts.get("baseline", {})
    candidate = attempts.get("candidate", {})
    valid = baseline.get("successful") is True and candidate.get("successful") is True
    compatible = valid and baseline.get("workload") == candidate.get("workload")
    if valid and not compatible:
        valid = False
    delta = None
    if valid:
        delta = int(candidate["median_duration_ns"]) - int(baseline["median_duration_ns"])
    return {
        "phase": phase, "phase_pair_index": phase_index, "global_pair_index": global_index,
        "order": list(order), "baseline_median_ns": baseline.get("median_duration_ns"),
        "candidate_median_ns": candidate.get("median_duration_ns"),
        "candidate_minus_baseline_ns": delta, "valid_for_timing": valid,
        "workload_compatible": compatible,
        "baseline_successful": baseline.get("successful") is True,
        "candidate_successful": candidate.get("successful") is True,
    }


def summarize(pairs: Sequence[Mapping[str, Any]]) -> dict[str, Any]:
    valid = [int(pair["candidate_minus_baseline_ns"]) for pair in pairs if pair.get("valid_for_timing") is True]
    if not valid:
        return {"valid_pairs": 0, "wins": 0, "losses": 0, "ties": 0, "median_candidate_minus_baseline_ns": None, "raw_deltas_ns": []}
    return {
        "valid_pairs": len(valid), "wins": sum(value < 0 for value in valid),
        "losses": sum(value > 0 for value in valid), "ties": sum(value == 0 for value in valid),
        "median_candidate_minus_baseline_ns": int(statistics.median(valid)),
        "raw_deltas_ns": valid,
    }


def run(args: argparse.Namespace) -> int:
    prereg_path = Path(args.preregistration).expanduser().resolve(strict=True)
    prereg_sha = sha_file(prereg_path)
    prereg = verify_preregistration(prereg_path)
    report_dir = Path(args.report_dir).expanduser().absolute()
    _expect(not report_dir.exists() and not report_dir.is_symlink(), f"refusing to overwrite paired report directory {report_dir}")
    report_dir.mkdir(parents=True, exist_ok=False)
    report_path = report_dir / "paired-report.json"
    report: dict[str, Any] = {
        "schema": "typed-json-carry-structured-paired-report/1", "created_at": utc_now(),
        "status": "running", "preregistration_path": str(prereg_path), "preregistration_sha256": prereg_sha,
        "runner": prereg["runner"], "protocol": prereg["protocol"], "workload_contract": prereg["workload"],
        "inputs": prereg["inputs"], "host": {"system": platform.system(), "release": platform.release(), "machine": platform.machine(), "python": sys.version},
        "python": prereg["python"],
        "runtime_environment": prereg["runtime_environment"],
        "attempts": [], "warmup_pairs": [], "measured_pairs": [],
    }
    write_json(report_path, report, exclusive=True)
    failed = False
    seen_workloads: list[Mapping[str, Any]] = []
    all_pairs = WARMUP_PAIRS + MEASURED_PAIRS
    for global_index in range(all_pairs):
        phase = "warmup" if global_index < WARMUP_PAIRS else "measurement"
        phase_index = global_index + 1 if phase == "warmup" else global_index - WARMUP_PAIRS + 1
        order = interleaved_order(global_index)
        attempts: dict[str, dict[str, Any]] = {}
        for position, side in enumerate(order, start=1):
            identity = prereg["inputs"][side]
            attempt_id = f"{phase}-{phase_index:02d}-{position}-{side}"
            metadata = {
                "attempt_id": attempt_id, "phase": phase, "phase_pair_index": phase_index,
                "global_pair_index": global_index, "position_in_pair": position, "side": side,
                "source_fingerprint_sha256": identity["source_fingerprint"]["sha256"],
                "build_receipt_sha256": identity["receipt_sha256"],
                "binary_sha256": identity["test_binary"]["binary_sha256"],
            }
            try:
                result = run_attempt(identity, report_dir / "attempts" / attempt_id, metadata)
            except (PairError, OSError, KeyError, TypeError, ValueError) as error:
                result = {
                    **metadata, "finished_at": utc_now(), "successful": False,
                    "execution_error": f"{type(error).__name__}: {error}", "returncode": None,
                    "timed_out": False, "retry": 0,
                    "attempt_dir": str((report_dir / "attempts" / attempt_id).absolute()),
                }
                failed = True
            report["attempts"].append(result)
            attempts[side] = result
            if result.get("successful") is not True:
                failed = True
            elif isinstance(result.get("workload"), dict):
                seen_workloads.append(result["workload"])
            write_json(report_path, report)
        pair = pair_result(global_index, phase, phase_index, order, attempts)
        if not pair["valid_for_timing"]:
            failed = True
        (report["warmup_pairs"] if phase == "warmup" else report["measured_pairs"]).append(pair)
        write_json(report_path, report)
    if seen_workloads and not workload_compatible(seen_workloads):
        failed = True
        report["cross_invocation_workload_mismatch"] = True
    try:
        metadata_after, arms_after = load_build_provenance()
        report["post_run_identity"] = {
            "provenance_stable": metadata_after == prereg["provenance"] and arms_after == prereg["inputs"],
            "source_bindings_sha256": metadata_after["source_bindings_sha256"],
            "arms": {side: {"source_fingerprint_sha256": arms_after[side]["source_fingerprint"]["sha256"],
                            "build_receipt_sha256": arms_after[side]["receipt_sha256"],
                            "binary_sha256": arms_after[side]["test_binary"]["binary_sha256"]}
                     for side in ("baseline", "candidate")},
        }
        failed |= not report["post_run_identity"]["provenance_stable"]
    except (PairError, OSError, KeyError, TypeError, ValueError) as error:
        report["post_run_identity"] = {"provenance_stable": False, "error": str(error)}
        failed = True
    report["post_run_identity"]["preregistration_stable"] = sha_file(prereg_path) == prereg_sha
    report["post_run_identity"]["runner_stable"] = sha_file(Path(prereg["runner"]["path"])) == prereg["runner"]["sha256"]
    failed |= not report["post_run_identity"]["preregistration_stable"] or not report["post_run_identity"]["runner_stable"]
    report["paired_summary"] = summarize(report["measured_pairs"])
    report["completed_at"] = utc_now()
    report["attempt_count"] = len(report["attempts"])
    report["successful_attempt_count"] = sum(item.get("successful") is True for item in report["attempts"])
    report["failed_attempt_count"] = report["attempt_count"] - report["successful_attempt_count"]
    complete = report["attempt_count"] == 2 * all_pairs and len(report["warmup_pairs"]) == WARMUP_PAIRS and len(report["measured_pairs"]) == MEASURED_PAIRS
    report["status"] = "complete" if complete and not failed else "failed"
    write_json(report_path, report)
    print(json.dumps({"report": str(report_path), "status": report["status"]}, sort_keys=True))
    return 0 if report["status"] == "complete" else 1


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    prepare_command = commands.add_parser("prepare", help="validate sources/builds and write a fresh preregistration")
    prepare_command.add_argument("--preregistration", required=True)
    prepare_command.set_defaults(function=prepare)
    run_command = commands.add_parser("run", help="run exactly 3 warmup and 48 measurement pairs")
    run_command.add_argument("--preregistration", required=True)
    run_command.add_argument("--report-dir", required=True)
    run_command.set_defaults(function=run)
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    try:
        _expect(sys.flags.optimize == 0, "runner must run with Python optimization disabled (-O is unsupported)")
        return int(args.function(args) or 0)
    except (PairError, OSError, KeyError, TypeError, ValueError) as error:
        print(f"typed JSON structured paired runner: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
