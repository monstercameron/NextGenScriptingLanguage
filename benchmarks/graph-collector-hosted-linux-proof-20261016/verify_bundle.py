#!/usr/bin/env python3
"""Verify package files, frozen source pins, and every member of the hosted ZIP."""
from __future__ import annotations
import hashlib
import json
import lzma
import pathlib
import sys
import zipfile

ROOT = pathlib.Path(__file__).resolve().parent
MANIFEST = ROOT / "manifest.json"
CHECKSUM = ROOT / "manifest.sha256"


def sha256_stream(stream):
    digest = hashlib.sha256()
    size = 0
    while True:
        chunk = stream.read(1024 * 1024)
        if not chunk:
            break
        digest.update(chunk)
        size += len(chunk)
    return digest.hexdigest(), size


def sha256_file(path):
    with path.open("rb") as stream:
        return sha256_stream(stream)


def require(condition, message):
    if not condition:
        raise SystemExit(f"FAIL: {message}")


def safe_path(value):
    path = pathlib.PurePosixPath(value)
    require(not path.is_absolute() and ".." not in path.parts, f"unsafe package path: {value}")
    return path


def main():
    require(MANIFEST.is_file() and CHECKSUM.is_file(), "manifest.json or manifest.sha256 is missing")
    manifest_digest, _ = sha256_file(MANIFEST)
    fields = CHECKSUM.read_text(encoding="ascii").strip().split()
    require(len(fields) == 2 and fields == [manifest_digest, "manifest.json"], "manifest.sha256 does not pin manifest.json")
    manifest = json.loads(MANIFEST.read_text(encoding="utf-8"))
    require(manifest.get("schema_version") == 1, "unsupported manifest schema")
    expected_files = {}
    for row in manifest.get("files", []):
        rel = safe_path(row["path"]).as_posix()
        require(rel not in expected_files, f"duplicate manifest path: {rel}")
        expected_files[rel] = row
    actual_files = {}
    for path in ROOT.rglob("*"):
        if path.is_symlink():
            raise SystemExit(f"FAIL: symlink found: {path.relative_to(ROOT)}")
        if path.is_file():
            rel = path.relative_to(ROOT).as_posix()
            if rel not in {"manifest.json", "manifest.sha256"}:
                actual_files[rel] = path
    require(set(actual_files) == set(expected_files),
            f"inventory mismatch; missing={sorted(set(expected_files)-set(actual_files))[:5]} extra={sorted(set(actual_files)-set(expected_files))[:5]}")
    for rel, row in expected_files.items():
        digest, size = sha256_file(actual_files[rel])
        require(digest == row["sha256"] and size == row["size_bytes"], f"file hash/size mismatch: {rel}")

    audit = json.loads((ROOT / "evidence/hosted-linux-independent-audit.json").read_text(encoding="utf-8"))
    archive = ROOT / "evidence/hosted-linux-run-38878319193.zip"
    archive_hash, archive_size = sha256_file(archive)
    require(archive_hash == audit["artifact_zip_sha256"] and archive_size == audit["artifact_zip_bytes"], "hosted ZIP byte pin mismatch")
    with zipfile.ZipFile(archive) as zf:
        infos = zf.infolist()
        expected = audit["zip_members"]
        expected_by_name = {row["name"]: row for row in expected}
        require(len(infos) == len(expected) == len(expected_by_name), "hosted ZIP member count or uniqueness mismatch")
        require({info.filename for info in infos} == set(expected_by_name), "hosted ZIP member names differ from audit")
        member_map = {}
        for index, info in enumerate(infos, 1):
            row = expected_by_name[info.filename]
            require(info.file_size == row["size"] and info.compress_size == row["compressed_size"], f"ZIP size mismatch: {info.filename}")
            require(f"{info.CRC:08x}" == row["crc32"] and info.is_dir() == row["directory"], f"ZIP CRC/type mismatch: {info.filename}")
            with zf.open(info, "r") as stream:
                digest, size = sha256_stream(stream)
            require(digest == row["sha256"] and size == row["size"], f"ZIP content mismatch: {info.filename}")
            member_map[info.filename] = row
            if index % 100 == 0:
                print(f"verified {index}/{len(infos)} hosted ZIP members", flush=True)
    mapping = json.loads((ROOT / "evidence/final-run/extracted-members.json").read_text(encoding="utf-8"))["extractions"]
    for package_rel, member in mapping.items():
        require(member in member_map, f"extracted evidence maps to missing ZIP member {member}")
        digest, size = sha256_file(ROOT / safe_path(package_rel).as_posix())
        require(digest == member_map[member]["sha256"] and size == member_map[member]["size"], f"extracted member mismatch: {member}")

    pins = json.loads((ROOT / "provenance/source-pins.json").read_text(encoding="utf-8"))
    require(pins["sample_auditor_sha256"] == audit["sample_oracle_auditor_sha256"], "sample auditor pin differs from audit receipt")
    for side in ("baseline", "candidate"):
        digest, _ = sha256_file(ROOT / f"provenance/source-snapshots/{side}-transport.rs")
        require(digest == pins[side]["sha256"] == pins["expected_audit_transport_sha256"][side], f"{side} transport snapshot mismatch")
    direct = {
        "workflow/graph-collector-e2e-hosted-replication.yml": "workflow_sha256",
        "workflow/build_graph_e2e_pair.py": "build_helper_sha256",
        "workflow/graph_transport_e2e_pair.py": "paired_runner_sha256",
        "workflow/graph_transport_e2e_bench.rs": "harness_sha256",
        "workflow/test_graph_transport_e2e_pair.py": "paired_runner_tests_sha256",
        "workflow/test_build_graph_e2e_pair.py": "build_helper_tests_sha256",
        "audit/audit_hosted_graph_transport_e2e_zip.py": "audit_source_sha256",
        "audit/test_audit_hosted_graph_transport_e2e_zip.py": "auditor_tests_sha256",
        "work/graph-collector-hosted-replication-audit/audit_hosted_graph_transport_e2e_zip.py": "audit_source_sha256",
        "work/graph-collector-hosted-replication-audit/test_audit_hosted_graph_transport_e2e_zip.py": "auditor_tests_sha256",
        "work/graph-transport-e2e-paired-runner/graph_transport_e2e_pair.py": "paired_runner_sha256",
        "work/build_graph_e2e_pair.py": "build_helper_sha256",
        "work/graph-collector-hosted-replication-publication/work/graph-transport-e2e-paired-runner/graph-collector-candidate.patch.xz": "candidate_patch_archive_sha256",
        "work/graph-collector-hosted-replication-publication/work/graph-transport-e2e-paired-runner/graph_transport_e2e_bench.rs": "harness_sha256",
        "evidence/graph-transport-e2e/audit_graph_transport_e2e.py": "sample_auditor_sha256",
        "evidence/final-run/extracted/paired-preregistration.json": "preregistration_sha256",
        "workflow/graph-collector-candidate.patch.xz": "candidate_patch_archive_sha256",
    }
    for rel, key in direct.items():
        digest, _ = sha256_file(ROOT / rel)
        require(digest == pins[key], f"source pin mismatch: {rel}")
    with lzma.open(ROOT / "workflow/graph-collector-candidate.patch.xz", "rb") as stream:
        decoded, _ = sha256_stream(stream)
    require(decoded == pins["candidate_patch_decoded_sha256"], "decoded candidate patch hash mismatch")
    print(f"PASS: {len(expected_files)} package files, {len(infos)} streamed ZIP members, source pins and extracted evidence")
    print(f"ZIP bytes: {archive_size}; SHA-256: {archive_hash}")
    print(f"Run {audit['run_id']}: {audit['measured_candidate_wins']} wins, {audit['measured_candidate_losses']} losses, {audit['measured_ties']} ties; paired median ratio {audit['median_paired_candidate_over_baseline']:.16f}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
