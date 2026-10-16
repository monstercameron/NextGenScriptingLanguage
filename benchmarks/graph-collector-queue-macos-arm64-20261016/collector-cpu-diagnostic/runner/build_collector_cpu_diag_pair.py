#!/usr/bin/env python3
"""Build (but never run) the frozen collector CPU diagnostic test binaries."""
from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import shutil
import stat
import subprocess
import sys
from datetime import datetime, timezone
from typing import Any


CAMPAIGN = Path(__file__).resolve().parents[1]
EVIDENCE = CAMPAIGN / "evidence/collector-cpu-diagnostic"
TARGET = CAMPAIGN / "work/perf-target-candidate"
BASELINE = CAMPAIGN / "work/collector-cpu-baseline-81bf"
CANDIDATE = CAMPAIGN / "work/collector-cpu-candidate-81bf"
HARNESS_RELATIVE = "tests/collector_cpu_diag_bench.rs"
HARNESS_SHA256 = "f3d484860ca6252fd0f4fdca53d9493719d199fcf66a660905e600949dcbc134"
EXPECTED_COMMIT = "81bf9ba766a775cccf059007b19f5def9b9bfb03"
EXPECTED_BASELINE_TRANSPORT = "049b2b13a6c82ba0c98f20b57a44e76c420d631815f1b673ba70d7902f24610c"
EXPECTED_CANDIDATE_TRANSPORT = "bfc03a7028b2871a4823ed0a130e3b735ca8dc535cac9c12cd39d876174dad9c"
EXPECTED_PATCH_SHA256 = "7d411b9b0fc3a8a37fbd589381720016916b6edabec43be06414e198cd39bca4"
PATCH = CAMPAIGN / "work/graph-collector-sleep-candidate-v2.patch"
TEST_NAME = "collector_cpu_diag_bench"
ATTEMPT = 1
CARGO_BIN = Path("/Users/earlcameron/Documents/Codex/2026-10-12/you/work/rust/cargo/bin")
CARGO_HOME_PIN = Path("/Users/earlcameron/Documents/Codex/2026-10-12/you/work/rust/cargo")
RUSTUP_HOME_PIN = Path("/Users/earlcameron/Documents/Codex/2026-10-12/you/work/rust/rustup")
TOOLCHAIN = "stable-aarch64-apple-darwin"
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
FIXED_BUILD_ENV = {
    "CARGO_HOME": str(CARGO_HOME_PIN),
    "RUSTUP_HOME": str(RUSTUP_HOME_PIN),
    "RUSTUP_TOOLCHAIN": TOOLCHAIN,
    "CARGO_TARGET_DIR": str(TARGET),
    "CARGO_BUILD_JOBS": "4",
}
CONFIG_FILENAMES = ("config", "config.toml")
CONFIG_FINGERPRINT_SCHEMA = "execute.graph.cargo_configuration_fingerprint/1"


class BuildError(RuntimeError):
    pass


def utc_now() -> str:
    return datetime.now(timezone.utc).isoformat(timespec="microseconds").replace("+00:00", "Z")


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def git(root: Path, *args: str) -> bytes:
    result = subprocess.run(
        ["git", "-C", str(root), *args],
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
    )
    if result.returncode:
        raise BuildError(result.stderr.decode("utf-8", errors="replace").strip())
    return result.stdout


def source_fingerprint(root: Path) -> dict[str, Any]:
    build_configs = {
        "Cargo.toml", "Cargo.lock", "build.rs", "rust-toolchain", "rust-toolchain.toml", "rustfmt.toml"
    }
    paths = sorted(
        item.decode("utf-8")
        for item in git(root, "ls-files", "-z").split(b"\0")
        if item and (
            item.decode("utf-8").startswith(("src/", "vendor/", ".cargo/"))
            or item.decode("utf-8") in build_configs
        )
    )
    files: dict[str, str] = {}
    digest = hashlib.sha256()
    for relative in paths:
        file_hash = sha256_file(root / relative)
        files[relative] = file_hash
        digest.update(relative.encode("utf-8"))
        digest.update(b"\0")
        digest.update(file_hash.encode("ascii"))
        digest.update(b"\n")
    if not files:
        raise BuildError(f"no tracked build inputs in {root}")
    return {"sha256": digest.hexdigest(), "files": files}


