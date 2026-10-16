#!/usr/bin/env python3
"""Clean-release build and preserve typed-JSON carry benchmark test binaries.

This build-only driver never runs tests or benchmark timings. It builds each
arm sequentially in one shared target directory, copies every requested test
binary to a new immutable evidence path, and records artifact/source identity.
"""
from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import shutil
import stat
import subprocess

C = Path(__file__).resolve().parent.parent
EVIDENCE_ROOT = C / "evidence/typed-json-carry-bench"
RUN = EVIDENCE_ROOT / "build-attempt-v3"
SOURCE_BINDINGS = EVIDENCE_ROOT / "source-fingerprints-v3.json"
BASELINE = C / "work/typed-json-carry-bench-baseline-e5fa516"
CANDIDATE = C / "work/typed-json-carry-v3-bench-candidate-e5fa516"
TARGET = C / "work/perf-target-candidate"
BASE_COMMIT = "e5fa5162b9bdca608f7f3c9d0d273db40a93c17b"
PRODUCT_PATCH_SHA256 = "99650ce4d9c2a2d4c8418e52bc3dbbda2f99a2a745e54c48332f9dc6a1958bb9"
HARNESS_PATCH_SHA256 = "3a227a33319519bbbcd64f29678c4d3879bba0121383f2cfc8afe9511c4d1141"
HARNESS_SOURCE_SHA256 = "ba0336a0749d86e4b4f28231daea6fa36cb85a3020f93302bb0b94d83260f68b"
EMPTY_DIFF_SHA256 = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"

PINNED_CARGO_BIN = Path("/Users/earlcameron/Documents/Codex/2026-10-12/you/work/rust/cargo/bin")
PINNED_CARGO_HOME = Path("/Users/earlcameron/Documents/Codex/2026-10-12/you/work/rust/cargo")
PINNED_RUSTUP_HOME = Path("/Users/earlcameron/Documents/Codex/2026-10-12/you/work/rust/rustup")
INHERITED_BUILD_ENV_KEYS = (
    "PATH",
    "HOME",
    "TMPDIR",
    "LANG",
    "LC_ALL",
    "TZ",
    "SDKROOT",
    "DEVELOPER_DIR",
    "MACOSX_DEPLOYMENT_TARGET",
)
FIXED_BUILD_ENVIRONMENT = {
    "CARGO_HOME": str(PINNED_CARGO_HOME),
    "RUSTUP_HOME": str(PINNED_RUSTUP_HOME),
    "RUSTUP_TOOLCHAIN": "stable-aarch64-apple-darwin",
    "CARGO_TARGET_DIR": str(TARGET.resolve()),
    "CARGO_BUILD_JOBS": "4",
}
CARGO_CONFIG_FILENAMES = ("config", "config.toml")
CARGO_CONFIG_SCHEMA = "execute.graph.cargo_configuration_fingerprint/1"
CARGO_CONFIG_POLICY = (
    "SHA-256 presence/path/symlink-target/content fingerprints for config and config.toml in CARGO_HOME and "
    "each .cargo directory from both source roots through filesystem root; verify before and after each "
    "sequential clean/build; record hashes, never config contents"
)
BUILD_ENVIRONMENT_POLICY = {
    "inherited_allowlist": list(INHERITED_BUILD_ENV_KEYS),
    "fixed_values": FIXED_BUILD_ENVIRONMENT,
    "recorded_actual_environment_values": True,
    "unlisted_inherited_variables_forwarded": False,
    "cargo_configuration_capture": CARGO_CONFIG_POLICY,
}

