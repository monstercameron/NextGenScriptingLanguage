#!/usr/bin/env python3
"""Build immutable line-framing benchmark and candidate differential-test binaries only."""

from __future__ import annotations

import argparse
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess
import sys


PAIR_EVIDENCE = Path(__file__).resolve().parents[1]
CAMPAIGN = PAIR_EVIDENCE.parents[1]
WORK = CAMPAIGN / "work"
BASELINE = WORK / "line-framing-baseline-a884"
CANDIDATE = WORK / "line-framing-candidate-a884"
TARGET = WORK / "perf-target-candidate"
CARGO_BUILD_TARGET = "aarch64-apple-darwin"
BASE_COMMIT = "a884f6561fd43f45e43b25a3cc2233be17ee3944"
BENCH_SHA256 = "7171a2cb59c6749d656ed8eb5202109c17810e22d5e41c4256daeeec926256ad"
PRODUCT_PATCH_SHA256 = "eeb7c5512fd78c0264d6d96a85b6959c8f1302ac481ab1e98047e3fb52145c94"
EXPECTED_SOURCES = {
    "baseline": {
        "Cargo.lock": "2be0aa74e5bfe3e4897ea9ca9121efce663a94a4df7850958dc2cf511c6f427c",
        "Cargo.toml": "2e2cd88356201ac07b3b8d68be1af9379d7c307bf2c5b67765a13d0f8e040874",
        "src/runtime/streaming.rs": "d7d18b92c9ad0ece03bdef875ca100734f36de5d19de1dad39fadcb63b9bfc3a",
        "tests/native_stream_status_bench.rs": BENCH_SHA256,
    },
    "candidate": {
        "Cargo.lock": "364ea897bd0db1094c5f235f9431aa10dfcd0fdde317afd6b9da824e84f1dfad",
        "Cargo.toml": "dd1ebd61a09bac5972ca8365e3be618ad4d8326361aff879568f7cae039a2b59",
        "src/runtime/streaming.rs": "5ada8d51fd5e1dea60a70a6e067bb56694ca48d6de1ed10a87e729f0e5f5049f",
        "tests/native_stream_status_bench.rs": BENCH_SHA256,
    },
}
BUILD_TARGETS = {
    "baseline": [
        {
            "key": "native_stream_status_bench",
            "cargo_args": ["--test", "native_stream_status_bench"],
            "target_name": "native_stream_status_bench",
            "kind": "test",
            "crate_type": "bin",
            "source": "tests/native_stream_status_bench.rs",
        },
    ],
    "candidate": [
        {
            "key": "native_stream_status_bench",
            "cargo_args": ["--test", "native_stream_status_bench"],
            "target_name": "native_stream_status_bench",
            "kind": "test",
            "crate_type": "bin",
            "source": "tests/native_stream_status_bench.rs",
        },
        {
            "key": "line_framing_lib_test",
            "cargo_args": ["--lib"],
            "target_name": "flow",
            "kind": "lib",
            "crate_type": "lib",
            "source": "src/lib.rs",
        },
    ],
}
RUST_BIN = Path("/Users/earlcameron/Documents/Codex/2026-10-12/you/work/rust/cargo/bin")
CARGO_HOME = Path("/Users/earlcameron/Documents/Codex/2026-10-12/you/work/rust/cargo")
RUSTUP_HOME = Path("/Users/earlcameron/Documents/Codex/2026-10-12/you/work/rust/rustup")
TOOLCHAIN = "stable-aarch64-apple-darwin"
BUILD_JOBS = "4"
INHERITED_BUILD_ENV_KEYS = (
    "PATH", "HOME", "TMPDIR", "LANG", "LC_ALL", "TZ",
    "SDKROOT", "DEVELOPER_DIR", "MACOSX_DEPLOYMENT_TARGET",
)
FIXED_BUILD_ENV = {
    "CARGO_HOME": str(CARGO_HOME),
    "RUSTUP_HOME": str(RUSTUP_HOME),
    "RUSTUP_TOOLCHAIN": TOOLCHAIN,
    "CARGO_BUILD_JOBS": BUILD_JOBS,
    "CARGO_TARGET_DIR": str(TARGET),
    "CARGO_BUILD_TARGET": CARGO_BUILD_TARGET,
    "RUSTC": str(RUST_BIN / "rustc"),
}
BUILD_ENVIRONMENT_SCHEMA = "execute.controlled-cargo-build-environment/1"
CONFIG_FINGERPRINT_SCHEMA = "execute.line-framing.cargo-configuration-fingerprint/1"
CONFIG_FILENAMES = ("config", "config.toml")


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for block in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def git(root: Path, *args: str) -> str:
    return subprocess.check_output(["git", *args], cwd=root, text=True).strip()