def inspect_source(root: Path, side: str) -> dict[str, Any]:
    root = root.resolve(strict=True)
    if git(root, "rev-parse", "--show-toplevel").decode().strip() != str(root):
        raise BuildError(f"{side} path is not the Git worktree root")
    commit = git(root, "rev-parse", "HEAD").decode("ascii").strip()
    if commit != EXPECTED_COMMIT:
        raise BuildError(f"{side} has unexpected commit {commit}")
    changed = git(root, "diff", "--name-only").decode().splitlines()
    staged = git(root, "diff", "--cached", "--name-only").decode().splitlines()
    untracked = [
        item.decode("utf-8")
        for item in git(root, "ls-files", "--others", "--exclude-standard", "-z").split(b"\0")
        if item
    ]
    expected_changed = [] if side == "baseline" else ["src/graph/transport.rs"]
    if sorted(changed) != expected_changed or staged or untracked != [HARNESS_RELATIVE]:
        raise BuildError(
            f"unexpected {side} worktree state: changed={changed}, staged={staged}, untracked={untracked}"
        )
    harness = root / HARNESS_RELATIVE
    if not harness.is_file() or sha256_file(harness) != HARNESS_SHA256:
        raise BuildError(f"{side} harness differs from the frozen source")
    transport = root / "src/graph/transport.rs"
    transport_hash = sha256_file(transport)
    expected_transport = EXPECTED_BASELINE_TRANSPORT if side == "baseline" else EXPECTED_CANDIDATE_TRANSPORT
    if transport_hash != expected_transport:
        raise BuildError(f"{side} transport source differs from the frozen source hash")
    return {
        "side": side,
        "root": str(root),
        "commit": commit,
        "transport_sha256": transport_hash,
        "harness_sha256": sha256_file(harness),
        "source_fingerprint": source_fingerprint(root),
    }


def controlled_build_environment(host_environment: dict[str, str]) -> dict[str, str]:
    if not host_environment.get("PATH") or not host_environment.get("HOME"):
        raise BuildError("the controlled build environment requires inherited PATH and HOME")
    env = {key: host_environment[key] for key in INHERITED_BUILD_ENV_KEYS if key in host_environment}
    env["PATH"] = os.pathsep.join([str(CARGO_BIN), env["PATH"]])
    env.update(FIXED_BUILD_ENV)
    return dict(sorted(env.items()))


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
        raise BuildError(f"Cargo configuration path is present but not a regular file: {path}")
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
    while True:
        directories.append(current)
        if current == home:
            return directories, True
        if current == current.parent:
            return directories, False
        current = current.parent


def cargo_configuration_fingerprint(source_roots: dict[str, Path], env: dict[str, str]) -> dict[str, Any]:
    home = Path(env["HOME"]).expanduser().resolve(strict=True)
    cargo_home = Path(env["CARGO_HOME"]).expanduser().absolute()
    if not home.is_dir() or not cargo_home.is_dir():
        raise BuildError("HOME and CARGO_HOME must be existing directories for configuration capture")

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
        "schema": CONFIG_FINGERPRINT_SCHEMA,
        "cargo_home": {"path": str(cargo_home), "config_files": cargo_home_files},
        "source_roots": roots,
    }
    encoded = json.dumps(body, sort_keys=True, separators=(",", ":"), ensure_ascii=False, allow_nan=False).encode("utf-8")
    return {**body, "sha256": hashlib.sha256(encoded).hexdigest()}


