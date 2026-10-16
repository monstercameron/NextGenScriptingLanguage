#!/usr/bin/env python3
"""Update one pam_systemd session row while preserving original line endings."""

from __future__ import annotations

import argparse
import re
from pathlib import Path


PAM_ROW = re.compile(rb"^(\s*-?session\s+\S+\s+pam_systemd\.so)(.*)$")


def split_ending(line: bytes) -> tuple[bytes, bytes]:
    if line.endswith(b"\r\n"):
        return line[:-2], b"\r\n"
    if line.endswith((b"\n", b"\r")):
        return line[:-1], line[-1:]
    return line, b""


def transform(contents: bytes, source: str = "<fixture>") -> bytes:
    updated: list[bytes] = []
    matches = 0
    for line in contents.splitlines(keepends=True):
        body, ending = split_ending(line)
        match = PAM_ROW.fullmatch(body)
        if match is None:
            updated.append(line)
            continue
        matches += 1
        prefix, tail = match.groups()
        if re.search(rb"\btype=", tail):
            tail = re.sub(rb"\btype=\S+", b"type=wayland", tail, count=1)
        else:
            tail = b" type=wayland" + tail
        updated.append(prefix + tail + ending)
    if matches != 1:
        raise ValueError(f"expected one pam_systemd session row in {source}, found {matches}")
    return b"".join(updated)


def self_test() -> None:
    fixture = (
        b"auth required pam_env.so\r\n"
        b"-session optional pam_systemd.so type=linger\r\n"
        b"session required pam_unix.so\n"
        b"session optional pam_lastlog.so"
    )
    expected = (
        b"auth required pam_env.so\r\n"
        b"-session optional pam_systemd.so type=wayland\r\n"
        b"session required pam_unix.so\n"
        b"session optional pam_lastlog.so"
    )
    actual = transform(fixture)
    if actual != expected:
        raise AssertionError(f"PAM transform changed line boundaries or content: {actual!r}")
    if actual.splitlines() != expected.splitlines():
        raise AssertionError("PAM rows following the edited row are no longer separate")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("path", nargs="?", type=Path)
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        if args.path is not None:
            parser.error("--self-test does not take a path")
        self_test()
        print("PAM line-ending fixture passed")
        return 0
    if args.path is None:
        parser.error("provide a PAM file path or use --self-test")
    original = args.path.read_bytes()
    args.path.write_bytes(transform(original, str(args.path)))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
