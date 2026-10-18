#!/usr/bin/env python3
"""Aggregate nextest JUnit reports from repeated runs and report every test that failed.

Each run writes one JUnit file (target/nextest/flaky/junit.xml). The nightly flake job
downloads them into one directory per run, then calls this script. A test is reported when
it failed in at least one run. It is `flaky` when it also passed in some run, and `failing`
when it failed in every run that executed it. The failure messages and captured output are
kept, so a report names the test, the runs it failed in, and what it printed.

Usage:
  python3 tools/collect_flaky.py REPORT_DIR [--json OUT] [--summary FILE]

Exit status is 1 when any test failed in any run, and 0 otherwise. A run whose JUnit file is
missing is listed, because a run that never produced a report hides its results.
"""

from __future__ import annotations

import argparse
import json
import re
import sys
import xml.etree.ElementTree as ET  # Reads only reports nextest wrote in the same workflow run.
from collections import defaultdict
from pathlib import Path

OUTPUT_LIMIT = 4000


def run_label(path: Path, root: Path) -> str:
    """The run is the first directory under the report root, such as junit-ubuntu-24.04-7."""
    relative = path.relative_to(root)
    return relative.parts[0] if len(relative.parts) > 1 else relative.stem


def testcase_outcome(case: ET.Element) -> tuple[str, str]:
    """Return (status, detail) for one testcase element."""
    for child in case:
        tag = child.tag
        if tag in ("failure", "error"):
            message = child.get("message") or ""
            body = (child.text or "").strip()
            return "failed", "\n".join(part for part in (message, body) if part)
        if tag == "skipped":
            return "skipped", ""
    return "passed", ""


def captured_output(case: ET.Element) -> str:
    parts = []
    for child in case:
        if child.tag in ("system-out", "system-err") and child.text:
            parts.append(f"[{child.tag}]\n{child.text.strip()}")
    return "\n".join(parts)


def os_label(run: str) -> str:
    return re.sub(r"-\d+$", "", run)


def collect(
    root: Path, expected_os: list[str] | None = None, expected_runs: int | None = None
) -> tuple[dict, list[str], list[str], list[str]]:
    reports = sorted(root.rglob("*.xml"))
    runs = sorted({run_label(path, root) for path in reports})
    tests: dict[str, dict] = defaultdict(
        lambda: {"passed": 0, "failed": 0, "failed_runs": [], "message": "", "output": "", "by_os": {}}
    )
    errors: list[str] = []
    for path in reports:
        run = run_label(path, root)
        try:
            tree = ET.parse(path)
        except ET.ParseError as error:
            errors.append(f"{path}: unreadable JUnit XML ({error})")
            continue
        for case in tree.iter("testcase"):
            name = f"{case.get('classname', '')} {case.get('name', '')}".strip()
            status, detail = testcase_outcome(case)
            entry = tests[name]
            platform = entry["by_os"].setdefault(
                os_label(run), {"passed": 0, "failed": 0}
            )
            if status == "passed":
                entry["passed"] += 1
                platform["passed"] += 1
            elif status == "failed":
                entry["failed"] += 1
                platform["failed"] += 1
                entry["failed_runs"].append(run)
                if not entry["message"]:
                    entry["message"] = detail[:OUTPUT_LIMIT]
                    entry["output"] = captured_output(case)[:OUTPUT_LIMIT]
    missing = []
    if expected_os is not None and expected_runs is not None:
        seen = set(runs)
        missing = [
            f"junit-{platform}-{number}"
            for platform in expected_os
            for number in range(1, expected_runs + 1)
            if f"junit-{platform}-{number}" not in seen
        ]
    return tests, runs, errors, missing


def classify(entry: dict) -> str:
    if entry["failed"] == 0:
        return "passed"
    platforms = entry.get("by_os", {})
    if any(counts["failed"] and not counts["passed"] for counts in platforms.values()):
        return "failing"
    return "flaky" if entry["passed"] > 0 else "failing"


def classify_platform(counts: dict) -> str:
    if counts["failed"] == 0:
        return "passed"
    return "flaky" if counts["passed"] else "failing"


def summary_markdown(
    tests: dict, runs: list[str], errors: list[str], missing: list[str] | None = None
) -> str:
    missing = missing or []
    failed = {name: entry for name, entry in tests.items() if entry["failed"]}
    lines = [
        "# Flaky test detection",
        "",
        f"Runs with reports: {len(runs)}. Tests seen: {len(tests)}. "
        f"Tests that failed in at least one run: {len(failed)}.",
        "",
    ]
    if errors:
        lines += ["## Unreadable reports", ""] + [f"- {error}" for error in errors] + [""]
    if missing:
        lines += ["## Missing reports", ""] + [f"- {run}" for run in missing] + [""]
    if not failed:
        lines.append("No test failed in any run with a report.")
        return "\n".join(lines) + "\n"
    for name, entry in sorted(failed.items(), key=lambda item: (classify(item[1]), item[0])):
        failing_platforms = [
            platform
            for platform, counts in sorted(entry.get("by_os", {}).items())
            if classify_platform(counts) == "failing"
        ]
        platform_note = f" on {', '.join(failing_platforms)}" if failing_platforms else ""
        lines += [
            f"## {name}",
            "",
            f"- Status: **{classify(entry)}**{platform_note} ({entry['failed']} failed, "
            f"{entry['passed']} passed)",
            f"- Failed in: {', '.join(sorted(set(entry['failed_runs'])))}",
            "",
            "```text",
            entry["message"] or "(no failure message)",
            "```",
        ]
        if entry["output"]:
            lines += ["", "<details><summary>Captured output</summary>", "", "```text",
                      entry["output"], "```", "", "</details>"]
        lines.append("")
    return "\n".join(lines) + "\n"


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("report_dir", type=Path)
    parser.add_argument("--json", type=Path, help="write the full report as JSON")
    parser.add_argument("--summary", type=Path, help="write the Markdown summary (e.g. $GITHUB_STEP_SUMMARY)")
    parser.add_argument("--expect-runs", type=int, help="expected number of reports per OS")
    parser.add_argument("--expect-os", action="append", help="expected OS label; repeat per platform")
    args = parser.parse_args(argv)
    if not args.report_dir.is_dir():
        print(f"no report directory at {args.report_dir}", file=sys.stderr)
        return 2
    if (args.expect_runs is None) != (args.expect_os is None):
        parser.error("--expect-runs and --expect-os must be supplied together")
    tests, runs, errors, missing = collect(args.report_dir, args.expect_os, args.expect_runs)
    markdown = summary_markdown(tests, runs, errors, missing)
    sys.stdout.write(markdown)
    if args.summary:
        with args.summary.open("a", encoding="utf-8") as handle:
            handle.write(markdown)
    if args.json:
        document = {
            "runs": runs,
            "missing_reports": missing,
            "errors": errors,
            "tests": {
                name: {**entry, "status": classify(entry)}
                for name, entry in sorted(tests.items())
                if entry["failed"]
            },
        }
        args.json.write_text(json.dumps(document, indent=2), encoding="utf-8")
    failed = any(entry["failed"] for entry in tests.values())
    return 1 if failed or errors or missing else 0


if __name__ == "__main__":
    sys.exit(main())
