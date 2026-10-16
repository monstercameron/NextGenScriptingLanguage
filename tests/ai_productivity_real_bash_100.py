#!/usr/bin/env python3
"""Synthetic contract checks; these rows are not real-script study evidence."""

import importlib.util
import json
import os
import shutil
import sys
import tempfile
import unittest
import uuid
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
MODULE_PATH = ROOT / "benchmarks" / "ai_productivity" / "real_bash_100.py"
SPEC = importlib.util.spec_from_file_location("ai_productivity_real_bash_100", MODULE_PATH)
STUDY = importlib.util.module_from_spec(SPEC)
assert SPEC.loader
SPEC.loader.exec_module(STUDY)

ZERO = "0" * 64
ONE = "1" * 64
TWO = "2" * 64
THREE = "3" * 64
ARM_IDS = ["arm_abcdefgh", "arm_ijklmnop", "arm_qrstuvwx"]
VARIANTS = ["bash", "nushell", "execute_program"]
CORPUS_DIR = Path(os.environ.get("EXECUTE_REAL_BASH_CORPUS", ROOT.parent / "work" / "heldout-bash-corpus"))


def link_or_copy(source, target):
    try:
        os.link(source, target)
    except OSError:
        shutil.copy2(source, target)


def encoded_hash(value):
    return STUDY.digest_bytes(json.dumps(value, sort_keys=True, separators=(",", ":")).encode())


def arm_map():
    common = {
        "provider": "codex-agent-surface",
        "model_alias": "gpt-6-luna",
        "model_snapshot_status": "unavailable",
        "model_snapshot": None,
        "comparison_block": "luna-block-1",
        "generation_parameters": {"temperature": None, "seed": None, "max_output_tokens": None},
        "programmer_prompt_sha256": ZERO,
        "language_instruction_sha256": ONE,
        "tool_configuration_sha256": ONE,
    }
    return {
        "schema": "execute.ai-productivity.real-bash-100-arms/1",
        "arms": {
            ARM_IDS[0]: {**common, "variant": VARIANTS[0], "language_version": "bash-version-pinned", "language_reference_sha256": TWO},
            ARM_IDS[1]: {**common, "variant": VARIANTS[1], "language_version": "nu-version-pinned", "language_reference_sha256": THREE},
            ARM_IDS[2]: {**common, "variant": VARIANTS[2], "language_version": "execute-version-pinned", "language_reference_sha256": "4" * 64},
        },
    }


def roster():
    tasks = []
    for index in range(100):
        tasks.append({
            "task_id": f"script_{index:03d}",
            "family_id": f"cluster_{index // 5:02d}",
            "split": "held_out",
            "stratum": "workflow_cli",
            "testability_stratum": "filesystem_fixture",
            "repo_url": "https://example.invalid/omarchy",
            "source_commit": ONE,
            "source_path": f"bin/task-{index:03d}",
            "source_sha256": f"{index + 1:064x}",
            "manifest_record_sha256": f"{index + 101:064x}",
            "prompt_sha256": TWO,
            "behavior_brief_sha256": THREE,
            "oracle_sha256": "4" * 64,
            "oracle_ref": f"oracle-ref-{index:03d}",
            "criteria": {
                "runs": "process_io_exact",
                "state_exact": "filesystem_final_exact",
                "all_cases_observed": "all_cases_observed",
            },
            "timing_case_set_sha256": "5" * 64,
            "timing_case_ref": f"timing-ref-{index:03d}",
        })
    return {
        "schema": "execute.ai-productivity.real-bash-100-roster/1",
        "status": "frozen_before_generation",
        "frozen_utc": "2026-10-15T12:00:00Z",
        "protocol_sha256": STUDY.digest_file(STUDY.PROTOCOL),
        "corpus_manifest_sha256": STUDY.R5_CANDIDATE["manifest_sha256"],
        "digest_receipt_sha256": STUDY.R5_CANDIDATE["receipt_sha256"],
        "task_package_sha256": "8" * 64,
        "tasks": tasks,
    }


def freeze_document(roster_sha, run_plan_sha, arms_sha):
    return {
        "schema": "execute.ai-productivity.real-bash-100-freeze/1",
        "status": "frozen_before_dispatch",
        "frozen_utc": "2026-10-15T12:01:00Z",
        "randomization_seed": 20261015,
        "comparison_block": "luna-block-1",
        "corpus_review_status": "accepted",
        "protocol_sha256": STUDY.digest_file(STUDY.PROTOCOL),
        "rubric_sha256": STUDY.digest_file(STUDY.RUBRIC),
        "corpus_review_sha256": "9" * 64,
        "corpus_manifest_sha256": STUDY.R5_CANDIDATE["manifest_sha256"],
        "corpus_digest_receipt_sha256": STUDY.R5_CANDIDATE["receipt_sha256"],
        "task_roster_sha256": roster_sha,
        "task_package_sha256": "8" * 64,
        "oracle_package_sha256": "a" * 64,
        "timing_case_package_sha256": "5" * 64,
        "runner_source_sha256": "b" * 64,
        "runner_image_sha256": "c" * 64,
        "language_reference_bundle_sha256": "d" * 64,
        "arm_map_sha256": arms_sha,
        "tool_configuration_sha256": ONE,
        "randomized_run_plan_sha256": run_plan_sha,
    }


