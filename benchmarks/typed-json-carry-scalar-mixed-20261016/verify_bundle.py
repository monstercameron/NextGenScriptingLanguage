#!/usr/bin/env python3
"""Verify bundle inventory and the frozen source-map-to-snapshot bindings."""
from __future__ import annotations

import hashlib
import json
import re
import stat
import sys
from pathlib import Path, PurePosixPath

ROOT = Path(__file__).resolve().parent
HEX64 = re.compile(r"^[0-9a-f]{64}$")


def sha_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def fail(message: str) -> None:
    raise ValueError(message)


def load_json(rel: str):
    return json.loads((ROOT / rel).read_text(encoding="utf-8"))


def check_rel(rel: str) -> Path:
    parsed = PurePosixPath(rel)
    if parsed.is_absolute() or not parsed.parts or any(part in ("", ".", "..") for part in parsed.parts):
        fail(f"unsafe bundle path: {rel!r}")
    path = ROOT.joinpath(*parsed.parts)
    if path.is_symlink() or not path.is_file():
        fail(f"missing, non-file, or symlink: {rel}")
    if path.resolve().parent != ROOT.resolve() and ROOT.resolve() not in path.resolve().parents:
        fail(f"path escapes bundle: {rel}")
    return path


def validate_hash_fields(value, where: str = "root") -> None:
    if isinstance(value, dict):
        for key, child in value.items():
            if key == "sha256" or key.endswith("_sha256"):
                if child is not None and (not isinstance(child, str) or HEX64.fullmatch(child) is None):
                    fail(f"malformed SHA-256 at {where}.{key}: {child!r}")
            validate_hash_fields(child, f"{where}.{key}")
    elif isinstance(value, list):
        for index, child in enumerate(value):
            validate_hash_fields(child, f"{where}[{index}]")


