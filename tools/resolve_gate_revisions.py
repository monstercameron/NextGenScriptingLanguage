#!/usr/bin/env python3
"""Resolve the baseline and candidate revisions for the instruction gate.

Same-runner policy (issue #571): the +2% gate compares two focused
cachegrind reports measured back-to-back on one runner. The baseline is the
merge base of the change and the candidate is the tip of the change:

- pull_request: base.sha versus head.sha from the event payload.
- push to main: event.before versus GITHUB_SHA; a zero/empty before (new
  branch) falls back to the parent of the head commit.
- workflow_dispatch: explicit base_sha/head_sha inputs when both are given,
  otherwise the parent of HEAD versus HEAD.
- explicit --base-input/--head-input overrides win on any event when both
  are valid and distinct (manual reruns).

Every failure is incomplete (exit 2), never acceptance: the workflow must
fail the job rather than gate on a single report or a synthetic canary.
"""

import argparse
import json
import re
import subprocess
import sys
from pathlib import Path

SHA_RE = re.compile(r"^[0-9a-f]{40}$")
ZERO_SHA = "0" * 40


def valid_sha(value):
    return isinstance(value, str) and bool(SHA_RE.match(value))


def git_parent(sha):
    """Return the parent commit of sha, or raise ValueError when unknown."""
    try:
        result = subprocess.run(
            ["git", "rev-parse", f"{sha}~1"],
            capture_output=True,
            text=True,
            check=False,
        )
    except OSError as error:
        raise ValueError(f"incomplete: cannot resolve the parent of {sha}: {error}")
    parent = result.stdout.strip()
    if result.returncode != 0 or not valid_sha(parent):
        raise ValueError(f"incomplete: cannot resolve the parent of {sha}")
    return parent


def resolve(event_name, payload, github_sha, *, base_input=None,
            head_input=None, parent_of=git_parent):
    """Return a (base_sha, head_sha) pair or raise ValueError (incomplete)."""
    if base_input or head_input:
        if not (valid_sha(base_input) and valid_sha(head_input)):
            raise ValueError(
                "incomplete: explicit base/head inputs must both be full 40-hex SHAs"
            )
        if base_input == head_input:
            raise ValueError("incomplete: explicit base and head must differ")
        return base_input, head_input
    if not isinstance(payload, dict):
        raise ValueError("incomplete: the GitHub event payload is unreadable")
    if event_name == "pull_request":
        pull = payload.get("pull_request") or {}
        base = (pull.get("base") or {}).get("sha")
        head = (pull.get("head") or {}).get("sha")
        if not (valid_sha(base) and valid_sha(head)):
            raise ValueError(
                "incomplete: the pull_request event lacks base/head SHAs"
            )
        if base == head:
            raise ValueError("incomplete: pull_request base and head must differ")
        return base, head
    if event_name == "push":
        before = payload.get("before")
        head = github_sha
        if not valid_sha(head):
            raise ValueError("incomplete: the push head SHA is missing")
        if valid_sha(before) and before != ZERO_SHA and before != head:
            return before, head
        # New branch (all-zero before) or a rewritten head: compare against
        # the parent commit so the gate still sees a cross-commit pair.
        return parent_of(head), head
    if event_name == "workflow_dispatch":
        inputs = payload.get("inputs") or {}
        base = inputs.get("base_sha") or None
        head = inputs.get("head_sha") or None
        if base or head:
            if not (valid_sha(base) and valid_sha(head)):
                raise ValueError(
                    "incomplete: dispatch inputs base_sha/head_sha must both "
                    "be full 40-hex SHAs"
                )
            if base == head:
                raise ValueError("incomplete: dispatch base and head must differ")
            return base, head
        if not valid_sha(github_sha):
            raise ValueError("incomplete: the dispatch head SHA is missing")
        return parent_of(github_sha), github_sha
    raise ValueError(
        f"incomplete: event {event_name!r} needs explicit base/head inputs"
    )


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--event-name", default=None)
    parser.add_argument("--event-path", type=Path, default=None)
    parser.add_argument("--sha", default=None)
    parser.add_argument("--base-input", default=None)
    parser.add_argument("--head-input", default=None)
    args = parser.parse_args(argv)

    import os

    event_name = args.event_name or os.environ.get("GITHUB_EVENT_NAME", "")
    event_path = args.event_path or (
        Path(os.environ["GITHUB_EVENT_PATH"])
        if os.environ.get("GITHUB_EVENT_PATH")
        else None
    )
    github_sha = args.sha or os.environ.get("GITHUB_SHA", "")
    payload = {}
    if event_path is not None:
        try:
            payload = json.loads(Path(event_path).read_text(encoding="utf-8"))
        except (OSError, ValueError) as error:
            print(f"error: incomplete: cannot read the event payload: {error}",
                  file=sys.stderr)
            return 2
    try:
        base, head = resolve(
            event_name, payload, github_sha,
            base_input=args.base_input or None,
            head_input=args.head_input or None,
        )
    except ValueError as error:
        print(f"error: {error}", file=sys.stderr)
        return 2
    print(f"BASE_SHA={base}")
    print(f"HEAD_SHA={head}")
    output = os.environ.get("GITHUB_OUTPUT")
    if output:
        with open(output, "a", encoding="utf-8") as handle:
            handle.write(f"base_sha={base}\nhead_sha={head}\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
