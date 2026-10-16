#!/usr/bin/env python3
"""Build immutable baseline/candidate text_json_decoder-benchmark test binaries.

This preparation driver does not execute tests or take timings. It performs a
clean optimized-release build of only `output_schema_text_json_decode` in each
arm, sequentially using the campaign's shared target directory, and preserves
the two exact test executables outside that directory.
"""
from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import shutil
import stat
import subprocess
import sys
from typing import Any

C = Path(__file__).resolve().parent.parent
EVIDENCE = C / "evidence/text-json-decoder"
RUN = EVIDENCE / "build-attempt-1"
SOURCE_BINDINGS = EVIDENCE / "source-fingerprints.json"
BASELINE = C / "work/text-json-decoder-baseline-3be9fe"
CANDIDATE = C / "work/text-json-decoder-candidate-3be9fe"
TARGET = C / "work/perf-target-candidate"
BASE_COMMIT = "3be9fe7eac71c371d3ec5c5a5fadb3161a83ea6c"
PRODUCT_PATCH_SHA256 = "95f4d413f86f0905421500f182e372e55a3c30163dc3364810ca71386ca03f36"
HARNESS_RELATIVE = Path("tests/output_schema_text_json_decode.rs")
TARGET_NAME = "output_schema_text_json_decode"
TEST_NAME = "text_json_decode_10000"
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
BUILD_ARGS = [
    "cargo", "test", "--locked", "--release", "-p", "execute",
    "--test", TARGET_NAME, "--no-run", "--message-format=json",
]
CLEAN_ARGS = ["cargo", "clean", "--release", "-p", "execute"]
CARGO_CONFIG_FILENAMES = ("config", "config.toml")
CARGO_CONFIG_SCHEMA = "execute.graph.cargo_configuration_fingerprint/1"


def sha_bytes(payload: bytes) -> str:
    return hashlib.sha256(payload).hexdigest()


def sha_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def require_unoptimized_python(optimize_level: int | None = None) -> None:
    level = sys.flags.optimize if optimize_level is None else optimize_level
    if level != 0:
        raise RuntimeError("build driver must run with Python optimization disabled (-O is unsupported)")


def git(root: Path, *arguments: str) -> bytes:
    return subprocess.run(
        ["git", *arguments], cwd=root, check=True,
        stdout=subprocess.PIPE, stderr=subprocess.PIPE,
    ).stdout


def listed_paths(root: Path) -> list[str]:
    tracked = git(root, "ls-files", "-z").split(b"\0")
    untracked = git(root, "ls-files", "--others", "--exclude-standard", "-z").split(b"\0")
    return sorted({os.fsdecode(item) for item in tracked + untracked if item})


def source_fingerprint(root: Path) -> dict[str, Any]:
    files: dict[str, str] = {}
    for relative in listed_paths(root):
        path = root / relative
        payload = b"symlink\0" + os.fsencode(os.readlink(path)) if path.is_symlink() else path.read_bytes()
        files[relative] = sha_bytes(payload)
    canonical = json.dumps(files, sort_keys=True, separators=(",", ":")).encode()
    return {"sha256": sha_bytes(canonical), "file_count": len(files), "files": files}


def diff_sha256(root: Path, *, staged: bool = False) -> str:
    arguments = ["diff", "--binary"]
    if staged:
        arguments.append("--cached")
    return sha_bytes(git(root, *arguments))


def status_paths(root: Path) -> list[str]:
    return sorted(
        line[3:]
        for line in git(root, "status", "--porcelain=v1", "--untracked-files=all").decode().splitlines()
    )


def write_new(path: Path, payload: bytes, mode: int = 0o444) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("xb") as stream:
        stream.write(payload)
        stream.flush()
        os.fsync(stream.fileno())
    path.chmod(mode)


def write_json_new(path: Path, value: Any, mode: int = 0o444) -> None:
    write_new(path, (json.dumps(value, indent=2, sort_keys=True, allow_nan=False) + "\n").encode(), mode)


def setup_environment(host: dict[str, str] | None = None) -> dict[str, str]:
    inherited = os.environ if host is None else host
    if not inherited.get("PATH") or not inherited.get("HOME"):
        raise RuntimeError("controlled build environment requires inherited PATH and HOME")
    environment = {key: inherited[key] for key in INHERITED_BUILD_ENV_KEYS if key in inherited}
    environment["PATH"] = os.pathsep.join((str(PINNED_CARGO_BIN), environment["PATH"]))
    environment.update(FIXED_BUILD_ENVIRONMENT)
    return dict(sorted(environment.items()))


