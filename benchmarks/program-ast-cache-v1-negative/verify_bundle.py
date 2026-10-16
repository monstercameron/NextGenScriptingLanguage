#!/usr/bin/env python3
"""Offline checksum and lossless-member verifier for one fixed48 bundle."""
from __future__ import annotations
import hashlib, json, lzma, sys, tarfile
from pathlib import Path, PurePosixPath

ROOT=Path(__file__).resolve().parent

def sha(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()

def read_json(path: Path):
    return json.loads(path.read_text(encoding="utf-8"))

def safe_path(raw: str) -> Path:
    p=PurePosixPath(raw)
    if p.is_absolute() or not p.parts or ".." in p.parts or "\\" in raw:
        raise ValueError(f"unsafe relative path: {raw!r}")
    return ROOT.joinpath(*p.parts)

def verify():
    sum_path=ROOT/"bundle.SHA256SUMS"
    rows={}
    for line in sum_path.read_text(encoding="utf-8").splitlines():
        h, rel=line.split("  ",1)
        if rel in rows: raise ValueError(f"duplicate bundle checksum path: {rel}")
        p=safe_path(rel)
        if not p.is_file() or sha(p.read_bytes())!=h: raise ValueError(f"bundle checksum mismatch: {rel}")
        rows[rel]=h
    actual={p.relative_to(ROOT).as_posix() for p in ROOT.rglob("*") if p.is_file() and p.name!="bundle.SHA256SUMS"}
    if set(rows)!=actual: raise ValueError(f"bundle file set differs: missing={sorted(actual-set(rows))} extra={sorted(set(rows)-actual)}")

    patchmap=read_json(ROOT/"source/patch-uncompressed-sha256.json")
    for item in patchmap["patches"]:
        compressed=safe_path(item["compressed_path"]).read_bytes()
        if sha(compressed)!=item["compressed_sha256"]: raise ValueError(f"compressed patch checksum mismatch: {item['compressed_path']}")
        plain=lzma.decompress(compressed,format=lzma.FORMAT_XZ)
        if sha(plain)!=item["uncompressed_sha256"] or len(plain)!=item["uncompressed_size_bytes"]: raise ValueError(f"patch uncompressed checksum mismatch: {item['uncompressed_name']}")

    source_manifest=read_json(ROOT/"source/source-manifest.json")
    for item in source_manifest["files"]:
        p=safe_path(item["path"])
        if not p.is_file() or sha(p.read_bytes())!=item["sha256"] or p.stat().st_size!=item["size_bytes"]: raise ValueError(f"source checksum mismatch: {item['path']}")

    raw_manifest=read_json(ROOT/"raw-artifacts.manifest.json")
    raw=ROOT/raw_manifest["archive"]
    if sha(raw.read_bytes())!=raw_manifest["archive_sha256"]: raise ValueError("raw archive compressed checksum mismatch")
    with tarfile.open(raw,mode="r:xz") as tf:
        infos=tf.getmembers(); names=[x.name for x in infos]
        if names!=sorted(names) or len(names)!=len(set(names)): raise ValueError("raw archive paths are duplicated or unsorted")
        if set(names)!={x["path"] for x in raw_manifest["members"]}: raise ValueError("raw archive member names differ from manifest")
        for item in raw_manifest["members"]:
            info=tf.getmember(item["path"])
            if not info.isfile(): raise ValueError(f"non-regular raw archive member: {item['path']}")
            data=tf.extractfile(info).read()
            if len(data)!=item["size_bytes"] or sha(data)!=item["sha256"]: raise ValueError(f"raw member checksum mismatch: {item['path']}")

    audit=read_json(ROOT/"independent-audit.json")
    audit_hash=sha((ROOT/"independent-audit.json").read_bytes())
    side=(ROOT/"independent-audit.sha256").read_text(encoding="utf-8").strip()
    if side!=f"{audit_hash}  independent-audit.json" or not audit["validation"]["passed"]: raise ValueError("independent audit checksum or validation status mismatch")
    summary=read_json(ROOT/"result-summary.json")
    if len(summary["all_measured_pairs"])!=48 or len(summary["all_candidate_slower_pairs"])!=summary["measured_outcomes"]["candidate_slower_pairs"]: raise ValueError("summary lost pair or loss rows")
    if summary["all_candidate_slower_pairs"]!=audit["result"]["all_candidate_slower_pairs"]: raise ValueError("summary loss ledger differs from independent audit")
    binaries=[p.name for p in ROOT.rglob("*") if p.is_file() and p.name.endswith("test-binary")]
    if binaries: raise ValueError(f"source-only bundle unexpectedly contains executable blobs: {binaries}")
    return {"status":"verified","bundle_manifest_entries":len(rows),"source_files":len(source_manifest["files"]),"patches":len(patchmap["patches"]),"raw_members":len(raw_manifest["members"]),"attempt_directories":raw_manifest["attempt_directory_count"],"loss_rows":len(summary["all_candidate_slower_pairs"]),"audit_sha256":audit_hash}

if __name__=="__main__":
    try: print(json.dumps(verify(),sort_keys=True))
    except Exception as error:
        print(f"bundle verification failed: {error}",file=sys.stderr); sys.exit(1)
