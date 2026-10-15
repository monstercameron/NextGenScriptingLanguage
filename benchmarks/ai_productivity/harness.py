#!/usr/bin/env python3
"""Offline validation and blinded-result summaries for the AI eval protocol.

This module never calls a model, runs a submitted artifact, or turns fixture
records into an evaluation. The evaluator's sealed runner supplies JSONL rows.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import math
import random
import re
import statistics
import sys
import uuid
from collections import defaultdict
from datetime import datetime
from decimal import Decimal, InvalidOperation
from pathlib import Path
from typing import Any


ROOT = Path(__file__).resolve().parent
PROTOCOL = ROOT / "preregistration.json"
RUBRIC = ROOT / "rubric.json"
HASH = re.compile(r"^[a-f0-9]{64}$")
ARM = re.compile(r"^arm_[a-z0-9]{8,32}$")
TASK_ID = re.compile(r"^[A-Za-z0-9_-]{1,80}$")
CRITERION = re.compile(r"^[a-z][a-z0-9_]{1,63}$")
VARIANTS_BY_STRATUM = {
    "workflow_cli": {"bash", "nushell", "execute_program"},
    "maintenance_change": {"bash", "nushell", "execute_program"},
    "desktop_composition": {"bash+qml", "nushell+qml", "execute_program+qml"},
}
REQUIRED_ROSTER_KEYS = {"schema", "fixture_only", "frozen_utc", "task_package_sha256", "tasks"}
REQUIRED_RESULT_KEYS = {
    "schema", "trial_type", "run_id", "blind_arm_id", "task_id", "replicate",
    "runner_image_sha256", "attempts", "review"
}
REQUIRED_ATTEMPT_KEYS = {
    "stage", "artifact_sha256", "started_utc", "generation_ms", "execution_ms",
    "usage", "cost", "oracle_status", "criteria_results", "diagnostic_feedback_sent"
}


class EvalError(ValueError):
    """Input does not match the frozen protocol."""


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for block in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def _keys(value: dict[str, Any], expected: set[str], label: str) -> None:
    missing = expected - value.keys()
    extra = value.keys() - expected
    if missing or extra:
        details = []
        if missing:
            details.append("missing " + ", ".join(sorted(missing)))
        if extra:
            details.append("unexpected " + ", ".join(sorted(extra)))
        raise EvalError(f"{label}: " + "; ".join(details))


def _utc(value: Any, label: str) -> None:
    if not isinstance(value, str):
        raise EvalError(f"{label} must be an ISO-8601 UTC timestamp")
    try:
        parsed = datetime.fromisoformat(value.replace("Z", "+00:00"))
    except ValueError as error:
        raise EvalError(f"{label} must be an ISO-8601 timestamp: {error}") from error
    if parsed.tzinfo is None or parsed.utcoffset().total_seconds() != 0:
        raise EvalError(f"{label} must include a UTC offset")


def _hash(value: Any, label: str) -> None:
    if not isinstance(value, str) or not HASH.fullmatch(value):
        raise EvalError(f"{label} must be a lowercase SHA-256 hex digest")


def validate_roster(roster: Any, *, production: bool = True) -> dict[str, dict[str, Any]]:
    if not isinstance(roster, dict):
        raise EvalError("roster must be a JSON object")
    _keys(roster, REQUIRED_ROSTER_KEYS, "roster")
    if roster["schema"] != "execute.ai-productivity.roster/1":
        raise EvalError("unsupported task roster schema")
    if type(roster["fixture_only"]) is not bool:
        raise EvalError("roster.fixture_only must be a boolean")
    if production and roster["fixture_only"]:
        raise EvalError("fixture-only task rosters cannot support evaluation imports")
    _utc(roster["frozen_utc"], "roster.frozen_utc")
    _hash(roster["task_package_sha256"], "roster.task_package_sha256")
    if not isinstance(roster["tasks"], list):
        raise EvalError("roster.tasks must be an array")

    tasks: dict[str, dict[str, Any]] = {}
    family_split: dict[str, str] = {}
    family_stratum: dict[str, str] = {}
    family_counts: dict[tuple[str, str], set[str]] = defaultdict(set)
    for index, task in enumerate(roster["tasks"]):
        label = f"roster.tasks[{index}]"
        if not isinstance(task, dict):
            raise EvalError(f"{label} must be an object")
        allowed = {"task_id", "family_id", "split", "stratum", "prompt_sha256", "oracle_sha256", "criteria", "optional_criteria"}
        required = allowed - {"optional_criteria"}
        if not required <= task.keys() or task.keys() - allowed:
            raise EvalError(f"{label} has missing or unexpected fields")
        task_id, family_id = task["task_id"], task["family_id"]
        if not isinstance(task_id, str) or not TASK_ID.fullmatch(task_id):
            raise EvalError(f"{label}.task_id is malformed")
        if not isinstance(family_id, str) or not TASK_ID.fullmatch(family_id):
            raise EvalError(f"{label}.family_id is malformed")
        if task_id in tasks:
            raise EvalError(f"duplicate task_id {task_id!r}")
        if task["split"] not in {"development", "calibration", "held_out"}:
            raise EvalError(f"{label}.split is unsupported")
        if task["stratum"] not in VARIANTS_BY_STRATUM:
            raise EvalError(f"{label}.stratum is unsupported")
        _hash(task["prompt_sha256"], f"{label}.prompt_sha256")
        _hash(task["oracle_sha256"], f"{label}.oracle_sha256")
        for field in ("criteria", "optional_criteria"):
            values = task.get(field, [])
            if not isinstance(values, list) or not all(isinstance(item, str) and CRITERION.fullmatch(item) for item in values):
                raise EvalError(f"{label}.{field} contains a malformed criterion ID")
            if len(set(values)) != len(values):
                raise EvalError(f"{label}.{field} contains duplicate criterion IDs")
        if not task["criteria"]:
            raise EvalError(f"{label} needs at least one mandatory criterion")
        overlap = set(task["criteria"]) & set(task.get("optional_criteria", []))
        if overlap:
            raise EvalError(f"{label} has criteria both mandatory and optional: {sorted(overlap)}")
        old_split = family_split.setdefault(family_id, task["split"])
        old_stratum = family_stratum.setdefault(family_id, task["stratum"])
        if old_split != task["split"]:
            raise EvalError(f"task family {family_id!r} crosses split boundaries")
        if old_stratum != task["stratum"]:
            raise EvalError(f"task family {family_id!r} crosses task strata")
        family_counts[(task["split"], task["stratum"])].add(family_id)
        tasks[task_id] = task

    if production:
        protocol = json.loads(PROTOCOL.read_text())
        for split, rule in protocol["splits"].items():
            minimum = rule["minimum_independent_families_per_stratum"]
            for stratum in VARIANTS_BY_STRATUM:
                actual = len(family_counts[(split, stratum)])
                if actual < minimum:
                    raise EvalError(f"{split}/{stratum} has {actual} task families; preregistration requires {minimum}")
    return tasks


def _nonnegative_int(value: Any, label: str, *, nullable: bool = False) -> None:
    if nullable and value is None:
        return
    if type(value) is not int or value < 0:
        raise EvalError(f"{label} must be a nonnegative integer" + (" or null" if nullable else ""))


def _validate_usage(usage: Any, label: str) -> None:
    expected = {"basis", "input_tokens", "output_tokens", "reasoning_tokens", "cached_input_tokens"}
    if not isinstance(usage, dict):
        raise EvalError(f"{label} must be an object")
    _keys(usage, expected, label)
    if usage["basis"] not in {"provider_reported", "tokenizer_estimate", "unavailable"}:
        raise EvalError(f"{label}.basis is unsupported")
    for name in ("input_tokens", "output_tokens", "reasoning_tokens", "cached_input_tokens"):
        _nonnegative_int(usage[name], f"{label}.{name}", nullable=True)
    if usage["basis"] == "unavailable":
        if any(usage[name] is not None for name in ("input_tokens", "output_tokens", "reasoning_tokens", "cached_input_tokens")):
            raise EvalError(f"{label} must use null counters when usage basis is unavailable")
    elif usage["input_tokens"] is None or usage["output_tokens"] is None:
        raise EvalError(f"{label} needs input and output counters when usage is available")
    if usage["reasoning_tokens"] is not None and usage["output_tokens"] is not None and usage["reasoning_tokens"] > usage["output_tokens"]:
        raise EvalError(f"{label}.reasoning_tokens cannot exceed output_tokens")
    if usage["cached_input_tokens"] is not None and usage["input_tokens"] is not None and usage["cached_input_tokens"] > usage["input_tokens"]:
        raise EvalError(f"{label}.cached_input_tokens cannot exceed input_tokens")


def _validate_cost(cost: Any, label: str) -> None:
    if not isinstance(cost, dict):
        raise EvalError(f"{label} must be an object")
    _keys(cost, {"basis", "amount", "currency"}, label)
    if cost["basis"] not in {"provider_reported", "price_sheet_estimate", "unavailable"}:
        raise EvalError(f"{label}.basis is unsupported")
    amount, currency = cost["amount"], cost["currency"]
    if cost["basis"] == "unavailable":
        if amount is not None or currency is not None:
            raise EvalError(f"{label} must use null amount and currency when unavailable")
        return
    if not isinstance(amount, str) or not re.fullmatch(r"[0-9]+(?:\.[0-9]+)?", amount):
        raise EvalError(f"{label}.amount must be a nonnegative decimal string")
    try:
        if Decimal(amount) < 0:
            raise EvalError(f"{label}.amount cannot be negative")
    except InvalidOperation as error:
        raise EvalError(f"{label}.amount is invalid") from error
    if not isinstance(currency, str) or not re.fullmatch(r"[A-Z]{3}", currency):
        raise EvalError(f"{label}.currency must be an ISO-style currency code")


def validate_result(record: Any, tasks: dict[str, dict[str, Any]], *, production: bool = True) -> dict[str, Any]:
    if not isinstance(record, dict):
        raise EvalError("each result line must be a JSON object")
    _keys(record, REQUIRED_RESULT_KEYS, "result")
    if record["schema"] != "execute.ai-productivity.run/1":
        raise EvalError("unsupported result schema")
    if record["trial_type"] != "evaluation" and production:
        raise EvalError("fixture or non-evaluation records cannot be imported as trials")
    if not isinstance(record["run_id"], str):
        raise EvalError("run_id must be a UUID string")
    try:
        uuid.UUID(record["run_id"])
    except ValueError as error:
        raise EvalError("run_id must be a UUID string") from error
    _hash(record["runner_image_sha256"], "runner_image_sha256")
    if not isinstance(record["blind_arm_id"], str) or not ARM.fullmatch(record["blind_arm_id"]):
        raise EvalError("blind_arm_id must be an opaque arm_ identifier")
    if record["task_id"] not in tasks:
        raise EvalError(f"result references unknown task_id {record['task_id']!r}")
    _nonnegative_int(record["replicate"], "replicate")
    if record["replicate"] == 0:
        raise EvalError("replicate starts at 1")
    if record["replicate"] != 1:
        raise EvalError("replicate must match the preregistered single replicate per cell")
    task = tasks[record["task_id"]]
    attempts = record["attempts"]
    if not isinstance(attempts, list) or not 1 <= len(attempts) <= 3:
        raise EvalError("attempts must contain one generation and at most two repairs")
    for index, attempt in enumerate(attempts):
        label = f"attempts[{index}]"
        if not isinstance(attempt, dict):
            raise EvalError(f"{label} must be an object")
        _keys(attempt, REQUIRED_ATTEMPT_KEYS, label)
        expected_stage = "generate" if index == 0 else "repair"
        if attempt["stage"] != expected_stage:
            raise EvalError(f"{label}.stage must be {expected_stage!r}")
        _hash(attempt["artifact_sha256"], f"{label}.artifact_sha256")
        _utc(attempt["started_utc"], f"{label}.started_utc")
        for timing in ("generation_ms", "execution_ms"):
            _nonnegative_int(attempt[timing], f"{label}.{timing}")
        _validate_usage(attempt["usage"], f"{label}.usage")
        _validate_cost(attempt["cost"], f"{label}.cost")
        if attempt["oracle_status"] not in {"pass", "fail", "infrastructure_error", "provider_refusal", "provider_timeout", "invalid_artifact"}:
            raise EvalError(f"{label}.oracle_status is unsupported")
        required_criteria = set(task["criteria"])
        optional_criteria = set(task.get("optional_criteria", []))
        results = attempt["criteria_results"]
        if not isinstance(results, dict) or results.keys() != required_criteria | optional_criteria:
            raise EvalError(f"{label}.criteria_results must contain exactly the roster criterion IDs")
        if not all(type(value) is bool for value in results.values()):
            raise EvalError(f"{label}.criteria_results values must be booleans")
        if attempt["oracle_status"] == "pass" and not all(results[name] for name in required_criteria):
            raise EvalError(f"{label} says oracle passed while a mandatory criterion failed")
        feedback = attempt["diagnostic_feedback_sent"]
        if not isinstance(feedback, list) or not all(isinstance(item, str) and CRITERION.fullmatch(item) for item in feedback):
            raise EvalError(f"{label}.diagnostic_feedback_sent must be criterion IDs")
        if index == 0 and feedback:
            raise EvalError("generation attempt cannot receive repair diagnostics")
        if index > 0 and not feedback:
            raise EvalError("a repair attempt must identify the bounded diagnostic categories sent")
    review = record["review"]
    if not isinstance(review, dict):
        raise EvalError("review must be an object")
    if {"scoring_locked", "ratings"} - review.keys() or review.keys() - {"scoring_locked", "ratings", "adjudication"}:
        raise EvalError("review must contain scoring_locked, ratings, and optional adjudication")
    if type(review["scoring_locked"]) is not bool:
        raise EvalError("review.scoring_locked must be a boolean")
    if production and not review["scoring_locked"]:
        raise EvalError("scores must be locked before import")
    ratings = review["ratings"]
    if not isinstance(ratings, list):
        raise EvalError("review.ratings must be an array")
    if production and len(ratings) != 2:
        raise EvalError("exactly two independent blinded ratings are required before adjudication")
    rater_ids = set()
    rubric_hash = json.loads(PROTOCOL.read_text())["scoring"]["maintainability_rubric_sha256"]
    if sha256_file(RUBRIC) != rubric_hash:
        raise EvalError("maintainability rubric bytes differ from the preregistered hash")
    for index, rating in enumerate(ratings):
        label = f"review.ratings[{index}]"
        fields = {"rater_id", "rubric_sha256", "requirement_coverage", "clarity", "changeability"}
        if not isinstance(rating, dict):
            raise EvalError(f"{label} must be an object")
        _keys(rating, fields, label)
        if not isinstance(rating["rater_id"], str) or not TASK_ID.fullmatch(rating["rater_id"]):
            raise EvalError(f"{label}.rater_id is malformed")
        if rating["rater_id"] in rater_ids:
            raise EvalError("review ratings must come from distinct raters")
        rater_ids.add(rating["rater_id"])
        _hash(rating["rubric_sha256"], f"{label}.rubric_sha256")
        if rating["rubric_sha256"] != rubric_hash:
            raise EvalError(f"{label}.rubric_sha256 does not match the preregistered rubric")
        for field in ("requirement_coverage", "clarity", "changeability"):
            if type(rating[field]) is not int or not 1 <= rating[field] <= 5:
                raise EvalError(f"{label}.{field} must be an integer from 1 to 5")
    first_scores = [
        tuple(rating[field] for field in ("requirement_coverage", "clarity", "changeability"))
        for rating in ratings
    ]
    disagreement = len(set(first_scores)) > 1
    adjudication = review.get("adjudication")
    if disagreement and production and not isinstance(adjudication, dict):
        raise EvalError("disagreeing blinded ratings require a third-rater adjudication")
    if not disagreement and adjudication is not None:
        raise EvalError("adjudication is only recorded when the first two raters disagree")
    if adjudication is not None:
        fields = {"rater_id", "rubric_sha256", "requirement_coverage", "clarity", "changeability"}
        if not isinstance(adjudication, dict):
            raise EvalError("review.adjudication must be an object or null")
        _keys(adjudication, fields, "review.adjudication")
        if not isinstance(adjudication["rater_id"], str) or not TASK_ID.fullmatch(adjudication["rater_id"]):
            raise EvalError("review.adjudication.rater_id is malformed")
        if adjudication["rater_id"] in rater_ids:
            raise EvalError("the adjudicator must be a third distinct rater")
        _hash(adjudication["rubric_sha256"], "review.adjudication.rubric_sha256")
        if adjudication["rubric_sha256"] != rubric_hash:
            raise EvalError("review.adjudication rubric does not match the preregistered rubric")
        for field in ("requirement_coverage", "clarity", "changeability"):
            if type(adjudication[field]) is not int or not 1 <= adjudication[field] <= 5:
                raise EvalError(f"review.adjudication.{field} must be an integer from 1 to 5")
    return record


def read_roster(path: Path, *, production: bool = True) -> tuple[Any, dict[str, dict[str, Any]]]:
    try:
        raw = json.loads(path.read_text())
    except (OSError, json.JSONDecodeError) as error:
        raise EvalError(f"cannot read roster {path}: {error}") from error
    return raw, validate_roster(raw, production=production)


def read_results(path: Path, tasks: dict[str, dict[str, Any]], *, production: bool = True) -> list[dict[str, Any]]:
    records = []
    seen = set()
    try:
        source = path.open(encoding="utf-8")
    except OSError as error:
        raise EvalError(f"cannot read results {path}: {error}") from error
    with source:
        for line_number, line in enumerate(source, 1):
            if not line.strip():
                continue
            try:
                record = json.loads(line)
            except json.JSONDecodeError as error:
                raise EvalError(f"results line {line_number}: invalid JSON: {error}") from error
            record = validate_result(record, tasks, production=production)
            key = (record["task_id"], record["blind_arm_id"], record["replicate"])
            if key in seen:
                raise EvalError(f"duplicate task/arm/replicate result at line {line_number}: {key}")
            seen.add(key)
            records.append(record)
    return records


def _attempt_success(attempt: dict[str, Any], task: dict[str, Any]) -> bool:
    return attempt["oracle_status"] == "pass" and all(attempt["criteria_results"][key] for key in task["criteria"])


def _median(values: list[float]) -> float | None:
    return statistics.median(values) if values else None


def _resolved_review(review: dict[str, Any]) -> tuple[dict[str, int], bool]:
    dimensions = ("requirement_coverage", "clarity", "changeability")
    first, second = review["ratings"]
    same = all(first[key] == second[key] for key in dimensions)
    selected = first if same else review["adjudication"]
    return {key: selected[key] for key in dimensions}, not same


def summarize(roster: dict[str, Any], tasks: dict[str, dict[str, Any]], records: list[dict[str, Any]], arms: dict[str, Any] | None = None) -> dict[str, Any]:
    preregistration = json.loads(PROTOCOL.read_text())
    bootstrap_seed = preregistration["analysis"]["randomization_seed"]
    heldout_ids = {task_id for task_id, task in tasks.items() if task["split"] == "held_out"}
    heldout_records = [record for record in records if record["task_id"] in heldout_ids]
    grouped: dict[str, list[dict[str, Any]]] = defaultdict(list)
    for record in heldout_records:
        grouped[record["blind_arm_id"]].append(record)

    per_arm = []
    for blind_id, trials in sorted(grouped.items()):
        initial = []
        final = []
        token_usage: dict[str, dict[str, list[int]]] = {
            basis: defaultdict(list)
            for basis in ("provider_reported", "tokenizer_estimate")
        }
        generation_times = []
        execution_times = []
        repairs = []
        resolved_ratings: dict[str, list[int]] = defaultdict(list)
        adjudicated_trials = 0
        exact_rating_agreements = 0
        provider_costs: dict[str, list[Decimal]] = defaultdict(list)
        estimated_costs: dict[str, list[Decimal]] = defaultdict(list)
        for record in trials:
            task = tasks[record["task_id"]]
            attempts = record["attempts"]
            initial.append(_attempt_success(attempts[0], task))
            final.append(_attempt_success(attempts[-1], task))
            repairs.append(len(attempts) - 1)
            resolved, was_adjudicated = _resolved_review(record["review"])
            adjudicated_trials += int(was_adjudicated)
            exact_rating_agreements += int(not was_adjudicated)
            for dimension, score in resolved.items():
                resolved_ratings[dimension].append(score)
            generation_times.append(sum(item["generation_ms"] for item in attempts))
            execution_times.append(sum(item["execution_ms"] for item in attempts))
            usages = [item["usage"] for item in attempts]
            usage_bases = {item["basis"] for item in usages}
            if len(usage_bases) == 1 and next(iter(usage_bases)) in token_usage:
                basis = next(iter(usage_bases))
                if all(item["input_tokens"] is not None and item["output_tokens"] is not None for item in usages):
                    token_usage[basis]["total_input_output_tokens"].append(
                        sum(item["input_tokens"] + item["output_tokens"] for item in usages)
                    )
                for component in ("input_tokens", "output_tokens", "reasoning_tokens", "cached_input_tokens"):
                    if all(item[component] is not None for item in usages):
                        token_usage[basis][component].append(sum(item[component] for item in usages))
            for basis, destination in (("provider_reported", provider_costs), ("price_sheet_estimate", estimated_costs)):
                matching = [item["cost"] for item in attempts if item["cost"]["basis"] == basis]
                if len(matching) == len(attempts) and matching:
                    currencies = {item["currency"] for item in matching}
                    if len(currencies) == 1:
                        destination[next(iter(currencies))].append(sum((Decimal(item["amount"]) for item in matching), Decimal(0)))
        entry = {
            "blind_arm_id": blind_id,
            "n_held_out_trials": len(trials),
            "n_held_out_tasks": len({item["task_id"] for item in trials}),
            "initial_success_rate": sum(initial) / len(initial) if initial else None,
            "final_success_rate": sum(final) / len(final) if final else None,
            "token_usage_by_basis": {
                basis: {
                    "complete_trials_by_metric": {metric: len(values) for metric, values in sorted(metrics.items())},
                    "median_by_metric": {metric: _median(values) for metric, values in sorted(metrics.items())},
                }
                for basis, metrics in sorted(token_usage.items())
            },
            "median_repairs": _median(repairs),
            "median_generation_ms": _median(generation_times),
            "median_execution_ms": _median(execution_times),
            "maintainability_review": {
                "rated_trials": len(trials),
                "adjudicated_trials": adjudicated_trials,
                "first_pair_exact_agreement_rate": exact_rating_agreements / len(trials) if trials else None,
                "resolved_mean_by_dimension": {
                    dimension: statistics.mean(values) for dimension, values in sorted(resolved_ratings.items())
                },
                "resolved_median_by_dimension": {
                    dimension: statistics.median(values) for dimension, values in sorted(resolved_ratings.items())
                },
            },
            "provider_reported_costs": {
                currency: {"n_trials": len(values), "median_amount": str(statistics.median(values))}
                for currency, values in sorted(provider_costs.items())
            },
            "price_sheet_estimates": {
                currency: {"n_trials": len(values), "median_amount": str(statistics.median(values))}
                for currency, values in sorted(estimated_costs.items())
            },
        }
        if arms is not None:
            entry["arm"] = arms[blind_id]
        per_arm.append(entry)

    summary: dict[str, Any] = {
        "schema": "execute.ai-productivity.summary/1",
        "status": "pending_no_held_out_trials" if not heldout_records else "descriptive_import_no_claim",
        "preregistration_sha256": sha256_file(PROTOCOL),
        "task_package_sha256": roster["task_package_sha256"],
        "randomization_seed": preregistration["analysis"]["randomization_seed"],
        "roster_sha256": hashlib.sha256(json.dumps(roster, sort_keys=True, separators=(",", ":")).encode()).hexdigest(),
        "result_file_records": len(records),
        "held_out_records": len(heldout_records),
        "held_out_roster_tasks": len(heldout_ids),
        "coverage": {
            blind_id: {
                "task_ids_present": sorted({record["task_id"] for record in trials}),
                "expected_task_ids": len(heldout_ids),
                "complete": {record["task_id"] for record in trials} == heldout_ids,
            }
            for blind_id, trials in sorted(grouped.items())
        },
        "arms": per_arm,
        "paired_analysis": None,
        "claim": "none",
        "notes": [
            "Import/fixture validation is not evidence of model productivity.",
            "No superiority claim is emitted by this harness.",
            "Infrastructure failures and missing usage remain in the raw rows and are not silently removed.",
        ],
    }
    if arms is not None and heldout_records:
        summary["paired_analysis"] = paired_success(heldout_records, tasks, arms, bootstrap_seed=bootstrap_seed)
    return summary


def read_arm_map(path: Path, records: list[dict[str, Any]]) -> dict[str, dict[str, Any]]:
    try:
        document = json.loads(path.read_text())
    except (OSError, json.JSONDecodeError) as error:
        raise EvalError(f"cannot read arm map {path}: {error}") from error
    if not isinstance(document, dict) or document.get("schema") != "execute.ai-productivity.arms/1" or not isinstance(document.get("arms"), dict):
        raise EvalError("arm map must use execute.ai-productivity.arms/1")
    arms = document["arms"]
    used = {record["blind_arm_id"] for record in records}
    if not used <= arms.keys():
        raise EvalError(f"arm map is missing IDs: {sorted(used - arms.keys())}")
    for blind_id, arm in arms.items():
        if not ARM.fullmatch(blind_id) or not isinstance(arm, dict):
            raise EvalError("arm map keys must be opaque IDs and values must be objects")
        required = {"variant", "provider", "model_snapshot", "comparison_block", "generation_parameters", "api_version", "language_version", "system_prompt_sha256", "tool_config_sha256"}
        _keys(arm, required, f"arm {blind_id}")
        if arm["variant"] not in {variant for variants in VARIANTS_BY_STRATUM.values() for variant in variants}:
            raise EvalError(f"arm {blind_id} has an unsupported variant")
        for field in ("provider", "model_snapshot", "comparison_block", "api_version", "language_version"):
            if not isinstance(arm[field], str) or not arm[field]:
                raise EvalError(f"arm {blind_id}.{field} must be a nonempty string")
        if not isinstance(arm["generation_parameters"], dict):
            raise EvalError(f"arm {blind_id}.generation_parameters must be an object; use null values for unavailable parameters")
        for field in ("system_prompt_sha256", "tool_config_sha256"):
            _hash(arm[field], f"arm {blind_id}.{field}")
    return arms


def _percentile(values: list[float], p: float) -> float:
    ordered = sorted(values)
    if not ordered:
        raise EvalError("cannot compute interval with no paired task families")
    if len(ordered) == 1:
        return ordered[0]
    location = (len(ordered) - 1) * p
    lower = math.floor(location)
    upper = math.ceil(location)
    if lower == upper:
        return ordered[lower]
    return ordered[lower] + (ordered[upper] - ordered[lower]) * (location - lower)


def paired_success(records: list[dict[str, Any]], tasks: dict[str, dict[str, Any]], arms: dict[str, dict[str, Any]], *, bootstrap_seed: int) -> dict[str, Any]:
    """Summarize matched final-success differences by held-out family.

    Missing task cells are surfaced and never imputed as successes or silently
    counted in an arm's paired result.
    """
    block_cells: dict[tuple[str, str, int, str, str], dict[str, dict[str, Any]]] = defaultdict(dict)
    for record in records:
        task = tasks[record["task_id"]]
        blind_id = record["blind_arm_id"]
        arm = arms[blind_id]
        key = (arm["comparison_block"], task["stratum"], record["replicate"], task["family_id"], record["task_id"])
        if arm["variant"] in block_cells[key]:
            raise EvalError(f"multiple arms for variant {arm['variant']!r} in the same comparison block/task cell")
        block_cells[key][arm["variant"]] = record
    variants = {arm["variant"] for arm in arms.values()}
    comparisons: dict[str, Any] = {}
    for stratum, required_variants in VARIANTS_BY_STRATUM.items():
        if not required_variants <= variants:
            continue
        base = "execute_program+qml" if stratum == "desktop_composition" else "execute_program"
        for comparator in sorted(required_variants - {base}):
            family_deltas: dict[tuple[str, str], list[float]] = defaultdict(list)
            expected_families = {task["family_id"] for task in tasks.values() if task["split"] == "held_out" and task["stratum"] == stratum}
            unmatched = 0
            for (block, cell_stratum, _replicate, family, _task_id), cells in block_cells.items():
                if cell_stratum != stratum:
                    continue
                if base not in cells or comparator not in cells:
                    unmatched += 1
                    continue
                base_record, other_record = cells[base], cells[comparator]
                if base_record["task_id"] != other_record["task_id"]:
                    unmatched += 1
                    continue
                task = tasks[base_record["task_id"]]
                difference = float(_attempt_success(base_record["attempts"][-1], task)) - float(_attempt_success(other_record["attempts"][-1], task))
                family_deltas[(block, family)].append(difference)
            deltas = [statistics.mean(values) for values in family_deltas.values()]
            if not deltas:
                continue
            rng = random.Random(bootstrap_seed)
            bootstrap = [statistics.mean(rng.choices(deltas, k=len(deltas))) for _ in range(10_000)]
            comparisons[f"{stratum}:execute_minus_{comparator}"] = {
                "paired_task_family_blocks": len(deltas),
                "expected_held_out_families_per_block": len(expected_families),
                "unmatched_family_blocks": unmatched,
                "mean_final_success_delta": statistics.mean(deltas),
                "bonferroni_97_5_percent_family_cluster_bootstrap_interval": [
                    _percentile(bootstrap, 0.0125), _percentile(bootstrap, 0.9875)
                ],
                "bootstrap_resamples": 10000,
                "seed": bootstrap_seed,
            }
    return {"method": "family-cluster paired bootstrap; descriptive until protocol and data are independently reviewed", "contrasts": comparisons}


def command_validate(args: argparse.Namespace) -> dict[str, Any]:
    roster, tasks = read_roster(args.roster, production=True)
    records = read_results(args.results, tasks, production=True)
    held_out = sum(tasks[item["task_id"]]["split"] == "held_out" for item in records)
    return {
        "schema": "execute.ai-productivity.validation/1",
        "status": "pending_no_held_out_trials" if not held_out else "valid_descriptive_import",
        "roster_sha256": hashlib.sha256(json.dumps(roster, sort_keys=True, separators=(",", ":")).encode()).hexdigest(),
        "results_sha256": sha256_file(args.results),
        "records": len(records),
        "held_out_records": held_out,
        "claim": "none",
    }


def command_summarize(args: argparse.Namespace) -> dict[str, Any]:
    roster, tasks = read_roster(args.roster, production=True)
    records = read_results(args.results, tasks, production=True)
    arms = read_arm_map(args.arms, records) if args.arms else None
    result = summarize(roster, tasks, records, arms)
    result["results_sha256"] = sha256_file(args.results)
    return result


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)
    for name in ("validate", "summarize"):
        command = subparsers.add_parser(name)
        command.add_argument("--roster", required=True, type=Path)
        command.add_argument("--results", required=True, type=Path)
        if name == "summarize":
            command.add_argument("--arms", type=Path, help="sealed arm map; pass only after blind scoring is locked")
            command.add_argument("--out", type=Path)
    args = parser.parse_args()
    try:
        result = command_validate(args) if args.command == "validate" else command_summarize(args)
        encoded = json.dumps(result, sort_keys=True, indent=2) + "\n"
        if getattr(args, "out", None):
            args.out.write_text(encoded)
        else:
            sys.stdout.write(encoded)
    except EvalError as error:
        parser.error(str(error))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
