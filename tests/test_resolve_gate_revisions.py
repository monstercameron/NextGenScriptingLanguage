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


if __name__ == "__main__":
    unittest.main()
