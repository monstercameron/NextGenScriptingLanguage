#!/usr/bin/env python3
"""Verify the vendored iana-time-zone 0.1.65 tree against its crates.io archive."""
import io
import sys
import tarfile
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
VERSION = "0.1.65"
ARCHIVE_URL = (
    "https://static.crates.io/crates/iana-time-zone/"
    "iana-time-zone-" + VERSION + ".crate"
)


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


def vendor_files(directory):
    return {
        path.relative_to(directory).as_posix(): path.read_bytes()
        for path in directory.rglob("*")
        if path.is_file()
    }


def compare_files(upstream, vendored):
    differences = []
    for name in sorted(upstream.keys() | vendored.keys()):
        if name not in upstream:
            differences.append("vendored-only: " + name)
        elif name not in vendored:
            differences.append("missing from vendor: " + name)
        elif upstream[name] != vendored[name]:
            differences.append("content differs: " + name)
    return differences


def main():
    try:
        with urllib.request.urlopen(ARCHIVE_URL, timeout=30) as response:
            upstream = archive_files(response.read())
        differences = compare_files(
            upstream, vendor_files(ROOT / "vendor/iana-time-zone")
        )
    except Exception as error:
        print("iana-time-zone upstream verification failed: " + str(error), file=sys.stderr)
        return 1
    if differences:
        print("iana-time-zone 0.1.65 differs from the pinned upstream crate:", file=sys.stderr)
        for difference in differences:
            print("  " + difference, file=sys.stderr)
        return 1
    print("vendor/iana-time-zone matches crates.io iana-time-zone 0.1.65")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
