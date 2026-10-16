#!/usr/bin/env python3
"""Fixed-source build and provenance support for the hosted JSON-argument replication."""
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

import resolved_json_arguments_pair as pair

BASE_COMMIT = "b824aeaa7bccaa4d3d75d3fc6a363a99138287d4"
RUNNER_SHA256 = "00afb9395517d065a90344eb1635bbbfaa20d3d0bb3f32b4cbe93b7fa7862571"
RUNNER_TEST_SHA256 = "f483c11e9785ec8d53cb3ff8e85377c08b4842dbd48560669657fb3b09f442f4"
HARNESS_SHA256 = "627d8466f0c0ef8264e483d456118bb7eb6f4e14757a752fbd226efecf917003"
WORKLOAD_SHA256 = "a2a95cfb03e55164e459467b088eeffd1d8034fb7023a7e3dfcd806274177b1d"
BASELINE_RUNTIME_SHA256 = "2d144408c177655b28eae66afe7303733f77aa91434f6ab5edc4b37cf76fa67f"
CANDIDATE_RUNTIME_SHA256 = "da5eb3d178f8c8716dd87c2af5064b71d46cc8b3cdfff6c21d437e1a22d53215"
BASELINE_TESTS_SHA256 = "df113191a596fa94eb8a5bf325081db3ce5a8bd03da09946b75a83b35f2f1da3"
CANDIDATE_TESTS_SHA256 = "ae87f2706fbeada5bd3565e90cc8a3bea1f6c637e07162b87c1dc903b7f94681"
BUILD_TIMEOUT_SECONDS = 2700
PATCHES = {
    "baseline": {
        "archive": "baseline-b824.patch.xz",
        "archive_sha256": "e77e63d99a94e2e3ded307f0f2876102267b3dfbcd6b4c3c5891d5b271a02778",
        "patch_sha256": "a7ac079649ce8a290354dec0885b30db1fc209e34cebc761cfe5cc83d0eb79cb",
        "runtime_sha256": BASELINE_RUNTIME_SHA256,
        "tests_sha256": BASELINE_TESTS_SHA256,
        "tracked_changes": ["src/runtime.rs"],
    },
    "candidate": {
        "archive": "candidate-b824.patch.xz",
        "archive_sha256": "60d49df4fb6983382c8dc4b57794ddbe8c2e315103e7c9fce3fbee64c6481be3",
        "patch_sha256": "4b65d2e5d6ffe356d70982c0735cbc44067cd28b6101e169867f1aa3b4b60ae2",
        "runtime_sha256": CANDIDATE_RUNTIME_SHA256,
        "tests_sha256": CANDIDATE_TESTS_SHA256,
        "tracked_changes": ["src/runtime.rs", "tests/structured_tasks.rs"],
    },
}
EXPECTED_UNTRACKED = [
    "benchmarks/workloads/resolved-json-arguments-repeat-1000.program",
    "src/runtime/resolved_json_arguments_bench.rs",
]


class EvidenceError(RuntimeError):
    """A frozen input, build, or receipt did not satisfy its exact contract."""


def utc_now() -> str:
    return datetime.now(timezone.utc).isoformat(timespec="seconds").replace("+00:00", "Z")


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


def _git(root: Path, *args: str) -> str:
    result = subprocess.run(
        ["git", "-C", str(root), *args],
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
        timeout=30,
    )
    if result.returncode:
        raise EvidenceError(result.stderr.decode("utf-8", errors="replace").strip())
    return result.stdout.decode("utf-8").strip()


def verify_repository_inputs(repo_root: Path) -> dict[str, str]:
    repo_root = repo_root.resolve(strict=True)
    here = Path(__file__).resolve().parent
    expected = {
        "resolved_json_arguments_pair.py": RUNNER_SHA256,
        "test_resolved_json_arguments_pair.py": RUNNER_TEST_SHA256,
        "resolved_json_arguments_bench.rs": HARNESS_SHA256,
        "resolved-json-arguments-repeat-1000.program": WORKLOAD_SHA256,
        PATCHES["baseline"]["archive"]: PATCHES["baseline"]["archive_sha256"],
        PATCHES["candidate"]["archive"]: PATCHES["candidate"]["archive_sha256"],
    }
    observed: dict[str, str] = {}
    for name, digest in expected.items():
        path = here / name
        if not path.is_file():
            raise EvidenceError(f"required frozen input is missing: {path}")
        actual = sha256_file(path)
        if actual != digest:
            raise EvidenceError(f"frozen input hash mismatch for {name}: {actual}")
        observed[name] = actual
    for side, config in PATCHES.items():
        try:
            patch = lzma.decompress((here / config["archive"]).read_bytes(), format=lzma.FORMAT_XZ)
        except lzma.LZMAError as error:
            raise EvidenceError(f"{side} frozen patch archive is not valid XZ: {error}") from error
        if sha256_bytes(patch) != config["patch_sha256"]:
            raise EvidenceError(f"{side} decompressed patch hash mismatch")
    return observed


