#!/usr/bin/env python3
"""Preregister and run the frozen finite-program-module AST-cache comparison."""
from __future__ import annotations

import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import re
import statistics
import subprocess
import sys
import time
from datetime import datetime, timezone
from typing import Any, Mapping, Sequence


FORMAT_VERSION = 1
PAIR_FORMAT = "execute.program_module_cache.paired_run.v1"
TEST_TARGET = "program_module_cache_bench"
TEST_NAME = "finite_module_repeated_calls_128_with_4kib_payload"
WORKLOAD = "finite-program-module-128-calls-4kib-name"
MODULE = "examples.records.report"
CALLS_PER_RUN = 128
NAME_BYTES = 4096
WARMUP_RUNS = 0
INNER_SAMPLES = 3
WARMUP_PAIRS = 3
MEASURED_PAIRS = 48
INVOCATION_TIMEOUT_SECONDS = 60
BASE_COMMIT = "0735e4bf2f10b4d366bd31cd0140acd574b80ee4"
PRODUCT_PATCH_SHA256 = "9b98360603c6ab8328dd9512cdb9cb036b644aee3672cd8bd33217b227180e51"
BENCHMARK_SHA256 = "6547f9f245b384b6095495931002c1d313aa4217d10709a944d98448e2682793"
CALLER_SOURCE = (
    "accept report as json with default null\n"
    "accept payload as json\n"
    "repeat 128 times\n"
    "  run examples.records.report with payload payload as report\n"
    "then encode report as json\n"
)
RETENTION_NOTE = (
    "RunResult retains only final caller output; every warmup and measured run output "
    "is checked against the exact module result after timing"
)
PROGRAM_RETENTION_NOTE = (
    "one validated finite child Program per ModuleKey in the execution session, "
    "shared across calls; retained only for that session"
)
MEASUREMENT_NOTE = (
    "release runtime::run duration; includes top-level validation and one module preload "
    "per run, excludes compilation and CLI startup; each successful run completes one "
    "module call in each of 128 repeat iterations"
)
SHA256_RE = re.compile(r"^[0-9a-f]{64}$")
COMMIT_RE = re.compile(r"^[0-9a-f]{40}$")
MAX_JSON_BYTES = 1_000_000
MAX_BUILD_STREAM_BYTES = 32 * 1024 * 1024
BUILD_ROOTS = ("src/", "tests/", "vendor/", ".cargo/", "examples/modules/")
BUILD_FILES = {
    "Cargo.toml",
    "Cargo.lock",
    "build.rs",
    "rust-toolchain",
    "rust-toolchain.toml",
    "rustfmt.toml",
    "tests/program_module_cache_bench.rs",
}
ALLOWED_PRODUCT_PATHS = {
    "src/modules.rs",
    "src/runtime.rs",
    "src/runtime/program_backend.rs",
    "tests/program_modules.rs",
}
INPUT_JSON = json.dumps(
    {"count": 3, "name": "x" * NAME_BYTES},
    ensure_ascii=False,
    separators=(",", ":"),
    sort_keys=True,
).encode("utf-8")
EXPECTED_OUTPUT = INPUT_JSON
INPUT_SHA256 = hashlib.sha256(INPUT_JSON).hexdigest()
OUTPUT_SHA256 = hashlib.sha256(EXPECTED_OUTPUT).hexdigest()


class PairError(RuntimeError):
    """A frozen-input, compiler-receipt, execution, or benchmark-contract failure."""


def utc_now() -> str:
    return datetime.now(timezone.utc).isoformat(timespec="milliseconds").replace("+00:00", "Z")


