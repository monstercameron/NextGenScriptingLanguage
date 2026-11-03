#!/usr/bin/env python3
"""Regression checks for the tiered CI pipeline (issue #441).

One push used to cost about two runner-hours and the pipeline stayed red, so
the one-off experiment workflows live in workflows-archive/ and the full
platform matrix runs only on merge to main. Pull requests run the fast Linux
lane (feature-matrix, linux-budgets, workflows).
"""

from __future__ import annotations

import pathlib
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[1]
WORKFLOWS = ROOT / ".github/workflows"
ARCHIVE = ROOT / "workflows-archive"

# One-off experiment replications: manual-only, kept as evidence in the
# archive instead of running on any schedule or trigger.
ARCHIVED = (
    "codegen-experiments",
    "emit-perf-hosted-replication",
    "graph-collector-e2e-hosted-replication",
    "graph-recv-any-hosted-paired",
    "literal-json-arguments-hosted-replication",
    "mixed-runtime-probe",
    "profile",
    "real-bash-100-runner-contract",
)

# Manual diagnostics intentionally kept active: each is pinned by an existing
# regression test that reads it from .github/workflows/.
RETAINED_MANUAL = {
    "latency-anchor-diagnostic": "tools/test_paired_latency.py",
    "desktop-live-acceptance": "tools/test_ci_feature_matrix.py",
}

# Ancillary per-push lanes that skip docs-only changes.
PATH_FILTERED = (
    "bash-compare",
    "sandbox-denial",
    "fuzz",
    "security",
)

# Full platform matrix: merge-to-main, manual dispatch, and release calls only.
NON_PR_JOBS = (
    ("unix", "unix-clippy"),
    ("unix-clippy", "windows"),
    ("windows", None),
)

# Fast pull-request lane: Linux build, unit tests, port compare, docs.
PR_LANE_JOBS = (
    ("feature-matrix", "workflows"),
    ("workflows", "linux-budgets"),
    ("linux-budgets", "unix"),
)

GATE = "if: github.event_name != 'pull_request'"


def on_block(text: str) -> str:
    start = text.index("\non:\n") + len("\non:\n")
    lines = []
    for line in text[start:].splitlines():
        if line and not line.startswith((" ", "#")) and line.rstrip().endswith(":"):
            break
        lines.append(line)
    return "\n".join(lines)


def job_block(text: str, name: str, following: str | None) -> str:
    start = text.index(f"\n  {name}:\n") + 1
    rest = text[start:]
    if following is None:
        return rest
    return rest[: rest.index(f"\n  {following}:\n")]


class PruneTests(unittest.TestCase):
    def test_one_off_experiment_workflows_are_archived_not_active(self) -> None:
        for name in ARCHIVED:
            with self.subTest(workflow=name):
                self.assertFalse(
                    (WORKFLOWS / f"{name}.yml").exists(),
                    f"{name}.yml still runs under .github/workflows/",
                )
                archived = ARCHIVE / f"{name}.yml"
                self.assertTrue(archived.exists(), f"{name}.yml missing from the archive")
                body = archived.read_text(encoding="utf-8")
                self.assertTrue(body.startswith("name: "))
                self.assertIn("workflow_dispatch:", body)

    def test_archive_readme_explains_the_split(self) -> None:
        readme = ARCHIVE / "README.md"
        self.assertTrue(readme.exists())
        body = readme.read_text(encoding="utf-8")
        self.assertIn("#441", body)
        self.assertIn(".github/workflows/", body)
        for name in ARCHIVED:
            with self.subTest(workflow=name):
                self.assertIn(name, body)

    def test_every_active_workflow_has_an_automatic_trigger_or_caller(self) -> None:
        callers: dict[str, list[str]] = {}
        for workflow in sorted(WORKFLOWS.glob("*.yml")):
            body = workflow.read_text(encoding="utf-8")
            for line in body.splitlines():
                line = line.strip()
                if line.startswith("uses: ./.github/workflows/"):
                    target = line.split("uses: ./.github/workflows/", 1)[1].strip()
                    callers.setdefault(target, []).append(workflow.name)
        for workflow in sorted(WORKFLOWS.glob("*.yml")):
            with self.subTest(workflow=workflow.name):
                body = workflow.read_text(encoding="utf-8")
                self.assertIn("\non:\n", body)
                triggers = on_block(body)
                if "push:" in triggers or "pull_request:" in triggers or "schedule:" in triggers:
                    continue
                if "workflow_call:" in triggers:
                    self.assertTrue(
                        callers.get(workflow.name),
                        f"{workflow.name} is workflow_call-only but nothing calls it",
                    )
                    continue
                pinning = RETAINED_MANUAL.get(workflow.stem)
                self.assertIsNotNone(
                    pinning, f"{workflow.name} is manual-only and not a retained diagnostic"
                )
                assert pinning is not None
                self.assertIn(
                    workflow.stem,
                    (ROOT / pinning).read_text(encoding="utf-8"),
                    f"{workflow.name} retention is no longer pinned by {pinning}",
                )


class TieringTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.workflow = (WORKFLOWS / "ci.yml").read_text(encoding="utf-8")

    def test_pull_requests_still_trigger_the_fast_lane(self) -> None:
        triggers = on_block(self.workflow)
        self.assertIn("pull_request:", triggers)
        self.assertIn("workflow_call:", triggers)

    def test_full_platform_matrix_is_gated_off_pull_requests(self) -> None:
        for name, following in NON_PR_JOBS:
            with self.subTest(job=name):
                self.assertIn(GATE, job_block(self.workflow, name, following))

    def test_fast_pr_lane_jobs_stay_on_every_event(self) -> None:
        for name, following in PR_LANE_JOBS:
            with self.subTest(job=name):
                self.assertNotIn(GATE, job_block(self.workflow, name, following))

    def test_tiering_does_not_shrink_the_main_matrix(self) -> None:
        self.assertIn("os: [ubuntu-24.04, windows-2022]", self.workflow)
        self.assertIn("os: [ubuntu-24.04, macos-15]", self.workflow)

    def test_docs_only_changes_skip_native_builds(self) -> None:
        for name in ("ci", *PATH_FILTERED):
            with self.subTest(workflow=name):
                body = (WORKFLOWS / f"{name}.yml").read_text(encoding="utf-8")
                triggers = on_block(body)
                self.assertIn("push:", triggers)
                self.assertIn("pull_request:", triggers)
                self.assertIn("paths-ignore:", triggers)
                self.assertIn("docs/**", triggers)


if __name__ == "__main__":
    unittest.main()