def unpack_patch(side: str, repo_root: Path, output: Path) -> dict[str, Any]:
    if side not in PATCHES:
        raise EvidenceError("patch side must be baseline or candidate")
    observed = verify_repository_inputs(repo_root)
    config = PATCHES[side]
    archive = Path(__file__).resolve().parent / config["archive"]
    payload = lzma.decompress(archive.read_bytes(), format=lzma.FORMAT_XZ)
    if sha256_bytes(payload) != config["patch_sha256"]:
        raise EvidenceError(f"{side} decompressed patch digest differs from the frozen digest")
    output = output.expanduser().absolute()
    if output.exists() or output.is_symlink():
        raise EvidenceError(f"refusing to overwrite patch output: {output}")
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_bytes(payload)
    return {
        "side": side,
        "archive": str(archive),
        "archive_sha256": observed[config["archive"]],
        "patch": str(output),
        "patch_sha256": sha256_file(output),
    }


def _source_input_summary(root: Path) -> dict[str, Any]:
    root = root.resolve(strict=True)
    if _git(root, "rev-parse", "HEAD") != BASE_COMMIT:
        raise EvidenceError(f"source root is not at frozen base commit {BASE_COMMIT}: {root}")
    fingerprint = pair.source_fingerprint(root)
    if fingerprint["files"].get(pair.HARNESS_RELATIVE) != HARNESS_SHA256:
        raise EvidenceError("source root benchmark harness does not match the frozen digest")
    if fingerprint["files"].get(pair.WORKLOAD_RELATIVE) != WORKLOAD_SHA256:
        raise EvidenceError("source root workload does not match the frozen digest")
    return {
        "root": str(root),
        "commit": BASE_COMMIT,
        "fingerprint": fingerprint,
        "runtime_sha256": fingerprint["files"].get("src/runtime.rs"),
        "structured_tasks_sha256": fingerprint["files"].get("tests/structured_tasks.rs"),
        "harness_sha256": fingerprint["files"][pair.HARNESS_RELATIVE],
        "workload_sha256": fingerprint["files"][pair.WORKLOAD_RELATIVE],
    }


def _validate_patched_arm(root: Path, side: str) -> dict[str, Any]:
    root = root.resolve(strict=True)
    config = PATCHES[side]
    summary = _source_input_summary(root)
    changed = _git(root, "diff", "--name-only").splitlines()
    staged = _git(root, "diff", "--cached", "--name-only").splitlines()
    untracked = sorted(item for item in _git(root, "ls-files", "--others", "--exclude-standard").splitlines() if item)
    if changed != config["tracked_changes"] or staged or untracked != EXPECTED_UNTRACKED:
        raise EvidenceError(
            f"{side} applied tree has unexpected changes: tracked={changed}, staged={staged}, untracked={untracked}"
        )
    if summary["runtime_sha256"] != config["runtime_sha256"]:
        raise EvidenceError(f"{side} src/runtime.rs hash differs from frozen source")
    if summary["structured_tasks_sha256"] != config["tests_sha256"]:
        raise EvidenceError(f"{side} tests/structured_tasks.rs hash differs from frozen source")
    checked = subprocess.run(
        ["git", "-C", str(root), "diff", "--check"],
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
        timeout=30,
    )
    if checked.returncode:
        raise EvidenceError(f"{side} git diff --check failed: {checked.stdout.decode(errors='replace')}{checked.stderr.decode(errors='replace')}")
    return summary


def verify_inputs_command(args: argparse.Namespace) -> None:
    observed = verify_repository_inputs(Path(args.repo_root))
    output = Path(args.evidence_root).expanduser().absolute() / "build/verified-inputs.json"
    write_json(output, {"verified_at": utc_now(), "hashes": observed})
    print(json.dumps({"verification_receipt": str(output), "hashes": observed}, sort_keys=True))


