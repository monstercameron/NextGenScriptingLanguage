#!/usr/bin/env python3
"""Check that every `unsafe` block and `unsafe impl` has a `// SAFETY:` comment.

This is a platform-independent companion to clippy's `undocumented_unsafe_blocks`:
clippy only sees the cfg of the target it compiles, while this scan reads every Rust
file, so Unix-only and Windows-only code are checked from either host.

A block passes when the comment block directly above its line (blank lines not
allowed in between) contains `SAFETY:`, or when the same line does. `unsafe fn`
declarations are documented with a `# Safety` section and are not checked here.

Usage:
  python3 tools/check_unsafe.py              check the tree; exit 1 on a finding
  python3 tools/check_unsafe.py --list       list every unsafe block with its status
"""

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SCAN_DIRS = ("src", "tests", "browser", "fuzz", "examples", "benches")
UNSAFE_BLOCK = re.compile(r"(?<![\w\"'])unsafe\s*\{")
UNSAFE_IMPL = re.compile(r"(?<![\w\"'])unsafe\s+impl\b")


def strip_strings(line: str) -> str:
    """Blank out string and char literals so `unsafe {` inside text is not matched."""
    out = []
    quote = None
    i = 0
    while i < len(line):
        ch = line[i]
        if quote:
            if ch == "\\":
                out.append("  ")
                i += 2
                continue
            if ch == quote:
                quote = None
            out.append(" ")
        elif ch in "\"'":
            # A lifetime (`'a`) is not a char literal; keep it.
            if ch == "'" and i + 2 < len(line) and line[i + 2] != "'" and line[i + 1].isalpha():
                out.append(ch)
            else:
                quote = ch
                out.append(" ")
        elif line.startswith("//", i):
            break
        else:
            out.append(ch)
        i += 1
    return "".join(out)


def documented(lines: list[str], index: int) -> bool:
    """True when the comment block above line `index` has a SAFETY: marker."""
    if "SAFETY:" in lines[index]:
        return True
    found = False
    i = index - 1
    while i >= 0:
        stripped = lines[i].strip()
        if not stripped.startswith("//"):
            break
        if "SAFETY:" in stripped:
            found = True
        i -= 1
    return found


def scan(path: Path) -> list[tuple[int, str, bool]]:
    lines = path.read_text(encoding="utf-8", errors="replace").splitlines()
    results = []
    for number, raw in enumerate(lines):
        code = strip_strings(raw)
        if UNSAFE_BLOCK.search(code) or UNSAFE_IMPL.search(code):
            kind = "impl" if UNSAFE_IMPL.search(code) else "block"
            results.append((number + 1, kind, documented(lines, number)))
    return results


def rust_files() -> list[Path]:
    files = []
    for directory in SCAN_DIRS:
        base = ROOT / directory
        if base.is_dir():
            files.extend(sorted(path for path in base.rglob("*.rs") if "target" not in path.parts))
    return files


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--list", action="store_true", help="list every unsafe block with its status")
    args = parser.parse_args(argv)
    total = 0
    missing = []
    for path in rust_files():
        for line, kind, ok in scan(path):
            total += 1
            relative = path.relative_to(ROOT).as_posix()
            if args.list:
                print(f"{'ok ' if ok else 'MISS'} {relative}:{line} {kind}")
            if not ok:
                missing.append(f"{relative}:{line} unsafe {kind} has no SAFETY: comment")
    if not args.list:
        for finding in missing:
            print(finding)
    print(f"{total} unsafe blocks and impls, {len(missing)} without a SAFETY: comment")
    return 1 if missing else 0


if __name__ == "__main__":
    sys.exit(main())