def cargo_config_file_identity(path: Path) -> dict[str, Any]:
    path = path.absolute()
    present = path.exists() or path.is_symlink()
    result: dict[str, Any] = {
        "path": str(path), "present": present, "resolved_path": None,
        "sha256": None, "symlink_target": None,
    }
    if not present:
        return result
    if not path.is_file():
        raise RuntimeError(f"Cargo configuration path is present but not a regular file: {path}")
    result["resolved_path"] = str(path.resolve(strict=True))
    result["sha256"] = sha_file(path)
    if path.is_symlink():
        result["symlink_target"] = os.readlink(path)
    return result


def source_ancestors(root: Path) -> list[Path]:
    current = root.resolve(strict=True)
    directories = []
    while True:
        directories.append(current)
        if current == current.parent:
            return directories
        current = current.parent


def cargo_configuration_fingerprint(roots: dict[str, Path], environment: dict[str, str]) -> dict[str, Any]:
    home = Path(environment["HOME"]).expanduser().resolve(strict=True)
    cargo_home = Path(environment["CARGO_HOME"]).expanduser().absolute()
    if not home.is_dir() or not cargo_home.is_dir():
        raise RuntimeError("HOME and CARGO_HOME must exist for Cargo configuration capture")
    body = {
        "schema": CARGO_CONFIG_SCHEMA,
        "cargo_home": {
            "path": str(cargo_home),
            "config_files": [cargo_config_file_identity(cargo_home / name) for name in CARGO_CONFIG_FILENAMES],
        },
        "source_roots": {},
    }
    for side, root in sorted(roots.items()):
        resolved = root.resolve(strict=True)
        ancestors = source_ancestors(resolved)
        body["source_roots"][side] = {
            "root": str(resolved),
            "home_is_ancestor": resolved == home or home in resolved.parents,
            "config_files": [
                cargo_config_file_identity(directory / ".cargo" / name)
                for directory in ancestors for name in CARGO_CONFIG_FILENAMES
            ],
        }
    encoded = json.dumps(body, sort_keys=True, separators=(",", ":"), ensure_ascii=False, allow_nan=False).encode()
    return {**body, "sha256": sha_bytes(encoded)}


def capture_tool_environment(environment: dict[str, str], config: dict[str, Any]) -> dict[str, Any]:
    allowed = set(INHERITED_BUILD_ENV_KEYS) | set(FIXED_BUILD_ENVIRONMENT)
    if set(environment) - allowed or set(FIXED_BUILD_ENVIRONMENT) - set(environment):
        raise RuntimeError("build environment is not the exact allowlisted/pinned environment")
    for key, value in FIXED_BUILD_ENVIRONMENT.items():
        if environment.get(key) != value:
            raise RuntimeError(f"build environment does not pin {key}")
    cargo = PINNED_CARGO_BIN / "cargo"
    rustc = PINNED_CARGO_BIN / "rustc"
    cargo_version = subprocess.run([str(cargo), "--version", "--verbose"], env=environment, check=True,
                                   stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True).stdout
    rustc_version = subprocess.run([str(rustc), "--version", "--verbose"], env=environment, check=True,
                                   stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True).stdout
    return {
        "schema": "output_schema_text_json_decode_tool_environment/1",
        "captured_before_clean_or_build": True,
        "actual_environment": environment,
        "inherited_allowlist": list(INHERITED_BUILD_ENV_KEYS),
        "fixed_environment": FIXED_BUILD_ENVIRONMENT,
        "unlisted_inherited_variables_forwarded": False,
        "cargo_path": str(cargo), "cargo_version_verbose": cargo_version,
        "rustc_path": str(rustc), "rustc_version_verbose": rustc_version,
        "python_executable": sys.executable, "python_version": sys.version,
        "python_flags": repr(sys.flags), "python_optimize": sys.flags.optimize,
        "target_directory": str(TARGET.resolve()),
        "cargo_configuration_fingerprint": config,
    }