def apply_patches(args: argparse.Namespace) -> None:
    repo_root = Path(args.repo_root).resolve(strict=True)
    source_parent = Path(args.source_parent).resolve(strict=True)
    evidence_root = Path(args.evidence_root).expanduser().absolute()
    observed = verify_repository_inputs(repo_root)
    build_dir = evidence_root / "build"
    patch_dir = build_dir / "patches"
    patch_dir.mkdir(parents=True, exist_ok=False)
    source_summaries: dict[str, Any] = {}
    patch_records: dict[str, Any] = {}
    for side in ("baseline", "candidate"):
        root = (source_parent / side).resolve(strict=True)
        if _git(root, "rev-parse", "HEAD") != BASE_COMMIT:
            raise EvidenceError(f"{side} checkout is not the fixed base {BASE_COMMIT}")
        status = _git(root, "status", "--porcelain", "--untracked-files=all")
        if status:
            raise EvidenceError(f"{side} checkout must be clean before patching: {status}")
        patch_path = patch_dir / f"{side}-b824.patch"
        patch_records[side] = unpack_patch(side, repo_root, patch_path)
        log_path = build_dir / f"patch-{side}.log"
        commands = [
            ["git", "-C", str(root), "apply", "--check", "--whitespace=error", str(patch_path)],
            ["git", "-C", str(root), "apply", "--whitespace=error", str(patch_path)],
        ]
        log = bytearray()
        for command in commands:
            result = subprocess.run(
                command,
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                check=False,
                timeout=60,
            )
            log.extend(("$ " + " ".join(command) + "\n").encode())
            log.extend(result.stdout)
            log.extend(result.stderr)
            if result.returncode:
                log_path.write_bytes(bytes(log))
                raise EvidenceError(f"{side} frozen patch application failed with status {result.returncode}")
        log_path.write_bytes(bytes(log))
        source_summaries[side] = _validate_patched_arm(root, side)
    baseline_files = source_summaries["baseline"]["fingerprint"]["files"]
    candidate_files = source_summaries["candidate"]["fingerprint"]["files"]
    if baseline_files.keys() != candidate_files.keys():
        raise EvidenceError("paired source arms have different build-input path sets")
    differences = sorted(path for path in baseline_files if baseline_files[path] != candidate_files[path])
    if differences != ["src/runtime.rs", "tests/structured_tasks.rs"]:
        raise EvidenceError(f"paired arms differ outside the two frozen implementation/test paths: {differences}")
    if source_summaries["baseline"]["harness_sha256"] != source_summaries["candidate"]["harness_sha256"]:
        raise EvidenceError("paired source arms do not share identical harness bytes")
    if source_summaries["baseline"]["workload_sha256"] != source_summaries["candidate"]["workload_sha256"]:
        raise EvidenceError("paired source arms do not share identical workload bytes")
    freeze = {
        "schema": "execute.runtime-json-arguments.hosted-source-freeze/1",
        "recorded_at": utc_now(),
        "base_commit": BASE_COMMIT,
        "patches": patch_records,
        "arms": source_summaries,
        "allowed_arm_differences": differences,
        "shared_harness_sha256": HARNESS_SHA256,
        "shared_workload_sha256": WORKLOAD_SHA256,
        "claim": "frozen Linux replication build inputs only; no timing or performance conclusion",
    }
    write_json(build_dir / "source-freeze.json", freeze)
    print(json.dumps({"source_freeze": str(build_dir / "source-freeze.json"), "patches": patch_records}, sort_keys=True))


def _artifact_rows(payload: bytes) -> Iterable[dict[str, Any]]:
    try:
        text = payload.decode("utf-8")
    except UnicodeDecodeError as error:
        raise EvidenceError(f"Cargo JSON output is not UTF-8: {error}") from error
    for line_number, line in enumerate(text.splitlines(), 1):
        if not line:
            continue
        try:
            record = json.loads(line)
        except json.JSONDecodeError as error:
            raise EvidenceError(f"Cargo stdout line {line_number} is not JSON: {error}") from error
        if isinstance(record, dict) and record.get("reason") == "compiler-artifact":
            yield record


def select_library_test_artifact(records: Iterable[dict[str, Any]], root: Path, target_dir: Path) -> tuple[dict[str, Any], Path]:
    root = root.resolve(strict=True)
    target_dir = target_dir.resolve(strict=True)
    manifest = (root / "Cargo.toml").resolve(strict=True)
    source = (root / "src/lib.rs").resolve(strict=True)
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
            executable_value = row.get("executable")
            executable = Path(executable_value).resolve(strict=True) if isinstance(executable_value, str) else None
        except (KeyError, OSError, TypeError, ValueError):
            continue
        kinds = target.get("kind")
        if (
            row.get("reason") == "compiler-artifact"
            and row.get("fresh") is False
            and row_manifest == manifest
            and row_source == source
            and target.get("name") == "flow"
            and target.get("test") is True
            and isinstance(kinds, list)
            and "lib" in kinds
            and isinstance(target.get("crate_types"), list)
            and "lib" in target["crate_types"]
            and profile.get("test") is True
            and profile.get("opt_level") == "3"
            and profile.get("debug_assertions") is False
            and executable is not None
            and executable.parent == release_deps
            and os.access(executable, os.X_OK)
            and isinstance(row.get("package_id"), str)
            and row["package_id"].startswith(f"path+{root.as_uri()}#execute@")
            and isinstance(row.get("filenames"), list)
            and executable_value in row["filenames"]
        ):
            matches.append((row, executable))
    if len(matches) != 1:
        raise EvidenceError(f"expected exactly one freshly compiled release library-test artifact, found {len(matches)}")
    return matches[0]


