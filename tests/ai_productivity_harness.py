#!/usr/bin/env python3
"""Local, synthetic checks for the offline AI-evaluation importer."""

import importlib.util
import json
import tempfile
import unittest
import uuid
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
MODULE_PATH = ROOT / "benchmarks" / "ai_productivity" / "harness.py"
SPEC = importlib.util.spec_from_file_location("ai_productivity_harness", MODULE_PATH)
HARNESS = importlib.util.module_from_spec(SPEC)
assert SPEC.loader
SPEC.loader.exec_module(HARNESS)

ZERO = "0" * 64
ONE = "1" * 64
TWO = "2" * 64
THREE = "3" * 64
RUBRIC_SHA = HARNESS.sha256_file(HARNESS.RUBRIC)


def roster():
    return {
        "schema": "execute.ai-productivity.roster/1",
        "fixture_only": True,
        "frozen_utc": "2026-10-14T12:00:00Z",
        "task_package_sha256": ZERO,
        "tasks": [
            {
                "task_id": "heldout_01",
                "family_id": "family_01",
                "split": "held_out",
                "stratum": "workflow_cli",
                "prompt_sha256": ONE,
                "oracle_sha256": TWO,
                "criteria": ["builds", "state_exact"],
                "optional_criteria": ["readability"],
            }
        ],
    }


def attempt(stage, passed, feedback=None, token_in=100, token_out=25, cost="0.001"):
    criteria = {"builds": passed, "state_exact": passed, "readability": True}
    return {
        "stage": stage,
        "artifact_sha256": THREE if passed else TWO,
        "started_utc": "2026-10-14T12:01:00Z",
        "generation_ms": 200,
        "execution_ms": 15,
        "usage": {
            "basis": "provider_reported",
            "input_tokens": token_in,
            "output_tokens": token_out,
            "reasoning_tokens": None,
            "cached_input_tokens": 0,
        },
        "cost": {"basis": "provider_reported", "amount": cost, "currency": "USD"},
        "oracle_status": "pass" if passed else "fail",
        "criteria_results": criteria,
        "diagnostic_feedback_sent": feedback or [],
    }


def record(arm, passed, repaired=False, task_id="heldout_01"):
    attempts = [attempt("generate", passed and not repaired)]
    if repaired:
        attempts.append(attempt("repair", passed, ["state_exact"]))
    return {
        "schema": "execute.ai-productivity.run/1",
        "trial_type": "evaluation",
        "run_id": str(uuid.uuid5(uuid.NAMESPACE_URL, arm + task_id)),
        "blind_arm_id": arm,
        "task_id": task_id,
        "replicate": 1,
        "runner_image_sha256": ZERO,
        "attempts": attempts,
        "review": {
            "scoring_locked": True,
            "ratings": [
                {"rater_id": "rater_a", "rubric_sha256": RUBRIC_SHA, "requirement_coverage": 4, "clarity": 3, "changeability": 4},
                {"rater_id": "rater_b", "rubric_sha256": RUBRIC_SHA, "requirement_coverage": 4, "clarity": 4, "changeability": 3},
            ],
            "adjudication": {"rater_id": "rater_c", "rubric_sha256": RUBRIC_SHA, "requirement_coverage": 4, "clarity": 4, "changeability": 4},
        },
    }


def arm(variant):
    return {
        "variant": variant,
        "provider": "provider-x",
        "model_snapshot": "snapshot-1",
        "comparison_block": "model-1",
        "generation_parameters": {"temperature": 0, "top_p": None, "seed": None, "max_output_tokens": 4096},
        "api_version": "v1",
        "language_version": "current",
        "system_prompt_sha256": ZERO,
        "tool_config_sha256": ONE,
    }


