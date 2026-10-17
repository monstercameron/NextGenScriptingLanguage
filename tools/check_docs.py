#!/usr/bin/env python3
"""Check local file/fragment links in maintained Markdown, without network access.

Historical reports and user-provided source documents are intentionally retained
verbatim. This checks the maintained entry points, not their historical contents.
It recognizes inline Markdown links and GitHub-style Markdown heading anchors,
plus explicit HTML ids. It is not a complete Markdown renderer or external URL checker.
"""
import argparse
from html.parser import HTMLParser
from pathlib import Path
import re
import sys
from urllib.parse import unquote, urlsplit

ROOT = Path(__file__).resolve().parents[1]
MAINTAINED = [
    "README.md",
    "CONTRIBUTING.md",
    "CHANGELOG.md",
    "SECURITY.md",
    "DESIGN.md",
    "docs/README.md",
    "docs/guide.md",
    "docs/language.md",
    "docs/language-server.md",
    "docs/applet-contracts.md",
    "docs/csv.md",
    "docs/graph.md",
    "docs/security-dataflow.md",
    "docs/program-modules.md",
    "docs/streaming.md",
    "docs/live-control.md",
    "docs/models.md",
    "docs/playground.md",
    "docs/releases.md",
    "docs/windows.md",
    "docs/site.md",
    "docs/top-cpu.md",
    "docs/github-stars.md",
    "docs/tui-catalog.md",
    "docs/production-readiness.md",
    "docs/replacement-challenge.md",
    "docs/unix-compatibility-review.md",
    "docs/thesis-progress.md",
    "browser/README.md",
    "web/README.md",
    "tools/README.md",
    "work/README.md",
    "bin/README.md",
    "vendor/README.md",
    "editors/vscode/README.md",
    "editors/vscode/CHANGELOG.md",
    "examples/ladder/README.md",
    "examples/models/README.md",
    "examples/live/README.md",
    "examples/remote/README.md",
    "examples/tasks/README.md",
    "examples/omarchy-ports/README.md",
]
LINK = re.compile(r'!?\[[^\]\n]*\]\((<[^>]+>|[^)\s]+)(?:\s+"[^"]*")?\)')


def prose(source):
    """Remove fenced code, preserving line numbers for diagnostics."""
    fence = None
    for line in source.splitlines():
        marker = re.match(r"^\s{0,3}(`{3,}|~{3,})", line)
        if marker:
            token = marker[1]
            if fence is None:
                fence = token
            elif token[0] == fence[0] and len(token) >= len(fence):
                fence = None
            yield ""
        else:
            yield line if fence is None else ""


class Ids(HTMLParser):
    def __init__(self):
        super().__init__()
        self.values = set()

    def handle_starttag(self, tag, attrs):
        for name, value in attrs:
            if name == "id" and value:
                self.values.add(value)


def anchors(source, markdown=True):
    lines = list(prose(source)) if markdown else source.splitlines()
    parser = Ids()
    parser.feed("\n".join(lines))
    result, counts = parser.values, {}
    if markdown:
        for line in lines:
            heading = re.match(r"^ {0,3}#{1,6}\s+(.+?)(?:\s+#+)?$", line)
            if not heading:
                continue
            slug = re.sub(r"[^\w\- ]", "", heading[1].lower()).replace(" ", "-")
            duplicate = counts.get(slug, 0)
            counts[slug] = duplicate + 1
            result.add(slug + ("-" + str(duplicate) if duplicate else ""))
    return result


def check_file(path, root=ROOT):
    errors = []
    if not path.is_file():
        return [f"{path}: missing maintained document"]
    for number, line in enumerate(prose(path.read_text(encoding="utf-8")), 1):
        for link in LINK.finditer(line):
            target = link[1].strip("<>")
            url = urlsplit(target)
            if url.scheme or url.netloc:
                continue
            destination = (
                (path.parent / unquote(url.path)).resolve()
                if url.path
                else path.resolve()
            )
            try:
                destination.relative_to(root.resolve())
            except ValueError:
                reason = "local target escapes repository"
            else:
                if not destination.exists():
                    reason = "target does not exist"
                elif url.fragment and destination.suffix in {".md", ".html"}:
                    ids = anchors(destination.read_text(encoding="utf-8"), destination.suffix == ".md")
                    reason = (
                        None
                        if unquote(url.fragment) in ids
                        else "fragment does not exist"
                    )
                else:
                    reason = None
            if reason:
                errors.append(f"{path.relative_to(root)}:{number}: {target}: {reason}")
    return errors


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "paths",
        nargs="*",
        help="Repository-relative Markdown files; defaults to maintained docs",
    )
    args = parser.parse_args()
    files = [ROOT / path for path in (args.paths or MAINTAINED)]
    errors = [error for path in files for error in check_file(path)]
    if errors:
        print("\n".join(errors), file=sys.stderr)
        return 1
    print(
        f"Documentation links passed ({len(files)} maintained files; local files and Markdown/HTML fragments)."
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