ARTIFACT_TARGETS = {
    "flow": Path("src/lib.rs"),
    "typed_contracts": Path("tests/typed_contracts.rs"),
    "opt_typed_json_carry": Path("tests/opt_typed_json_carry.rs"),
}
HELPER_TARGET = "execute-applet"
HELPER_SOURCE = Path("src/bin/execute-applet.rs")
BUILD_ARGS = [
    "cargo", "test", "--locked", "--release", "-p", "execute",
    "--lib", "--test", "typed_contracts", "--test", "opt_typed_json_carry",
    "--no-run", "--message-format=json",
]
HELPER_BUILD_ARGS = [
    "cargo", "build", "--locked", "--release", "-p", "execute", "--bin",
    HELPER_TARGET, "--message-format=json",
]
CLEAN_ARGS = ["cargo", "clean", "--release", "-p", "execute"]
CANDIDATE_TYPED_CONTRACT_TESTS = [
    "deep_in_process_json_binding_matches_cache_hit_byte_decoding",
    "in_process_json_output_binding_preserves_result_envelope_and_stdout_bytes",
]
CANDIDATE_LIB_TESTS = [
    "contracts::typed_json_decode_tests::predecoded_json_uses_the_same_schema_validation_as_bytes",
    "contracts::typed_json_decode_tests::predecoded_json_keeps_schema_errors_and_result_envelope_semantics",
    "contracts::typed_json_decode_tests::predecoded_json_falls_back_to_bytes_at_the_strict_parser_depth_boundary",
]