class HarnessTests(unittest.TestCase):
    def setUp(self):
        self.roster = roster()
        self.tasks = HARNESS.validate_roster(self.roster, production=False)

    def test_success_metrics_include_repair_cost_and_remain_descriptive(self):
        records = [
            record("arm_abcdefgh", True),
            record("arm_ijklmnop", True),
            record("arm_qrstuvwx", True, repaired=True),
        ]
        for item in records:
            HARNESS.validate_result(item, self.tasks)
        arms = {
            "arm_abcdefgh": arm("bash"),
            "arm_ijklmnop": arm("nushell"),
            "arm_qrstuvwx": arm("execute_program"),
        }
        report = HARNESS.summarize(self.roster, self.tasks, records, arms)
        self.assertEqual(report["status"], "descriptive_import_no_claim")
        execute_row = next(row for row in report["arms"] if row["blind_arm_id"] == "arm_qrstuvwx")
        self.assertEqual(execute_row["initial_success_rate"], 0)
        self.assertEqual(execute_row["final_success_rate"], 1)
        self.assertEqual(execute_row["median_repairs"], 1)
        self.assertEqual(execute_row["maintainability_review"]["adjudicated_trials"], 1)
        self.assertEqual(
            execute_row["maintainability_review"]["resolved_mean_by_dimension"]["clarity"],
            4,
        )
        self.assertEqual(
            execute_row["token_usage_by_basis"]["provider_reported"]["median_by_metric"]["total_input_output_tokens"],
            250,
        )
        self.assertEqual(report["claim"], "none")
        comparison = report["paired_analysis"]["contrasts"]["workflow_cli:execute_minus_bash"]
        self.assertEqual(comparison["paired_task_family_blocks"], 1)
        self.assertEqual(comparison["mean_final_success_delta"], 0)

    def test_empty_result_set_is_pending_and_not_a_zero_score(self):
        report = HARNESS.summarize(self.roster, self.tasks, [])
        self.assertEqual(report["status"], "pending_no_held_out_trials")
        self.assertEqual(report["arms"], [])
        self.assertIsNone(report["paired_analysis"])

    def test_cli_fixtures_cannot_be_imported_as_evaluation(self):
        fixture = record("arm_abcdefgh", True)
        fixture["trial_type"] = "fixture"
        with self.assertRaisesRegex(HARNESS.EvalError, "fixture or non-evaluation"):
            HARNESS.validate_result(fixture, self.tasks, production=True)

    def test_task_family_cannot_cross_splits(self):
        changed = roster()
        second = dict(changed["tasks"][0])
        second.update(task_id="dev_01", split="development")
        changed["tasks"].append(second)
        with self.assertRaisesRegex(HARNESS.EvalError, "crosses split"):
            HARNESS.validate_roster(changed, production=False)

    def test_rater_disagreement_requires_a_distinct_third_rater(self):
        invalid = record("arm_abcdefgh", True)
        invalid["review"].pop("adjudication")
        with self.assertRaisesRegex(HARNESS.EvalError, "third-rater adjudication"):
            HARNESS.validate_result(invalid, self.tasks, production=True)

    def test_missing_criterion_is_rejected(self):
        invalid = record("arm_abcdefgh", True)
        invalid["attempts"][0]["criteria_results"].pop("state_exact")
        with self.assertRaisesRegex(HARNESS.EvalError, "exactly the roster criterion IDs"):
            HARNESS.validate_result(invalid, self.tasks)

    def test_unavailable_usage_must_stay_missing(self):
        invalid = record("arm_abcdefgh", True)
        invalid["attempts"][0]["usage"] = {
            "basis": "unavailable",
            "input_tokens": 0,
            "output_tokens": 0,
            "reasoning_tokens": None,
            "cached_input_tokens": None,
        }
        with self.assertRaisesRegex(HARNESS.EvalError, "null counters"):
            HARNESS.validate_result(invalid, self.tasks)

    def test_jsonl_duplicate_cell_is_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "results.jsonl"
            item = record("arm_abcdefgh", True)
            path.write_text(json.dumps(item) + "\n" + json.dumps(item) + "\n")
            with self.assertRaisesRegex(HARNESS.EvalError, "duplicate task/arm/replicate"):
                HARNESS.read_results(path, self.tasks)


if __name__ == "__main__":
    unittest.main()