def attempt(stage, status, criteria, feedback=(), artifact_index=1):
    criteria = dict(criteria)
    criteria.setdefault("all_cases_observed", None if status in {"infrastructure_error", "provider_refusal", "provider_timeout"} else True)
    return {
        "stage": stage,
        "artifact_sha256": f"{artifact_index:064x}",
        "artifact_ref": f"candidate-{artifact_index}.py",
        "archive_ref": f"synthetic-fixture/{artifact_index}",
        "archive_manifest_ref": "archive-manifest.json",
        "archive_manifest_sha256": ZERO,
        "observation_sha256": ZERO,
        "started_utc": "2026-10-15T12:02:00Z",
        "conductor_elapsed_ms": 100,
        "elapsed_basis": "dispatch_to_response_wall_clock",
        "usage": {
            "basis": "unavailable", "input_tokens": None, "output_tokens": None,
            "reasoning_tokens": None, "cached_input_tokens": None,
        },
        "tool_calls": {"basis": "unavailable", "count": None},
        "cost": {"basis": "unavailable", "amount": None, "currency": None},
        "oracle_status": status,
        "criteria_results": criteria,
        "diagnostic_feedback_sent": list(feedback),
    }


def locked_review():
    rubric_sha = STUDY.digest_file(STUDY.RUBRIC)
    rating_a = {
        "rater_id": "rater_a", "rubric_sha256": rubric_sha,
        "requirement_coverage": 4, "readability": 3, "changeability": 4,
    }
    rating_b = dict(rating_a, rater_id="rater_b")
    return {
        "status": "locked",
        "ratings": [rating_a, rating_b],
        "adjudication": None,
        "score_locked_utc": "2026-10-20T12:00:00Z",
    }


def trial_for(task_id, arm_id, dispatch, freeze, freeze_sha, roster_sha, attempts, runtime_status, runtime_ms=()):
    return {
        "schema": "execute.ai-productivity.real-bash-100-run/1",
        "trial_type": "evaluation",
        "run_id": str(uuid.uuid5(uuid.NAMESPACE_URL, f"{task_id}/{arm_id}")),
        "session_id": f"synthetic-session-{task_id}-{arm_id}",
        "task_id": task_id,
        "blind_arm_id": arm_id,
        "replicate": 1,
        "dispatch_index": dispatch,
        "freeze_sha256": freeze_sha,
        **{field: freeze[freeze_field] for field, freeze_field in STUDY.RESULT_FREEZE_FIELDS.items()},
        "attempts": attempts,
        "runtime": {
            "status": runtime_status,
            "timing_case_set_sha256": "5" * 64,
            "warmup_runs": 1 if runtime_status == "measured" else None,
            "repetitions_ms": list(runtime_ms),
        },
        "review": locked_review(),
    }


def fixture_environment():
    frozen_roster = roster()
    roster_sha = encoded_hash(frozen_roster)
    arms = arm_map()
    arms_sha = encoded_hash(arms)
    plan = STUDY.make_run_plan(frozen_roster, roster_sha, list(arms["arms"]), 20261015)
    plan_sha = encoded_hash(plan)
    freeze = freeze_document(roster_sha, plan_sha, arms_sha)
    freeze_sha = encoded_hash(freeze)
    checked_freeze = STUDY.validate_freeze(freeze)
    tasks = STUDY.validate_roster(frozen_roster, checked_freeze, roster_sha)
    arm_rows = STUDY.validate_arm_map(arms, arms_sha, checked_freeze)
    plan_by_dispatch = STUDY.validate_run_plan(plan, checked_freeze, plan_sha, roster_sha, tasks, arm_rows)
    return frozen_roster, roster_sha, arms, arms_sha, plan, plan_sha, freeze, freeze_sha, tasks, plan_by_dispatch


