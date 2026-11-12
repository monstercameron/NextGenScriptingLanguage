#!/usr/bin/env python3
"""Example-corpus quality gate (issue #509).

Scans the example programs (examples/**/*.program) for workarounds that have a
first-class replacement in Execute, counts findings per rule, and compares the counts
with the committed baseline tools/example_lint_baseline.json. The check fails when the
count of any rule grows. Counts may go down; lower the baseline with --update-baseline.

Rules:
  example.jq_object_build   jq -n with --arg/--argjson that builds an object literal
                            from bindings. Object literals with references work.
  example.printf_bind       printf whose only job is to bind a value (`... as NAME`).
  example.text_concat_json  text.concat that builds JSON text from literal parts.
  example.unread_binding    `as NAME` binding whose name is never read as a code
                            identifier anywhere else in the same file (approximate).

Suppress one finding with a `# execute-allow: <rule-id>` comment on the same line.
For example.unread_binding the existing `lint.unused_binding` allow also applies.

Usage:
  python tools/example_lint.py                    check counts against the baseline
  python tools/example_lint.py --list             also print every finding
  python tools/example_lint.py --update-baseline  write the current counts as baseline

Exit status: 0 when no count grew, 1 when a count grew, 2 when the baseline is unusable.
Standard library only.
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
RULES = (
    "example.jq_object_build",
    "example.printf_bind",
    "example.text_concat_json",
    "example.unread_binding",
)
# Allow comments that also suppress a rule. Keys are rule ids.
ALLOW_ALIASES = {"example.unread_binding": {"lint.unused_binding"}}
# Words that follow `as` as a type or format name rather than a binding.
TYPE_WORDS = {"text", "boolean", "integer", "number", "list", "json", "raw"}

STRING_RE = re.compile(r'"((?:[^"\\]|\\.)*)"')
ALLOW_RE = re.compile(r"execute-allow:\s*([A-Za-z0-9_.]+)")
JQ_CALL_RE = re.compile(r'(?:run|then)\s+"?jq"?\s+with\b')
PRINTF_CALL_RE = re.compile(r'(?:run|then)\s+"?printf"?\s+with\b')
CONCAT_CALL_RE = re.compile(r'(?:run|then)\s+"?text\.concat"?\s+with\b')
BIND_AS_RE = re.compile(r"\bas\s+([A-Za-z_][A-Za-z0-9_]*)\b")
BIND_TAIL_RE = re.compile(r"\bas\s+([A-Za-z_][A-Za-z0-9_]*)\s*$")
IDENT_RE = re.compile(r"\b[A-Za-z_][A-Za-z0-9_]*\b")


def split_comment(line: str) -> tuple[str, str]:
    """Split a line into (code, comment). A # inside a string literal is not a comment."""
    in_string = False
    i = 0
    while i < len(line):
        ch = line[i]
        if in_string:
            if ch == "\\":
                i += 2
                continue
            if ch == '"':
                in_string = False
        elif ch == '"':
            in_string = True
        elif ch == "#":
            return line[:i], line[i:]
        i += 1
    return line, ""


def string_literals(code: str) -> list[str]:
    return STRING_RE.findall(code)


def is_jq_object_build(code: str) -> bool:
    if not JQ_CALL_RE.search(code):
        return False
    literals = string_literals(code)
    return (
        "-n" in literals
        and any(lit in ("--arg", "--argjson") for lit in literals)
        and any("{" in lit for lit in literals)
    )


def is_printf_bind(code: str) -> bool:
    return bool(PRINTF_CALL_RE.search(code) and BIND_TAIL_RE.search(code.rstrip()))


def is_text_concat_json(code: str) -> bool:
    if not CONCAT_CALL_RE.search(code):
        return False
    return any("{" in lit or '\\"' in lit for lit in string_literals(code))


def binding_names(code: str) -> list[str]:
    """Names bound by `as NAME` on one statement line. accept lines declare types only."""
    if code.lstrip().startswith("accept "):
        return []
    return [m for m in BIND_AS_RE.findall(code) if m not in TYPE_WORDS]


def allowed(rule: str, comment: str) -> bool:
    ids = {rule} | ALLOW_ALIASES.get(rule, set())
    return any(found in ids for found in ALLOW_RE.findall(comment))


