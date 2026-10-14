#!/usr/bin/env python3
"""Read a pinned Omarchy checkout; never execute its scripts or installers."""
import argparse
import collections
import hashlib
import json
from pathlib import Path
import re
import subprocess

PIN = "c668141e9c42b13c80c9ca4ea108e11708c5e8a5"
SIGNALS = {
    "positional_arguments": r"\$(?:[0-9@*#]|\{[0-9@*#])",
    "environment_or_uppercase_variable": r"\$(?:[A-Z_][A-Z_0-9]*|\{[A-Z_])",
    "command_substitution": r"\$\(|`",
    "parameter_expansion": r"\$\{",
    "conditions": r"^\s*(?:if|elif|case)\b|\[\[",
    "loops": r"^\s*(?:for|while|until|select)\b",
    "functions": r"^\s*(?:function\s+\w+|[A-Za-z_]\w*\s*\(\))",
    "sourcing": r"^\s*(?:source|\.)\s+",
    "redirection": r"(?:\d|&)?[<>](?:[<>]|&|\()?|<<<",
    "background_or_jobs": r"(?:^|[^&])&\s*$|\b(?:wait|disown|nohup)\b",
    "traps": r"\btrap\s+",
    "exec": r"^\s*exec\s+",
    "explicit_exit": r"\bexit(?:\s|$)",
    "privileged_or_package_effect": r"\b(?:sudo|pacman|yay|reboot|poweroff|systemctl)\b",
}


def inventory(checkout):
    checkout = checkout.resolve(strict=True)
    commit = subprocess.run(
        ["git", "-C", str(checkout), "rev-parse", "HEAD"],
        check=True,
        capture_output=True,
        text=True,
        timeout=10,
    ).stdout.strip()
    if commit != PIN:
        raise ValueError(f"expected pinned Omarchy commit {PIN}; found {commit}")
    dirty = subprocess.run(
        [
            "git",
            "-C",
            str(checkout),
            "status",
            "--porcelain",
            "--untracked-files=all",
            "--ignored",
            "--",
            "bin",
            "LICENSE",
        ],
        check=True,
        capture_output=True,
        text=True,
        timeout=10,
    ).stdout
    if dirty:
        raise ValueError(
            "pinned Omarchy bin/ and LICENSE must be clean, including untracked files"
        )
    commands = []
    totals = collections.Counter()
    interpreters = collections.Counter()
    for path in sorted((checkout / "bin").iterdir()):
        if path.is_symlink() or not path.is_file():
            raise ValueError(f"unexpected non-regular command: {path.name}")
        raw = path.read_bytes()
        text = raw.decode("utf-8")
        lines = text.splitlines()
        shebang = lines[0] if lines else ""
        interpreters[shebang] += 1
        # Lexical discovery only. Strings/comments can produce false positives.
        hits = {}
        for name, pattern in SIGNALS.items():
            matched = [
                index
                for index, line in enumerate(lines, 1)
                if not line.lstrip().startswith("#") and re.search(pattern, line)
            ]
            if matched:
                hits[name] = matched
                totals[name] += 1
        summary = next(
            (
                line.partition("omarchy:summary=")[2].strip()
                for line in lines
                if "omarchy:summary=" in line
            ),
            "",
        )
        commands.append(
            {
                "path": "bin/" + path.name,
                "sha256": hashlib.sha256(raw).hexdigest(),
                "bytes": len(raw),
                "lines": len(lines),
                "shebang": shebang,
                "summary": summary,
                "lexical_signals": hits,
                "port_status": "unverified",
            }
        )
    license_path = checkout / "LICENSE"
    return {
        "version": 1,
        "repository": "https://github.com/omacom/omarchy",
        "release_tag": "v4.0.4",
        "commit": commit,
        "license_sha256": hashlib.sha256(license_path.read_bytes()).hexdigest(),
        "scope": "Direct files in bin/ only; sourced shell configuration, installers, migrations, QML and package integration require separate inventories.",
        "method": "Read-only lexical discovery, not Bash parsing, port coverage or behavioral certification. No upstream code was executed.",
        "command_count": len(commands),
        "interpreters": dict(interpreters),
        "commands_with_lexical_signal": dict(sorted(totals.items())),
        "commands": commands,
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--checkout", required=True, type=Path)
    parser.add_argument("--report", required=True, type=Path)
    args = parser.parse_args()
    report = inventory(args.checkout)
    args.report.write_text(json.dumps(report, indent=2, ensure_ascii=False) + "\n")
    print(
        json.dumps(
            {
                "commands": report["command_count"],
                "interpreters": report["interpreters"],
                "lexical_signals": report["commands_with_lexical_signal"],
            },
            indent=2,
        )
    )


if __name__ == "__main__":
    main()
