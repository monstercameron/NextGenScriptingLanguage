#!/usr/bin/env python3
"""Measure the Bash baseline of an AI gauntlet workflow on its held-out set.

Runs examples/ai-gauntlet/<workflow>/baseline.sh once per held-out input (the
input is piped on stdin) and scores the labels it prints. No LLM and no Execute
binary are involved. Skips cleanly when bash cannot be run.

Run directly to print the results table:
    python tools/test_ai_gauntlet_baseline.py
"""
from __future__ import annotations

import json
import shutil
import subprocess
import sys
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
PROJECT = HERE.parent
WORKFLOW = "001-log-errors"
WORKFLOW_DIR = PROJECT / "examples" / "ai-gauntlet" / WORKFLOW
ABSTAIN = "abstain"
# Measured with the baseline at this commit. Update only when baseline.sh or
# heldout.json changes on purpose.
EXPECTED_CORRECT = 6
EXPECTED_ABSTAIN = 4


def find_bash() -> str | None:
    """Return the bash that shutil.which resolves, or None when it does not run.

    A bare "bash" can resolve to the WSL launcher in System32 on Windows, so the
    resolved path is used for every call.
    """
    path = shutil.which("bash")
    if path is None:
        return None
    try:
        probe = subprocess.run([path, "-c", "exit 0"], capture_output=True, timeout=30, check=False)
    except (OSError, subprocess.SubprocessError):
        return None
    return path if probe.returncode == 0 else None


BASH = find_bash()


def run_baseline(script_text: str, line: str) -> str:
    """Feed one line to the baseline and return the single label it prints."""
    result = subprocess.run(
        [BASH, "-c", script_text, "baseline.sh"],
        input=line.encode("utf-8"),
        capture_output=True,
        timeout=30,
        check=False,
    )
    if result.returncode != 0:
        raise RuntimeError(f"baseline failed ({result.returncode}): {result.stderr.decode('utf-8', 'replace')}")
    return result.stdout.decode("utf-8").strip()


def score(script_text: str, items: list[dict]) -> dict:
    counts = {"items": len(items), "correct": 0, "abstain": 0, "wrong": 0}
    rows = []
    for item in items:
        observed = run_baseline(script_text, item["input"])
        if observed == ABSTAIN:
            counts["abstain"] += 1
        elif observed == item["expected"]:
            counts["correct"] += 1
        else:
            counts["wrong"] += 1
        rows.append((item["id"], item["expected"], observed))
    counts["rows"] = rows
    return counts


def results_table(counts: dict) -> str:
    total = counts["items"]
    lines = [
        f"| Workflow | Arm | Items | Accuracy | Abstention | Wrong action |",
        f"| --- | --- | --- | --- | --- | --- |",
        f"| {WORKFLOW} | Bash baseline | {total} | "
        f"{counts['correct'] / total:.0%} | {counts['abstain'] / total:.0%} | {counts['wrong'] / total:.0%} |",
    ]
    return "\n".join(lines)


@unittest.skipUnless(BASH is not None, "bash is not available on this machine")
class GauntletBaselineTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.script_text = (WORKFLOW_DIR / "baseline.sh").read_text(encoding="utf-8")
        cls.heldout = json.loads((WORKFLOW_DIR / "heldout.json").read_text(encoding="utf-8"))
        cls.items = cls.heldout["items"]
        cls.counts = score(cls.script_text, cls.items)

    def test_every_item_gets_a_known_label_or_abstains(self):
        allowed = set(self.heldout["labels"]) | {ABSTAIN}
        for item_id, _expected, observed in self.counts["rows"]:
            self.assertIn(observed, allowed, item_id)

    def test_held_out_set_has_ten_items_with_expected_labels(self):
        self.assertEqual(len(self.items), 10)
        for item in self.items:
            self.assertIn(item["expected"], self.heldout["labels"], item["id"])

    def test_held_out_inputs_do_not_appear_in_fixtures(self):
        fixture_text = "\n".join(
            path.read_text(encoding="utf-8", errors="replace")
            for path in (WORKFLOW_DIR.parent / "fixtures" / "001").rglob("*")
            if path.is_file()
        )
        for item in self.items:
            self.assertNotIn(item["input"], fixture_text, item["id"])

    def test_baseline_never_answers_wrongly(self):
        # Abstaining is allowed; a confident wrong label is the failure this measures.
        self.assertEqual(self.counts["wrong"], 0, self.counts["rows"])

    def test_recorded_accuracy_and_abstention_match_measurement(self):
        self.assertEqual(self.counts["correct"], EXPECTED_CORRECT, self.counts["rows"])
        self.assertEqual(self.counts["abstain"], EXPECTED_ABSTAIN, self.counts["rows"])


if __name__ == "__main__":
    if BASH is None:
        print("bash is not available; nothing measured")
        sys.exit(0)
    script = (WORKFLOW_DIR / "baseline.sh").read_text(encoding="utf-8")
    heldout = json.loads((WORKFLOW_DIR / "heldout.json").read_text(encoding="utf-8"))
    measured = score(script, heldout["items"])
    for item_id, expected, observed in measured["rows"]:
        print(f"{item_id}: expected={expected} observed={observed}")
    print(results_table(measured))
