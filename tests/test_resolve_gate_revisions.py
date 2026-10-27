#!/usr/bin/env python3
"""Contract tests for the instruction-gate revision policy (issue #571)."""
import importlib.util
import json
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location(
    "resolve_gate_revisions", ROOT / "tools" / "resolve_gate_revisions.py"
)
resolver = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(resolver)

BASE = "a" * 40
HEAD = "b" * 40
PARENT = "c" * 40


def pr_payload(base=BASE, head=HEAD):
    return {"pull_request": {"base": {"sha": base}, "head": {"sha": head}}}


class ResolveTests(unittest.TestCase):
    def test_pull_request_uses_base_and_head(self):
        self.assertEqual(
            resolver.resolve("pull_request", pr_payload(), "0" * 40), (BASE, HEAD)
        )

    def test_pull_request_rejects_missing_or_equal_shas(self):
        with self.assertRaisesRegex(ValueError, "incomplete"):
            resolver.resolve("pull_request", {}, HEAD)
        with self.assertRaisesRegex(ValueError, "must differ"):
            resolver.resolve(
                "pull_request", pr_payload(base=HEAD, head=HEAD), HEAD
            )

    def test_push_uses_before_and_sha(self):
        self.assertEqual(
            resolver.resolve("push", {"before": BASE}, HEAD), (BASE, HEAD)
        )

    def test_push_zero_before_falls_back_to_parent(self):
        calls = []
        base, head = resolver.resolve(
            "push",
            {"before": "0" * 40},
            HEAD,
            parent_of=lambda sha: (calls.append(sha), PARENT)[1],
        )
        self.assertEqual((base, head), (PARENT, HEAD))
        self.assertEqual(calls, [HEAD])

    def test_push_rejects_missing_head(self):
        with self.assertRaisesRegex(ValueError, "incomplete"):
            resolver.resolve("push", {"before": BASE}, "")

    def test_dispatch_inputs_win(self):
        payload = {"inputs": {"base_sha": BASE, "head_sha": HEAD}}
        self.assertEqual(
            resolver.resolve("workflow_dispatch", payload, PARENT), (BASE, HEAD)
        )

    def test_dispatch_without_inputs_uses_parent(self):
        base, head = resolver.resolve(
            "workflow_dispatch", {}, HEAD,
            parent_of=lambda sha: PARENT,
        )
        self.assertEqual((base, head), (PARENT, HEAD))

    def test_dispatch_partial_inputs_are_incomplete(self):
        payload = {"inputs": {"base_sha": BASE}}
        with self.assertRaisesRegex(ValueError, "incomplete"):
            resolver.resolve("workflow_dispatch", payload, HEAD)

    def test_explicit_inputs_override_any_event(self):
        base, head = resolver.resolve(
            "push", {"before": PARENT}, HEAD,
            base_input=BASE, head_input=HEAD,
        )
        self.assertEqual((base, head), (BASE, HEAD))
        with self.assertRaisesRegex(ValueError, "incomplete"):
            resolver.resolve(
                "push", {"before": BASE}, HEAD,
                base_input=BASE, head_input="short",
            )

    def test_unknown_event_needs_explicit_inputs(self):
        with self.assertRaisesRegex(ValueError, "explicit base/head"):
            resolver.resolve("schedule", {}, HEAD)
        self.assertEqual(
            resolver.resolve(
                "schedule", {}, HEAD, base_input=BASE, head_input=HEAD
            ),
            (BASE, HEAD),
        )

    def test_cli_prints_both_shas(self):
        with tempfile.TemporaryDirectory(prefix="gate-revs-") as tmp:
            event = Path(tmp) / "event.json"
            event.write_text(json.dumps(pr_payload()), encoding="utf-8")
            with tempfile.TemporaryDirectory(prefix="gate-out-") as out:
                output = Path(out) / "github_output"
                import os

                env = dict(os.environ)
                env["GITHUB_OUTPUT"] = str(output)
                code = None
                old = dict(os.environ)
                try:
                    os.environ["GITHUB_OUTPUT"] = str(output)
                    code = resolver.main([
                        "--event-name", "pull_request",
                        "--event-path", str(event),
                        "--sha", HEAD,
                    ])
                finally:
                    os.environ.clear()
                    os.environ.update(old)
                _ = env
                self.assertEqual(code, 0)
                written = output.read_text(encoding="utf-8")
            self.assertIn(f"base_sha={BASE}", written)
            self.assertIn(f"head_sha={HEAD}", written)

    def test_cli_reports_incomplete_without_payload(self):
        self.assertEqual(
            resolver.main(["--event-name", "pull_request", "--sha", HEAD]), 2
        )


class FetchOrderingTests(unittest.TestCase):
    """Fork-PR heads live on pull/N/head, not origin: refetch first (issue #571).

    The "Fetch gate revisions" step must fetch the pull-request head ref
    before the required BASE_SHA/HEAD_SHA fetch, or a fork-PR head SHA
    fails the job before the ref that provides it runs. Static checks only:
    no network, no checkout, no untrusted code execution.
    """

    WORKFLOW = ROOT / ".github" / "workflows" / "bash-compare.yml"

    def fetch_step(self):
        text = self.WORKFLOW.read_text(encoding="utf-8")
        marker = "Fetch gate revisions"
        start = text.index(marker)
        # Bound the step at the next "- name:" so later build steps cannot
        # satisfy the ordering assertions by accident.
        following = text.find("\n      - name:", start + len(marker))
        return text[start:following if following != -1 else len(text)]

    def test_fork_head_refetch_precedes_required_sha_fetch(self):
        step = self.fetch_step()
        pr_ref = step.index("pull/${{ github.event.pull_request.number }}/head")
        sha_fetch = step.index('origin "$BASE_SHA" "$HEAD_SHA"')
        self.assertLess(
            pr_ref, sha_fetch,
            "pull/N/head refetch must run before the required SHA fetch "
            "so a fork-PR head SHA is present when it is fetched",
        )

    def test_required_sha_fetch_stays_fail_closed(self):
        step = self.fetch_step()
        sha_line = next(
            line for line in step.splitlines() if '"$BASE_SHA" "$HEAD_SHA"' in line
        )
        self.assertIn("exit 2", step[step.index(sha_line):step.index(sha_line) + 200])
        self.assertNotIn("|| true", sha_line)

    def test_fork_refetch_is_guarded_and_best_effort(self):
        step = self.fetch_step()
        refetch_line = next(
            line for line in step.splitlines()
            if "git fetch" in line and "pull/" in line
        )
        self.assertIn("|| true", refetch_line)
        self.assertIn('GITHUB_EVENT_NAME" = pull_request', step)
        self.assertIn("github.event.pull_request.number", step)

    def test_fetched_revisions_are_verified_before_use(self):
        step = self.fetch_step()
        sha_fetch = step.index('origin "$BASE_SHA" "$HEAD_SHA"')
        base_check = step.index('cat-file -e "$BASE_SHA^{commit}"')
        head_check = step.index('cat-file -e "$HEAD_SHA^{commit}"')
        self.assertLess(sha_fetch, base_check)
        self.assertLess(sha_fetch, head_check)


if __name__ == "__main__":
    unittest.main()
