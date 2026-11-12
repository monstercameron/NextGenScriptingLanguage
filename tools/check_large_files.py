#!/usr/bin/env python3
"""Fail if a tracked file is over the size limit or is an executable or archive.

Lists the files git tracks (read-only `git ls-files`) and flags any file that
is larger than MAX_BYTES, or whose first bytes match a native executable or an
archive format, unless its repository-relative path matches ALLOWED_PATTERNS.
Pass paths to check only those (for example the staged files in a hook).
Python 3.9+.
"""
from __future__ import annotations

import argparse
import fnmatch
import subprocess
import sys
from pathlib import Path

MAX_BYTES = 1024 * 1024

# Repository-relative glob patterns (fnmatch, forward slashes) that may hold a
# large or binary file. Keep this list short and say why each entry exists.
ALLOWED_PATTERNS = (
    # Bundled manifest index is text; the compressed body blob is binary by design.
    "src/bundled_manifests/bodies.bin",
    # Vendored upstream source is reviewed as third-party code, not project code.
    "vendor/*",
)

# Leading bytes of native executables and archive containers.
# Each entry is (offset, magic bytes, kind).
SIGNATURES = (
    (0, b"\x7fELF", "ELF executable"),
    (0, b"MZ", "Windows executable"),
    (0, b"\xfe\xed\xfa\xce", "Mach-O executable"),
    (0, b"\xfe\xed\xfa\xcf", "Mach-O executable"),
    (0, b"\xce\xfa\xed\xfe", "Mach-O executable"),
    (0, b"\xcf\xfa\xed\xfe", "Mach-O executable"),
    (0, b"\xca\xfe\xba\xbe", "Mach-O universal binary"),
    (0, b"\x00asm", "WebAssembly module"),
    (0, b"PK\x03\x04", "zip archive"),
    (0, b"\x1f\x8b", "gzip archive"),
    (0, b"BZh", "bzip2 archive"),
    (0, b"\xfd7zXZ\x00", "xz archive"),
    (0, b"7z\xbc\xaf\x27\x1c", "7z archive"),
    (0, b"\x28\xb5\x2f\xfd", "zstd archive"),
    (0, b"Rar!\x1a\x07", "rar archive"),
    (257, b"ustar", "tar archive"),
)
SNIFF_BYTES = 512


def repo_root() -> Path:
    return Path(__file__).resolve().parent.parent


def tracked_files(root: Path) -> list[str]:
    out = subprocess.run(
        ["git", "-C", str(root), "ls-files", "-z"],
        capture_output=True,
        check=True,
    ).stdout
    return [name for name in out.decode("utf-8", errors="replace").split("\0") if name]


def is_allowed(rel: str) -> bool:
    return any(fnmatch.fnmatchcase(rel, pattern) for pattern in ALLOWED_PATTERNS)


def binary_kind(head: bytes) -> str | None:
    for offset, magic, kind in SIGNATURES:
        if head[offset : offset + len(magic)] == magic:
            return kind
    return None


def problems_for(root: Path, rel: str) -> list[str]:
    path = root / rel
    if path.is_symlink() or not path.is_file():
        return []
    size = path.stat().st_size
    problems = []
    if size > MAX_BYTES:
        problems.append(f"{size} bytes is over the {MAX_BYTES} byte limit")
    with path.open("rb") as handle:
        kind = binary_kind(handle.read(SNIFF_BYTES))
    if kind is not None:
        problems.append(f"{kind} content")
    return problems


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--root", type=Path, default=repo_root())
    parser.add_argument(
        "paths",
        nargs="*",
        help="repository-relative paths to check instead of every tracked file",
    )
    args = parser.parse_args(argv)

    root = args.root.resolve()
    try:
        candidates = args.paths or tracked_files(root)
    except (OSError, subprocess.CalledProcessError) as error:
        print(f"error: could not list tracked files in {root}: {error}", file=sys.stderr)
        return 2

    flagged = 0
    for rel in candidates:
        rel = rel.replace("\\", "/")
        if is_allowed(rel):
            continue
        for problem in problems_for(root, rel):
            print(f"{rel}: {problem}")
            flagged += 1
            break

    if flagged:
        print(f"{flagged} file(s) flagged; move raw evidence out of git or add a reviewed allow-list entry")
        return 1
    print(f"checked {len(candidates)} file(s): none flagged")
    return 0


if __name__ == "__main__":
    sys.exit(main())