def analyze_text(rel_path: str, text: str) -> list[dict]:
    """Return findings for one program: dicts with rule, path, line and text."""
    lines = text.splitlines()
    code_lines = [split_comment(line) for line in lines]
    # Identifier reads: code outside string literals and comments, counted per file.
    read_counts: dict[str, int] = {}
    for code, _comment in code_lines:
        for ident in IDENT_RE.findall(STRING_RE.sub('""', code)):
            read_counts[ident] = read_counts.get(ident, 0) + 1
    bindings: dict[str, int] = {}
    sites: list[tuple[int, str]] = []
    for index, (code, _comment) in enumerate(code_lines):
        for name in binding_names(code):
            bindings[name] = bindings.get(name, 0) + 1
            sites.append((index, name))

    findings: list[dict] = []

    def add(rule: str, index: int) -> None:
        comment = code_lines[index][1]
        if allowed(rule, comment):
            return
        findings.append(
            {"rule": rule, "path": rel_path, "line": index + 1, "text": lines[index].strip()}
        )

    for index, (code, _comment) in enumerate(code_lines):
        if is_jq_object_build(code):
            add("example.jq_object_build", index)
        if is_printf_bind(code):
            add("example.printf_bind", index)
        if is_text_concat_json(code):
            add("example.text_concat_json", index)
    for index, name in sites:
        # Every identifier occurrence is a binding site: nothing reads the name.
        if read_counts.get(name, 0) == bindings[name]:
            add("example.unread_binding", index)
    return sorted(findings, key=lambda f: (f["path"], f["line"], f["rule"]))


def lint_tree(root: Path) -> list[dict]:
    corpus = root / "examples"
    findings: list[dict] = []
    if not corpus.is_dir():
        return findings
    for path in sorted(corpus.rglob("*.program")):
        rel = path.relative_to(root).as_posix()
        text = path.read_text(encoding="utf-8", errors="replace")
        findings.extend(analyze_text(rel, text))
    return findings


def count_by_rule(findings: list[dict]) -> dict[str, int]:
    counts = {rule: 0 for rule in RULES}
    for finding in findings:
        counts[finding["rule"]] += 1
    return counts


def load_baseline(path: Path) -> dict[str, int]:
    data = json.loads(path.read_text(encoding="utf-8"))
    counts = data["counts"]
    if not isinstance(counts, dict) or not all(isinstance(v, int) for v in counts.values()):
        raise ValueError("baseline counts must map rule ids to integers")
    return {rule: int(counts.get(rule, 0)) for rule in RULES}


def write_baseline(path: Path, counts: dict[str, int]) -> None:
    payload = {"version": 1, "corpus": "examples", "counts": counts}
    text = json.dumps(payload, indent=2, sort_keys=True) + "\n"
    with open(path, "w", encoding="utf-8", newline="\n") as handle:
        handle.write(text)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Example-corpus quality gate (issue #509).")
    parser.add_argument("--root", type=Path, default=REPO_ROOT, help="repository root")
    parser.add_argument("--baseline", type=Path, default=None, help="baseline JSON path")
    parser.add_argument("--list", action="store_true", help="print every finding")
    parser.add_argument(
        "--update-baseline", action="store_true", help="write current counts as the baseline"
    )
    args = parser.parse_args(argv)

    root = args.root.resolve()
    baseline_path = args.baseline or root / "tools" / "example_lint_baseline.json"
    findings = lint_tree(root)
    counts = count_by_rule(findings)

    if args.list:
        for finding in findings:
            print(f"{finding['path']}:{finding['line']}: {finding['rule']}: {finding['text']}")

    if args.update_baseline:
        write_baseline(baseline_path, counts)
        print(f"wrote {baseline_path.as_posix()}")
        for rule in RULES:
            print(f"  {rule} {counts[rule]}")
        return 0

    try:
        baseline = load_baseline(baseline_path)
    except (OSError, ValueError, KeyError) as error:
        print(f"example_lint: cannot read baseline {baseline_path}: {error}", file=sys.stderr)
        print("example_lint: create it with --update-baseline", file=sys.stderr)
        return 2

    grew = []
    for rule in RULES:
        now, was = counts[rule], baseline[rule]
        status = "ok"
        if now > was:
            status = "GREW"
            grew.append(rule)
        elif now < was:
            status = "lower: run --update-baseline"
        print(f"{rule:26} {now:5} baseline {was:5}  {status}")
    if grew:
        print("example_lint: finding count grew for: " + ", ".join(grew), file=sys.stderr)
        print("example_lint: fix the new examples or add an execute-allow comment", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.stdout.reconfigure(errors="backslashreplace")
    sys.exit(main())
