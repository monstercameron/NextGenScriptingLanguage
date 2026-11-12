#!/usr/bin/env python3
"""Inventory a directory of Ubuntu scripts for the Ubuntu target corpus (#415).

Walks a directory tree (for example an extracted copy of /etc/update-motd.d/,
/etc/cron.daily/ or an unpacked package's maintainer scripts) and writes one
record per regular file: its path relative to the root, SHA-256, size in bytes,
the interpreter named by the shebang, and the sorted list of lexical signals
found in its text. Signals are regex hits for commands Ubuntu admin scripts
commonly use (apt, dpkg, snap, systemctl) and for shell constructs that map to
Execute features (heredocs, traps, command substitution, pipelines).

The output is deterministic: paths are sorted, keys are sorted, and no
timestamps or absolute paths are written, so two runs over the same tree give
byte-identical JSON. Symlinks are skipped so the walk cannot leave the root.
Pinning the real Ubuntu 24.04 files still needs an Ubuntu host; this tool only
measures what it is given. Standard library only.

Usage:
  python tools/ubuntu_inventory.py --root DIR [--json PATH]
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import sys

SCHEMA_VERSION = 1

# Signal name -> pattern. Command names must not be part of a longer word or path
# segment, so "apt" does not match "aptitude" and "snap" does not match "snapshot".
SIGNALS = {
    "apt": re.compile(r"(?<![\w.-])apt(?![\w.-])"),
    "apt-get": re.compile(r"(?<![\w.-])apt-get(?![\w.-])"),
    "dpkg": re.compile(r"(?<![\w.-])dpkg(?![\w.-])"),
    "snap": re.compile(r"(?<![\w.-])snap(?![\w.-])"),
    "systemctl": re.compile(r"(?<![\w.-])systemctl(?![\w.-])"),
    "heredoc": re.compile(r"<<-?\s*['\"]?[A-Za-z_]\w*"),
    "trap": re.compile(r"(?<![\w.-])trap(?![\w.-])"),
    "command_substitution": re.compile(r"\$\(|`"),
    "pipeline": re.compile(r"\|(?!\|)"),
}

SHEBANG_INTERPRETER = re.compile(r"^#!\s*(?:/usr/bin/env\s+)?(\S+)")


def sha256_of(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def shebang_of(text: str) -> str | None:
    """Return the interpreter named on the first line, or None."""
    first = text.split("\n", 1)[0].rstrip("\r")
    if not first.startswith("#!"):
        return None
    match = SHEBANG_INTERPRETER.match(first)
    if match is None:
        return None
    return os.path.basename(match.group(1))


def signals_of(text: str) -> list[str]:
    """Return the sorted names of every signal whose pattern matches the text."""
    return sorted(name for name, pattern in SIGNALS.items() if pattern.search(text))


def inventory_file(root: str, path: str) -> dict:
    with open(path, "rb") as handle:
        data = handle.read()
    text = data.decode("utf-8", errors="replace")
    return {
        "path": os.path.relpath(path, root).replace(os.sep, "/"),
        "sha256": sha256_of(data),
        "size": len(data),
        "shebang": shebang_of(text),
        "signals": signals_of(text),
    }


def walk(root: str) -> list[dict]:
    """Return one record per regular, non-symlink file under root, sorted by path."""
    records = []
    for directory, subdirs, files in os.walk(root, followlinks=False):
        subdirs.sort()
        for name in files:
            path = os.path.join(directory, name)
            if os.path.islink(path) or not os.path.isfile(path):
                continue
            records.append(inventory_file(root, path))
    records.sort(key=lambda record: record["path"])
    return records


def build_inventory(root: str) -> dict:
    files = walk(root)
    return {"schema": SCHEMA_VERSION, "count": len(files), "files": files}


def render(inventory: dict) -> str:
    return json.dumps(inventory, indent=2, sort_keys=True) + "\n"


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n", 1)[0])
    parser.add_argument("--root", required=True, help="directory of scripts to inventory")
    parser.add_argument("--json", metavar="PATH", help="write the JSON inventory to PATH")
    args = parser.parse_args(argv)

    if not os.path.isdir(args.root):
        print(f"ubuntu_inventory: not a directory: {args.root}", file=sys.stderr)
        return 2

    inventory = build_inventory(args.root)
    text = render(inventory)
    if args.json:
        with open(args.json, "w", encoding="utf-8", newline="\n") as handle:
            handle.write(text)
    else:
        sys.stdout.write(text)
    return 0


if __name__ == "__main__":
    sys.exit(main())