def write_json(path: Path, value: object) -> None:
    with path.open("x", encoding="utf-8") as output:
        json.dump(value, output, indent=2, sort_keys=True)
        output.write("\n")
        output.flush()
        os.fsync(output.fileno())


def controlled_build_environment(host_environment: dict[str, str]) -> dict[str, str]:
    if not host_environment.get("PATH") or not host_environment.get("HOME"):
        raise RuntimeError("the controlled build environment requires inherited PATH and HOME")
    env = {key: host_environment[key] for key in INHERITED_BUILD_ENV_KEYS if key in host_environment}
    env["PATH"] = os.pathsep.join([str(RUST_BIN), env["PATH"]])
    env.update(FIXED_BUILD_ENV)
    return dict(sorted(env.items()))


def build_environment_snapshot(env: dict[str, str]) -> dict[str, object]:
    encoded = json.dumps(env, sort_keys=True, separators=(",", ":"), ensure_ascii=False).encode("utf-8")
    return {
        "schema": BUILD_ENVIRONMENT_SCHEMA,
        "inherited_allowlist": list(INHERITED_BUILD_ENV_KEYS),
        "fixed_values": FIXED_BUILD_ENV,
        "unlisted_inherited_variables_forwarded": False,
        "effective_environment": dict(sorted(env.items())),
        "effective_environment_sha256": hashlib.sha256(encoded).hexdigest(),
    }