def sha_bytes(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def sha_file(path: Path) -> str:
    return sha_bytes(path.read_bytes())


def write_new(path: Path, data: bytes, mode: int = 0o444) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("xb") as output:
        output.write(data)
        output.flush()
        os.fsync(output.fileno())
    path.chmod(mode)


def write_json_new(path: Path, value) -> None:
    write_new(path, (json.dumps(value, indent=2, sort_keys=True) + "\n").encode())


def git(root: Path, *args: str) -> bytes:
    return subprocess.run(
        ["git", *args], cwd=root, check=True, stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    ).stdout


def listed_paths(root: Path) -> list[str]:
    tracked = git(root, "ls-files", "-z").split(b"\0")
    untracked = git(root, "ls-files", "--others", "--exclude-standard", "-z").split(b"\0")
    paths = {os.fsdecode(item) for item in tracked + untracked if item}
    return sorted(paths)


def source_fingerprint(root: Path) -> dict:
    files = {}
    for relative in listed_paths(root):
        path = root / relative
        if path.is_symlink():
            content = b"symlink\0" + os.fsencode(os.readlink(path))
        else:
            content = path.read_bytes()
        files[relative] = sha_bytes(content)
    canonical = json.dumps(files, sort_keys=True, separators=(",", ":")).encode()
    return {
        "sha256": sha_bytes(canonical),
        "file_count": len(files),
        "files": files,
    }


def tracked_diff_sha256(root: Path) -> str:
    return sha_bytes(git(root, "diff", "--binary"))


def staged_diff_sha256(root: Path) -> str:
    return sha_bytes(git(root, "diff", "--cached", "--binary"))


def status_paths(root: Path) -> list[str]:
    raw = git(root, "status", "--porcelain=v1", "--untracked-files=all").decode()
    return sorted(line[3:] for line in raw.splitlines())


def expected_status(side: str) -> list[str]:
    harness = "tests/opt_typed_json_carry.rs"
    if side == "baseline":
        return [harness]
    return sorted([
        "src/contracts.rs",
        "src/manifest.rs",
        "src/runtime.rs",
        "src/runtime/program_backend.rs",
        "src/runtime/windows_process.rs",
        "tests/opt_typed_json_carry.rs",
        "tests/typed_contracts.rs",
    ])


def setup_environment(host_environment: dict[str, str] | None = None) -> dict[str, str]:
    host = os.environ if host_environment is None else host_environment
    if not host.get("PATH") or not host.get("HOME"):
        raise RuntimeError("the controlled build environment requires inherited PATH and HOME")
    environment = {key: host[key] for key in INHERITED_BUILD_ENV_KEYS if key in host}
    environment["PATH"] = os.pathsep.join((str(PINNED_CARGO_BIN), environment["PATH"]))
    environment.update(FIXED_BUILD_ENVIRONMENT)
    return dict(sorted(environment.items()))


def cargo_config_file_identity(path: Path) -> dict:
    path = path.absolute()
    present = path.exists() or path.is_symlink()
    identity: dict[str, str | bool | None] = {
        "path": str(path),
        "present": present,
        "resolved_path": None,
        "sha256": None,
        "symlink_target": None,
    }
    if not present:
        return identity
    if not path.is_file():
        raise RuntimeError(f"Cargo configuration path is present but not a regular file: {path}")
    identity["resolved_path"] = str(path.resolve(strict=True))
    identity["sha256"] = sha_file(path)
    if path.is_symlink():
        identity["symlink_target"] = os.readlink(path)
    return identity


def source_ancestor_directories(root: Path, home: Path) -> tuple[list[Path], bool]:
    root = root.resolve(strict=True)
    home = home.resolve(strict=True)
    directories: list[Path] = []
    current = root
    home_is_ancestor = current == home or home in current.parents
    while True:
        directories.append(current)
        if current == current.parent:
            return directories, home_is_ancestor
        current = current.parent


def cargo_configuration_fingerprint(
    source_roots: dict[str, Path], environment: dict[str, str]
) -> dict:
    home = Path(environment["HOME"]).expanduser().resolve(strict=True)
    cargo_home = Path(environment["CARGO_HOME"]).expanduser().absolute()
    if not home.is_dir() or not cargo_home.is_dir():
        raise RuntimeError("HOME and CARGO_HOME must be existing directories for configuration capture")
    cargo_home_files = [
        cargo_config_file_identity(cargo_home / filename)
        for filename in CARGO_CONFIG_FILENAMES
    ]
    roots = {}
    for side, root in sorted(source_roots.items()):
        resolved_root = root.resolve(strict=True)
        ancestors, home_is_ancestor = source_ancestor_directories(resolved_root, home)
        config_files = [
            cargo_config_file_identity(directory / ".cargo" / filename)
            for directory in ancestors
            for filename in CARGO_CONFIG_FILENAMES
        ]
        roots[side] = {
            "root": str(resolved_root),
            "home_is_ancestor": home_is_ancestor,
            "config_files": config_files,
        }
    body = {
        "schema": CARGO_CONFIG_SCHEMA,
        "cargo_home": {"path": str(cargo_home), "config_files": cargo_home_files},
        "source_roots": roots,
    }
    encoded = json.dumps(
        body, sort_keys=True, separators=(",", ":"), ensure_ascii=False, allow_nan=False
    ).encode("utf-8")
    return {**body, "sha256": sha_bytes(encoded)}


def verify_cargo_configuration_fingerprint(
    expected: dict, source_roots: dict[str, Path], environment: dict[str, str], label: str
) -> None:
    current = cargo_configuration_fingerprint(source_roots, environment)
    if current != expected:
        raise RuntimeError(f"Cargo configuration inputs changed {label} the sequential build")


def capture_tool_environment(environment: dict[str, str], cargo_configuration: dict) -> dict:
    allowed = set(INHERITED_BUILD_ENV_KEYS) | set(FIXED_BUILD_ENVIRONMENT)
    if set(environment) - allowed:
        raise RuntimeError("controlled build environment contains an unallowlisted variable")
    if set(FIXED_BUILD_ENVIRONMENT) - set(environment):
        raise RuntimeError("controlled build environment is missing a fixed build variable")
    for key, expected in FIXED_BUILD_ENVIRONMENT.items():
        if environment.get(key) != expected:
            raise RuntimeError(f"controlled build environment does not use pinned {key}")
    cargo_path = str(PINNED_CARGO_BIN / "cargo")
    rustc_path = str(PINNED_CARGO_BIN / "rustc")
    if not Path(cargo_path).is_file() or not Path(rustc_path).is_file():
        raise RuntimeError("pinned cargo/rustc are unavailable in the captured tool directory")
    cargo = subprocess.run(
        [cargo_path, "--version", "--verbose"], env=environment,
        stdout=subprocess.PIPE, stderr=subprocess.STDOUT, check=True, text=True,
    ).stdout
    rustc = subprocess.run(
        [rustc_path, "--version", "--verbose"], env=environment,
        stdout=subprocess.PIPE, stderr=subprocess.STDOUT, check=True, text=True,
    ).stdout
    return {
        "environment": dict(sorted(environment.items())),
        "environment_policy": BUILD_ENVIRONMENT_POLICY,
        "cargo_configuration_fingerprint": cargo_configuration,
        "cargo_path": cargo_path,
        "cargo_version_verbose": cargo,
        "rustc_path": rustc_path,
        "rustc_version_verbose": rustc,
        "target_directory": str(TARGET.resolve()),
    }


def run_logged(args: list[str], cwd: Path, environment: dict[str, str], stdout_path: Path, stderr_path: Path) -> int:
    stdout_path.parent.mkdir(parents=True, exist_ok=True)
    with stdout_path.open("xb") as stdout, stderr_path.open("xb") as stderr:
        try:
            completed = subprocess.run(args, cwd=cwd, env=environment, stdout=stdout, stderr=stderr, check=False)
        finally:
            stdout.flush()
            stderr.flush()
            os.fsync(stdout.fileno())
            os.fsync(stderr.fileno())
            stdout_path.chmod(0o444)
            stderr_path.chmod(0o444)
    return completed.returncode


def validate_artifact(artifact: dict, side: str, root: Path, target_name: str) -> Path:
    assert artifact.get("fresh") is False, (side, target_name, artifact.get("fresh"))
    assert artifact.get("profile", {}).get("test") is True, (side, target_name, artifact.get("profile"))
    assert artifact.get("profile", {}).get("opt_level") == "3", (side, target_name, artifact.get("profile"))
    assert artifact.get("profile", {}).get("debug_assertions") is False, (side, target_name, artifact.get("profile"))
    assert Path(artifact["manifest_path"]).resolve() == (root / "Cargo.toml").resolve(), (side, target_name, artifact["manifest_path"])
    assert artifact.get("target", {}).get("name") == target_name, (side, target_name, artifact.get("target"))
    assert Path(artifact["target"]["src_path"]).resolve() == (root / ARTIFACT_TARGETS[target_name]).resolve(), (side, target_name, artifact["target"].get("src_path"))
    binary = Path(artifact["executable"]).resolve()
    assert binary.is_file(), (side, target_name, str(binary))
    assert binary.is_relative_to(TARGET.resolve() / "release" / "deps"), (side, target_name, str(binary))
    return binary


def validate_helper_artifact(artifact: dict, side: str, root: Path) -> Path:
    assert artifact.get("fresh") is False, (side, HELPER_TARGET, artifact.get("fresh"))
    assert artifact.get("profile", {}).get("test") is False, (side, HELPER_TARGET, artifact.get("profile"))
    assert artifact.get("profile", {}).get("opt_level") == "3", (side, HELPER_TARGET, artifact.get("profile"))
    assert artifact.get("profile", {}).get("debug_assertions") is False, (side, HELPER_TARGET, artifact.get("profile"))
    assert Path(artifact["manifest_path"]).resolve() == (root / "Cargo.toml").resolve(), (side, HELPER_TARGET, artifact["manifest_path"])
    assert artifact.get("target", {}).get("name") == HELPER_TARGET, (side, HELPER_TARGET, artifact.get("target"))
    assert Path(artifact["target"]["src_path"]).resolve() == (root / HELPER_SOURCE).resolve(), (side, HELPER_TARGET, artifact["target"].get("src_path"))
    binary = Path(artifact["executable"]).resolve()
    expected = TARGET.resolve() / "release" / HELPER_TARGET
    assert binary.is_file(), (side, HELPER_TARGET, str(binary))
    assert binary == expected, (side, HELPER_TARGET, str(binary), str(expected))
    return binary


def immutable_binary_copy(source: Path, destination: Path) -> str:
    destination.parent.mkdir(parents=True, exist_ok=True)
    with source.open("rb") as input_file, destination.open("xb") as output_file:
        shutil.copyfileobj(input_file, output_file)
        output_file.flush()
        os.fsync(output_file.fileno())
    destination.chmod((source.stat().st_mode & 0o555) or 0o555)
    return sha_file(destination)


def list_tests(binary: Path, root: Path, environment: dict[str, str], destination: Path) -> dict:
    completed = subprocess.run(
        [str(binary), "--list"], cwd=root, env=environment,
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=False,
    )
    write_new(destination, completed.stdout)
    stderr_path = destination.with_suffix(destination.suffix + ".stderr.log")
    write_new(stderr_path, completed.stderr)
    if completed.returncode != 0:
        raise RuntimeError(f"test-list command failed for {binary}: {completed.returncode}")
    listing = completed.stdout.decode("utf-8", errors="strict")
    assert listing.strip(), f"empty test list from {binary}"
    return {
        "binary": str(binary),
        "command": [str(binary), "--list"],
        "return_code": completed.returncode,
        "test_list_path": str(destination),
        "test_list_sha256": sha_bytes(completed.stdout),
        "stderr_path": str(stderr_path),
        "stderr_sha256": sha_bytes(completed.stderr),
        "test_count_lines": sum(line.rstrip().endswith(": test") for line in listing.splitlines()),
        "tests_not_executed": True,
    }


def require_named_tests(listings: dict[str, str], side: str) -> None:
    assert "literal_number_add_1000: test" in listings["opt_typed_json_carry"]
    if side == "baseline":
        assert "invalid_typed_output_schemas_fail_during_manifest_load: test" in listings["typed_contracts"]
        return
    for name in CANDIDATE_TYPED_CONTRACT_TESTS:
        assert f"{name}: test" in listings["typed_contracts"], name
    for name in CANDIDATE_LIB_TESTS:
        assert f"{name}: test" in listings["flow"], name


def build_arm(
    side: str,
    root: Path,
    environment: dict[str, str],
    expected_fingerprint: dict,
    expected_cargo_configuration: dict,
    tool_environment_sha256: str,
) -> dict:
    arm_dir = RUN / side
    arm_dir.mkdir()
    before = source_fingerprint(root)
    cargo_configuration_before = cargo_configuration_fingerprint(
        {"baseline": BASELINE, "candidate": CANDIDATE}, environment
    )
    assert before == expected_fingerprint["tree"], f"{side} source differs from frozen source fingerprint"
    assert tracked_diff_sha256(root) == expected_fingerprint["tracked_diff_sha256"]
    assert staged_diff_sha256(root) == expected_fingerprint["staged_diff_sha256"]
    assert status_paths(root) == expected_status(side), (side, status_paths(root))
    assert (root / "tests/opt_typed_json_carry.rs").is_file()
    assert sha_file(root / "tests/opt_typed_json_carry.rs") == HARNESS_SOURCE_SHA256

    commands = {
        "side": side,
        "worktree": str(root),
        "head": git(root, "rev-parse", "HEAD").decode().strip(),
        "clean": CLEAN_ARGS,
        "build": BUILD_ARGS,
        "helper_build": HELPER_BUILD_ARGS,
        "target_directory": str(TARGET.resolve()),
        "expected_artifacts": {
            name: str(source) for name, source in ARTIFACT_TARGETS.items()
        },
        "source_fingerprint_sha256": before["sha256"],
        "tracked_diff_sha256": expected_fingerprint["tracked_diff_sha256"],
        "staged_diff_sha256": expected_fingerprint["staged_diff_sha256"],
        "product_patch_sha256": PRODUCT_PATCH_SHA256,
        "harness_patch_sha256": HARNESS_PATCH_SHA256,
        "harness_source_sha256": HARNESS_SOURCE_SHA256,
        "build_driver_sha256": sha_file(Path(__file__).resolve()),
        "tool_environment_sha256": tool_environment_sha256,
        "cargo_configuration_fingerprint_before_sha256": cargo_configuration_before["sha256"],
        "tests_executed": False,
    }
    write_json_new(arm_dir / "build-command.json", commands)
    receipt = {
        **commands,
        "status": "running",
        "clean_log": str(arm_dir / "clean.log"),
        "build_stdout_jsonl": str(arm_dir / "build.jsonl"),
        "build_stderr_log": str(arm_dir / "build.stderr.log"),
        "helper_build_stdout_jsonl": str(arm_dir / "helper-build.jsonl"),
        "helper_build_stderr_log": str(arm_dir / "helper-build.stderr.log"),
        "helper_target": HELPER_TARGET,
        "helper_source": str(HELPER_SOURCE),
        "cargo_configuration_fingerprint_before": cargo_configuration_before,
        "cargo_configuration_fingerprint_after": None,
        "cargo_configuration_stable": None,
        "artifacts": {},
        "helper_artifact": {},
        "test_lists": {},
    }

    try:
        verify_cargo_configuration_fingerprint(
            expected_cargo_configuration,
            {"baseline": BASELINE, "candidate": CANDIDATE},
            environment,
            f"before {side} clean/build",
        )
        if cargo_configuration_before != expected_cargo_configuration:
            raise RuntimeError(f"Cargo configuration changed before {side} clean/build")
        clean_code = run_logged(CLEAN_ARGS, root, environment, arm_dir / "clean.log", arm_dir / "clean.stderr.log")
        receipt["clean_return_code"] = clean_code
        if clean_code != 0:
            raise RuntimeError(f"cargo clean failed for {side} with code {clean_code}")

        helper_build_code = run_logged(
            HELPER_BUILD_ARGS,
            root,
            environment,
            arm_dir / "helper-build.jsonl",
            arm_dir / "helper-build.stderr.log",
        )
        receipt["helper_build_return_code"] = helper_build_code
        if helper_build_code != 0:
            raise RuntimeError(f"cargo helper build failed for {side} with code {helper_build_code}")

        helper_rows = [
            json.loads(line)
            for line in (arm_dir / "helper-build.jsonl").read_text().splitlines()
            if line.strip()
        ]
        helper_artifacts = [
            row for row in helper_rows
            if row.get("reason") == "compiler-artifact"
            and row.get("target", {}).get("name") == HELPER_TARGET
            and row.get("executable")
        ]
        assert len(helper_artifacts) == 1, (side, HELPER_TARGET, helper_artifacts)
        helper_artifact = helper_artifacts[0]
        helper_binary = validate_helper_artifact(helper_artifact, side, root)
        saved_helper = arm_dir / "binaries" / HELPER_TARGET
        helper_sha = immutable_binary_copy(helper_binary, saved_helper)
        receipt["helper_artifact"] = {
            "cargo_artifact": helper_artifact,
            "binary_path": str(helper_binary),
            "binary_sha256": sha_file(helper_binary),
            "saved_binary": str(saved_helper),
            "saved_binary_sha256": helper_sha,
            "saved_binary_mode": oct(stat.S_IMODE(saved_helper.stat().st_mode)),
            "saved_beside_test_binaries": True,
            "helper_not_executed": True,
        }

        build_code = run_logged(BUILD_ARGS, root, environment, arm_dir / "build.jsonl", arm_dir / "build.stderr.log")
        receipt["build_return_code"] = build_code
        if build_code != 0:
            raise RuntimeError(f"cargo build failed for {side} with code {build_code}")

        rows = [json.loads(line) for line in (arm_dir / "build.jsonl").read_text().splitlines() if line.strip()]
        artifacts = [
            row for row in rows
            if row.get("reason") == "compiler-artifact"
            and row.get("profile", {}).get("test")
            and row.get("executable")
            and row.get("target", {}).get("name") in ARTIFACT_TARGETS
        ]
        for target_name, expected_source in ARTIFACT_TARGETS.items():
            matched = [
                item for item in artifacts
                if item["target"]["name"] == target_name
                and Path(item["target"]["src_path"]).resolve() == (root / expected_source).resolve()
            ]
            assert len(matched) == 1, (side, target_name, matched)
            artifact = matched[0]
            binary = validate_artifact(artifact, side, root, target_name)
            saved = arm_dir / "binaries" / f"{target_name}-test-binary"
            binary_sha = immutable_binary_copy(binary, saved)
            receipt["artifacts"][target_name] = {
                "cargo_artifact": artifact,
                "binary_path": str(binary),
                "binary_sha256": sha_file(binary),
                "saved_binary": str(saved),
                "saved_binary_sha256": binary_sha,
                "saved_binary_mode": oct(stat.S_IMODE(saved.stat().st_mode)),
            }

        listings = {}
        for target_name in ARTIFACT_TARGETS:
            saved = Path(receipt["artifacts"][target_name]["saved_binary"])
            test_list_path = arm_dir / "test-lists" / f"{target_name}.txt"
            listing_receipt = list_tests(saved, root, environment, test_list_path)
            receipt["test_lists"][target_name] = listing_receipt
            listings[target_name] = test_list_path.read_text()
        require_named_tests(listings, side)

        after = source_fingerprint(root)
        assert after == before, f"{side} source changed during build/listing"
        receipt["source_fingerprint_after"] = {
            "sha256": after["sha256"],
            "file_count": after["file_count"],
        }
        cargo_configuration_after = cargo_configuration_fingerprint(
            {"baseline": BASELINE, "candidate": CANDIDATE}, environment
        )
        receipt["cargo_configuration_fingerprint_after"] = cargo_configuration_after
        receipt["cargo_configuration_stable"] = cargo_configuration_after == cargo_configuration_before
        if not receipt["cargo_configuration_stable"]:
            raise RuntimeError(f"Cargo configuration changed during {side} clean/build")
        receipt["source_stable"] = True
        receipt["status"] = "success"
    except Exception as error:
        receipt["status"] = "failed"
        receipt["error"] = repr(error)
        try:
            after = source_fingerprint(root)
            receipt["source_fingerprint_after"] = {
                "sha256": after["sha256"],
                "file_count": after["file_count"],
            }
            receipt["source_stable"] = after == before
        except Exception as fingerprint_error:
            receipt["source_fingerprint_error"] = repr(fingerprint_error)
        if receipt["cargo_configuration_fingerprint_after"] is None:
            try:
                cargo_configuration_after = cargo_configuration_fingerprint(
                    {"baseline": BASELINE, "candidate": CANDIDATE}, environment
                )
                receipt["cargo_configuration_fingerprint_after"] = cargo_configuration_after
                receipt["cargo_configuration_stable"] = cargo_configuration_after == cargo_configuration_before
            except Exception as configuration_error:
                receipt["cargo_configuration_fingerprint_after_error"] = repr(configuration_error)
        for log_name, log_path in (
            ("clean_log_sha256", arm_dir / "clean.log"),
            ("clean_stderr_sha256", arm_dir / "clean.stderr.log"),
            ("build_stdout_sha256", arm_dir / "build.jsonl"),
            ("build_stderr_sha256", arm_dir / "build.stderr.log"),
            ("helper_build_stdout_sha256", arm_dir / "helper-build.jsonl"),
            ("helper_build_stderr_sha256", arm_dir / "helper-build.stderr.log"),
        ):
            if log_path.exists():
                receipt[log_name] = sha_file(log_path)
        write_json_new(arm_dir / "build-receipt.json", receipt)
        raise

    receipt["clean_log_sha256"] = sha_file(arm_dir / "clean.log")
    receipt["clean_stderr_sha256"] = sha_file(arm_dir / "clean.stderr.log")
    receipt["build_stdout_sha256"] = sha_file(arm_dir / "build.jsonl")
    receipt["build_stderr_sha256"] = sha_file(arm_dir / "build.stderr.log")
    receipt["helper_build_stdout_sha256"] = sha_file(arm_dir / "helper-build.jsonl")
    receipt["helper_build_stderr_sha256"] = sha_file(arm_dir / "helper-build.stderr.log")
    write_json_new(arm_dir / "build-receipt.json", receipt)
    return receipt


def main() -> None:
    if RUN.exists():
        raise SystemExit(f"refusing to reuse existing build evidence directory: {RUN}")
    if not SOURCE_BINDINGS.is_file():
        raise SystemExit(f"missing frozen source bindings: {SOURCE_BINDINGS}")
    binding = json.loads(SOURCE_BINDINGS.read_text())
    assert binding["base_commit"] == BASE_COMMIT
    assert binding["product_patch_sha256"] == PRODUCT_PATCH_SHA256
    assert binding["harness_patch_sha256"] == HARNESS_PATCH_SHA256
    assert binding["harness_source_sha256"] == HARNESS_SOURCE_SHA256

    arms = {"baseline": BASELINE, "candidate": CANDIDATE}
    for side, root in arms.items():
        assert root.is_dir(), (side, str(root))
        assert git(root, "rev-parse", "HEAD").decode().strip() == BASE_COMMIT
        assert tracked_diff_sha256(root) == binding["arms"][side]["tracked_diff_sha256"]
        assert staged_diff_sha256(root) == binding["arms"][side]["staged_diff_sha256"] == EMPTY_DIFF_SHA256
        assert binding["arms"][side]["tracked_diff_sha256"] == (EMPTY_DIFF_SHA256 if side == "baseline" else PRODUCT_PATCH_SHA256)

    RUN.mkdir()
    environment = setup_environment()
    source_roots = {"baseline": BASELINE, "candidate": CANDIDATE}
    cargo_configuration_before = cargo_configuration_fingerprint(source_roots, environment)
    build_driver_sha256 = sha_file(Path(__file__).resolve())
    tool_environment = capture_tool_environment(environment, cargo_configuration_before)
    write_json_new(RUN / "tool-environment.json", {
        "captured_before_clean_or_build": True,
        "build_driver_sha256": build_driver_sha256,
        **tool_environment,
    })
    tool_environment_sha256 = sha_file(RUN / "tool-environment.json")
    write_json_new(RUN / "build-pair-command.json", {
        "arms": {side: str(root) for side, root in arms.items()},
        "order": ["baseline", "candidate"],
        "shared_target_directory": str(TARGET.resolve()),
        "build_driver_sha256": build_driver_sha256,
        "tool_environment_sha256": tool_environment_sha256,
        "cargo_configuration_fingerprint_before_sha256": cargo_configuration_before["sha256"],
        "clean_command_per_arm": CLEAN_ARGS,
        "build_command_per_arm": BUILD_ARGS,
        "helper_build_command_per_arm": HELPER_BUILD_ARGS,
        "source_bindings_sha256": sha_file(SOURCE_BINDINGS),
        "tool_environment_sha256": sha_file(RUN / "tool-environment.json"),
        "tests_executed": False,
        "test_binaries_listed_only": True,
        "execute_applet_helper_built_and_copied_not_executed": True,
    })

    receipts = {}
    for side, root in arms.items():
        receipts[side] = build_arm(
            side,
            root,
            environment,
            binding["arms"][side],
            cargo_configuration_before,
            tool_environment_sha256,
        )
    cargo_configuration_after = cargo_configuration_fingerprint(source_roots, environment)
    if cargo_configuration_after != cargo_configuration_before:
        raise RuntimeError("Cargo configuration changed during the sequential baseline/candidate builds")
    write_json_new(RUN / "build-pair-receipt.json", {
        "status": "success",
        "source_bindings_sha256": sha_file(SOURCE_BINDINGS),
        "product_patch_sha256": PRODUCT_PATCH_SHA256,
        "harness_patch_sha256": HARNESS_PATCH_SHA256,
        "helper_build_command_per_arm": HELPER_BUILD_ARGS,
        "shared_target_directory": str(TARGET.resolve()),
        "tool_environment_sha256": tool_environment_sha256,
        "cargo_configuration_fingerprint_before": cargo_configuration_before,
        "cargo_configuration_fingerprint_after": cargo_configuration_after,
        "cargo_configuration_stable": cargo_configuration_after == cargo_configuration_before,
        "arms": {
            side: str(RUN / side / "build-receipt.json") for side in receipts
        },
        "tests_executed": False,
    })
    print(json.dumps({
        "status": "success",
        "saved_binary_root": str(RUN),
        "arms": sorted(receipts),
    }), flush=True)


if __name__ == "__main__":
    main()
