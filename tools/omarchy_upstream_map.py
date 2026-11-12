#!/usr/bin/env python3
"""Map Omarchy upstream shell tests to the bin/ commands they exercise.

Upstream Omarchy's shell tests (test/shell.d/ and test/acceptance.d/) call commands
through Bash explicitly, for example:

  bash "$ROOT/bin/omarchy-install-app" ...

A port of such a command can only be certified by the upstream tests that call it.
This tool scans a directory of upstream test files and reports, for every bin/ command
the tests call, which files call it and how often. It also lists the bin/ files that a
test sources instead of running (`source "$ROOT/bin/NAME"`); those cannot be run
through a Bash stub and are incompatible by design.

The tool reads text only. It does not run any test, does not need the upstream
checkout to be pinned, and uses the standard library only.

Usage:
  python3 tools/omarchy_upstream_map.py test/shell.d            text table
  python3 tools/omarchy_upstream_map.py test/shell.d --json     JSON for CI
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from collections import Counter
from pathlib import Path
from typing import Any

# Files larger than this are skipped. Upstream test files are small; the cap only
# stops a stray binary from being read whole.
MAX_BYTES = 16 * 1024 * 1024

# bash "$ROOT/bin/NAME" and bash $ROOT/bin/NAME, with ${ROOT} accepted as well.
CALL_RE = re.compile(r'\b(?:bash|sh)\s+"?\$\{?ROOT\}?/bin/([A-Za-z0-9][A-Za-z0-9._-]*)"?')

# A line that sources a bin/ file: source "$ROOT/bin/NAME" or . "$ROOT/bin/NAME".
SOURCE_RE = re.compile(
    r'^\s*(?:source|\.)\s+"?\$\{?ROOT\}?/bin/([A-Za-z0-9][A-Za-z0-9._-]*)', re.MULTILINE
)


def scan_text(text: str) -> tuple[Counter[str], set[str]]:
    """Return (call counts per command, names sourced) for one file's text."""
    calls: Counter[str] = Counter(match.group(1) for match in CALL_RE.finditer(text))
    sourced = {match.group(1) for match in SOURCE_RE.finditer(text)}
    return calls, sourced


def map_upstream(test_root: Path) -> dict[str, Any]:
    """Scan every regular file under test_root and build the command-to-test map.

    Paths in the result are relative to test_root, in POSIX form.
    """
    commands: dict[str, dict[str, int]] = {}
    sourced: dict[str, set[str]] = {}
    files_scanned = 0
    for path in sorted(p for p in test_root.rglob("*") if p.is_file()):
        try:
            if path.stat().st_size > MAX_BYTES:
                continue
            text = path.read_bytes().decode("utf-8", errors="replace")
        except OSError:
            continue
        files_scanned += 1
        rel = path.relative_to(test_root).as_posix()
        calls, names = scan_text(text)
        for name, count in calls.items():
            commands.setdefault(name, {})[rel] = count
        for name in names:
            sourced.setdefault(name, set()).add(rel)
    return {
        "test_root": test_root.as_posix(),
        "files_scanned": files_scanned,
        "commands": {
            name: {
                "total_calls": sum(files.values()),
                "files": dict(sorted(files.items())),
            }
            for name, files in sorted(commands.items())
        },
        "sourced": {name: sorted(files) for name, files in sorted(sourced.items())},
    }


def render_text(result: dict[str, Any]) -> str:
    lines = [
        f"upstream tests: {result['test_root']}",
        f"files scanned: {result['files_scanned']}",
        f"commands called: {len(result['commands'])}",
    ]
    for name, info in result["commands"].items():
        lines.append(f"{name}  ({info['total_calls']} calls in {len(info['files'])} files)")
        for rel, count in info["files"].items():
            lines.append(f"    {rel}  x{count}")
    if result["sourced"]:
        lines.append("sourced (incompatible with a Bash stub):")
        for name, files in result["sourced"].items():
            lines.append(f"  {name}: {', '.join(files)}")
    return "\n".join(lines) + "\n"


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("test_root", type=Path, help="directory of upstream test files")
    parser.add_argument("--json", action="store_true", help="print JSON instead of text")
    args = parser.parse_args(argv)
    if not args.test_root.is_dir():
        print(f"not a directory: {args.test_root}", file=sys.stderr)
        return 2
    result = map_upstream(args.test_root)
    if args.json:
        print(json.dumps(result, indent=2, sort_keys=True))
    else:
        print(render_text(result), end="")
    return 0


if __name__ == "__main__":
    sys.exit(main())
