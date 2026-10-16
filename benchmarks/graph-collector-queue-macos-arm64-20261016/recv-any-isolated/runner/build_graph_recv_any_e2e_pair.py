#!/usr/bin/env python3
"""Build and save immutable recv_any E2E test binaries, cleaning one shared target between arms."""
from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
from typing import Any


CAMPAIGN = Path(__file__).resolve().parents[1]
EVIDENCE = CAMPAIGN / "evidence/graph-recv-any-single-queue"
BASELINE_ROOT = CAMPAIGN / "work/graph-recv-any-collector-baseline-a884"
CANDIDATE_ROOT = CAMPAIGN / "work/graph-recv-any-collector-queue-a884"
TARGET = CAMPAIGN / "work/perf-target-candidate"
HARNESS_RELATIVE = Path("tests/graph_recv_any_e2e_bench.rs")
HARNESS_SHA256 = "c38f59198e056576b69481e10bfa0cf2ac95124095869550a0d863ceb2bb18c1"
BASE_COMMIT = "a884f6561fd43f45e43b25a3cc2233be17ee3944"
TRANSPORT_SHA256 = {
    "baseline": "bfc03a7028b2871a4823ed0a130e3b735ca8dc535cac9c12cd39d876174dad9c",
    "candidate": "ac546546ac93266b675ba0f37418961b3bc1672d6cd34c68974aeaf1f6c8ee45",
}
TARGET_NAME = "graph_recv_any_e2e_bench"
TEST_NAME = "bounded_native_stream_recv_any_has_exact_end_to_end_samples"
BUILD_ATTEMPT = 1
TOOLCHAIN_BIN = Path("/Users/earlcameron/Documents/Codex/2026-10-12/you/work/rust/cargo/bin")
RUSTUP_HOME = Path("/Users/earlcameron/Documents/Codex/2026-10-12/you/work/rust/rustup")
CARGO_HOME = Path("/Users/earlcameron/Documents/Codex/2026-10-12/you/work/rust/cargo")

SELECTED_ENVIRONMENT_NAMES = {
    "PATH",
    "CARGO_HOME",
    "RUSTUP_HOME",
    "RUSTUP_TOOLCHAIN",
    "CARGO_BUILD_JOBS",
    "CARGO_TARGET_DIR",
    "RUSTFLAGS",
    "CARGO_ENCODED_RUSTFLAGS",
    "RUSTDOCFLAGS",
    "CC",
    "CXX",
    "AR",
    "SDKROOT",
    "MACOSX_DEPLOYMENT_TARGET",
}


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def git(root: Path, *args: str) -> str:
    return subprocess.check_output(["git", "-C", str(root), *args], text=True).rstrip("\n")


def write_json_exclusive(path: Path, value: Any) -> None:
    with path.open("x", encoding="utf-8") as stream:
        json.dump(value, stream, indent=2, sort_keys=True)
        stream.write("\n")
        stream.flush()
        os.fsync(stream.fileno())


def selected_build_environment(environment: dict[str, str]) -> dict[str, str]:
    return {
        key: value
        for key, value in sorted(environment.items())
        if key in SELECTED_ENVIRONMENT_NAMES or key.startswith("CARGO_PROFILE_RELEASE_")
    }


def build_environment() -> tuple[dict[str, str], dict[str, Any]]:
    environment = os.environ.copy()
    current_path = environment.get("PATH", "")
    environment.update(
        {
            "PATH": f"{TOOLCHAIN_BIN}{os.pathsep}{current_path}" if current_path else str(TOOLCHAIN_BIN),
            "CARGO_HOME": str(CARGO_HOME),
            "RUSTUP_HOME": str(RUSTUP_HOME),
            "RUSTUP_TOOLCHAIN": "stable-aarch64-apple-darwin",
            "CARGO_BUILD_JOBS": "4",
            "CARGO_TARGET_DIR": str(TARGET),
        }
    )
    metadata = {
        "rustc_vv": subprocess.check_output(["rustc", "-Vv"], env=environment, text=True),
        "cargo_version": subprocess.check_output(["cargo", "--version"], env=environment, text=True),
        "selected_build_environment": selected_build_environment(environment),
        "captured_before_clean_and_build": True,
    }
    return environment, metadata


