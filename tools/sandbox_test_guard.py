#!/usr/bin/env python3
"""Fail closed when a mandatory sandbox-denial log is green for the wrong reason.

Issue #457: a filter or cfg regression can make `cargo test` exit 0 while
running zero tests, and an unavailable OS facility historically printed
"skipping ...". The sandbox-denial lane turns those silent greens into
failures: every per-binary log must contain a
`test result: ok. N passed` summary with N > 0, must not contain zero-test
markers, and must not contain a skip marker. `EXECUTE_REQUIRE_OS_SANDBOX`
(set by the lane) already turns facility-absent skips in the Rust tests
into panics; this guard is the workflow-side backstop for the same
failure mode.
"""

from __future__ import annotations

import re
import sys

SKIP_RE = re.compile(r"skipping ")
ZERO_RE = re.compile(r"test result: ok\. 0 passed|running 0 tests")
POSITIVE_RE = re.compile(r"test result: ok\. [1-9][0-9]* passed")


def check_log(text: str) -> list[str]:
    """Return one error per silent-green marker found in a cargo log."""
    errors = []
    if SKIP_RE.search(text):
        errors.append("log reports skipped tests in the mandatory lane")
    if ZERO_RE.search(text):
        errors.append("log ran zero tests in the mandatory lane")
    if not POSITIVE_RE.search(text):
        errors.append("log has no passing-test summary in the mandatory lane")
    return errors


def main(argv: list[str]) -> int:
    if len(argv) != 3:
        print(f"usage: {argv[0]} <cargo-log> <label>", file=sys.stderr)
        return 2
    log_path, label = argv[1], argv[2]
    try:
        with open(log_path, encoding="utf-8", errors="replace") as handle:
            text = handle.read()
    except OSError as error:
        print(f"::error::{label}: cannot read cargo log {log_path}: {error}")
        return 1
    errors = check_log(text)
    if errors:
        for error in errors:
            print(f"::error::{label}: {error}")
        return 1
    print(f"{label}: sandbox-denial log is green with passing tests")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