def sha256_bytes(payload: bytes) -> str:
    return hashlib.sha256(payload).hexdigest()


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for block in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def _unique_object(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            raise ValueError(f"duplicate JSON key: {key}")
        result[key] = value
    return result


def _reject_json_constant(value: str) -> None:
    raise ValueError(f"non-finite JSON constant: {value}")


def load_json(path: Path, label: str) -> Any:
    try:
        payload = path.expanduser().resolve(strict=True).read_bytes()
    except OSError as error:
        raise PairError(f"cannot read {label}: {error}") from error
    if len(payload) > MAX_JSON_BYTES:
        raise PairError(f"{label} exceeds {MAX_JSON_BYTES} bytes")
    try:
        return json.loads(
            payload.decode("utf-8"),
            object_pairs_hook=_unique_object,
            parse_constant=_reject_json_constant,
        )
    except (UnicodeDecodeError, json.JSONDecodeError, ValueError) as error:
        raise PairError(f"{label} is not strict UTF-8 JSON: {error}") from error


def write_json(path: Path, value: Any, *, exclusive: bool = False) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    if exclusive and (path.exists() or path.is_symlink()):
        raise PairError(f"refusing to overwrite existing file: {path}")
    temporary = path.with_name(path.name + ".tmp")
    temporary.write_text(
        json.dumps(value, sort_keys=True, indent=2, allow_nan=False) + "\n",
        encoding="utf-8",
    )
    temporary.replace(path)


def _git(root: Path, *args: str) -> bytes:
    try:
        completed = subprocess.run(
            ["git", "-C", str(root), *args],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=20,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        raise PairError(f"cannot inspect source checkout {root}: {error}") from error
    if completed.returncode:
        raise PairError(completed.stderr.decode("utf-8", errors="replace").strip())
    return completed.stdout


def _tracked_build_input(relative: str) -> bool:
    return relative.startswith(BUILD_ROOTS) or relative in BUILD_FILES


def source_fingerprint(root: Path) -> dict[str, Any]:
    root = root.expanduser().resolve(strict=True)
    raw_paths = _git(root, "ls-files", "-z")
    paths = {
        raw.decode("utf-8")
        for raw in raw_paths.split(b"\0")
        if raw and _tracked_build_input(raw.decode("utf-8"))
    }
    # The harness is intentionally an explicit untracked test file until the
    # product patch and benchmark source are integrated by the coordinator.
    harness = root / "tests/program_module_cache_bench.rs"
    if not harness.is_file():
        raise PairError(f"benchmark harness is absent from {root}")
    paths.add("tests/program_module_cache_bench.rs")
    files: dict[str, str] = {}
    digest = hashlib.sha256()
    for relative in sorted(paths):
        path = root / relative
        if not path.is_file():
            raise PairError(f"tracked build/runtime input is missing: {relative}")
        content_hash = sha256_file(path)
        files[relative] = content_hash
        digest.update(relative.encode("utf-8"))
        digest.update(b"\0")
        digest.update(content_hash.encode("ascii"))
        digest.update(b"\n")
    return {"sha256": digest.hexdigest(), "files": files}


def _commit(root: Path) -> str:
    return _git(root, "rev-parse", "HEAD").decode("ascii").strip()


def _status(root: Path) -> tuple[list[str], list[str], list[str]]:
    unstaged = _git(root, "diff", "--name-only").decode("utf-8").splitlines()
    staged = _git(root, "diff", "--cached", "--name-only").decode("utf-8").splitlines()
    untracked = [
        path.decode("utf-8")
        for path in _git(root, "ls-files", "--others", "--exclude-standard", "-z").split(b"\0")
        if path
    ]
    return unstaged, staged, untracked


def _validate_tree(root: Path, side: str) -> dict[str, Any]:
    root = root.expanduser().resolve(strict=True)
    commit = _commit(root)
    if commit != BASE_COMMIT:
        raise PairError(f"{side} source checkout has unexpected HEAD {commit}; expected {BASE_COMMIT}")
    unstaged, staged, untracked = _status(root)
    expected_untracked = ["tests/program_module_cache_bench.rs"]
    if staged or untracked != expected_untracked:
        raise PairError(f"{side} has unexpected staged/untracked files: staged={staged} untracked={untracked}")
    if side == "baseline":
        if unstaged:
            raise PairError(f"baseline tracked source is dirty: {unstaged}")
    elif sorted(unstaged) != sorted(ALLOWED_PRODUCT_PATHS):
        raise PairError(f"candidate changed unexpected tracked paths: {unstaged}")
    patch_hash = sha256_bytes(_git(root, "diff", "--binary", "HEAD"))
    expected_patch = PRODUCT_PATCH_SHA256 if side == "candidate" else sha256_bytes(b"")
    if patch_hash != expected_patch:
        raise PairError(f"{side} tracked diff hash {patch_hash} differs from expected {expected_patch}")
    harness_hash = sha256_file(root / "tests/program_module_cache_bench.rs")
    if harness_hash != BENCHMARK_SHA256:
        raise PairError(f"{side} benchmark harness hash differs from frozen source")
    return {
        "root": str(root),
        "commit": commit,
        "unstaged_paths": unstaged,
        "staged_paths": staged,
        "untracked_paths": untracked,
        "tracked_diff_sha256": patch_hash,
        "benchmark_sha256": harness_hash,
        "source_fingerprint": source_fingerprint(root),
    }


def _valid_hash(value: Any) -> bool:
    return isinstance(value, str) and SHA256_RE.fullmatch(value) is not None


def _required_build_argv(argv: Any) -> bool:
    if not isinstance(argv, list) or not all(isinstance(part, str) for part in argv):
        return False
    required = [
        "test",
        "--locked",
        "--release",
        "--test",
        TEST_TARGET,
        "--test",
        "program_modules",
        "--lib",
        "--no-run",
        "--message-format=json",
    ]
    return Path(argv[0]).name == "cargo" and all(item in argv[1:] for item in required)


def _load_build_stream(path: Path, expected_artifact: Mapping[str, Any], side: str) -> dict[str, Any]:
    path = path.expanduser().resolve(strict=True)
    try:
        payload = path.read_bytes()
    except OSError as error:
        raise PairError(f"cannot read {side} Cargo JSON output: {error}") from error
    if len(payload) > MAX_BUILD_STREAM_BYTES:
        raise PairError(f"{side} Cargo JSON output exceeds {MAX_BUILD_STREAM_BYTES} bytes")
    try:
        decoded = payload.decode("utf-8")
        rows = [
            json.loads(
                line,
                object_pairs_hook=_unique_object,
                parse_constant=_reject_json_constant,
            )
            for line in decoded.splitlines()
            if line
        ]
    except (UnicodeDecodeError, json.JSONDecodeError, ValueError) as error:
        raise PairError(f"{side} Cargo JSON output is invalid: {error}") from error
    matches: list[tuple[int, dict[str, Any]]] = []
    for index, row in enumerate(rows):
        if not isinstance(row, dict) or row.get("reason") != "compiler-artifact":
            continue
        target = row.get("target")
        profile = row.get("profile")
        if (
            isinstance(target, dict)
            and target.get("name") == TEST_TARGET
            and isinstance(profile, dict)
            and profile.get("test") is True
        ):
            matches.append((index, row))
    finished = [
        (index, row)
        for index, row in enumerate(rows)
        if isinstance(row, dict) and row.get("reason") == "build-finished"
    ]
    if len(matches) != 1 or matches[0][1] != expected_artifact:
        raise PairError(f"{side} Cargo JSON output must contain exactly the receipted test artifact")
    if len(finished) != 1 or finished[0][1].get("success") is not True:
        raise PairError(f"{side} Cargo JSON output lacks one successful build-finished record")
    if finished[0][0] != len(rows) - 1:
        raise PairError(f"{side} Cargo build-finished record is not the final JSON event")
    errors = [
        row
        for row in rows
        if isinstance(row, dict)
        and row.get("reason") == "compiler-message"
        and isinstance(row.get("message"), dict)
        and row["message"].get("level") == "error"
    ]
    if errors:
        raise PairError(f"{side} Cargo JSON output contains compiler errors")
    return {
        "path": str(path),
        "sha256": sha256_bytes(payload),
        "artifact_record_index": matches[0][0],
        "build_finished_record_index": finished[0][0],
        "record_count": len(rows),
    }


def _selected_build_environment() -> dict[str, str]:
    # These are a bounded snapshot at receipt preparation time. The original
    # build script did not persist its complete inherited process environment.
    names = (
        "RUSTFLAGS",
        "CARGO_ENCODED_RUSTFLAGS",
        "CARGO_BUILD_TARGET",
        "CARGO_PROFILE_RELEASE_OPT_LEVEL",
        "CARGO_PROFILE_RELEASE_DEBUG",
        "CARGO_PROFILE_RELEASE_DEBUG_ASSERTIONS",
        "CC",
        "AR",
        "CFLAGS",
        "CXX",
        "CXXFLAGS",
    )
    return {name: os.environ[name] for name in names if name in os.environ}


def _prepare_build_environment(source_root: Path) -> dict[str, Any]:
    # Match the explicit overrides in work/build_program_ast_pair.py. Values
    # inherited by that script cannot be recovered from its raw receipt, so
    # the selected snapshot is deliberately labelled as post-build evidence.
    rust_root = Path("/Users/earlcameron/Documents/Codex/2026-10-12/you/work/rust")
    target_root = source_root.parent / "perf-target-candidate"
    return {
        "explicit_build_driver_overrides": {
            "CARGO_HOME": str(rust_root / "cargo"),
            "RUSTUP_HOME": str(rust_root / "rustup"),
            "RUSTUP_TOOLCHAIN": "stable-aarch64-apple-darwin",
            "CARGO_BUILD_JOBS": "4",
            "CARGO_TARGET_DIR": str(target_root),
            "PATH_prefix": str(rust_root / "cargo" / "bin"),
        },
        "selected_compile_environment_observed_at_prepare": _selected_build_environment(),
        "inherited_build_environment_complete": False,
    }


def prepare_receipt(args: argparse.Namespace) -> None:
    side = args.side
    raw_path = Path(args.raw_receipt).expanduser().resolve(strict=True)
    raw = load_json(raw_path, f"{side} raw compiler receipt")
    expected_keys = {"artifact", "saved_binary", "sha256", "source_sha256", "source_stable", "attempt"}
    if not isinstance(raw, dict) or set(raw) != expected_keys:
        raise PairError(f"{side} raw receipt has an unexpected schema")
    if raw.get("source_stable") is not True or raw.get("attempt") != 1:
        raise PairError(f"{side} raw receipt does not attest to one stable first build attempt")
    artifact = raw.get("artifact")
    if not isinstance(artifact, dict):
        raise PairError(f"{side} raw receipt lacks its Cargo artifact")
    root = Path(args.source_root).expanduser().resolve(strict=True)
    try:
        manifest = Path(artifact["manifest_path"]).expanduser().resolve(strict=True)
        artifact_binary = Path(artifact["executable"]).expanduser().resolve(strict=True)
        saved_binary = Path(raw["saved_binary"]).expanduser().resolve(strict=True)
    except (KeyError, OSError, TypeError, ValueError) as error:
        raise PairError(f"{side} raw receipt paths are invalid: {error}") from error
    if manifest != (root / "Cargo.toml").resolve(strict=True):
        raise PairError(f"{side} raw receipt identifies a different source checkout")
    if not _valid_hash(raw.get("sha256")):
        raise PairError(f"{side} raw receipt has an invalid saved-binary digest")
    if not saved_binary.is_file() or not os.access(saved_binary, os.X_OK) or sha256_file(saved_binary) != raw["sha256"]:
        raise PairError(f"{side} saved binary is absent, non-executable, or differs from the raw receipt")
    backend_source = root / "src/runtime/program_backend.rs"
    if not backend_source.is_file() or sha256_file(backend_source) != raw.get("source_sha256"):
        raise PairError(f"{side} receipted program-backend source hash has changed")
    tree = _validate_tree(root, side)
    harness = Path(args.harness_source).expanduser().resolve(strict=True)
    if sha256_file(harness) != BENCHMARK_SHA256 or harness != (root / "tests/program_module_cache_bench.rs").resolve(strict=True):
        raise PairError(f"{side} harness is not the frozen benchmark source in this checkout")
    artifact_stream = _load_build_stream(Path(args.build_jsonl), artifact, side)
    build_log = Path(args.build_log).expanduser().resolve(strict=True)
    clean_log = Path(args.release_clean_log).expanduser().resolve(strict=True)
    build_driver = Path(args.build_driver).expanduser().resolve(strict=True)
    source_root = Path(__file__).resolve(strict=True).parents[3]
    expected_driver = source_root / "work/build_program_ast_pair.py"
    if build_driver != expected_driver:
        raise PairError("build driver path differs from the fixed root build script")
    build_driver_hash = sha256_file(build_driver)
    build = {
        "argv": [
            "cargo",
            "test",
            "--locked",
            "--release",
            "--test",
            TEST_TARGET,
            "--test",
            "program_modules",
            "--lib",
            "--no-run",
            "--message-format=json",
        ],
        "returncode": 0,
        "log": str(build_log),
        "log_sha256": sha256_file(build_log),
        "json_output": artifact_stream,
        "release_clean_log": {
            "path": str(clean_log),
            "sha256": sha256_file(clean_log),
        },
        "build_driver": {"path": str(build_driver), "sha256": build_driver_hash},
        "rustc_vv": "not recorded by the build attempt; no compiler command run during preparation",
        "cargo_version": "not recorded by the build attempt; no Cargo command run during preparation",
        "environment": _prepare_build_environment(root),
    }
    prepared = {
        **raw,
        "raw_receipt": {"path": str(raw_path), "sha256": sha256_file(raw_path)},
        "source_fingerprint_sha256": tree["source_fingerprint"]["sha256"],
        "source_fingerprint": tree["source_fingerprint"],
        "benchmark_sha256": BENCHMARK_SHA256,
        "build": build,
    }
    destination = Path(args.output).expanduser().absolute()
    write_json(destination, prepared, exclusive=True)
    print(json.dumps({"prepared_receipt": str(destination), "sha256": sha256_file(destination)}, sort_keys=True))


def receipt_identity(path: Path, side: str) -> dict[str, Any]:
    path = path.expanduser().resolve(strict=True)
    receipt = load_json(path, f"{side} compiler receipt")
    if not isinstance(receipt, dict):
        raise PairError(f"{side} compiler receipt must be an object")
    artifact = receipt.get("artifact")
    build = receipt.get("build")
    raw_info = receipt.get("raw_receipt")
    fingerprint = receipt.get("source_fingerprint")
    if (
        not isinstance(artifact, dict)
        or not isinstance(build, dict)
        or not isinstance(raw_info, dict)
        or not isinstance(fingerprint, dict)
    ):
        raise PairError(f"{side} receipt must be prepared from the raw Cargo receipt")
    target = artifact.get("target")
    profile = artifact.get("profile")
    if (
        artifact.get("reason") != "compiler-artifact"
        or not isinstance(target, dict)
        or target.get("name") != TEST_TARGET
        or target.get("test") is not True
        or not isinstance(profile, dict)
        or profile.get("test") is not True
        or profile.get("opt_level") != "3"
        or profile.get("debug_assertions") is not False
        or artifact.get("fresh") is not False
        or type(build.get("returncode")) is not int
        or build.get("returncode") != 0
        or not _required_build_argv(build.get("argv"))
        or not isinstance(build.get("rustc_vv"), str)
        or not build.get("rustc_vv")
        or not isinstance(build.get("cargo_version"), str)
        or not build.get("cargo_version")
        or not isinstance(build.get("environment"), dict)
    ):
        raise PairError(f"{side} receipt is not a successful fresh release integration-test build")
    try:
        manifest = Path(artifact["manifest_path"]).expanduser().resolve(strict=True)
        root = manifest.parent
        target_source = Path(target["src_path"]).expanduser().resolve(strict=True)
        artifact_binary = Path(artifact["executable"]).expanduser().resolve(strict=True)
    except (KeyError, OSError, TypeError, ValueError) as error:
        raise PairError(f"{side} receipt paths are invalid: {error}") from error
    expected_manifest = (root / "Cargo.toml").resolve(strict=True)
    expected_test = (root / "tests/program_module_cache_bench.rs").resolve(strict=True)
    if manifest != expected_manifest or target_source != expected_test:
        raise PairError(f"{side} Cargo artifact does not identify the expected checkout/test target")
    saved_binary = Path(receipt.get("saved_binary", "")).expanduser().resolve(strict=True)
    binary_hash = receipt.get("sha256")
    if not _valid_hash(binary_hash):
        raise PairError(f"{side} receipt has no valid saved-binary SHA-256")
    if not saved_binary.is_file() or not os.access(saved_binary, os.X_OK) or sha256_file(saved_binary) != binary_hash:
        raise PairError(f"{side} saved binary is absent, non-executable, or differs from the receipt")
    if not _valid_hash(receipt.get("source_fingerprint_sha256")):
        raise PairError(f"{side} receipt lacks source_fingerprint_sha256")
    tree = _validate_tree(root, side)
    if (
        receipt["source_fingerprint_sha256"] != tree["source_fingerprint"]["sha256"]
        or fingerprint != tree["source_fingerprint"]
    ):
        raise PairError(f"{side} source fingerprint changed after its build receipt")
    if receipt.get("benchmark_sha256") != BENCHMARK_SHA256 or tree["benchmark_sha256"] != BENCHMARK_SHA256:
        raise PairError(f"{side} compiler receipt benchmark hash differs from frozen source")
    raw_path = Path(raw_info.get("path", "")).expanduser().resolve(strict=True)
    if not _valid_hash(raw_info.get("sha256")) or sha256_file(raw_path) != raw_info["sha256"]:
        raise PairError(f"{side} raw compiler receipt changed after preparation")
    raw_receipt = load_json(raw_path, f"{side} raw compiler receipt")
    raw_projection = {
        key: receipt.get(key)
        for key in ("artifact", "saved_binary", "sha256", "source_sha256", "source_stable", "attempt")
    }
    if raw_receipt != raw_projection or raw_receipt.get("source_stable") is not True:
        raise PairError(f"{side} prepared receipt differs from its raw compiler receipt")
    if sha256_file(root / "src/runtime/program_backend.rs") != raw_receipt.get("source_sha256"):
        raise PairError(f"{side} raw receipt program-backend source hash changed")
    package_id = artifact.get("package_id")
    if not isinstance(package_id, str) or "#execute@" not in package_id:
        raise PairError(f"{side} artifact has unexpected package identity")
    package_version = package_id.rsplit("#execute@", 1)[1]
    build_log = build.get("log")
    build_log_sha256 = build.get("log_sha256")
    json_output = build.get("json_output")
    clean_log = build.get("release_clean_log")
    build_driver = build.get("build_driver")
    if (
        not isinstance(build_log, str)
        or not _valid_hash(build_log_sha256)
        or not isinstance(json_output, dict)
        or not isinstance(clean_log, dict)
        or not isinstance(build_driver, dict)
        or type(build.get("returncode")) is not int
        or build.get("returncode") != 0
        or not isinstance(build.get("environment"), dict)
    ):
        raise PairError(f"{side} receipt lacks complete build evidence")
    log_path = Path(build_log).expanduser().resolve(strict=True)
    if sha256_file(log_path) != build_log_sha256:
        raise PairError(f"{side} compiler log differs from receipt")
    stream = _load_build_stream(Path(json_output.get("path", "")), artifact, side)
    if stream != json_output:
        raise PairError(f"{side} Cargo JSON output differs from prepared receipt")
    clean_path = Path(clean_log.get("path", "")).expanduser().resolve(strict=True)
    if not _valid_hash(clean_log.get("sha256")) or sha256_file(clean_path) != clean_log["sha256"]:
        raise PairError(f"{side} release-clean log differs from receipt")
    driver_path = Path(build_driver.get("path", "")).expanduser().resolve(strict=True)
    if not _valid_hash(build_driver.get("sha256")) or sha256_file(driver_path) != build_driver["sha256"]:
        raise PairError(f"{side} build driver differs from receipt")
    return {
        "side": side,
        "receipt_path": str(path),
        "receipt_sha256": sha256_file(path),
        "root": tree["root"],
        "commit": tree["commit"],
        "tracked_diff_sha256": tree["tracked_diff_sha256"],
        "source_fingerprint": tree["source_fingerprint"],
        "benchmark_sha256": tree["benchmark_sha256"],
        "package_version": package_version,
        "artifact": artifact,
        "build": {
            "argv": build["argv"],
            "returncode": build["returncode"],
            "log": str(log_path),
            "log_sha256": build_log_sha256,
            "rustc_vv": build["rustc_vv"],
            "cargo_version": build["cargo_version"],
            "environment": build["environment"],
            "json_output": json_output,
            "release_clean_log": clean_log,
            "build_driver": build_driver,
        },
        "binary": str(saved_binary),
        "artifact_binary": str(artifact_binary),
        "binary_sha256": binary_hash,
    }


def _protocol() -> dict[str, Any]:
    return {
        "test_target": TEST_TARGET,
        "test_name": TEST_NAME,
        "warmup_pairs": WARMUP_PAIRS,
        "measured_pairs": MEASURED_PAIRS,
        "pair_order": "AB, BA, AB warmups; then 48 measured pairs alternating BA, AB, starting BA",
        "inner_warmup_runs": WARMUP_RUNS,
        "inner_samples": INNER_SAMPLES,
        "invocation_timeout_seconds": INVOCATION_TIMEOUT_SECONDS,
        "invocation_environment": {
            "PROGRAM_MODULE_CACHE_BENCH_WARMUPS": str(WARMUP_RUNS),
            "PROGRAM_MODULE_CACHE_BENCH_SAMPLES": str(INNER_SAMPLES),
            "LC_ALL": "C",
            "TZ": "UTC",
        },
        "metric": "median of three Rust runtime::run samples per binary invocation; candidate minus baseline by pair; positive means candidate slower",
        "correctness_oracle": "both arms must match the fixed 4KiB input/output hashes, caller-source hash, 128-call workload shape/count, module identity, and zero-process assertion; the pinned Rust test asserts zero launches on every measured run and returns success only if all three outputs equal the expected final module record",
        "prepared_ast_memory_tradeoff": PROGRAM_RETENTION_NOTE,
        "no_retries": True,
        "all_failures_retained": True,
    }


def preregister(args: argparse.Namespace) -> None:
    harness = Path(args.harness_source).expanduser().resolve(strict=True)
    if sha256_file(harness) != BENCHMARK_SHA256:
        raise PairError("provided benchmark harness hash differs from the frozen source")
    baseline = receipt_identity(Path(args.baseline_receipt), "baseline")
    candidate = receipt_identity(Path(args.candidate_receipt), "candidate")
    if baseline["root"] == candidate["root"] or baseline["binary"] == candidate["binary"]:
        raise PairError("baseline and candidate require distinct source roots and saved binaries")
    if baseline["commit"] != candidate["commit"] or baseline["package_version"] != candidate["package_version"]:
        raise PairError("baseline and candidate must use the same base commit and package version")
    for field in ("rustc_vv", "cargo_version", "environment"):
        if baseline["build"][field] != candidate["build"][field]:
            raise PairError(f"baseline and candidate compiler {field} differs")
    if baseline["source_fingerprint"] == candidate["source_fingerprint"]:
        raise PairError("baseline and candidate source fingerprints are unexpectedly equal")
    script = Path(__file__).resolve(strict=True)
    prereg = {
        "schema_version": FORMAT_VERSION,
        "format": PAIR_FORMAT,
        "protocol": _protocol(),
        "inputs": {"baseline": baseline, "candidate": candidate},
        "benchmark_harness": {"path": str(harness), "sha256": BENCHMARK_SHA256},
        "runtime_environment": _capture_environment(),
        "driver": {"path": str(script), "sha256": sha256_file(script)},
        "host_at_preregistration": {
            "system": platform.system(),
            "release": platform.release(),
            "machine": platform.machine(),
            "python": sys.version,
        },
        "prepared_at": utc_now(),
    }
    destination = Path(args.preregistration).expanduser().absolute()
    write_json(destination, prereg, exclusive=True)
    print(json.dumps({"preregistration": str(destination), "sha256": sha256_file(destination)}, sort_keys=True))


def validate_harness_report(payload: bytes, package_version: str) -> dict[str, Any]:
    if len(payload) > MAX_JSON_BYTES:
        raise PairError(f"benchmark report exceeds {MAX_JSON_BYTES} bytes")
    try:
        report = json.loads(
            payload.decode("utf-8"),
            object_pairs_hook=_unique_object,
            parse_constant=_reject_json_constant,
        )
    except (UnicodeDecodeError, json.JSONDecodeError, ValueError) as error:
        raise PairError(f"benchmark report is not strict UTF-8 JSON: {error}") from error
    fields = {
        "version",
        "workload",
        "measurement",
        "module",
        "calls_per_run",
        "successful_module_invocations_per_run",
        "per_call_output_retention",
        "prepared_program_retention",
        "name_utf8_bytes",
        "warmup_runs",
        "measured_runs",
        "input_sha256",
        "caller_source_sha256",
        "expected_output_bytes",
        "expected_output_sha256",
        "sample_ns",
    }
    if not isinstance(report, dict) or set(report) != fields:
        raise PairError("benchmark report has an unexpected schema")
    expected = {
        "version": package_version,
        "workload": WORKLOAD,
        "measurement": MEASUREMENT_NOTE,
        "module": MODULE,
        "calls_per_run": CALLS_PER_RUN,
        "successful_module_invocations_per_run": CALLS_PER_RUN,
        "per_call_output_retention": RETENTION_NOTE,
        "prepared_program_retention": PROGRAM_RETENTION_NOTE,
        "name_utf8_bytes": NAME_BYTES,
        "warmup_runs": WARMUP_RUNS,
        "measured_runs": INNER_SAMPLES,
        "input_sha256": INPUT_SHA256,
        "caller_source_sha256": sha256_bytes(CALLER_SOURCE.encode("utf-8")),
        "expected_output_bytes": len(EXPECTED_OUTPUT),
        "expected_output_sha256": OUTPUT_SHA256,
    }
    if any(type(report.get(key)) is not type(value) or report.get(key) != value for key, value in expected.items()):
        raise PairError("benchmark report does not match the exact frozen workload and output oracle")
    samples = report["sample_ns"]
    if not isinstance(samples, list) or len(samples) != INNER_SAMPLES:
        raise PairError("benchmark report must retain exactly three raw runtime samples")
    for sample in samples:
        if (
            isinstance(sample, bool)
            or not isinstance(sample, (int, float))
            or not math.isfinite(sample)
            or sample <= 0
        ):
            raise PairError("benchmark report contains a nonpositive or invalid runtime sample")
    return {
        "oracle": {key: report[key] for key in expected},
        "samples_ns": samples,
        "median_ns": float(statistics.median(samples)),
    }


def _runtime_environment(report_path: Path, pinned: Mapping[str, str]) -> dict[str, str]:
    environment = dict(pinned)
    environment.update(
        {
            "PROGRAM_MODULE_CACHE_BENCH_REPORT": str(report_path.resolve()),
            "PROGRAM_MODULE_CACHE_BENCH_WARMUPS": str(WARMUP_RUNS),
            "PROGRAM_MODULE_CACHE_BENCH_SAMPLES": str(INNER_SAMPLES),
        }
    )
    return environment


def pair_order(global_pair_index: int) -> tuple[str, str]:
    return ("baseline", "candidate") if global_pair_index % 2 == 0 else ("candidate", "baseline")


def _capture_environment() -> dict[str, str]:
    keys = (
        "HOME",
        "PATH",
        "TMPDIR",
        "TEMP",
        "SystemRoot",
        "WINDIR",
        "DYLD_LIBRARY_PATH",
        "DYLD_FALLBACK_LIBRARY_PATH",
    )
    result = {key: os.environ[key] for key in keys if key in os.environ}
    result["LC_ALL"] = "C"
    result["TZ"] = "UTC"
    return result


def _verify_preregistration(path: Path) -> dict[str, Any]:
    prereg = load_json(path, "preregistration")
    if not isinstance(prereg, dict) or prereg.get("schema_version") != FORMAT_VERSION or prereg.get("format") != PAIR_FORMAT:
        raise PairError("preregistration has an unexpected schema")
    if prereg.get("protocol") != _protocol():
        raise PairError("preregistered protocol differs from this runner")
    driver = prereg.get("driver", {})
    if driver.get("sha256") != sha256_file(Path(__file__).resolve(strict=True)):
        raise PairError("runner changed after preregistration")
    harness = prereg.get("benchmark_harness", {})
    harness_path = Path(harness.get("path", "")).expanduser().resolve(strict=True)
    if harness.get("sha256") != BENCHMARK_SHA256 or sha256_file(harness_path) != BENCHMARK_SHA256:
        raise PairError("benchmark harness changed after preregistration")
    runtime_environment = prereg.get("runtime_environment")
    if (
        not isinstance(runtime_environment, dict)
        or any(not isinstance(key, str) or not isinstance(value, str) for key, value in runtime_environment.items())
        or runtime_environment.get("LC_ALL") != "C"
        or runtime_environment.get("TZ") != "UTC"
    ):
        raise PairError("preregistration has an invalid pinned runtime environment")
    for side in ("baseline", "candidate"):
        identity = prereg.get("inputs", {}).get(side, {})
        current = receipt_identity(Path(identity.get("receipt_path", "")), side)
        if current != identity:
            raise PairError(f"{side} compiler receipt/source identity changed after preregistration")
        if sha256_file(Path(identity["binary"])) != identity["binary_sha256"]:
            raise PairError(f"{side} saved binary changed after preregistration")
    return prereg


def _attempt(
    identity: Mapping[str, Any],
    attempt_dir: Path,
    metadata: Mapping[str, Any],
    pinned_environment: Mapping[str, str],
) -> dict[str, Any]:
    attempt_dir.mkdir(parents=True, exist_ok=False)
    raw_path = attempt_dir / "raw.json"
    binary = Path(identity["binary"])
    expected_hash = str(identity["binary_sha256"])
    before_hash = sha256_file(binary) if binary.is_file() else None
    command = [
        str(binary),
        "--exact",
        TEST_NAME,
        "--ignored",
        "--nocapture",
        "--test-threads=1",
    ]
    environment = _runtime_environment(raw_path, pinned_environment)
    started = {
        **dict(metadata),
        "started_at": utc_now(),
        "command": command,
        "cwd": identity["root"],
        "timeout_seconds": INVOCATION_TIMEOUT_SECONDS,
        "binary_sha256_before": before_hash,
        "environment": environment,
        "raw_report_path": str(raw_path.resolve()),
    }
    write_json(attempt_dir / "started.json", started, exclusive=True)
    start_ns = time.monotonic_ns()
    returncode: int | None = None
    timed_out = False
    execution_error: str | None = None
    stdout = b""
    stderr = b""
    if before_hash != expected_hash:
        execution_error = "saved binary SHA-256 differed from preregistration before invocation"
    else:
        try:
            completed = subprocess.run(
                command,
                cwd=identity["root"],
                env=environment,
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                timeout=INVOCATION_TIMEOUT_SECONDS,
                check=False,
            )
            returncode = completed.returncode
            stdout, stderr = completed.stdout, completed.stderr
        except subprocess.TimeoutExpired as error:
            timed_out = True
            stdout, stderr = error.stdout or b"", error.stderr or b""
        except OSError as error:
            execution_error = f"{type(error).__name__}: {error}"
            stderr = execution_error.encode("utf-8", errors="replace")
    elapsed_wall_ns = time.monotonic_ns() - start_ns
    after_hash = sha256_file(binary) if binary.is_file() else None
    if after_hash != expected_hash:
        execution_error = "saved binary SHA-256 changed during or after invocation"
    (attempt_dir / "stdout.bin").write_bytes(stdout)
    (attempt_dir / "stderr.bin").write_bytes(stderr)
    parsed: dict[str, Any] | None = None
    raw_hash: str | None = None
    validation_error: str | None = None
    if raw_path.is_file():
        raw = raw_path.read_bytes()
        raw_hash = sha256_bytes(raw)
        try:
            parsed = validate_harness_report(raw, str(identity["package_version"]))
        except PairError as error:
            validation_error = str(error)
    else:
        validation_error = "benchmark test did not create a raw report"
    successful = (
        returncode == 0
        and not timed_out
        and execution_error is None
        and validation_error is None
    )
    oracle = dict(parsed["oracle"]) if parsed else None
    if successful and oracle is not None:
        # The pinned harness checks result.launched_processes == 0 for all
        # three measured runtime calls; a successful process exit proves each
        # assertion passed. No benchmark output vector exists per iteration.
        oracle["zero_native_process_launches_asserted"] = True
    finished = {
        **dict(metadata),
        "finished_at": utc_now(),
        "returncode": returncode,
        "timed_out": timed_out,
        "execution_error": execution_error,
        "elapsed_wall_ns": elapsed_wall_ns,
        "binary_sha256_before": before_hash,
        "binary_sha256_after": after_hash,
        "raw_report_present": raw_path.is_file(),
        "raw_report_sha256": raw_hash,
        "validation_error": validation_error,
        "successful": successful,
        "oracle": oracle,
        "samples_ns": parsed["samples_ns"] if parsed else None,
        "median_ns": parsed["median_ns"] if parsed else None,
        "stdout_sha256": sha256_bytes(stdout),
        "stderr_sha256": sha256_bytes(stderr),
        "attempt_dir": str(attempt_dir.resolve()),
    }
    write_json(attempt_dir / "finished.json", finished, exclusive=True)
    return finished


def _pair_result(pair_index: int, baseline: Mapping[str, Any], candidate: Mapping[str, Any]) -> dict[str, Any]:
    oracle_equal = (
        baseline.get("successful") is True
        and candidate.get("successful") is True
        and baseline.get("oracle") == candidate.get("oracle")
    )
    valid = baseline.get("successful") is True and candidate.get("successful") is True and oracle_equal
    pair: dict[str, Any] = {
        "pair_index": pair_index,
        "baseline_attempt_id": baseline["attempt_id"],
        "candidate_attempt_id": candidate["attempt_id"],
        "valid_for_timing_comparison": valid,
        "full_exact_oracle_equal": oracle_equal,
    }
    if not valid:
        pair["failure_reasons"] = {
            "baseline": baseline.get("validation_error") or baseline.get("execution_error") or f"successful={baseline.get('successful')} returncode={baseline.get('returncode')} timed_out={baseline.get('timed_out')}",
            "candidate": candidate.get("validation_error") or candidate.get("execution_error") or f"successful={candidate.get('successful')} returncode={candidate.get('returncode')} timed_out={candidate.get('timed_out')}",
        }
        return pair
    baseline_ns = float(baseline["median_ns"])
    candidate_ns = float(candidate["median_ns"])
    delta = candidate_ns - baseline_ns
    pair["measurement"] = {
        "baseline_median_ns": baseline_ns,
        "candidate_median_ns": candidate_ns,
        "candidate_minus_baseline_ns": delta,
        "candidate_over_baseline": candidate_ns / baseline_ns,
        "candidate_loss": delta > 0,
        "tie": delta == 0,
    }
    return pair


def _summary(pairs: Sequence[Mapping[str, Any]]) -> dict[str, Any]:
    deltas = [
        float(pair["measurement"]["candidate_minus_baseline_ns"])
        for pair in pairs
        if pair.get("valid_for_timing_comparison") is True
    ]
    return {
        "valid_measured_pairs": len(deltas),
        "invalid_measured_pairs": sum(pair.get("valid_for_timing_comparison") is not True for pair in pairs),
        "paired_candidate_minus_baseline_ns": deltas,
        "median_paired_delta_ns": statistics.median(deltas) if deltas else None,
        "candidate_losses": sum(delta > 0 for delta in deltas),
        "candidate_improvements": sum(delta < 0 for delta in deltas),
        "ties": sum(delta == 0 for delta in deltas),
    }


def run(args: argparse.Namespace) -> int:
    prereg_path = Path(args.preregistration).expanduser().resolve(strict=True)
    prereg = _verify_preregistration(prereg_path)
    report_dir = Path(args.report_dir).expanduser().absolute()
    consumed_marker = prereg_path.with_name(prereg_path.name + ".consumed.json")
    if consumed_marker.exists() or consumed_marker.is_symlink():
        raise PairError(f"preregistration has already been consumed: {consumed_marker}")
    if report_dir.exists() or report_dir.is_symlink():
        raise PairError(f"refusing to overwrite report directory: {report_dir}")
    report_dir.mkdir(parents=True)
    write_json(
        consumed_marker,
        {"preregistration_sha256": sha256_file(prereg_path), "report_dir": str(report_dir), "consumed_at": utc_now()},
        exclusive=True,
    )
    inputs = prereg["inputs"]
    initial_identity = {
        side: {
            "commit": inputs[side]["commit"],
            "tracked_diff_sha256": inputs[side]["tracked_diff_sha256"],
            "source_fingerprint_sha256": inputs[side]["source_fingerprint"]["sha256"],
            "binary_sha256": sha256_file(Path(inputs[side]["binary"])),
        }
        for side in ("baseline", "candidate")
    }
    protocol = prereg["protocol"]
    pinned_environment = prereg["runtime_environment"]
    report: dict[str, Any] = {
        "schema_version": FORMAT_VERSION,
        "format": PAIR_FORMAT,
        "created_at": utc_now(),
        "status": "running",
        "preregistration_path": str(prereg_path),
        "preregistration_sha256": sha256_file(prereg_path),
        "protocol": protocol,
        "host": {
            "system": platform.system(),
            "release": platform.release(),
            "machine": platform.machine(),
            "python": sys.version,
        },
        "initial_identity": initial_identity,
        "attempts": [],
        "warmup_pairs": [],
        "measured_pairs": [],
    }
    report_path = report_dir / "paired-report.json"
    write_json(report_path, report, exclusive=True)
    overall_failed = False
    total_pairs = WARMUP_PAIRS + MEASURED_PAIRS
    for global_pair in range(total_pairs):
        phase = "warmup" if global_pair < WARMUP_PAIRS else "measurement"
        phase_index = global_pair + 1 if phase == "warmup" else global_pair - WARMUP_PAIRS + 1
        order = pair_order(global_pair)
        pair_attempts: dict[str, dict[str, Any]] = {}
        for position, side in enumerate(order, start=1):
            identity = inputs[side]
            attempt_id = f"{phase}-{phase_index:02d}-{position}-{side}"
            metadata = {
                "attempt_id": attempt_id,
                "phase": phase,
                "phase_pair_index": phase_index,
                "global_pair_index": global_pair,
                "position_in_pair": position,
                "side": side,
                "commit": identity["commit"],
                "source_fingerprint_sha256": identity["source_fingerprint"]["sha256"],
                "binary_sha256": identity["binary_sha256"],
            }
            try:
                result = _attempt(identity, report_dir / "attempts" / attempt_id, metadata, pinned_environment)
            except (PairError, OSError) as error:
                result = {
                    **metadata,
                    "finished_at": utc_now(),
                    "successful": False,
                    "execution_error": str(error),
                    "attempt_dir": None,
                }
                overall_failed = True
            report["attempts"].append(result)
            pair_attempts[side] = result
            if result.get("successful") is not True:
                overall_failed = True
            write_json(report_path, report)
        pair = {
            "pair_index": phase_index,
            "global_pair_index": global_pair,
            "phase": phase,
            "order": list(order),
            **_pair_result(phase_index, pair_attempts["baseline"], pair_attempts["candidate"]),
        }
        if pair.get("full_exact_oracle_equal") is not True:
            overall_failed = True
        key = "warmup_pairs" if phase == "warmup" else "measured_pairs"
        report[key].append(pair)
        write_json(report_path, report)
    post_identity: dict[str, Any] = {}
    for side in ("baseline", "candidate"):
        identity = inputs[side]
        try:
            tree = _validate_tree(Path(identity["root"]), side)
            binary_hash = sha256_file(Path(identity["binary"]))
            unchanged = (
                tree["source_fingerprint"] == identity["source_fingerprint"]
                and binary_hash == identity["binary_sha256"]
            )
            if not unchanged:
                overall_failed = True
            post_identity[side] = {
                "source_fingerprint_sha256": tree["source_fingerprint"]["sha256"],
                "binary_sha256": binary_hash,
                "unchanged": unchanged,
            }
        except (PairError, OSError) as error:
            overall_failed = True
            post_identity[side] = {"unchanged": False, "error": str(error)}
    measured_pairs = report["measured_pairs"]
    report["post_identity"] = post_identity
    report["summary"] = _summary(measured_pairs)
    report["status"] = "completed_with_failures" if overall_failed else "completed"
    report["finished_at"] = utc_now()
    write_json(report_path, report)
    return 1 if overall_failed else 0


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)
    prepare_parser = subparsers.add_parser(
        "prepare-receipt",
        help="enrich a raw saved-binary receipt with source and compiler-log pins",
    )
    prepare_parser.add_argument("--side", choices=("baseline", "candidate"), required=True)
    prepare_parser.add_argument("--raw-receipt", required=True)
    prepare_parser.add_argument("--build-jsonl", required=True)
    prepare_parser.add_argument("--build-log", required=True)
    prepare_parser.add_argument("--release-clean-log", required=True)
    prepare_parser.add_argument("--source-root", required=True)
    prepare_parser.add_argument("--harness-source", required=True)
    prepare_parser.add_argument("--build-driver", required=True)
    prepare_parser.add_argument("--output", required=True)
    prepare_parser.set_defaults(handler=prepare_receipt)
    prepare_parser = subparsers.add_parser("preregister", help="validate receipts and freeze protocol before timing")
    prepare_parser.add_argument("--baseline-receipt", required=True)
    prepare_parser.add_argument("--candidate-receipt", required=True)
    prepare_parser.add_argument("--harness-source", required=True)
    prepare_parser.add_argument("--preregistration", required=True)
    prepare_parser.set_defaults(handler=preregister)
    run_parser = subparsers.add_parser("run", help="run one frozen no-retry paired attempt")
    run_parser.add_argument("--preregistration", required=True)
    run_parser.add_argument("--report-dir", required=True)
    run_parser.set_defaults(handler=run)
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)
    try:
        result = args.handler(args)
        return result if isinstance(result, int) else 0
    except PairError as error:
        parser.exit(2, f"program-module-cache-pair: error: {error}\n")
    except OSError as error:
        parser.exit(2, f"program-module-cache-pair: operating-system error: {error}\n")


if __name__ == "__main__":
    raise SystemExit(main())