def verify_cargo_configuration_fingerprint(
    expected: dict[str, Any], source_roots: dict[str, Path], env: dict[str, str], label: str
) -> None:
    current = cargo_configuration_fingerprint(source_roots, env)
    if current != expected:
        raise BuildError(f"Cargo configuration inputs changed {label} the sequential build")


def write_exclusive(path: Path, value: Any) -> None:
    with path.open("x", encoding="utf-8") as out:
        json.dump(value, out, ensure_ascii=False, sort_keys=True, indent=2, allow_nan=False)
        out.write("\n")
        out.flush()
        os.fsync(out.fileno())


def build_metadata(
    env: dict[str, str], driver_hash: str, configuration_fingerprint: dict[str, Any]
) -> dict[str, Any]:
    rustc = subprocess.check_output([str(CARGO_BIN / "rustc"), "-Vv"], env=env, text=True)
    cargo = subprocess.check_output([str(CARGO_BIN / "cargo"), "--version"], env=env, text=True)
    return {
        "rustc_vv": rustc,
        "cargo_version": cargo,
        "selected_build_environment": dict(sorted(env.items())),
        "cargo_configuration_fingerprint": configuration_fingerprint,
        "captured_before_clean_and_build": True,
        "build_driver_sha256": driver_hash,
    }


def run_logged(command: list[str], cwd: Path, env: dict[str, str], stdout_path: Path, stderr_path: Path) -> None:
    with stdout_path.open("x") as stdout, stderr_path.open("x") as stderr:
        result = subprocess.run(
            command,
            cwd=cwd,
            env=env,
            stdin=subprocess.DEVNULL,
            stdout=stdout,
            stderr=stderr,
            check=False,
        )
    if result.returncode:
        raise BuildError(f"command failed with exit {result.returncode}: {command}")


def artifact_from_jsonl(path: Path, root: Path) -> dict[str, Any]:
    found: list[dict[str, Any]] = []
    with path.open("r", encoding="utf-8") as stream:
        for line_number, line in enumerate(stream, start=1):
            if not line.strip():
                continue
            row = json.loads(line)
            if not isinstance(row, dict):
                raise BuildError(f"Cargo JSONL row {line_number} is not an object")
            target = row.get("target")
            if row.get("reason") == "compiler-artifact" and isinstance(target, dict) and target.get("name") == TEST_NAME:
                found.append(row)
    if len(found) != 1:
        raise BuildError(f"expected one {TEST_NAME} compiler-artifact row; found {len(found)}")
    artifact = found[0]
    profile = artifact.get("profile")
    target = artifact.get("target")
    if (
        not isinstance(profile, dict)
        or not isinstance(target, dict)
        or target.get("kind") != ["test"]
        or target.get("test") is not True
        or profile.get("test") is not True
        or profile.get("opt_level") != "3"
        or profile.get("debug_assertions") is not False
        or artifact.get("fresh") is not False
    ):
        raise BuildError("Cargo artifact is not a fresh optimized integration-test build")
    if Path(artifact.get("manifest_path", "")).resolve(strict=True) != root / "Cargo.toml":
        raise BuildError("Cargo artifact manifest path differs from selected source root")
    if Path(target.get("src_path", "")).resolve(strict=True) != root / HARNESS_RELATIVE:
        raise BuildError("Cargo artifact source path differs from frozen diagnostic harness")
    binary_text = artifact.get("executable")
    if not isinstance(binary_text, str):
        raise BuildError("Cargo artifact has no executable path")
    binary = Path(binary_text).resolve(strict=True)
    if not binary.is_relative_to(TARGET.resolve(strict=True) / "release/deps"):
        raise BuildError("Cargo artifact is outside shared release dependency target")
    filenames = artifact.get("filenames")
    if not isinstance(filenames, list) or binary_text not in filenames:
        raise BuildError("Cargo artifact executable is absent from its filenames")
    return artifact