def _run_capture(command: list[str], *, env: dict[str, str] | None = None, timeout: int = 30) -> bytes:
    result = subprocess.run(
        command,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        env=env,
        check=False,
        timeout=timeout,
    )
    if result.returncode:
        raise EvidenceError(f"command failed ({result.returncode}): {command!r}: {result.stderr.decode(errors='replace')}")
    return result.stdout


def _tool_versions(root: Path, output: Path) -> None:
    py = _run_capture([sys.executable, "--version"])
    cargo = _run_capture(["cargo", "--version"])
    rustc = _run_capture(["rustc", "--version", "--verbose"])
    output.write_bytes(b"python: " + py + b"cargo: " + cargo + b"rustc:\n" + rustc)


def build_pair(args: argparse.Namespace) -> int:
    source_parent = Path(args.source_parent).resolve(strict=True)
    evidence_root = Path(args.evidence_root).expanduser().absolute()
    target_parent = Path(args.target_parent).expanduser().absolute()
    freeze_path = evidence_root / "build/source-freeze.json"
    freeze = json.loads(freeze_path.read_text(encoding="utf-8"))
    if freeze.get("base_commit") != BASE_COMMIT or set(freeze.get("arms", {})) != {"baseline", "candidate"}:
        raise EvidenceError("source freeze is missing or does not bind both fixed arms")
    target_parent.mkdir(parents=True, exist_ok=True)
    failures: list[str] = []
    for side in ("baseline", "candidate"):
        root = (source_parent / side).resolve(strict=True)
        out_dir = evidence_root / "build" / side
        out_dir.mkdir(parents=True, exist_ok=False)
        target_dir = target_parent / f"literal-json-arguments-{side}-target"
        command = [
            "cargo", "test", "--manifest-path", str(root / "Cargo.toml"), "--locked",
            "--release", "--lib", "--no-run", "--message-format=json",
        ]
        environment = os.environ.copy()
        environment["CARGO_TARGET_DIR"] = str(target_dir)
        environment["CARGO_INCREMENTAL"] = "0"
        environment_record = {
            key: environment[key]
            for key in ("CARGO_TARGET_DIR", "CARGO_INCREMENTAL", "RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "RUSTC_WRAPPER", "CARGO_BUILD_TARGET", "RUSTUP_TOOLCHAIN")
            if key in environment
        }
        write_json(out_dir / "build-command.json", {"side": side, "argv": command, "environment": environment_record, "timeout_seconds": BUILD_TIMEOUT_SECONDS, "attempt": 1})
        version_error = None
        try:
            _tool_versions(root, out_dir / "tool-versions.txt")
        except (EvidenceError, OSError, subprocess.TimeoutExpired) as error:
            version_error = str(error)
            (out_dir / "tool-versions-error.txt").write_text(version_error + "\n", encoding="utf-8")
        target_existed = target_dir.exists() or target_dir.is_symlink()
        write_json(out_dir / "target-freshness.json", {"path": str(target_dir), "existed_before_build": target_existed, "confirmed_fresh": not target_existed})
        status = None
        stdout = b""
        stderr = b""
        timed_out = False
        error = version_error
        before = pair.source_fingerprint(root)
        if before != freeze["arms"][side]["fingerprint"]:
            error = "source fingerprint differs from the source-freeze receipt before build"
        elif target_existed:
            error = f"target directory was not fresh: {target_dir}"
        elif version_error is None:
            target_dir.mkdir(parents=True)
            try:
                completed = subprocess.run(command, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=environment, check=False, timeout=BUILD_TIMEOUT_SECONDS)
                status, stdout, stderr = completed.returncode, completed.stdout, completed.stderr
            except subprocess.TimeoutExpired as exception:
                timed_out = True
                status = 124
                stdout = exception.stdout or b""
                stderr = exception.stderr or b""
                error = f"Cargo build exceeded {BUILD_TIMEOUT_SECONDS} seconds; no retry"
            except OSError as exception:
                error = f"{type(exception).__name__}: {exception}"
                status = 127
                stderr = error.encode("utf-8", errors="replace")
        (out_dir / "cargo-stdout.jsonl").write_bytes(stdout)
        (out_dir / "cargo-stderr.log").write_bytes(stderr)
        (out_dir / "cargo-exit-status.txt").write_text(("not-started" if status is None else str(status)) + "\n", encoding="utf-8")
        after = pair.source_fingerprint(root)
        source_stable = before == after and before == freeze["arms"][side]["fingerprint"]
        write_json(out_dir / "source-stability.json", {"before": before, "after": after, "stable": source_stable})
        artifact = None
        build_receipt = None
        receipt_status = 1
        receipt_error = error
        if status not in (None, 0) and receipt_error is None:
            receipt_error = f"Cargo build failed with exit status {status}; no retry"
        if status == 0 and not source_stable and receipt_error is None:
            receipt_error = "source fingerprint changed during build"
        if status == 0 and source_stable:
            try:
                rows = list(_artifact_rows(stdout))
                artifact, compiler_binary = select_library_test_artifact(rows, root, target_dir)
                output_binary = out_dir / f"{side}-test-binary"
                if output_binary.exists() or output_binary.is_symlink():
                    raise EvidenceError(f"refusing to overwrite saved binary: {output_binary}")
                shutil.copy2(compiler_binary, output_binary)
                output_binary.chmod(output_binary.stat().st_mode | 0o111)
                digest = sha256_file(output_binary)
                runtime_hash = before["files"].get("src/runtime.rs")
                if runtime_hash != freeze["arms"][side]["runtime_sha256"]:
                    raise EvidenceError("src/runtime.rs digest changed after source freeze")
                build_receipt = {
                    "artifact": artifact,
                    "saved_binary": str(output_binary.resolve(strict=True)),
                    "sha256": digest,
                    "source_sha256": runtime_hash,
                    "source_stable": source_stable,
                    "attempt": 1,
                }
                write_json(out_dir / "build-receipt.json", build_receipt)
                receipt_status = 0
                receipt_error = None
            except (EvidenceError, OSError, ValueError, KeyError) as exception:
                receipt_error = str(exception)
        (out_dir / "receipt-exit-status.txt").write_text(f"{receipt_status}\n", encoding="utf-8")
        if receipt_error:
            (out_dir / "receipt-error.log").write_text(receipt_error + "\n", encoding="utf-8")
        result = {
            "side": side,
            "cargo_exit_status": status,
            "timed_out": timed_out,
            "target_directory_fresh": not target_existed,
            "source_stable": source_stable,
            "artifact_selected": artifact is not None,
            "receipt_exit_status": receipt_status,
            "build_receipt_sha256": sha256_file(out_dir / "build-receipt.json") if build_receipt else None,
            "error": receipt_error,
        }
        write_json(out_dir / "build-result.json", result)
        if status != 0 or timed_out or not source_stable or receipt_status != 0:
            failures.append(side)
    write_json(evidence_root / "build/build-pair-result.json", {"completed_at": utc_now(), "sides": ["baseline", "candidate"], "failed_sides": failures, "no_retries": True})
    print(json.dumps({"failed_sides": failures}, sort_keys=True))
    return 1 if failures else 0


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    verify = commands.add_parser("verify-inputs", help="verify exact checked-in runner, harness, workload, and patch pins")
    verify.add_argument("--repo-root", required=True)
    verify.add_argument("--evidence-root", required=True)
    verify.set_defaults(function=verify_inputs_command)
    apply = commands.add_parser("apply-patches", help="apply the fixed baseline/candidate source diffs")
    apply.add_argument("--repo-root", required=True)
    apply.add_argument("--source-parent", required=True)
    apply.add_argument("--evidence-root", required=True)
    apply.set_defaults(function=apply_patches)
    build = commands.add_parser("build-pair", help="build both release library-test binaries once and record Cargo receipts")
    build.add_argument("--source-parent", required=True)
    build.add_argument("--evidence-root", required=True)
    build.add_argument("--target-parent", required=True)
    build.set_defaults(function=build_pair)
    return parser


def main(argv: list[str] | None = None) -> int:
    args = _parser().parse_args(argv)
    try:
        result = args.function(args)
        if result is not None:
            print(json.dumps(result, sort_keys=True))
        return 0
    except (EvidenceError, OSError, subprocess.TimeoutExpired, KeyError, ValueError) as error:
        print(f"literal JSON hosted replication: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