def validate_source_bindings(binding: dict[str, Any]) -> None:
    assert binding["schema"] == "output-schema-text-json-decoder-source-fingerprints/1"
    assert binding["base_commit"] == BASE_COMMIT
    assert binding["product_patch_sha256"] == PRODUCT_PATCH_SHA256
    assert binding["arms"]["baseline"]["tracked_diff_sha256"] == sha_bytes(b"")
    assert binding["arms"]["candidate"]["tracked_diff_sha256"] == PRODUCT_PATCH_SHA256
    assert binding["arms"]["baseline"]["staged_diff_sha256"] == sha_bytes(b"")
    assert binding["arms"]["candidate"]["staged_diff_sha256"] == sha_bytes(b"")
    for side, root in (("baseline", BASELINE), ("candidate", CANDIDATE)):
        frozen = binding["arms"][side]
        assert Path(frozen["root"]).resolve(strict=True) == root.resolve(strict=True)
        assert frozen["head"] == BASE_COMMIT
        assert frozen["tree"]["file_count"] == len(frozen["tree"]["files"]) == 5917
        assert source_fingerprint(root) == frozen["tree"], f"{side} source fingerprint drift"
        assert diff_sha256(root) == frozen["tracked_diff_sha256"]
        assert diff_sha256(root, staged=True) == frozen["staged_diff_sha256"]
        assert (root / HARNESS_RELATIVE).is_file()
        assert sha_file(root / HARNESS_RELATIVE) == binding["harness_source_sha256"]
        expected = [str(HARNESS_RELATIVE)] if side == "baseline" else sorted(["src/contracts.rs", str(HARNESS_RELATIVE)])
        assert status_paths(root) == expected


def run_logged(args: list[str], root: Path, environment: dict[str, str], stdout_path: Path, stderr_path: Path) -> int:
    with stdout_path.open("xb") as stdout, stderr_path.open("xb") as stderr:
        completed = subprocess.run(args, cwd=root, env=environment, stdin=subprocess.DEVNULL,
                                   stdout=stdout, stderr=stderr, check=False)
        stdout.flush(); stderr.flush()
        os.fsync(stdout.fileno()); os.fsync(stderr.fileno())
    stdout_path.chmod(0o444); stderr_path.chmod(0o444)
    return completed.returncode


def immutable_binary_copy(source: Path, destination: Path) -> str:
    if not source.is_file() or not os.access(source, os.X_OK):
        raise RuntimeError(f"Cargo test executable is absent or not executable: {source}")
    destination.parent.mkdir(parents=True, exist_ok=False)
    with source.open("rb") as input_file, destination.open("xb") as output_file:
        shutil.copyfileobj(input_file, output_file)
        output_file.flush(); os.fsync(output_file.fileno())
    destination.chmod(0o555)
    return sha_file(destination)


def validate_artifact(row: dict[str, Any], side: str, root: Path) -> Path:
    target = row.get("target", {})
    profile = row.get("profile", {})
    assert row.get("reason") == "compiler-artifact" and row.get("fresh") is False
    assert target.get("name") == TARGET_NAME and target.get("kind") == ["test"] and target.get("test") is True
    assert Path(row.get("manifest_path", "")).resolve() == (root / "Cargo.toml").resolve()
    assert Path(target.get("src_path", "")).resolve() == (root / HARNESS_RELATIVE).resolve()
    assert profile.get("test") is True and profile.get("opt_level") == "3" and profile.get("debug_assertions") is False
    assert isinstance(row.get("package_id"), str) and row["package_id"].startswith(f"path+{root.as_uri()}#execute@")
    executable = row.get("executable")
    assert isinstance(executable, str) and executable in row.get("filenames", [])
    binary = Path(executable).absolute()
    assert binary.is_relative_to(TARGET.resolve() / "release" / "deps")
    return binary