def build_one(
    side: str,
    root: Path,
    env: dict[str, str],
    metadata: dict[str, Any],
    driver_hash: str,
    build_order: int,
    configuration_fingerprint: dict[str, Any],
) -> None:
    started_at = utc_now()
    verify_cargo_configuration_fingerprint(
        configuration_fingerprint, {"baseline": BASELINE, "candidate": CANDIDATE}, env, f"before {side} clean/build"
    )
    source_before = inspect_source(root, side)
    clean_out = EVIDENCE / f"{side}-release-clean-attempt-{ATTEMPT}.log"
    clean_err = EVIDENCE / f"{side}-release-clean-attempt-{ATTEMPT}.stderr.log"
    run_logged(
        [str(CARGO_BIN / "cargo"), "clean", "--release", "--package", "execute"],
        root,
        env,
        clean_out,
        clean_err,
    )
    build_jsonl = EVIDENCE / f"{side}-build-attempt-{ATTEMPT}.jsonl"
    build_stderr = EVIDENCE / f"{side}-build-attempt-{ATTEMPT}.log"
    run_logged(
        [
            str(CARGO_BIN / "cargo"), "test", "--locked", "--release", "--package", "execute", "--test",
            TEST_NAME, "--no-run", "--message-format=json",
        ],
        root,
        env,
        build_jsonl,
        build_stderr,
    )
    artifact = artifact_from_jsonl(build_jsonl, root)
    source_after = inspect_source(root, side)
    if source_after != source_before:
        raise BuildError(f"{side} source changed during its clean/build")
    verify_cargo_configuration_fingerprint(
        configuration_fingerprint, {"baseline": BASELINE, "candidate": CANDIDATE}, env, f"during {side} clean/build"
    )

    artifact_path = Path(artifact["executable"]).resolve(strict=True)
    saved_binary = EVIDENCE / f"{side}-test-binary-attempt-{ATTEMPT}"
    if saved_binary.exists():
        raise BuildError(f"refusing to overwrite saved binary: {saved_binary}")
    shutil.copy2(artifact_path, saved_binary)
    saved_binary.chmod(stat.S_IRUSR | stat.S_IXUSR | stat.S_IRGRP | stat.S_IXGRP | stat.S_IROTH | stat.S_IXOTH)
    saved_hash = sha256_file(saved_binary)
    if saved_hash != sha256_file(artifact_path):
        raise BuildError(f"{side} saved binary copy hash mismatch")
    receipt = {
        "artifact": artifact,
        "saved_binary": str(saved_binary.resolve()),
        "sha256": saved_hash,
        "source_sha256": source_after["transport_sha256"],
        "source_fingerprint_sha256": source_after["source_fingerprint"]["sha256"],
        "source_stable": True,
        "attempt": ATTEMPT,
        "build_order": build_order,
        "started_at": started_at,
        "finished_at": utc_now(),
        "build_metadata": metadata,
        "driver_sha256": driver_hash,
        "clean_log_sha256": sha256_file(clean_out),
        "clean_stderr_sha256": sha256_file(clean_err),
        "build_jsonl_sha256": sha256_file(build_jsonl),
        "build_stderr_sha256": sha256_file(build_stderr),
        "immutable_copy_mode": oct(saved_binary.stat().st_mode & 0o777),
    }
    write_exclusive(EVIDENCE / f"{side}-build-receipt-attempt-{ATTEMPT}.json", receipt)