def archive_fixture(root, *, candidate_source=None, limits=None):
    runner = STUDY.RUNNER
    task_id = "script_000"
    criteria = {
        "process_contract": "process_io_exact",
        "filesystem_contract": "filesystem_final_exact",
        "mock_contract": "mock_command_effects_exact",
        "all_cases_observed": "all_cases_observed",
    }
    prompt = "Create a fixture program that writes and prints ok."
    behavior = "Write exactly ok followed by LF to out.txt and stdout; exit 0."
    brief_task = {
        "task_id": task_id, "family_id": "cluster_00", "split": "held_out",
        "stratum": "workflow_cli", "testability_stratum": "command_fixture",
        "prompt": prompt, "behavior_brief": behavior,
        "prompt_sha256": STUDY.digest_bytes(prompt.encode()),
        "behavior_brief_sha256": STUDY.digest_bytes(behavior.encode()),
    }
    brief = {
        "schema": runner.BRIEF_SCHEMA, "status": "draft_fixture", "fixture_only": True,
        "protocol_sha256": ZERO,
        "corpus_manifest_sha256": STUDY.R5_CANDIDATE["manifest_sha256"],
        "tasks": [brief_task],
    }
    brief_raw = runner.canonical(brief)
    case = {
        "case_id": "write_print_ok", "cwd": ".", "argv": [], "stdin_base64": "",
        "env": {}, "filesystem_initial": [], "mock_commands": [],
        "expected": {
            "returncode": 0, "stdout_base64": "b2sK", "stderr_base64": "",
            "filesystem_after": [
                {"path": "home", "type": "directory", "mode": "0755"},
                {"path": "out.txt", "type": "file", "mode": "0644", "data_base64": "b2sK"},
                {"path": "tmp", "type": "directory", "mode": "0755"},
            ],
            "mock_events": [],
        },
    }
    oracle_task = {"task_id": task_id, "criteria": criteria, "cases": [case]}
    oracle_task["oracle_sha256"] = STUDY.digest_bytes(runner.canonical(runner.oracle_material(oracle_task)))
    oracle = {
        "schema": runner.ORACLE_SCHEMA, "status": "draft_fixture", "fixture_only": True,
        "task_package_sha256": STUDY.digest_bytes(brief_raw), "tasks": [oracle_task],
    }
    oracle_raw = runner.canonical(oracle)
    checked_oracles = runner.validate_oracle_package(
        oracle, STUDY.digest_bytes(brief_raw), {task_id}, allow_fixture=True,
    )
    source_roster = roster()
    source_roster["tasks"][0]["oracle_sha256"] = oracle_task["oracle_sha256"]
    source_roster["tasks"][0]["criteria"] = dict(criteria)
    roster_raw = runner.canonical(source_roster)
    roster_sha = STUDY.digest_bytes(roster_raw)
    candidate = root / "candidate.py"
    candidate.write_text(candidate_source or (
        "from pathlib import Path\nimport sys\n"
        "Path('out.txt').write_bytes(b'ok\\n')\n"
        "sys.stdout.buffer.write(b'ok\\n')\n"
    ), encoding="utf-8")
    archive_root = root / "archives"
    archive_root.mkdir()
    observation = runner.run_attempt(
        brief_package=brief, brief_raw=brief_raw, oracle_package=oracle, oracle_raw=oracle_raw,
        roster_package=source_roster, roster_raw=roster_raw,
        task_id=task_id, blind_arm_id=ARM_IDS[0], dispatch_index=1,
        stage="generate", attempt_index=1, candidate=candidate,
        runtime_argv=[sys.executable, "{candidate}"], runtime_version=sys.version,
        output_dir=archive_root / "attempt-0001", sandbox_wrapper=None, fixture_only=True, limits=limits,
    )
    archive_dir = archive_root / "attempt-0001"
    observation_path = archive_dir / "observation.json"
    stored = json.loads(observation_path.read_text(encoding="utf-8"))
    stored["trial_type"] = "candidate_observation_not_trial"
    observation_path.write_bytes(runner.canonical(stored) + b"\n")
    _manifest, manifest_sha, observation_sha = runner._write_archive_manifest(archive_dir)
    observation = stored

    arm_rows = arm_map()
    arm_sha = STUDY.digest_bytes(STUDY.canonical_json_bytes(arm_rows))
    freeze = freeze_document(roster_sha, ZERO, arm_sha)
    freeze["task_roster_sha256"] = roster_sha
    freeze["task_package_sha256"] = STUDY.digest_bytes(brief_raw)
    freeze["oracle_package_sha256"] = STUDY.digest_bytes(oracle_raw)
    freeze["runner_source_sha256"] = observation["runner_source_sha256"]
    checked_freeze = STUDY.validate_freeze(freeze)
    task = source_roster["tasks"][0]
    derived = observation["criteria_results"]
    attempt_row = {
        "stage": "generate", "artifact_sha256": observation["attempt"]["artifact_sha256"],
        "artifact_ref": observation["attempt"]["artifact_ref"], "archive_ref": "attempt-0001",
        "archive_manifest_ref": "archive-manifest.json", "archive_manifest_sha256": manifest_sha,
        "observation_sha256": observation_sha, "started_utc": observation["started_utc"],
        "conductor_elapsed_ms": 100, "elapsed_basis": "dispatch_to_response_wall_clock",
        "usage": {"basis": "unavailable", "input_tokens": None, "output_tokens": None,
                  "reasoning_tokens": None, "cached_input_tokens": None},
        "tool_calls": {"basis": "unavailable", "count": None},
        "cost": {"basis": "unavailable", "amount": None, "currency": None},
        "oracle_status": "fail" if False in derived.values() else "infrastructure_error" if None in derived.values() else "pass",
        "criteria_results": derived, "diagnostic_feedback_sent": [],
    }
    record = {
        "run_id": observation["run_id"], "session_id": observation["session_id"],
        "task_id": task_id, "blind_arm_id": ARM_IDS[0], "dispatch_index": 1,
    }
    return archive_root, checked_oracles[task_id], task, checked_freeze, roster_sha, record, attempt_row, manifest_sha


def make_records():
    roster_doc, roster_sha, arms, _, plan, plan_sha, freeze, freeze_sha, tasks, plan_by_dispatch = fixture_environment()
    cells = {(cell["task_id"], cell["blind_arm_id"]): cell["dispatch_index"] for cell in plan["cells"]}
    records = []
    plans = {
        ("script_000", ARM_IDS[0]): ([("generate", "fail", {"runs": True, "state_exact": False}, ()), ("repair", "pass", {"runs": True, "state_exact": True}, ("state_exact",))], "measured", [10] * 10),
        ("script_000", ARM_IDS[1]): ([("generate", "fail", {"runs": True, "state_exact": False}, ()), ("repair", "pass", {"runs": True, "state_exact": True}, ("state_exact",))], "measured", [20] * 10),
        ("script_000", ARM_IDS[2]): ([("generate", "pass", {"runs": True, "state_exact": True}, ())], "measured", [15] * 10),
        ("script_001", ARM_IDS[0]): ([("generate", "fail", {"runs": True, "state_exact": False}, ()), ("repair", "fail", {"runs": True, "state_exact": False}, ("state_exact",))], "not_measured_functional_failure", []),
        ("script_001", ARM_IDS[1]): ([("generate", "pass", {"runs": True, "state_exact": True}, ())], "measured", [10] * 10),
        ("script_001", ARM_IDS[2]): ([("generate", "pass", {"runs": True, "state_exact": True}, ())], "measured", [14] * 10),
    }
    for (task_id, arm_id), (steps, runtime_status, runtime_ms) in plans.items():
        attempts = [attempt(stage, status, criteria, feedback, index + 1) for index, (stage, status, criteria, feedback) in enumerate(steps)]
        records.append(trial_for(task_id, arm_id, cells[(task_id, arm_id)], freeze, freeze_sha, roster_sha, attempts, runtime_status, runtime_ms))
    return roster_doc, roster_sha, arms, plan, plan_sha, freeze, freeze_sha, tasks, plan_by_dispatch, records