def verify_source(side: str, root: Path) -> dict[str, str]:
    expected_root = (BASELINE_ROOT if side == "baseline" else CANDIDATE_ROOT).resolve(strict=True)
    root = root.resolve(strict=True)
    if root != expected_root:
        raise RuntimeError(f"{side} source root is not the frozen worktree: {root}")
    commit = git(root, "rev-parse", "HEAD")
    if commit != BASE_COMMIT:
        raise RuntimeError(f"{side} source has unexpected commit {commit}")
    status = git(root, "status", "--porcelain", "--untracked-files=all").splitlines()
    if sorted(status) != [" M src/graph/transport.rs", "?? tests/graph_recv_any_e2e_bench.rs"]:
        raise RuntimeError(f"{side} worktree has unexpected changes: {status}")
    harness = root / HARNESS_RELATIVE
    if not harness.is_file() or sha256_file(harness) != HARNESS_SHA256:
        raise RuntimeError(f"{side} integration harness differs from the frozen source")
    transport = root / "src/graph/transport.rs"
    transport_hash = sha256_file(transport)
    if transport_hash != TRANSPORT_SHA256[side]:
        raise RuntimeError(f"{side} transport source differs from its frozen source pin")
    return {"commit": commit, "transport_sha256": transport_hash}


def parse_build_jsonl(path: Path) -> list[dict[str, Any]]:
    rows: list[dict[str, Any]] = []
    for line_number, line in enumerate(path.read_text(encoding="utf-8").splitlines(), start=1):
        try:
            row = json.loads(line)
        except json.JSONDecodeError as error:
            raise RuntimeError(f"invalid Cargo JSON on line {line_number}: {error}") from error
        if not isinstance(row, dict):
            raise RuntimeError(f"Cargo JSON line {line_number} is not an object")
        rows.append(row)
    return rows


def artifact_for(rows: list[dict[str, Any]], root: Path) -> dict[str, Any]:
    matches = [
        row
        for row in rows
        if row.get("reason") == "compiler-artifact"
        and row.get("target", {}).get("name") == TARGET_NAME
        and row.get("executable")
    ]
    if len(matches) != 1:
        raise RuntimeError(f"expected one {TARGET_NAME} compiler-artifact row, found {len(matches)}")
    artifact = matches[0]
    target = artifact.get("target")
    profile = artifact.get("profile")
    if (
        artifact.get("fresh") is not False
        or not isinstance(target, dict)
        or target.get("kind") != ["test"]
        or target.get("test") is not True
        or not isinstance(profile, dict)
        or profile.get("test") is not True
        or profile.get("opt_level") != "3"
        or profile.get("debug_assertions") is not False
    ):
        raise RuntimeError("Cargo artifact is not the exact fresh optimized integration-test binary")
    if Path(artifact.get("manifest_path", "")).resolve(strict=True) != root / "Cargo.toml":
        raise RuntimeError("Cargo artifact manifest does not identify the expected source root")
    if Path(target.get("src_path", "")).resolve(strict=True) != root / HARNESS_RELATIVE:
        raise RuntimeError("Cargo artifact source path does not identify the frozen benchmark harness")
    executable = Path(artifact["executable"]).resolve(strict=True)
    target_dir = TARGET.resolve(strict=True)
    if not executable.is_relative_to(target_dir / "release" / "deps"):
        raise RuntimeError("Cargo artifact executable is outside the shared release deps directory")
    if not any(Path(filename).resolve(strict=True) == executable for filename in artifact.get("filenames", [])):
        raise RuntimeError("Cargo executable is absent from the artifact filenames list")
    return artifact


def run_logged(command: list[str], root: Path, environment: dict[str, str], log_path: Path) -> None:
    with log_path.open("x", encoding="utf-8") as log:
        subprocess.run(
            command,
            cwd=root,
            env=environment,
            stdin=subprocess.DEVNULL,
            stdout=log,
            stderr=subprocess.STDOUT,
            check=True,
        )
        log.flush()
        os.fsync(log.fileno())