def main() -> int:
    EVIDENCE.mkdir(parents=True, exist_ok=True)
    planned_outputs = [
        EVIDENCE / f"build-environment-attempt-{ATTEMPT}.json",
        *(
            EVIDENCE / f"{side}-{suffix}-attempt-{ATTEMPT}{extension}"
            for side in ("baseline", "candidate")
            for suffix, extension in (
                ("release-clean", ".log"),
                ("release-clean", ".stderr.log"),
                ("build", ".jsonl"),
                ("build", ".log"),
                ("build-receipt", ".json"),
                ("test-binary", ""),
            )
        ),
    ]
    existing = [str(path) for path in planned_outputs if path.exists() or path.is_symlink()]
    if existing:
        raise BuildError(f"refusing to overwrite preexisting build evidence: {existing}")
    if sha256_file(PATCH) != EXPECTED_PATCH_SHA256:
        raise BuildError("collector product patch hash changed")
    if sha256_file(CAMPAIGN / "evidence/collector-cpu-diagnostic/collector_cpu_diag_bench.rs") != HARNESS_SHA256:
        raise BuildError("canonical harness hash changed")
    baseline_state = inspect_source(BASELINE, "baseline")
    candidate_state = inspect_source(CANDIDATE, "candidate")
    if baseline_state["source_fingerprint"]["files"].keys() != candidate_state["source_fingerprint"]["files"].keys():
        raise BuildError("source build-input sets differ")
    changed = {
        key
        for key, value in baseline_state["source_fingerprint"]["files"].items()
        if candidate_state["source_fingerprint"]["files"][key] != value
    }
    if changed != {"src/graph/transport.rs"}:
        raise BuildError(f"source differs outside intended transport file: {sorted(changed)}")
    if baseline_state["harness_sha256"] != candidate_state["harness_sha256"]:
        raise BuildError("baseline and candidate harness copies differ")

    target_parent = TARGET.parent
    target_parent.mkdir(parents=True, exist_ok=True)
    cargo_env = controlled_build_environment(dict(os.environ))
    configuration_fingerprint = cargo_configuration_fingerprint(
        {"baseline": BASELINE, "candidate": CANDIDATE}, cargo_env
    )
    driver_hash = sha256_file(Path(__file__).resolve(strict=True))
    metadata = build_metadata(cargo_env, driver_hash, configuration_fingerprint)
    env_receipt = {
        **metadata,
        "schema": "execute.graph.collector_cpu_build_environment/1",
        "build_driver_path": str(Path(__file__).resolve(strict=True)),
        "attempt": ATTEMPT,
        "baseline_source": baseline_state,
        "candidate_source": candidate_state,
        "target_directory": str(TARGET),
        "preclean_commands": [
            ["cargo", "clean", "--release", "--package", "execute"],
            ["cargo", "clean", "--release", "--package", "execute"],
        ],
        "build_commands": [
            ["cargo", "test", "--locked", "--release", "--package", "execute", "--test", TEST_NAME, "--no-run", "--message-format=json"],
            ["cargo", "test", "--locked", "--release", "--package", "execute", "--test", TEST_NAME, "--no-run", "--message-format=json"],
        ],
        "sequential": True,
        "binaries_are_run": False,
        "captured_before_any_clean_or_build": True,
        "build_environment_policy": {
            "inherited_allowlist": list(INHERITED_BUILD_ENV_KEYS),
            "fixed_values": FIXED_BUILD_ENV,
            "recorded_actual_environment_values": True,
            "unlisted_inherited_variables_forwarded": False,
            "cargo_configuration_capture": (
                "SHA-256 presence/path/symlink-target/content fingerprints for config and config.toml in CARGO_HOME and "
                "each .cargo directory from both source roots through filesystem root; verify before and after each "
                "sequential clean/build; record hashes, never config contents"
            ),
        },
    }
    write_exclusive(EVIDENCE / f"build-environment-attempt-{ATTEMPT}.json", env_receipt)
    build_one("baseline", BASELINE, cargo_env, metadata, driver_hash, build_order=1, configuration_fingerprint=configuration_fingerprint)
    build_one("candidate", CANDIDATE, cargo_env, metadata, driver_hash, build_order=2, configuration_fingerprint=configuration_fingerprint)
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (BuildError, OSError, ValueError, subprocess.SubprocessError) as error:
        print(f"CPU diagnostic build failed: {type(error).__name__}: {error}", file=sys.stderr)
        raise SystemExit(1)
