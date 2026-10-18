#!/usr/bin/env python3
"""Verify a downloaded image against an independently supplied SHA-256."""

from __future__ import annotations

import hashlib
import hmac
import re
import sys
from pathlib import Path


def verify(path: Path, expected: str) -> bool:
    if not re.fullmatch(r"[0-9a-fA-F]{64}", expected):
        return False
    digest = hashlib.sha256()
    with path.open("rb") as image:
        for chunk in iter(lambda: image.read(1024 * 1024), b""):
            digest.update(chunk)
    actual = digest.hexdigest()
    return hmac.compare_digest(actual, expected.lower())


def main(argv: list[str]) -> int:
    if len(argv) != 3:
        print("usage: verify_image_sha256.py IMAGE EXPECTED_SHA256", file=sys.stderr)
        return 2
    path = Path(argv[1])
    try:
        matches = verify(path, argv[2])
    except OSError as error:
        print(f"cannot read image: {error}", file=sys.stderr)
        return 2
    if not matches:
        print("image SHA-256 does not match the independently supplied digest", file=sys.stderr)
        return 1
    print(f"verified image SHA-256: {argv[2].lower()}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
