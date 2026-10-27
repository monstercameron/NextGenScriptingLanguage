#!/usr/bin/env python3
"""Compare rebuilt Linux binaries with the exact binaries in a release archive."""
import argparse
import hashlib
from pathlib import Path, PurePosixPath
import shutil
import tarfile

from release import validate

BINARIES = ("execute", "execute-model", "execute-applet")
MAX_BINARY_BYTES = 512 * 1024 * 1024


def extract_binaries(archive, value, output):
    """Extract only the three exact regular-file members; reject duplicates and links."""
    validate(value)
    output.mkdir(parents=True, exist_ok=False)
    expected = {f"execute-{value}/bin/{name}": name for name in BINARIES}
    seen = set()
    all_names = set()
    with tarfile.open(archive, "r:gz") as tar:
        for member in tar:
            path = PurePosixPath(member.name)
            if (path.is_absolute() or ".." in path.parts or not path.parts
                    or path.parts[0] != f"execute-{value}"
                    or path.as_posix() != member.name or member.name in all_names
                    or not (member.isfile() or member.isdir())):
                raise ValueError(f"invalid or duplicate release archive member {member.name}")
            all_names.add(member.name)
            if member.name not in expected:
                continue
            if member.name in seen or not member.isfile() or not 0 < member.size <= MAX_BINARY_BYTES:
                raise ValueError(f"invalid or duplicate shipped binary {member.name}")
            seen.add(member.name)
            with tar.extractfile(member) as source, (output / expected[member.name]).open("xb") as target:
                shutil.copyfileobj(source, target, length=1024 * 1024)
    if seen != set(expected):
        raise ValueError("release archive is missing binaries: " + ", ".join(sorted(set(expected) - seen)))


def same_bytes(left, right):
    with left.open("rb") as a, right.open("rb") as b:
        while True:
            first, second = a.read(1024 * 1024), b.read(1024 * 1024)
            if first != second:
                return False
            if not first:
                return True


def compare(output, rebuilds):
    if len(rebuilds) != 2:
        raise ValueError("exactly two independent rebuilds are required")
    for name in BINARIES:
        shipped = output / name
        for root in rebuilds:
            candidate = root / name
            if candidate.is_symlink() or not candidate.is_file() or not same_bytes(shipped, candidate):
                raise ValueError(f"rebuilt {candidate} differs from shipped {name}")
        digest = hashlib.sha256()
        with shipped.open("rb") as file:
            for chunk in iter(lambda: file.read(1024 * 1024), b""):
                digest.update(chunk)
        print(f"{digest.hexdigest()}  shipped/{name} (both rebuilds match)")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--archive", type=Path, required=True)
    parser.add_argument("--version", required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--rebuild", type=Path, action="append", required=True)
    args = parser.parse_args()
    extract_binaries(args.archive, args.version, args.output)
    compare(args.output, args.rebuild)


if __name__ == "__main__":
    main()