class RealBash100Tests(unittest.TestCase):
    def test_r5_amendment_has_a_separate_pinned_frame_and_rejects_stale_lineage(self):
        protocol, protocol_sha = STUDY._load_r5_protocol()
        self.assertEqual(protocol_sha, STUDY.PROTOCOL_R5_SHA256)
        self.assertEqual(protocol["status"], "draft_revision5_candidate_unsealed_trials_and_original_behavior_proofs_not_run")
        self.assertEqual(protocol["source_corpus"]["r5_frame"], STUDY.R5_FRAME)
        self.assertEqual(protocol["prior_revision3_history"]["candidate_frame"]["eligible_bash_sources"], 406)
        self.assertEqual(protocol["prior_revision3_history"]["candidate_frame"]["functional_family_count"], 27)

        changed = json.loads(json.dumps(protocol))
        changed["source_corpus"]["r5_lineage"]["revision4_snapshot_sha256"] = ZERO
        with self.assertRaisesRegex(STUDY.StudyError, "revision-4 history snapshot binding is stale"):
            STUDY._validate_r5_protocol_lineage(changed)

    def test_r5_frame_verifier_rejects_stale_or_mixed_candidate_hashes(self):
        protocol, _ = STUDY._load_r5_protocol()
        source = protocol["source_corpus"]
        frame = source["r5_frame"]
        manifest = {
            "corpus_id": STUDY.R5_CANDIDATE["corpus_id"],
            "status": "candidate_unsealed",
            "proposed_split": "held_out",
            "source_population": {
                "inventory_bash_script_count": 439,
                "repository_url": "https://github.com/omacom/omarchy",
                "release_tag": "v4.0.4",
                "commit": "c668141e9c42b13c80c9ca4ea108e11708c5e8a5",
                "archive_sha256": STUDY.R5_SOURCE_ARCHIVE_SHA256,
                "inventory_sha256": STUDY.R5_INVENTORY_SHA256,
            },
            "selection": {
                "revision": 5,
                "eligible_frame": {
                    "published_bash_inventory_count": 439,
                    "known_exact_exposed_sources_removed": 20,
                    "material_adjacency_sources_removed": 14,
                    "purpose_only_campaign_scope_sources_removed": 8,
                    "remaining_before_curation": 397,
                },
                "selected_task_count": 100,
                "target_task_count": 100,
                "testability_stratum_counts": frame["fixture_feasibility_quotas"],
                "size_band_counts": frame["size_bands"],
                "low_complexity_count": frame["low_complexity_count"],
            },
            "statistical_scope": {
                "distinct_script_task_count": 100,
                "functional_family_count": 26,
                "family_counts": frame["family_counts"],
                "independent_repository_family_claim": False,
                "source_repository_count": 1,
                "source_release_count": 1,
            },
        }
        identity = source["r5_candidate_identity"]
        STUDY._validate_r5_manifest_frame(
            manifest, protocol, identity["manifest_sha256"], identity["receipt_sha256"],
            identity["source_map_sha256"],
        )
        with self.assertRaisesRegex(STUDY.StudyError, "manifest is mixed, stale"):
            STUDY._validate_r5_manifest_frame(
                manifest, protocol, STUDY.R5_HISTORY[3]["manifest_sha256"],
                identity["receipt_sha256"], identity["source_map_sha256"],
            )
        stale_frame = json.loads(json.dumps(manifest))
        stale_frame["selection"]["eligible_frame"]["remaining_before_curation"] = 406
        with self.assertRaisesRegex(STUDY.StudyError, "selection frame"):
            STUDY._validate_r5_manifest_frame(
                stale_frame, protocol, identity["manifest_sha256"], identity["receipt_sha256"],
                identity["source_map_sha256"],
            )

    def test_r5_purpose_exclusion_links_resolve_only_in_historical_r3(self):
        replacement_ids = {f"old_replacement_{index}" for index in range(8)}
        r3_ids = replacement_ids | {f"other_r3_script_{index}" for index in range(92)}
        r3_manifest = {"scripts": [{"script_id": item} for item in r3_ids]}
        r3_scope = []
        r5_scope = []
        swaps = []
        for index, replacement in enumerate(sorted(replacement_ids)):
            path = f"bin/purpose-only-{index}"
            row = {
                "path": path, "sha256": f"{index + 1:064x}", "bytes": index + 5,
                "lines": index + 1, "mode_octal": "0775", "replacement_script_id": replacement,
            }
            r3_scope.append(row)
            r5_scope.append({key: value for key, value in row.items() if key != "replacement_script_id"})
            swaps.append({
                "remove": {"source_path": path},
                "add": {"script_id": replacement},
            })
        STUDY._validate_historical_r3_scope_linkage(
            r5_scope, r3_scope, r3_manifest, {"swaps": swaps},
        )
        swaps[0]["add"]["script_id"] = "wrong_historical_id"
        with self.assertRaisesRegex(STUDY.StudyError, "does not resolve within the R3 snapshot"):
            STUDY._validate_historical_r3_scope_linkage(
                r5_scope, r3_scope, r3_manifest, {"swaps": swaps},
            )

    @unittest.skipUnless(CORPUS_DIR.is_dir(), "campaign R5 corpus is outside a normal checkout")
    def test_real_r5_candidate_verifies_and_r3_remains_historically_verifiable(self):
        candidate = STUDY.verify_corpus_candidate(CORPUS_DIR)
        self.assertEqual(candidate["candidate_revision"], 5)
        self.assertEqual(candidate["candidate_ledger_counts"]["excluded_material_adjacency"], 14)
        self.assertEqual(candidate["candidate_ledger_counts"]["not_selected_in_fixed_size_slice"], 297)
        self.assertEqual(candidate["distinct_functional_clusters"], 26)
        self.assertEqual(candidate["testability_counts"], STUDY.R5_FRAME["fixture_feasibility_quotas"])
        self.assertEqual(candidate["corpus_acceptance"], "pending_not_inferred_by_local_integrity_verification")
        self.assertEqual(candidate["r5_lineage_verification"]["snapshot_file_count"], 593)
        self.assertEqual(
            candidate["r5_lineage_verification"]["snapshot_self_excluded_path"],
            "history/revision5/SNAPSHOT.json",
        )

        r3 = STUDY.verify_corpus_candidate(
            CORPUS_DIR / "history" / "revision3",
            historical_evidence_root=CORPUS_DIR,
        )
        self.assertEqual(r3["candidate_revision"], 3)
        self.assertEqual(r3["candidate_ledger_counts"]["excluded_material_adjacency"], 5)
        self.assertEqual(r3["source_review_scope"], "R3 source review is historical and rejects held-out admission")
        self.assertEqual(r3["corpus_acceptance"], "historical_R3_rejected_for_held_out_admission")

    @unittest.skipUnless(CORPUS_DIR.is_dir(), "campaign R5 corpus is outside a normal checkout")
    def test_historically_rejected_r3_cannot_pass_an_r5_freeze_or_review(self):
        candidate = STUDY.verify_corpus_candidate(
            CORPUS_DIR / "history" / "revision3",
            historical_evidence_root=CORPUS_DIR,
        )
        frozen_roster, _, _, _, _, _, r5_freeze, _, _, _ = fixture_environment()
        r3_freeze = dict(
            r5_freeze,
            corpus_manifest_sha256=candidate["manifest_sha256"],
            corpus_digest_receipt_sha256=candidate["receipt_sha256"],
        )
        with self.assertRaisesRegex(STUDY.StudyError, "does not match the R5 candidate"):
            STUDY.validate_freeze(r3_freeze)

        task_ids = candidate["candidate_task_ids"]
        tasks = {task_id: {} for task_id in task_ids}
        review = {
            "schema": "execute.ai-productivity.real-bash-100-corpus-review/1",
            "decision": "accepted_held_out",
            "reviewed_utc": "2026-10-15T12:00:00Z",
            "reviewer_id": "synthetic-independent-reviewer",
            "independence": {"not_corpus_curator": True, "not_programmer_operator": True},
            "corpus_manifest_sha256": candidate["manifest_sha256"],
            "corpus_receipt_sha256": candidate["receipt_sha256"],
            "candidate_source_count": 100,
            "accepted_task_ids": task_ids,
            "exact_source_overlap_count": 0,
            "license_status": "accepted",
            "selection_status": "accepted_curated_sample_as_described",
            "family_adjacency_review": {
                "ledger_sha256": candidate["companion_sha256"]["exposure-adjacency.json"],
                "rows_reviewed": 9,
                "material_overlap_rows": 0,
                "unresolved_rows": 0,
            },
            "review_notes_sha256": "8" * 64,
        }
        review_sha = encoded_hash(review)
        r3_freeze["corpus_review_sha256"] = review_sha
        with self.assertRaisesRegex(STUDY.StudyError, "requires a verified revision-5 candidate"):
            STUDY.validate_corpus_review(review, review_sha, r3_freeze, candidate, tasks)

    @unittest.skipUnless(CORPUS_DIR.is_dir(), "campaign R5 corpus is outside a normal checkout")
    def test_real_r5_verifier_detects_companion_and_source_tampering(self):
        with tempfile.TemporaryDirectory(prefix="real-bash-r5-tamper-") as temp:
            temp_root = Path(temp)
            companion_copy = temp_root / "companion"
            shutil.copytree(CORPUS_DIR, companion_copy, copy_function=link_or_copy)
            companion = companion_copy / "README.md"
            original_mode = companion.stat().st_mode & 0o777
            raw = companion.read_bytes()
            companion.unlink()
            companion.write_bytes(raw + b"tamper")
            companion.chmod(original_mode)
            with self.assertRaisesRegex(STUDY.StudyError, "companion bytes do not match"):
                STUDY.verify_corpus_candidate(companion_copy)

        with tempfile.TemporaryDirectory(prefix="real-bash-r5-source-tamper-") as temp:
            source_copy = Path(temp) / "source"
            shutil.copytree(CORPUS_DIR, source_copy, copy_function=link_or_copy)
            manifest = json.loads((source_copy / "manifest.json").read_text(encoding="utf-8"))
            source = source_copy / manifest["scripts"][0]["source"]["copy_path"]
            original_mode = source.stat().st_mode & 0o777
            raw = source.read_bytes()
            source.unlink()
            source.write_bytes(raw + b"tamper")
            source.chmod(original_mode)
            with self.assertRaisesRegex(STUDY.StudyError, "candidate source-copy content"):
                STUDY.verify_corpus_candidate(source_copy)

    @unittest.skipUnless(CORPUS_DIR.is_dir(), "campaign R5 corpus is outside a normal checkout")
    def test_r5_snapshot_rejects_unlisted_files_and_symbolic_links(self):
        with tempfile.TemporaryDirectory(prefix="real-bash-r5-unlisted-") as temp:
            copied = Path(temp) / "corpus"
            shutil.copytree(CORPUS_DIR, copied, copy_function=link_or_copy)
            extra = copied / "sources" / "bin" / "unlisted-stale-source"
            extra.write_bytes(b"unlisted test artifact\n")
            with self.assertRaisesRegex(STUDY.StudyError, "unlisted=.*unlisted-stale-source"):
                STUDY.verify_corpus_candidate(copied)

        with tempfile.TemporaryDirectory(prefix="real-bash-r5-symlink-") as temp:
            copied = Path(temp) / "corpus"
            shutil.copytree(CORPUS_DIR, copied, copy_function=link_or_copy)
            link = copied / "sources" / "bin" / "unlisted-link"
            try:
                link.symlink_to("missing-target")
            except OSError as error:
                self.skipTest(f"cannot create a symbolic-link fixture: {error}")
            with self.assertRaisesRegex(STUDY.StudyError, "corpus tree contains a symbolic link"):
                STUDY.verify_corpus_candidate(copied)

    def test_plan_is_balanced_deterministic_and_keeps_clusters_distinct_from_tasks(self):
        frozen_roster = roster()
        roster_sha = encoded_hash(frozen_roster)
        first = STUDY.make_run_plan(frozen_roster, roster_sha, ARM_IDS, 20261015)
        second = STUDY.make_run_plan(frozen_roster, roster_sha, ARM_IDS, 20261015)
        self.assertEqual(first, second)
        self.assertEqual(len(first["cells"]), 300)
        self.assertEqual(len({cell["task_id"] for cell in first["cells"]}), 100)
        self.assertEqual(len({task["family_id"] for task in frozen_roster["tasks"]}), 20)
        self.assertEqual([cell["dispatch_index"] for cell in first["cells"]], list(range(1, 301)))

    def test_duplicate_keys_are_rejected_in_json_and_jsonl_inputs(self):
        with self.assertRaisesRegex(STUDY.StudyError, "duplicate JSON object key"):
            STUDY.strict_json_loads(b'{"outer":{"x":1,"x":2}}', "fixture JSON")
        _, roster_sha, _, _, _, freeze, freeze_sha, tasks, plan_by_dispatch, _ = make_records()
        with tempfile.TemporaryDirectory(prefix="rb100-jsonl-test-") as temp:
            result_path = Path(temp) / "rows.jsonl"
            result_path.write_text('{"schema":"first","schema":"second"}\n', encoding="utf-8")
            with self.assertRaisesRegex(STUDY.StudyError, "duplicate JSON object key"):
                STUDY.read_trial_rows(result_path, tasks, freeze, freeze_sha, roster_sha, plan_by_dispatch, production=False)
            result_path.write_bytes(b"\xff\n")
            with self.assertRaisesRegex(STUDY.StudyError, "not valid UTF-8 JSONL"):
                STUDY.read_trial_rows(result_path, tasks, freeze, freeze_sha, roster_sha, plan_by_dispatch, production=False)

    def test_roster_requires_the_reserved_completion_id_and_operation(self):
        frozen_roster, _, _, _, _, _, freeze, _, _, _ = fixture_environment()
        for replacement in (None, "process_io_exact"):
            changed = json.loads(json.dumps(frozen_roster))
            if replacement is None:
                changed["tasks"][0]["criteria"].pop("all_cases_observed")
            else:
                changed["tasks"][0]["criteria"]["all_cases_observed"] = replacement
            changed_sha = encoded_hash(changed)
            changed_freeze = dict(freeze, task_roster_sha256=changed_sha)
            with self.assertRaisesRegex(STUDY.StudyError, "reserved all_cases_observed ID/op"):
                STUDY.validate_roster(changed, changed_freeze, changed_sha)

    def test_model_snapshot_status_and_value_must_match_across_arms(self):
        _, _, arms, arms_sha, _, _, freeze, _, _, _ = fixture_environment()
        changed = json.loads(json.dumps(arms))
        changed["arms"][ARM_IDS[0]]["model_snapshot_status"] = "recorded"
        changed["arms"][ARM_IDS[0]]["model_snapshot"] = "snapshot-a"
        changed["arms"][ARM_IDS[1]]["model_snapshot_status"] = "recorded"
        changed["arms"][ARM_IDS[1]]["model_snapshot"] = "snapshot-b"
        changed_sha = encoded_hash(changed)
        changed_freeze = dict(freeze, arm_map_sha256=changed_sha)
        with self.assertRaisesRegex(STUDY.StudyError, "snapshot, generation settings"):
            STUDY.validate_arm_map(changed, changed_sha, changed_freeze)

    def test_attempt_archive_recomputes_outcomes_from_frozen_oracles_and_raw_bytes(self):
        with tempfile.TemporaryDirectory(prefix="rb100-archive-test-") as temp:
            values = archive_fixture(Path(temp))
            archive_root, oracle_task, task, freeze, roster_sha, record, attempt_row, _ = values
            outcome = STUDY.verify_attempt_archive(
                attempt_row, record, task, oracle_task, archive_root, freeze, roster_sha,
                line_number=1, attempt_number=1,
            )
            self.assertEqual(outcome, {
                "oracle_status": "pass",
                "criteria_results": {
                    "process_contract": True,
                    "filesystem_contract": True,
                    "mock_contract": True,
                    "all_cases_observed": True,
                },
            })

            false_claim = json.loads(json.dumps(attempt_row))
            false_claim["oracle_status"] = "fail"
            with self.assertRaisesRegex(STUDY.StudyError, "oracle_status differs from archive-derived"):
                STUDY.verify_attempt_archive(
                    false_claim, record, task, oracle_task, archive_root, freeze, roster_sha,
                    line_number=1, attempt_number=1,
                )
            false_criterion = json.loads(json.dumps(attempt_row))
            false_criterion["criteria_results"]["filesystem_contract"] = False
            with self.assertRaisesRegex(STUDY.StudyError, "row criteria are self-reported incorrectly"):
                STUDY.verify_attempt_archive(
                    false_criterion, record, task, oracle_task, archive_root, freeze, roster_sha,
                    line_number=1, attempt_number=1,
                )

    def test_timeout_fails_completion_even_when_captured_files_match(self):
        with tempfile.TemporaryDirectory(prefix="rb100-timeout-archive-") as temp:
            limits = dict(STUDY.RUNNER.DEFAULT_LIMITS, timeout_seconds=1)
            candidate_source = (
                "from pathlib import Path\nimport sys\nimport time\n"
                "Path('out.txt').write_bytes(b'ok\\n')\n"
                "sys.stdout.buffer.write(b'ok\\n'); sys.stdout.buffer.flush()\n"
                "time.sleep(30)\n"
            )
            values = archive_fixture(Path(temp), candidate_source=candidate_source, limits=limits)
            archive_root, oracle_task, task, freeze, roster_sha, record, attempt_row, _ = values
            observation = json.loads((archive_root / attempt_row["archive_ref"] / "observation.json").read_text(encoding="utf-8"))
            self.assertEqual(observation["cases"][0]["process_status"], "timeout")
            self.assertTrue(observation["cases"][0]["criteria_results"]["filesystem_final_exact"])
            outcome = STUDY.verify_attempt_archive(
                attempt_row, record, task, oracle_task, archive_root, freeze, roster_sha,
                line_number=1, attempt_number=1,
            )
            self.assertEqual(outcome["oracle_status"], "fail")
            self.assertFalse(outcome["criteria_results"]["all_cases_observed"])
            self.assertFalse(outcome["criteria_results"]["process_contract"])

    def test_attempt_archive_rejects_changed_candidate_bytes(self):
        with tempfile.TemporaryDirectory(prefix="rb100-archive-tamper-") as temp:
            values = archive_fixture(Path(temp))
            archive_root, oracle_task, task, freeze, roster_sha, record, attempt_row, _ = values
            candidate = archive_root / attempt_row["archive_ref"] / attempt_row["artifact_ref"]
            candidate.write_bytes(candidate.read_bytes() + b"# changed after observation\n")
            with self.assertRaisesRegex(STUDY.StudyError, "differs from the attempt manifest"):
                STUDY.verify_attempt_archive(
                    attempt_row, record, task, oracle_task, archive_root, freeze, roster_sha,
                    line_number=1, attempt_number=1,
                )

    def test_import_reports_paired_correctness_repairs_runtime_and_missing_cells(self):
        roster_doc, roster_sha, arms, plan, plan_sha, freeze, freeze_sha, tasks, plan_by_dispatch, records = make_records()
        for row in records:
            STUDY.validate_trial(row, tasks, freeze, freeze_sha, roster_sha, plan_by_dispatch)
        report = STUDY.summarize(roster_doc, roster_sha, freeze, freeze_sha, plan, plan_sha, tasks, records, arms["arms"])
        self.assertEqual(report["expected_cells"], 300)
        self.assertEqual(report["observed_cells"], 6)
        self.assertEqual(report["missing_cells"], 294)
        bash = report["paired_analysis"]["execute_program_vs_bash"]
        self.assertEqual(bash["matched_behavior_pairs"], 2)
        self.assertEqual(bash["correctness"]["execute_wins"], 1)
        self.assertEqual(bash["repair_efficiency"]["mean_execute_minus_comparator_repairs"], -1)
        self.assertEqual(bash["runtime_on_matched_functional_passes"]["matched_pairs"], 1)
        self.assertEqual(bash["runtime_on_matched_functional_passes"]["median_paired_execute_minus_comparator_ms"], 5)
        self.assertEqual(len(bash["paired_task_evidence"]), 100)
        self.assertEqual(bash["missing_cell_task_ids"][:2], ["script_002", "script_003"])
        self.assertEqual(bash["family_counts"]["matched_behavior_pairs"]["cluster_00"], 2)
        self.assertEqual(bash["runtime_on_matched_functional_passes"]["omission_task_ids_by_reason"]["comparator_final_artifact_failed"], ["script_001"])
        self.assertEqual(report["claim"], "none")
        bash_arm = next(row for row in report["arms"] if row["blind_arm_id"] == ARM_IDS[0])
        self.assertEqual(bash_arm["final_oracle_status_counts"], {"fail": 1, "pass": 1})
        self.assertEqual(bash_arm["final_passes_over_expected_tasks"], 0.01)
        self.assertEqual(bash_arm["final_success_rate_among_behavioral_outcomes"], 0.5)

    def test_partial_observation_can_establish_failure_but_not_infrastructure_success(self):
        _, roster_sha, _, _, _, freeze, freeze_sha, tasks, plan_by_dispatch, records = make_records()
        failed = json.loads(json.dumps(records[0]))
        failed["attempts"] = [attempt("generate", "fail", {"runs": False, "state_exact": None})]
        failed["runtime"] = {
            "status": "not_measured_functional_failure",
            "timing_case_set_sha256": "5" * 64,
            "warmup_runs": None,
            "repetitions_ms": [],
        }
        STUDY.validate_trial(failed, tasks, freeze, freeze_sha, roster_sha, plan_by_dispatch)

        incomplete = json.loads(json.dumps(records[0]))
        incomplete["attempts"] = [attempt("generate", "infrastructure_error", {"runs": True, "state_exact": None})]
        incomplete["runtime"] = {
            "status": "not_measured_incomplete_outcome",
            "timing_case_set_sha256": "5" * 64,
            "warmup_runs": None,
            "repetitions_ms": [],
        }
        incomplete["review"] = {
            "status": "pending", "ratings": [], "adjudication": None, "score_locked_utc": None,
        }
        STUDY.validate_trial(incomplete, tasks, freeze, freeze_sha, roster_sha, plan_by_dispatch)

        mislabeled = json.loads(json.dumps(incomplete))
        mislabeled["attempts"] = [attempt("generate", "infrastructure_error", {"runs": False, "state_exact": None})]
        with self.assertRaisesRegex(STUDY.StudyError, "infrastructure outcome"):
            STUDY.validate_trial(mislabeled, tasks, freeze, freeze_sha, roster_sha, plan_by_dispatch)

    def test_duplicate_source_file_cannot_be_counted_as_a_second_task(self):
        frozen_roster, _, _, _, _, _, freeze, _, _, _ = fixture_environment()
        repeated = json.loads(json.dumps(frozen_roster))
        repeated["tasks"][1]["source_path"] = repeated["tasks"][0]["source_path"]
        repeated_sha = encoded_hash(repeated)
        adjusted_freeze = dict(freeze, task_roster_sha256=repeated_sha)
        with self.assertRaisesRegex(STUDY.StudyError, "source file occurs more than once"):
            STUDY.validate_roster(repeated, adjusted_freeze, repeated_sha)

    def test_roster_accepts_pinned_sha1_git_commit_ids(self):
        frozen_roster, _, _, _, _, _, freeze, _, _, _ = fixture_environment()
        sha1_roster = json.loads(json.dumps(frozen_roster))
        for task in sha1_roster["tasks"]:
            task["source_commit"] = "c" * 40
        sha1_roster_hash = encoded_hash(sha1_roster)
        adjusted_freeze = dict(freeze, task_roster_sha256=sha1_roster_hash)
        parsed_tasks = STUDY.validate_roster(sha1_roster, adjusted_freeze, sha1_roster_hash)
        self.assertEqual(len(parsed_tasks), 100)
        self.assertEqual({task["source_commit"] for task in parsed_tasks.values()}, {"c" * 40})

    def test_result_cannot_claim_a_dispatch_outside_the_frozen_300_cells(self):
        _, roster_sha, _, _, _, freeze, freeze_sha, tasks, plan_by_dispatch, records = make_records()
        broken = json.loads(json.dumps(records[0]))
        broken["dispatch_index"] = 301
        with self.assertRaisesRegex(STUDY.StudyError, "dispatch_index must reference"):
            STUDY.validate_trial(broken, tasks, freeze, freeze_sha, roster_sha, plan_by_dispatch)

    def test_corpus_acceptance_binds_the_exact_candidate_and_resolves_exposure_adjacency(self):
        _, _, _, _, _, _, freeze, _, tasks, _ = fixture_environment()
        candidate = {
            "candidate_revision": 5,
            "manifest_sha256": STUDY.R5_CANDIDATE["manifest_sha256"],
            "receipt_sha256": STUDY.R5_CANDIDATE["receipt_sha256"],
            "source_map_sha256": STUDY.R5_CANDIDATE["source_map_sha256"],
            "corpus_acceptance": "pending_not_inferred_by_local_integrity_verification",
            "candidate_task_ids": sorted(tasks),
            "companion_sha256": {"exposure-adjacency.json": "5" * 64},
        }
        review = {
            "schema": "execute.ai-productivity.real-bash-100-corpus-review/1",
            "decision": "accepted_held_out",
            "reviewed_utc": "2026-10-15T12:00:00Z",
            "reviewer_id": "independent-reviewer",
            "independence": {"not_corpus_curator": True, "not_programmer_operator": True},
            "corpus_manifest_sha256": candidate["manifest_sha256"],
            "corpus_receipt_sha256": candidate["receipt_sha256"],
            "candidate_source_count": 100,
            "accepted_task_ids": sorted(tasks),
            "exact_source_overlap_count": 0,
            "license_status": "accepted",
            "selection_status": "accepted_curated_sample_as_described",
            "family_adjacency_review": {
                "ledger_sha256": "5" * 64,
                "rows_reviewed": 9,
                "material_overlap_rows": 0,
                "unresolved_rows": 0,
            },
            "review_notes_sha256": "8" * 64,
        }
        review_sha = encoded_hash(review)
        checked_freeze = dict(
            freeze,
            corpus_manifest_sha256=candidate["manifest_sha256"],
            corpus_digest_receipt_sha256=candidate["receipt_sha256"],
            corpus_review_sha256=review_sha,
        )
        STUDY.validate_corpus_review(review, review_sha, checked_freeze, candidate, tasks)
        review["family_adjacency_review"]["unresolved_rows"] = 1
        changed_sha = encoded_hash(review)
        checked_freeze["corpus_review_sha256"] = changed_sha
        with self.assertRaisesRegex(STUDY.StudyError, "all prior-exposure family-adjacency rows"):
            STUDY.validate_corpus_review(review, changed_sha, checked_freeze, candidate, tasks)

    def test_unavailable_usage_cannot_be_replaced_with_zero(self):
        roster_doc, roster_sha, _, _, _, freeze, freeze_sha, tasks, plan_by_dispatch, records = make_records()
        broken = json.loads(json.dumps(records[0]))
        broken["attempts"][0]["usage"]["input_tokens"] = 0
        with self.assertRaisesRegex(STUDY.StudyError, "null token counters"):
            STUDY.validate_trial(broken, tasks, freeze, freeze_sha, roster_sha, plan_by_dispatch)

    def test_repair_feedback_must_match_the_failed_behavior_criteria(self):
        roster_doc, roster_sha, _, _, _, freeze, freeze_sha, tasks, plan_by_dispatch, records = make_records()
        broken = json.loads(json.dumps(records[1]))
        broken["attempts"][1]["diagnostic_feedback_sent"] = ["runs"]
        with self.assertRaisesRegex(STUDY.StudyError, "failed criterion IDs"):
            STUDY.validate_trial(broken, tasks, freeze, freeze_sha, roster_sha, plan_by_dispatch)

    def test_runtime_is_rejected_for_a_functionally_failing_final_artifact(self):
        _, roster_sha, _, _, _, freeze, freeze_sha, tasks, plan_by_dispatch, records = make_records()
        broken = json.loads(json.dumps(records[3]))
        broken["runtime"] = {
            "status": "measured", "timing_case_set_sha256": "5" * 64,
            "warmup_runs": 1, "repetitions_ms": [9] * 10,
        }
        with self.assertRaisesRegex(STUDY.StudyError, "only for a final artifact passing"):
            STUDY.validate_trial(broken, tasks, freeze, freeze_sha, roster_sha, plan_by_dispatch)

    def test_blind_arm_map_cannot_be_opened_before_submitted_reviews_are_locked(self):
        roster_doc, roster_sha, arms, plan, plan_sha, freeze, freeze_sha, tasks, plan_by_dispatch, records = make_records()
        records[0]["review"] = {"status": "pending", "ratings": [], "adjudication": None, "score_locked_utc": None}
        with self.assertRaisesRegex(STUDY.StudyError, "before all submitted artifacts"):
            STUDY.summarize(roster_doc, roster_sha, freeze, freeze_sha, plan, plan_sha, tasks, records, arms["arms"])


if __name__ == "__main__":
    unittest.main()