def build_arm(side: str, root: Path, binding: dict[str, Any], environment: dict[str, str], config: dict[str, Any],
              tool_environment_sha256: str, binding_sha256: str, driver_sha256: str) -> dict[str, Any]:
    arm_dir = RUN / side
    arm_dir.mkdir()
    before = source_fingerprint(root)
    frozen = binding["arms"][side]
    assert before == frozen["tree"] and diff_sha256(root) == frozen["tracked_diff_sha256"]
    assert diff_sha256(root, staged=True) == frozen["staged_diff_sha256"]
    receipt: dict[str, Any] = {
        "schema": "output_schema_text_json_decode_build_receipt/1", "side": side,
        "worktree": str(root.resolve()), "head": BASE_COMMIT, "status": "running",
        "source_fingerprint_sha256": before["sha256"], "source_fingerprint_file_count": before["file_count"],
        "tracked_diff_sha256": frozen["tracked_diff_sha256"], "staged_diff_sha256": frozen["staged_diff_sha256"],
        "product_patch_sha256": PRODUCT_PATCH_SHA256,
        "harness_source_sha256": binding["harness_source_sha256"],
        "harness_patch_sha256": binding["harness_patch_sha256"],
        "source_bindings_sha256": binding_sha256, "build_driver_sha256": driver_sha256,
        "tool_environment_sha256": tool_environment_sha256,
        "clean": CLEAN_ARGS, "build": BUILD_ARGS, "target_directory": str(TARGET.resolve()),
        "expected_artifact": {"target": TARGET_NAME, "source": str(HARNESS_RELATIVE)},
        "tests_executed": False, "clean_log": str(arm_dir / "clean.log"),
        "clean_stderr_log": str(arm_dir / "clean.stderr.log"),
        "build_stdout_jsonl": str(arm_dir / "build.jsonl"),
        "build_stderr_log": str(arm_dir / "build.stderr.log"),
        "cargo_configuration_fingerprint_before": config,
    }
    try:
        current = cargo_configuration_fingerprint({"baseline": BASELINE, "candidate": CANDIDATE}, environment)
        if current != config:
            raise RuntimeError("Cargo configuration changed before sequential build")
        clean_code = run_logged(CLEAN_ARGS, root, environment, arm_dir / "clean.log", arm_dir / "clean.stderr.log")
        receipt["clean_return_code"] = clean_code
        if clean_code != 0:
            raise RuntimeError(f"cargo clean failed: {clean_code}")
        build_code = run_logged(BUILD_ARGS, root, environment, arm_dir / "build.jsonl", arm_dir / "build.stderr.log")
        receipt["build_return_code"] = build_code
        if build_code != 0:
            raise RuntimeError(f"cargo test --no-run failed: {build_code}")
        rows = [json.loads(line) for line in (arm_dir / "build.jsonl").read_text().splitlines() if line.strip()]
        matches = [
            item for item in rows
            if item.get("reason") == "compiler-artifact"
            and item.get("target", {}).get("name") == TARGET_NAME
            and Path(item.get("target", {}).get("src_path", "")).resolve() == (root / HARNESS_RELATIVE).resolve()
        ]
        if len(matches) != 1:
            raise RuntimeError(f"expected exactly one Cargo artifact row, got {len(matches)}")
        artifact = matches[0]
        cargo_binary = validate_artifact(artifact, side, root)
        saved = arm_dir / "binaries" / TARGET_NAME
        saved_sha256 = immutable_binary_copy(cargo_binary, saved)
        receipt["artifact"] = {
            "cargo_json": artifact,
            "cargo_binary_path_lexical": str(cargo_binary),
            "saved_binary": str(saved),
            "saved_binary_sha256": saved_sha256,
            "saved_binary_mode": oct(stat.S_IMODE(saved.stat().st_mode)),
        }
        after = source_fingerprint(root)
        if after != before:
            raise RuntimeError("source changed during build")
        config_after = cargo_configuration_fingerprint({"baseline": BASELINE, "candidate": CANDIDATE}, environment)
        receipt["cargo_configuration_fingerprint_after"] = config_after
        receipt["cargo_configuration_stable"] = config_after == config
        if config_after != config:
            raise RuntimeError("Cargo configuration changed during build")
        receipt["source_fingerprint_after"] = {"sha256": after["sha256"], "file_count": after["file_count"]}
        receipt["source_stable"] = True
        receipt["status"] = "success"
    except Exception as error:
        receipt["status"] = "failed"
        receipt["error"] = f"{type(error).__name__}: {error}"
        receipt["source_stable"] = source_fingerprint(root) == before
        receipt["source_fingerprint_after"] = {
            "sha256": source_fingerprint(root)["sha256"], "file_count": source_fingerprint(root)["file_count"],
        }
        try:
            receipt["cargo_configuration_fingerprint_after"] = cargo_configuration_fingerprint(
                {"baseline": BASELINE, "candidate": CANDIDATE}, environment
            )
            receipt["cargo_configuration_stable"] = receipt["cargo_configuration_fingerprint_after"] == config
        except Exception as config_error:
            receipt["cargo_configuration_fingerprint_after_error"] = repr(config_error)
    for field, path in (("clean_log_sha256", arm_dir / "clean.log"),
                        ("clean_stderr_sha256", arm_dir / "clean.stderr.log"),
                        ("build_stdout_sha256", arm_dir / "build.jsonl"),
                        ("build_stderr_sha256", arm_dir / "build.stderr.log")):
        if path.is_file():
            receipt[field] = sha_file(path)
    write_json_new(arm_dir / "build-receipt.json", receipt)
    if receipt["status"] != "success":
        raise RuntimeError(receipt["error"])
    return receipt


