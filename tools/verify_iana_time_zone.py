#!/usr/bin/env python3
"""Verify the vendored iana-time-zone 0.1.65 tree against its crates.io archive.

The vendored copy is a deliberate patch (`[patch.crates-io]` in Cargo.toml). Every
difference from upstream must be listed in PATCH_RECORD, and each recorded file must
still hash to the recorded SHA-256. Anything else fails the check.
"""
import hashlib
import io
import subprocess
import sys
import tarfile
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
VENDOR = ROOT / "vendor/iana-time-zone"
VERSION = "0.1.65"
ARCHIVE_URL = (
    "https://static.crates.io/crates/iana-time-zone/"
    "iana-time-zone-" + VERSION + ".crate"
)

# Reviewed record of the vendored patch. Hashes are SHA-256 of the committed vendor
# blobs (`git show HEAD:vendor/iana-time-zone/<path>`), so they hold on CRLF and LF
# checkouts. Update only after reviewing the patch change itself.
PATCH_RECORD = {
    # Upstream files that the vendored tree leaves out.
    "missing": frozenset({"Cargo.lock"}),
    # Vendored files with no upstream counterpart: path -> SHA-256 of vendored content.
    "vendored_only": {
        "src/tz_darwin_macos.rs": "111110fb12aa78a05f5ad43ac56dae17ae709632e5a937c16b0c21e5613f15c4",
    },
    # Upstream files the patch changes: path -> SHA-256 of vendored content.
    "changed": {
        "Cargo.toml": "f868dc1fdd37e8ef303b377850615d61fc99976dc7b6382bb1e544038ed6ea22",
        "Cargo.toml.orig": "b446ea14d48dbe43a2a8b9d92d3c5cbd0b2ec431595e473b6467c7273c71dcef",
        "src/lib.rs": "d623d788c995387371bba919818a488b27a477d7f76e6f5ee34c573803c72733",
    },
}

_LABELS = {
    "missing": "missing from vendor",
    "vendored-only": "vendored-only",
    "content": "content differs",
}


def archive_files(archive_bytes):
    """Return regular files from a .crate tarball by crate-relative path."""
    files = {}
    with tarfile.open(fileobj=io.BytesIO(archive_bytes), mode="r:gz") as archive:
        for member in archive.getmembers():
            if not member.isfile():
                continue
            parts = Path(member.name).parts
            if len(parts) < 2 or parts[0] != "iana-time-zone-" + VERSION:
                raise ValueError("unexpected path in upstream archive: " + member.name)
            extracted = archive.extractfile(member)
            if extracted is None:
                raise ValueError("could not read upstream file: " + member.name)
            files[Path(*parts[1:]).as_posix()] = extracted.read()
    return files


def _disk_files(directory):
    return {
        path.relative_to(directory).as_posix(): path.read_bytes()
        for path in directory.rglob("*")
        if path.is_file()
    }


def _git(directory, *args):
    return subprocess.run(
        ["git", "-C", str(directory), *args], capture_output=True, check=True
    ).stdout


def _in_git_work_tree(directory):
    try:
        out = subprocess.run(
            ["git", "-C", str(directory), "rev-parse", "--is-inside-work-tree"],
            capture_output=True,
            check=False,
        ).stdout
    except OSError:
        return False
    return out.strip() == b"true"


def _git_files(directory):
    """Committed (index) blob contents, plus untracked files read from disk."""
    files = {}
    listing = _git(directory, "ls-files", "-s", "-z", "--", ".")
    for entry in listing.split(b"\0"):
        if not entry:
            continue
        meta, _, name = entry.partition(b"\t")
        mode, blob, stage = meta.split(b" ")
        if stage != b"0" or mode not in (b"100644", b"100755"):
            raise ValueError(
                "unsupported index entry in vendor tree: " + name.decode("utf-8", "replace")
            )
        files[name.decode("utf-8")] = _git(
            directory, "cat-file", "blob", blob.decode("ascii")
        )
    untracked = _git(directory, "ls-files", "-o", "--exclude-standard", "-z", "--", ".")
    for entry in untracked.split(b"\0"):
        if entry:
            name = entry.decode("utf-8")
            files[name] = (directory / name).read_bytes()
    return files


def vendor_files(directory=VENDOR):
    """Vendored file contents by relative path.

    Inside a git work tree the committed blobs are used, so a CRLF checkout compares
    the same bytes as an LF checkout. Otherwise the bytes are read from disk.
    """
    if _in_git_work_tree(directory):
        return _git_files(directory)
    return _disk_files(directory)


def _differences(upstream, vendored):
    """(kind, path) pairs with kind 'missing', 'vendored-only' or 'content'."""
    result = []
    for name in sorted(upstream.keys() | vendored.keys()):
        if name not in upstream:
            result.append(("vendored-only", name))
        elif name not in vendored:
            result.append(("missing", name))
        elif upstream[name] != vendored[name]:
            result.append(("content", name))
    return result


def compare_files(upstream, vendored):
    """Raw differences between upstream and vendored trees, with no patch record applied."""
    return [_LABELS[kind] + ": " + name for kind, name in _differences(upstream, vendored)]


def _sha256(data):
    return hashlib.sha256(data).hexdigest()


def _hash_problem(name, data, expected):
    return "patched file changed: %s (sha256 %s, recorded %s)" % (
        name,
        _sha256(data),
        expected,
    )


def check_patch(upstream, vendored, record=PATCH_RECORD):
    """Return a list of problems. An empty list means every difference is the recorded patch."""
    problems = []
    differences = _differences(upstream, vendored)
    for kind, name in differences:
        if kind == "missing":
            if name not in record["missing"]:
                problems.append("unexpected missing from vendor: " + name)
        elif kind == "vendored-only":
            expected = record["vendored_only"].get(name)
            if expected is None:
                problems.append("unexpected vendored-only file: " + name)
            elif _sha256(vendored[name]) != expected:
                problems.append(_hash_problem(name, vendored[name], expected))
        else:
            expected = record["changed"].get(name)
            if expected is None:
                problems.append("unexpected content difference: " + name)
            elif _sha256(vendored[name]) != expected:
                problems.append(_hash_problem(name, vendored[name], expected))

    # A recorded entry must still describe the tree; a stale record is a failure too.
    for name in sorted(record["missing"]):
        if name not in upstream or name in vendored:
            problems.append("recorded as missing from vendor but not so: " + name)
    for name in sorted(record["vendored_only"]):
        if name not in vendored or name in upstream:
            problems.append("recorded as vendored-only but not so: " + name)
    for name in sorted(record["changed"]):
        if name not in upstream or name not in vendored or vendored[name] == upstream[name]:
            problems.append("recorded patch no longer differs from upstream: " + name)
    return problems


def main():
    try:
        with urllib.request.urlopen(ARCHIVE_URL, timeout=30) as response:
            upstream = archive_files(response.read())
        problems = check_patch(upstream, vendor_files(VENDOR))
    except Exception as error:
        print("iana-time-zone upstream verification failed: " + str(error), file=sys.stderr)
        return 1
    if problems:
        print(
            "vendor/iana-time-zone differs from crates.io iana-time-zone 0.1.65 "
            "outside the recorded patch (PATCH_RECORD in tools/verify_iana_time_zone.py):",
            file=sys.stderr,
        )
        for problem in problems:
            print("  " + problem, file=sys.stderr)
        return 1
    print(
        "vendor/iana-time-zone matches crates.io iana-time-zone 0.1.65 "
        "except the recorded patch"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