def main() -> int:
    manifest_path = ROOT / "bundle-manifest.json"
    if manifest_path.is_symlink() or not manifest_path.is_file():
        fail("bundle-manifest.json is missing or a symlink")
    manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    if manifest.get("schema") != "typed-json-scalar-mixed-publication/1":
        fail("unsupported bundle manifest schema")
    entries = manifest.get("files")
    if not isinstance(entries, list):
        fail("manifest files must be a list")
    listed = {}
    for item in entries:
        rel = item.get("path")
        if not isinstance(rel, str) or rel in listed:
            fail(f"invalid or duplicate manifest path: {rel!r}")
        if HEX64.fullmatch(item.get("sha256", "")) is None:
            fail(f"invalid file digest for {rel}")
        path = check_rel(rel)
        actual = sha_file(path)
        if actual != item["sha256"] or path.stat().st_size != item.get("bytes"):
            fail(f"file digest/size mismatch: {rel}")
        listed[rel] = item

    actual_files = set()
    for path in ROOT.rglob("*"):
        if path.is_symlink():
            fail(f"symlink is not allowed in bundle: {path.relative_to(ROOT)}")
        if path.is_file():
            rel = path.relative_to(ROOT).as_posix()
            if rel not in ("bundle-manifest.json", "bundle-sha256sums.txt"):
                actual_files.add(rel)
    if actual_files != set(listed):
        fail(f"manifest inventory mismatch: unlisted={sorted(actual_files-set(listed))}; missing={sorted(set(listed)-actual_files)}")

    sums_path = check_rel("bundle-sha256sums.txt")
    expected_sums = {"bundle-manifest.json", *listed.keys()}
    sums = {}
    for line_number, line in enumerate(sums_path.read_text(encoding="ascii").splitlines(), 1):
        match = re.fullmatch(r"([0-9a-f]{64})  ([^\n]+)", line)
        if match is None:
            fail(f"malformed SHA256SUMS line {line_number}")
        digest, rel = match.groups()
        if rel in sums:
            fail(f"duplicate SHA256SUMS path: {rel}")
        path = check_rel(rel)
        if sha_file(path) != digest:
            fail(f"SHA256SUMS mismatch: {rel}")
        sums[rel] = digest
    if set(sums) != expected_sums:
        fail(f"SHA256SUMS inventory mismatch: expected {len(expected_sums)}, got {len(sums)}")

    source_map = load_json("source/source-map.json")
    fingerprints = load_json("source/source-fingerprints-v3.json")
    validate_hash_fields(source_map, "source_map")
    validate_hash_fields(fingerprints, "source_fingerprints")
    if source_map.get("schema") != "typed-json-scalar-mixed-source-map/1":
        fail("unsupported source-map schema")
    fp_binding = source_map["fingerprints"]
    fp_path = check_rel(fp_binding["path"])
    if sha_file(fp_path) != fp_binding["sha256"]:
        fail("source-fingerprints file hash differs from source map")
    if fp_binding["path_count"] != 2197:
        fail("source path count changed")

    for side in ("baseline", "candidate"):
        frozen = fingerprints["arms"][side]
        declared = source_map["arms"][side]
        if declared["head"] != frozen["head"] or declared["tracked_diff_sha256"] != frozen["tracked_diff_sha256"]:
            fail(f"{side} source identity mismatch")
        tree = frozen["tree"]
        if declared["file_count"] != tree["file_count"]:
            fail(f"{side} source file count mismatch")
        expected_tree_hash = fp_binding[f"{side}_tree_sha256"]
        if declared["tree_sha256"] != expected_tree_hash:
            fail(f"{side} tree hash mismatch")
        seen = set()
        for item in declared["files"]:
            source_path = item["source_path"]
            if source_path in seen:
                fail(f"duplicate {side} source path in bundle map: {source_path}")
            seen.add(source_path)
            expected = tree["files"].get(source_path)
            if expected is None or expected != item["sha256"]:
                fail(f"{side} mapped source digest differs from complete fingerprint: {source_path}")
            snapshot = check_rel(item["snapshot_path"])
            if sha_file(snapshot) != expected:
                fail(f"{side} source snapshot digest mismatch: {source_path}")
        required = set(fingerprints.get("arms", {}).get(side, {}).get("tree", {}).get("files", {}))
        # The full fingerprint covers the whole tree; this compact package intentionally snapshots only
        # the changed product/test files, fixed benchmark harness, and Cargo build inputs.
        if not {"Cargo.toml", "Cargo.lock", ".cargo/config.toml", "tests/opt_typed_json_carry.rs"}.issubset(seen):
            fail(f"{side} build-input snapshot set incomplete")
        for path in source_map["product"]["changed_tracked_files"]:
            if path not in seen:
                fail(f"{side} changed source missing from snapshot set: {path}")

        receipt = load_json(declared["build_receipt_path"])
        if receipt["source_fingerprint_sha256"] != declared["tree_sha256"]:
            fail(f"{side} build receipt is not bound to declared source tree")
        for artifact_name, bundle_key in (("opt_typed_json_carry", "benchmark_binary_sha256"),):
            if receipt["artifacts"][artifact_name]["saved_binary_sha256"] != declared[bundle_key]:
                fail(f"{side} benchmark binary receipt mismatch")
        if receipt["helper_artifact"]["saved_binary_sha256"] != declared["helper_binary_sha256"]:
            fail(f"{side} helper binary receipt mismatch")
        for field, path_key in (("benchmark_binary_sha256", "saved_benchmark_binary_path"), ("helper_binary_sha256", "saved_helper_binary_path")):
            binary = check_rel(declared[path_key])
            if sha_file(binary) != declared[field]:
                fail(f"{side} retained binary digest mismatch: {path_key}")
            if stat.S_IMODE(binary.stat().st_mode) != 0o555:
                fail(f"{side} retained binary permissions differ from saved artifact mode")

    # The copied trial inputs and audit record must remain mutually bound.
    audit = load_json("provenance/paired-run-v3-attempt-1-independent-audit.json")
    report_path = check_rel(source_map["report"]["path"])
    prereg_path = check_rel(source_map["preregistration"]["path"])
    runner_path = check_rel(source_map["runner"]["path"])
    driver_path = check_rel(source_map["build_driver"]["path"])
    build_receipt_path = check_rel(source_map["build_pair_receipt"]["path"])
    if sha_file(report_path) != audit["hashes"]["report"]:
        fail("audit report hash binding mismatch")
    if sha_file(prereg_path) != audit["hashes"]["preregistration"]:
        fail("audit preregistration hash binding mismatch")
    if sha_file(runner_path) != audit["hashes"]["runner"] or sha_file(runner_path) != source_map["runner"]["sha256"]:
        fail("runner source hash binding mismatch")
    if sha_file(driver_path) != audit["hashes"]["build_driver"] or sha_file(driver_path) != source_map["build_driver"]["sha256"]:
        fail("build driver hash binding mismatch")
    if sha_file(build_receipt_path) != audit["hashes"]["build_pair_receipt"]:
        fail("build receipt hash binding mismatch")
    if sha_file(fp_path) != audit["hashes"]["source_map"]:
        fail("independent audit source-map binding mismatch")
    product_patch = check_rel(source_map["product"]["patch_path"])
    harness_patch = check_rel(source_map["harness"]["patch_path"])
    if sha_file(product_patch) != source_map["product"]["patch_sha256"] or sha_file(product_patch) != freeze_product_hash():
        fail("candidate product patch hash mismatch")
    if sha_file(harness_patch) != source_map["harness"]["patch_sha256"]:
        fail("harness patch hash mismatch")

    report = load_json(source_map["report"]["path"])
    if report["attempt_count"] != 102 or report["successful_attempt_count"] != 102 or report["failed_attempt_count"] != 0:
        fail("paired report has unexpected attempt outcome")
    attempts_dir = ROOT / "raw/paired-run-v3-attempt-1/attempts"
    attempts = sorted(p for p in attempts_dir.iterdir() if p.is_dir())
    if len(attempts) != 102:
        fail(f"expected 102 preserved attempt directories, found {len(attempts)}")
    for attempt in attempts:
        required_files = {"raw.json", "stdout.bin", "stderr.bin", "started.json", "finished.json"}
        if not required_files.issubset({p.name for p in attempt.iterdir() if p.is_file()}):
            fail(f"attempt evidence incomplete: {attempt.name}")
    checks = audit.get("checks", [])
    if not checks or any(check.get("passed") is not True for check in checks):
        fail("independent audit does not report all checks passing")

    print(f"bundle verification passed: {len(listed)} files, {len(attempts)} raw attempts")
    return 0


def freeze_product_hash() -> str:
    return load_json("provenance/v3-source-freeze-receipt.json")["product"]["v3_cumulative_patch_sha256"]


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, KeyError, TypeError, ValueError, json.JSONDecodeError) as error:
        print(f"bundle verification failed: {error}", file=sys.stderr)
        raise SystemExit(1)