def main() -> None:
    require_unoptimized_python()
    if RUN.exists():
        raise SystemExit(f"refusing existing build evidence directory: {RUN}")
    bindings_path = SOURCE_BINDINGS.resolve(strict=True)
    binding_bytes = bindings_path.read_bytes()
    binding = json.loads(binding_bytes)
    validate_source_bindings(binding)
    binding_sha = sha_bytes(binding_bytes)
    driver_sha = sha_file(Path(__file__).resolve(strict=True))
    environment = setup_environment()
    roots = {"baseline": BASELINE, "candidate": CANDIDATE}
    configuration = cargo_configuration_fingerprint(roots, environment)
    tool_environment = capture_tool_environment(environment, configuration)
    RUN.mkdir(parents=True, exist_ok=False)
    tool_path = RUN / "tool-environment.json"
    write_json_new(tool_path, tool_environment)
    tool_sha = sha_file(tool_path)
    command = {
        "schema": "output_schema_text_json_decode_build_pair_command/1",
        "created_before_builds": True, "build_driver": str(Path(__file__).resolve()),
        "build_driver_sha256": driver_sha, "source_bindings": str(bindings_path),
        "source_bindings_sha256": binding_sha, "product_patch_sha256": PRODUCT_PATCH_SHA256,
        "harness_source_sha256": binding["harness_source_sha256"],
        "harness_patch_sha256": binding["harness_patch_sha256"],
        "tool_environment": str(tool_path), "tool_environment_sha256": tool_sha,
        "cargo_configuration_fingerprint_before_sha256": configuration["sha256"],
        "shared_target_directory": str(TARGET.resolve()), "arms": {key: str(value.resolve()) for key, value in roots.items()},
        "clean_command_per_arm": CLEAN_ARGS, "build_command_per_arm": BUILD_ARGS,
        "tests_executed": False, "artifact_target": TARGET_NAME,
    }
    write_json_new(RUN / "build-pair-command.json", command)
    receipts = {}
    for side, root in roots.items():
        receipts[side] = build_arm(side, root, binding, environment, configuration, tool_sha, binding_sha, driver_sha)
    after_config = cargo_configuration_fingerprint(roots, environment)
    pair_receipt = {
        "schema": "output_schema_text_json_decode_build_pair_receipt/1",
        "status": "success", "tests_executed": False,
        "build_driver_sha256": driver_sha, "source_bindings_sha256": binding_sha,
        "product_patch_sha256": PRODUCT_PATCH_SHA256,
        "harness_source_sha256": binding["harness_source_sha256"],
        "harness_patch_sha256": binding["harness_patch_sha256"],
        "tool_environment_sha256": tool_sha,
        "cargo_configuration_fingerprint_before": configuration,
        "cargo_configuration_fingerprint_after": after_config,
        "cargo_configuration_stable": configuration == after_config,
        "shared_target_directory": str(TARGET.resolve()),
        "arms": {side: str(RUN / side / "build-receipt.json") for side in receipts},
    }
    if not pair_receipt["cargo_configuration_stable"]:
        raise RuntimeError("Cargo configuration changed across paired builds")
    if any(item["status"] != "success" for item in receipts.values()):
        raise RuntimeError("a build arm failed")
    write_json_new(RUN / "build-pair-receipt.json", pair_receipt)
    print(json.dumps({"build_run": str(RUN), "status": "success", "arms": sorted(receipts)}, sort_keys=True))


if __name__ == "__main__":
    main()