def cargo_config_file_identity(path: Path) -> dict[str, object]:
    path = path.absolute()
    present = path.exists() or path.is_symlink()
    identity: dict[str, object] = {
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
    identity["sha256"] = sha256(path)
    if path.is_symlink():
        identity["symlink_target"] = os.readlink(path)
    return identity


def source_ancestor_directories(root: Path, home: Path) -> tuple[list[Path], bool]:
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


def cargo_configuration_fingerprint(
    env: dict[str, str], source_roots: dict[str, Path] | None = None
) -> dict[str, object]:
    home = Path(env["HOME"]).expanduser().resolve(strict=True)
    cargo_home = Path(env["CARGO_HOME"]).expanduser().absolute()
    if not home.is_dir() or not cargo_home.is_dir():
        raise RuntimeError("HOME and CARGO_HOME must be existing directories for configuration capture")
    cargo_home_files = [cargo_config_file_identity(cargo_home / name) for name in CONFIG_FILENAMES]
    roots: dict[str, object] = {}
    roots_to_check = source_roots or {"baseline": BASELINE, "candidate": CANDIDATE}
    for side, root in sorted(roots_to_check.items()):
        resolved_root = root.resolve(strict=True)
        ancestors, home_is_ancestor = source_ancestor_directories(resolved_root, home)
        config_files = [
            cargo_config_file_identity(directory / ".cargo" / name)
            for directory in ancestors
            for name in CONFIG_FILENAMES
        ]
        roots[side] = {
            "root": str(resolved_root),
            "home_is_ancestor": home_is_ancestor,
            "config_files": config_files,
        }
    body: dict[str, object] = {
        "schema": CONFIG_FINGERPRINT_SCHEMA,
        "cargo_home": {"path": str(cargo_home.resolve(strict=True)), "config_files": cargo_home_files},
        "source_roots": roots,
    }
    encoded = json.dumps(body, sort_keys=True, separators=(",", ":"), ensure_ascii=False).encode("utf-8")
    return {**body, "sha256": hashlib.sha256(encoded).hexdigest()}


def verify_cargo_configuration(expected: dict[str, object], env: dict[str, str], label: str) -> dict[str, object]:
    current = cargo_configuration_fingerprint(env)
    if current != expected:
        raise RuntimeError(f"Cargo configuration inputs changed {label} the sequential build")
    return current


def run_logged(command: list[str], cwd: Path, env: dict[str, str], stdout_path: Path, stderr_path: Path) -> int:
    with stdout_path.open("x", encoding="utf-8") as stdout, stderr_path.open("x", encoding="utf-8") as stderr:
        return subprocess.run(command, cwd=cwd, env=env, stdout=stdout, stderr=stderr, check=False).returncode


def expected_tracked_changes(side: str, root: Path) -> None:
    commit = git(root, "rev-parse", "HEAD")
    if commit != BASE_COMMIT:
        raise RuntimeError(f"{side} HEAD changed: {commit}")
    paths = list(EXPECTED_SOURCES[side])
    actual = {path: sha256(root / path) for path in paths}
    if actual != EXPECTED_SOURCES[side]:
        raise RuntimeError(f"{side} pinned source hashes changed: {actual}")
    if side == "baseline":
        tracked_patch = subprocess.check_output(["git", "diff", "--binary", "HEAD"], cwd=root)
        if tracked_patch:
            raise RuntimeError("baseline contains tracked product changes")
    else:
        tracked_patch = subprocess.check_output(
            ["git", "diff", "--binary", "HEAD", "--", "Cargo.lock", "Cargo.toml", "src/runtime/streaming.rs"],
            cwd=root,
        )
        patch_sha = hashlib.sha256(tracked_patch).hexdigest()
        if patch_sha != PRODUCT_PATCH_SHA256:
            raise RuntimeError(f"candidate product patch changed: {patch_sha}")
        changed_paths = git(root, "diff", "--name-only", "HEAD").splitlines()
        if changed_paths != ["Cargo.lock", "Cargo.toml", "src/runtime/streaming.rs"]:
            raise RuntimeError(f"unexpected candidate tracked paths: {changed_paths}")
    status_text = subprocess.check_output(
        ["git", "status", "--short", "--untracked-files=all"], cwd=root, text=True
    )
    status = status_text.rstrip("\n").splitlines() if status_text else []
    allowed = {"?? tests/native_stream_status_bench.rs"}
    if side == "candidate":
        allowed.update({" M Cargo.lock", " M Cargo.toml", " M src/runtime/streaming.rs"})
    if set(status) != allowed:
        raise RuntimeError(f"{side} worktree has unexpected status: {status}")


def build_side(
    side: str,
    root: Path,
    attempt_dir: Path,
    env: dict[str, str],
    cargo_configuration_before_build: dict[str, object],
    effective_build_environment_sha256: str,
) -> dict[str, object]:
    expected_tracked_changes(side, root)
    before = {name: sha256(root / name) for name in EXPECTED_SOURCES[side]}
    records = []
    configuration_checks: dict[str, object] = {}
    configuration_checks["before_clean"] = verify_cargo_configuration(
        cargo_configuration_before_build, env, f"before {side} clean"
    )
    clean_command = ["cargo", "clean", "--release", "--package", "execute"]
    clean_out = attempt_dir / f"{side}-clean.stdout.log"
    clean_err = attempt_dir / f"{side}-clean.stderr.log"
    clean_status = run_logged(clean_command, root, env, clean_out, clean_err)
    configuration_checks["after_clean"] = verify_cargo_configuration(
        cargo_configuration_before_build, env, f"after {side} clean"
    )
    records.append({
        "phase": "clean",
        "command": clean_command,
        "exit_code": clean_status,
        "stdout": str(clean_out),
        "stderr": str(clean_err),
        "target_dir": str(TARGET),
    })
    if clean_status != 0:
        write_json(attempt_dir / f"{side}-build-receipt.json", {
            "schema": "execute.line-framing-build-receipt/1",
            "side": side,
            "status": "clean_failed",
            "commit": git(root, "rev-parse", "HEAD"),
            "build_driver_sha256": sha256(Path(__file__).resolve()),
            "source_sha256": before,
            "commands": records,
            "effective_build_environment_sha256": effective_build_environment_sha256,
            "cargo_configuration_checks": configuration_checks,
        })
        raise RuntimeError(f"{side} cargo clean failed with status {clean_status}")

    artifacts = []
    configuration_checks["before_build"] = verify_cargo_configuration(
        cargo_configuration_before_build, env, f"before {side} build"
    )
    configuration_checks["before_each_target"] = {}
    for target_spec in BUILD_TARGETS[side]:
        target_key = target_spec["key"]
        target_name = target_spec["target_name"]
        configuration_checks["before_each_target"][target_key] = verify_cargo_configuration(
            cargo_configuration_before_build, env, f"before {side}/{target_key} build"
        )
        command = [
            "cargo", "test", "--locked", "--release", *target_spec["cargo_args"],
            "--no-run", "--message-format=json",
        ]
        stdout_path = attempt_dir / f"{side}-{target_key}-build.jsonl"
        stderr_path = attempt_dir / f"{side}-{target_key}-build.stderr.log"
        status = run_logged(command, root, env, stdout_path, stderr_path)
        configuration_checks.setdefault("after_each_target", {})[target_key] = verify_cargo_configuration(
            cargo_configuration_before_build, env, f"after {side}/{target_key} build"
        )
        record = {
            "phase": "build_test_target" if target_spec["kind"] == "test" else "build_library_test_target",
            "target": target_key,
            "target_spec": target_spec,
            "command": command,
            "exit_code": status,
            "stdout_jsonl": str(stdout_path),
            "stderr": str(stderr_path),
        }
        records.append(record)
        if status != 0:
            write_json(attempt_dir / f"{side}-build-receipt.json", {
                "schema": "execute.line-framing-build-receipt/1",
                "side": side,
                "status": "build_failed",
                "commit": git(root, "rev-parse", "HEAD"),
                "build_driver_sha256": sha256(Path(__file__).resolve()),
                "source_sha256": before,
                "commands": records,
                "completed_artifacts": artifacts,
                "effective_build_environment_sha256": effective_build_environment_sha256,
                "cargo_configuration_checks": configuration_checks,
            })
            raise RuntimeError(f"{side} build of {target_key} failed with status {status}")

        rows = [json.loads(line) for line in stdout_path.read_text(encoding="utf-8").splitlines() if line]
        matching = [
            row for row in rows
            if row.get("reason") == "compiler-artifact"
            and row.get("target", {}).get("name") == target_spec["target_name"]
            and row.get("profile", {}).get("test") is True
            and row.get("executable")
        ]
        matching = [
            row for row in matching
            if target_spec["kind"] in row.get("target", {}).get("kind", [])
            and target_spec["crate_type"] in row.get("target", {}).get("crate_types", [])
        ]
        if len(matching) != 1:
            raise RuntimeError(f"expected exactly one {target_key} test artifact, got {len(matching)}")
        artifact = matching[0]
        if artifact.get("fresh") is not False:
            raise RuntimeError(f"{side}/{target_key} artifact was reused: fresh={artifact.get('fresh')!r}")
        profile = artifact.get("profile", {})
        if profile.get("opt_level") != "3" or profile.get("debug_assertions") is not False:
            raise RuntimeError(f"{side}/{target_key} profile mismatch: {profile}")
        if Path(artifact["manifest_path"]) != root / "Cargo.toml":
            raise RuntimeError(f"{side}/{target_key} manifest path mismatch")
        if Path(artifact["target"]["src_path"]) != root / target_spec["source"]:
            raise RuntimeError(f"{side}/{target_key} source path mismatch")
        executable = Path(artifact["executable"])
        if not executable.is_relative_to(TARGET / CARGO_BUILD_TARGET / "release" / "deps"):
            raise RuntimeError(f"{side}/{target_key} artifact escaped the pinned target dir: {executable}")

        saved = attempt_dir / f"{side}-{target_key}-test-binary"
        with executable.open("rb") as source, saved.open("xb") as destination:
            shutil.copyfileobj(source, destination)
        shutil.copystat(executable, saved)
        artifact_record = {
            "target": target_key,
            "target_spec": target_spec,
            "artifact": artifact,
            "saved_binary": str(saved),
            "saved_binary_sha256": sha256(saved),
            "cargo_stdout_jsonl_sha256": sha256(stdout_path),
            "cargo_stderr_sha256": sha256(stderr_path),
            "fresh": artifact["fresh"],
            "opt_level": profile["opt_level"],
            "debug_assertions": profile["debug_assertions"],
        }
        artifacts.append(artifact_record)

    expected_tracked_changes(side, root)
    after = {name: sha256(root / name) for name in EXPECTED_SOURCES[side]}
    if before != after:
        raise RuntimeError(f"{side} source changed during build: before={before}, after={after}")
    configuration_checks["after_build"] = verify_cargo_configuration(
        cargo_configuration_before_build, env, f"after {side} build"
    )
    write_json(attempt_dir / f"{side}-build-receipt.json", {
        "schema": "execute.line-framing-build-receipt/1",
        "side": side,
        "status": "complete_build_only",
        "commit": git(root, "rev-parse", "HEAD"),
        "build_driver_sha256": sha256(Path(__file__).resolve()),
        "tracked_product_patch_sha256": PRODUCT_PATCH_SHA256 if side == "candidate" else None,
        "source_sha256_before": before,
        "source_sha256_after": after,
        "harness_sha256": BENCH_SHA256,
        "effective_build_environment_sha256": effective_build_environment_sha256,
        "cargo_configuration_checks": configuration_checks,
        "commands": records,
        "artifacts": artifacts,
        "test_execution": "not run",
        "benchmark_timing": "not run",
    })
    return {
        "side": side,
        "status": "built",
        "receipt_path": str(attempt_dir / f"{side}-build-receipt.json"),
        "receipt_sha256": sha256(attempt_dir / f"{side}-build-receipt.json"),
        "saved_targets": [target["key"] for target in BUILD_TARGETS[side]],
    }


def tool_metadata(env: dict[str, str]) -> dict:
    def capture(command: list[str]) -> dict[str, object]:
        executable = shutil.which(command[0], path=env.get("PATH"))
        if executable is None:
            raise RuntimeError(f"required tool not found in controlled build PATH: {command[0]}")
        argv = [executable, *command[1:]]
        result = subprocess.run(argv, env=env, text=True, stdout=subprocess.PIPE,
                                stderr=subprocess.STDOUT, check=False)
        if result.returncode:
            raise RuntimeError(f"tool identity command failed {argv}: {result.stdout.strip()}")
        return {
            "argv": argv,
            "executable_path": executable,
            "executable_sha256": sha256(Path(executable)),
            "output": result.stdout.strip(),
        }

    cargo_configuration = cargo_configuration_fingerprint(env)
    cargo_version = capture(["cargo", "--version", "--verbose"])
    rustc_version = capture(["rustc", "--version", "--verbose"])
    rustc_sysroot = capture(["rustc", "--print", "sysroot"])
    active_toolchain = capture(["rustup", "show", "active-toolchain"])
    if cargo_version["executable_path"] != str(RUST_BIN / "cargo"):
        raise RuntimeError("cargo identity query did not use the fixed toolchain directory")
    if rustc_version["executable_path"] != env["RUSTC"] or rustc_sysroot["executable_path"] != env["RUSTC"]:
        raise RuntimeError("rustc identity queries did not use the fixed RUSTC executable")
    if active_toolchain["executable_path"] != str(RUST_BIN / "rustup"):
        raise RuntimeError("rustup identity query did not use the fixed toolchain directory")
    if not active_toolchain["output"].startswith(TOOLCHAIN):
        raise RuntimeError("active rustup toolchain differs from the fixed RUSTUP_TOOLCHAIN")
    if cargo_configuration_fingerprint(env) != cargo_configuration:
        raise RuntimeError("Cargo configuration changed during pre-clean toolchain capture")
    return {
        "captured_at_utc": datetime.now(timezone.utc).isoformat(),
        "platform": platform.platform(),
        "uname": dict(zip(("system", "node", "release", "version", "machine", "processor"), platform.uname())),
        "python": sys.version,
        "cargo_version_verbose": cargo_version,
        "rustc_version_verbose": rustc_version,
        "rustc_sysroot": rustc_sysroot,
        "rustup_active_toolchain": active_toolchain,
        "build_environment": build_environment_snapshot(env),
        "cargo_configuration_before_build": cargo_configuration,
        "target_directory_is_shared_across_arms": str(TARGET),
        "preclean_capture": True,
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--attempt", default="attempt-1", help="new immutable output directory name")
    args = parser.parse_args()
    if not args.attempt.replace("-", "").isalnum():
        raise SystemExit("--attempt must contain only letters, digits, and hyphens")
    if platform.system() != "Darwin" or platform.machine() not in {"arm64", "aarch64"}:
        raise SystemExit("line-framing paired build is pinned to macOS arm64")
    for root in (BASELINE, CANDIDATE):
        if not root.is_dir():
            raise SystemExit(f"missing pinned worktree: {root}")

    attempt_dir = PAIR_EVIDENCE / "build" / args.attempt
    attempt_dir.mkdir(parents=True, exist_ok=False)
    TARGET.mkdir(parents=True, exist_ok=True)
    env = controlled_build_environment(dict(os.environ))
    metadata = tool_metadata(env)
    metadata["build_driver_sha256"] = sha256(Path(__file__).resolve())
    metadata["baseline_commit"] = git(BASELINE, "rev-parse", "HEAD")
    metadata["candidate_commit"] = git(CANDIDATE, "rev-parse", "HEAD")
    metadata["candidate_product_patch_sha256"] = hashlib.sha256(
        subprocess.check_output(
        ["git", "diff", "--binary", "HEAD", "--", "Cargo.lock", "Cargo.toml", "src/runtime/streaming.rs"],
            cwd=CANDIDATE,
        )
    ).hexdigest()
    metadata["benchmark_harness_sha256"] = sha256(BASELINE / "tests/native_stream_status_bench.rs")
    write_json(attempt_dir / "preclean-tool-metadata.json", metadata)

    if metadata["baseline_commit"] != BASE_COMMIT or metadata["candidate_commit"] != BASE_COMMIT:
        raise SystemExit("worktree commit pin mismatch")
    if metadata["candidate_product_patch_sha256"] != PRODUCT_PATCH_SHA256:
        raise SystemExit("candidate product patch pin mismatch")
    if metadata["benchmark_harness_sha256"] != BENCH_SHA256:
        raise SystemExit("baseline benchmark harness pin mismatch")
    if sha256(CANDIDATE / "tests/native_stream_status_bench.rs") != BENCH_SHA256:
        raise SystemExit("candidate benchmark harness differs from baseline")

    effective_environment_sha256 = metadata["build_environment"]["effective_environment_sha256"]
    cargo_configuration_before_build = metadata["cargo_configuration_before_build"]
    try:
        baseline_result = build_side(
            "baseline", BASELINE, attempt_dir, env,
            cargo_configuration_before_build, effective_environment_sha256,
        )
        # This package-scoped clean invalidates baseline artifacts from the shared
        # target. Its immutable saved test binary and receipt remain untouched.
        candidate_result = build_side(
            "candidate", CANDIDATE, attempt_dir, env,
            cargo_configuration_before_build, effective_environment_sha256,
        )
        cargo_configuration_after_build = verify_cargo_configuration(
            cargo_configuration_before_build, env, "after the paired build"
        )
    except Exception as error:
        write_json(attempt_dir / "pair-build-failure.json", {
            "schema": "execute.line-framing-build-pair-failure/1",
            "attempt": args.attempt,
            "error": f"{type(error).__name__}: {error}",
            "no_retry": True,
            "preserved_logs_and_partial_receipts": True,
        })
        raise
    write_json(attempt_dir / "build-pair-receipt.json", {
        "schema": "execute.line-framing-build-pair/1",
        "status": "complete_build_only",
        "attempt": args.attempt,
        "base_commit": BASE_COMMIT,
        "candidate_product_patch_sha256": PRODUCT_PATCH_SHA256,
        "benchmark_harness_sha256": BENCH_SHA256,
        "build_driver_sha256": sha256(Path(__file__).resolve()),
        "target_directory": str(TARGET),
        "effective_build_environment_sha256": effective_environment_sha256,
        "cargo_configuration_before_build_sha256": cargo_configuration_before_build["sha256"],
        "cargo_configuration_after_build": cargo_configuration_after_build,
        "baseline_receipt_sha256": baseline_result["receipt_sha256"],
        "candidate_receipt_sha256": candidate_result["receipt_sha256"],
        "clean_before_each_arm": "cargo clean --release --package execute",
        "baseline_receipt": str(attempt_dir / "baseline-build-receipt.json"),
        "candidate_receipt": str(attempt_dir / "candidate-build-receipt.json"),
        "test_execution": "not run",
        "benchmark_timing": "not run",
    })
    print(json.dumps({"status": "complete_build_only", "attempt_dir": str(attempt_dir)}), flush=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