def build_arm(side: str, root: Path, environment: dict[str, str], metadata: dict[str, Any]) -> None:
    source_before = verify_source(side, root)
    prefix = EVIDENCE / f"{side}"
    run_logged(
        ["cargo", "clean", "--release", "--package", "execute"],
        root,
        environment,
        EVIDENCE / f"{side}-release-clean-attempt-{BUILD_ATTEMPT}.log",
    )
    build_jsonl = EVIDENCE / f"{side}-build-attempt-{BUILD_ATTEMPT}.jsonl"
    build_log = EVIDENCE / f"{side}-build-attempt-{BUILD_ATTEMPT}.log"
    with build_jsonl.open("x", encoding="utf-8") as output, build_log.open("x", encoding="utf-8") as errors:
        subprocess.run(
            [
                "cargo",
                "test",
                "--locked",
                "--release",
                "--package",
                "execute",
                "--test",
                TARGET_NAME,
                "--test",
                "graph_runtime",
                "--test",
                "graph_lifecycles",
                "--test",
                "graph_result_events",
                "--lib",
                "--no-run",
                "--message-format=json",
            ],
            cwd=root,
            env=environment,
            stdin=subprocess.DEVNULL,
            stdout=output,
            stderr=errors,
            check=True,
        )
        output.flush()
        os.fsync(output.fileno())
        errors.flush()
        os.fsync(errors.fileno())

    artifact = artifact_for(parse_build_jsonl(build_jsonl), root)
    source_executable = Path(artifact["executable"]).resolve(strict=True)
    saved_binary = EVIDENCE / f"{side}-test-binary"
    if saved_binary.exists():
        raise RuntimeError(f"refusing to overwrite immutable saved binary: {saved_binary}")
    shutil.copy2(source_executable, saved_binary)
    if not os.access(saved_binary, os.X_OK):
        raise RuntimeError(f"saved test binary is not executable: {saved_binary}")
    test_list = subprocess.check_output([str(saved_binary), "--list"], text=True, timeout=60)
    expected_test_listing = f"{TEST_NAME}: test"
    if expected_test_listing not in test_list.splitlines():
        raise RuntimeError(f"saved test binary does not list its exact ignored benchmark: {TEST_NAME}")
    test_list_path = EVIDENCE / f"{side}-test-list.txt"
    with test_list_path.open("x", encoding="utf-8") as stream:
        stream.write(test_list)
        stream.flush()
        os.fsync(stream.fileno())

    source_after = verify_source(side, root)
    if source_after != source_before:
        raise RuntimeError(f"{side} source changed during the build")
    receipt = {
        "artifact": artifact,
        "saved_binary": str(saved_binary.resolve()),
        "sha256": sha256_file(saved_binary),
        "source_sha256": source_after["transport_sha256"],
        "source_stable": True,
        "attempt": BUILD_ATTEMPT,
        "build_metadata": metadata,
    }
    receipt_path = EVIDENCE / f"{side}-binary-receipt.json"
    with receipt_path.open("x", encoding="utf-8") as stream:
        json.dump(receipt, stream, indent=2, sort_keys=True)
        stream.write("\n")
        stream.flush()
        os.fsync(stream.fileno())
    print(
        json.dumps(
            {"side": side, "saved_binary": str(saved_binary.resolve()), "sha256": receipt["sha256"]},
            sort_keys=True,
        ),
        flush=True,
    )


def main() -> None:
    EVIDENCE.mkdir(parents=True, exist_ok=True)
    environment, metadata = build_environment()
    environment_path = EVIDENCE / f"build-environment-attempt-{BUILD_ATTEMPT}.json"
    write_json_exclusive(environment_path, metadata)
    # The shared target is cleaned for each arm. Baseline's immutable binary and
    # receipt are saved before candidate cleaning can remove Cargo's artifact.
    build_arm("baseline", BASELINE_ROOT, environment, metadata)
    build_arm("candidate", CANDIDATE_ROOT, environment, metadata)


if __name__ == "__main__":
    main()
