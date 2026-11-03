#!/usr/bin/env python3
"""Render and validate the replacement-gate summary embedded in its challenge page."""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
LEDGER = ROOT / "docs" / "replacement-gates.json"
PAGE = ROOT / "docs" / "replacement-challenge.md"
START = "<!-- replacement-gates:generated:start -->"
END = "<!-- replacement-gates:generated:end -->"


def current_release() -> str:
    text = (ROOT / "Cargo.toml").read_text(encoding="utf-8")
    match = re.search(r'^version\s*=\s*"([^"]+)"', text, re.MULTILINE)
    if not match:
        raise ValueError("package version is missing from Cargo.toml")
    return match.group(1)


def current_commit() -> str:
    return subprocess.check_output(
        ["git", "rev-parse", "HEAD"], cwd=ROOT, text=True
    ).strip()


def table(ledger: dict) -> str:
    lines = [
        f"**Ledger snapshot:** Execute `{ledger['release']}` at `{ledger['commit']}`.",
        "",
        "| Gate | Requirement | Status | Evidence and remaining work |",
        "| --- | --- | --- | --- |",
    ]
    for gate in ledger["gates"]:
        evidence = gate["evidence_and_gap"].replace("|", "\\|").replace("\n", " ")
        status = gate["status"].replace("_", " ").title()
        lines.append(
            f"| {gate['id']} · {gate['priority']} | {gate['requirement']} | {status} | {evidence} |"
        )
    return "\n".join(lines)


def render(ledger: dict, page: str) -> str:
    if page.count(START) != 1 or page.count(END) != 1:
        raise ValueError("challenge page must contain one generated-summary marker pair")
    before, rest = page.split(START, 1)
    _, after = rest.split(END, 1)
    return f"{before}{START}\n{table(ledger)}\n{END}{after}"


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--write", action="store_true", help="refresh ledger snapshot and page")
    args = parser.parse_args(argv)
    ledger = json.loads(LEDGER.read_text(encoding="utf-8"))
    if args.write:
        ledger["release"] = current_release()
        ledger["commit"] = current_commit()
        LEDGER.write_text(json.dumps(ledger, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
    elif ledger.get("release") != current_release():
        print("replacement ledger release is stale; run tools/replacement_gates.py --write", file=sys.stderr)
        return 1
    elif not re.fullmatch(r"[0-9a-f]{40}", ledger.get("commit", "")):
        print("replacement ledger must name a full source commit; run tools/replacement_gates.py --write", file=sys.stderr)
        return 1
    page = PAGE.read_text(encoding="utf-8")
    generated = render(ledger, page)
    if args.write:
        PAGE.write_text(generated, encoding="utf-8")
    elif generated != page:
        print("replacement challenge summary disagrees with replacement-gates.json", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
