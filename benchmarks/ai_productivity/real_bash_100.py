#!/usr/bin/env python3
"""Offline schedule creation and evidence import for the focused 100-script study.

This module never calls a provider, executes candidate code, or opens oracle
references. It checks immutable commitments and summarizes independently
collected JSON evidence. Test fixtures are synthetic and are not study rows.
"""

from __future__ import annotations

import argparse
import base64
import hashlib
import importlib.util
import json
import os
import random
import re
import statistics
import stat
import sys
import uuid
from collections import Counter, defaultdict
from datetime import datetime
from pathlib import Path, PurePosixPath
from typing import Any


HERE = Path(__file__).resolve().parent
PROTOCOL_R3 = HERE / "preregistration.real-bash-100.json"
PROTOCOL_R5 = HERE / "preregistration.real-bash-100-r5.json"
PROTOCOL = PROTOCOL_R5
PROTOCOL_R5_SHA256 = "032e7d854764ab55b286273af31a13440f6ffc991c938fcfc27237bb550d6187"
RUBRIC = HERE / "real-bash-100-rubric.json"
HASH_RE = re.compile(r"^[a-f0-9]{64}$")
ID_RE = re.compile(r"^[A-Za-z0-9_.-]{1,100}$")
ARM_RE = re.compile(r"^arm_[a-z0-9]{8,32}$")
CRITERION_RE = re.compile(r"^[a-z][a-z0-9_]{1,63}$")
VARIANTS = {"bash", "nushell", "execute_program"}
HASHED_FREEZE_FIELDS = (
    "protocol_sha256", "rubric_sha256", "corpus_review_sha256", "corpus_manifest_sha256",
    "corpus_digest_receipt_sha256", "task_roster_sha256", "task_package_sha256",
    "oracle_package_sha256", "timing_case_package_sha256", "runner_source_sha256",
    "runner_image_sha256", "language_reference_bundle_sha256", "arm_map_sha256",
    "tool_configuration_sha256", "randomized_run_plan_sha256",
)
FREEZE_REQUIRED_FIELDS = {
    "schema", "status", "frozen_utc", "randomization_seed", "comparison_block",
    "corpus_review_status", *HASHED_FREEZE_FIELDS,
}
RESULT_FREEZE_FIELDS = {
    "protocol_sha256": "protocol_sha256",
    "corpus_review_sha256": "corpus_review_sha256",
    "corpus_manifest_sha256": "corpus_manifest_sha256",
    "corpus_digest_receipt_sha256": "corpus_digest_receipt_sha256",
    "task_roster_sha256": "task_roster_sha256",
    "task_package_sha256": "task_package_sha256",
    "oracle_package_sha256": "oracle_package_sha256",
    "timing_case_package_sha256": "timing_case_package_sha256",
    "runner_source_sha256": "runner_source_sha256",
    "runner_image_sha256": "runner_image_sha256",
    "language_reference_bundle_sha256": "language_reference_bundle_sha256",
    "arm_map_sha256": "arm_map_sha256",
    "tool_configuration_sha256": "tool_configuration_sha256",
    "randomized_run_plan_sha256": "randomized_run_plan_sha256",
}
RESULT_KEYS = {
    "schema", "trial_type", "run_id", "session_id", "task_id", "blind_arm_id",
    "replicate", "dispatch_index", "freeze_sha256", *RESULT_FREEZE_FIELDS,
    "attempts", "runtime", "review",
}
ATTEMPT_KEYS = {
    "stage", "artifact_sha256", "artifact_ref", "started_utc", "conductor_elapsed_ms",
    "archive_ref", "archive_manifest_ref", "archive_manifest_sha256", "observation_sha256",
    "elapsed_basis", "usage", "tool_calls", "cost", "oracle_status",
    "criteria_results", "diagnostic_feedback_sent",
}
RATING_DIMENSIONS = ("requirement_coverage", "readability", "changeability")
CRITERION_OPERATIONS = {
    "process_io_exact", "filesystem_final_exact", "mock_command_effects_exact", "all_cases_observed",
}
R5_FRAME = {
    "direct_bin_inventory_files": 444,
    "bash_sources": 439,
    "non_bash_files": 5,
    "exact_exposure_exclusions": 20,
    "material_adjacency_exclusions": 14,
    "purpose_only_campaign_scope_exclusions": 8,
    "eligible_before_curation": 397,
    "selected_distinct_sources": 100,
    "unselected_eligible_bash_sources": 297,
    "functional_family_count": 26,
    "fixture_feasibility_quotas": {
        "command_fixture": 30,
        "filesystem_fixture": 40,
        "desktop_cli_simulation": 30,
    },
    "family_counts": {
        "browser-url-utility": 1,
        "configuration-channel": 3,
        "configuration-font": 2,
        "configuration-theme-boot": 6,
        "default-application-selection": 1,
        "desktop-branding": 3,
        "desktop-capture": 3,
        "desktop-device-controls": 19,
        "desktop-notification-session": 7,
        "developer-tooling": 3,
        "git-remote-validation": 1,
        "hardware-model-probe": 3,
        "menu-file-selection": 1,
        "network-tools": 2,
        "package-lifecycle": 17,
        "plugin-management": 3,
        "service-management": 2,
        "shell-command-discovery": 2,
        "shell-development-environment": 4,
        "storage-and-drives": 2,
        "system-provisioning": 6,
        "text-and-formatting": 3,
        "user-config-and-state": 2,
        "user-hook-dispatch": 1,
        "weather-output": 1,
        "webcam-capture": 2,
    },
    "low_complexity_count": 16,
    "size_bands": {
        "large_151_300_lines": 9,
        "medium_61_150_lines": 23,
        "small_16_60_lines": 52,
        "tiny_1_15_lines": 16,
    },
    "analytic_strata": {"workflow_cli": 100},
    "independent_family_claim": False,
}
R5_CANDIDATE = {
    "corpus_id": "omarchy-v4.0.4-bin-bash-candidate-100-v5",
    "repository_url": "https://github.com/omacom/omarchy",
    "release_tag": "v4.0.4",
    "commit": "c668141e9c42b13c80c9ca4ea108e11708c5e8a5",
    "source_archive_sha256": "8cc6b1d9d903c606600395e3b3d80ad9f15bd3ab78f0f19b0f49041b1df82013",
    "inventory_sha256": "ef6be335479f9f09f5f69ad8cd838cdca724a49da630eb7b3532f42e2a3cc2a4",
    "manifest_sha256": "10b08d64a560312989eef554f6ba5eeb44ec0f8952c15dca1aac07dd8c5a3cfe",
    "receipt_sha256": "b969d0e76d65fa0f839b910205f35ce26555277c24a4dca4b16472c4fb443afb",
    "source_map_sha256": "939971e1ec4417958a4fea3b0b3d63a4f40b72a136191d28641591ac79841ad3",
    "snapshot_path": "history/revision5/SNAPSHOT.json",
    "snapshot_sha256": "4cd93f30a2d0593a1522c2f03859f0fb768827ffb9281b4c11d5a621c56893d4",
}
R5_COMPANIONS = {
    "candidate-exclusion-ledger.json", "exclusions.json", "exposure-adjacency.json",
    "exposure-log.jsonl", "similarity-audit.json", "README.md", "independent-review.json",
    "revision2-changes.json", "revision3-changes.json", "revision3-independent-review.json",
    "revision3-proposal.json", "revision4-changes.json", "revision4-independent-review.json",
    "revision4-proposal.json", "revision4-exposure-addendum.json", "revision5-changes.json",
    "revision5-proposal.json", "revision5-proposal-amendment.json",
    "revision5-exposure-closure-screen.json", "revision5-literal-closure-intersections.json",
    "revision5-bundled-proposal.json", "revision5-full-candidate-token-screen.json",
    "revision5-final-screen-binding.json", "revision5-curation-amendment.json",
    "revision5-bundled-independent-review.json", "revision5-curation-exposure-log.jsonl",
    "revision5-curation-exposure-log-addendum.jsonl", "receipt.json",
}
R5_HISTORY = {
    2: {
        "snapshot_path": "history/revision2/SNAPSHOT.json",
        "snapshot_sha256": "d2b9425269da53838b9dd814881fbb9c76162c2977a3a099e8db24352247f976",
        "manifest_sha256": "50008376769e612e8e66baaebd5ec9bf2488e20e1d8d483d19e1e7f7e48c841a",
        "receipt_sha256": "6d32ae1fed253d4e2f6a676e759192deb8417a6f136971f0638eafdd36b6abf6",
        "source_map_sha256": "7ce96d3c9b05ebb298adade14854bfea4002a7082200e4cbdd6c4f90401933d9",
    },
    3: {
        "snapshot_path": "history/revision3/SNAPSHOT.json",
        "snapshot_sha256": "66f8dfb8c4137fd529d45655095e01cf84c7fae12caec91e598e23835a8c2642",
        "manifest_sha256": "97e0d208890ea207f6513d6b5156b126e22bbd5153ad4aa63447fb8f58794949",
        "receipt_sha256": "c8ce7314a1373461ebd3f9373bc3ced417f5739897605db05d32b72f06a5a689",
        "source_map_sha256": "f3fff1f25357388ed5ffdd9de980420320cdd063430a23d0b64abd0cecc27406",
    },
    4: {
        "snapshot_path": "history/revision4/SNAPSHOT.json",
        "snapshot_sha256": "4070f8f5939ba0d1306631de0d0b5db4cd3bc2d03784856a7609d93a6a69202a",
        "manifest_sha256": "67c8c69fae42f202eedbfe9ae5f694b6d97063cbbe998ff633b582856b2ade71",
        "receipt_sha256": "6521663758359b5b9c4b3c42f3296b86293e5f3566de442994974aeffc63543c",
        "source_map_sha256": "3d2d70ed37cced0af2072e18dc8f5e0fec26a83244c0e6629acc1d47065566a8",
    },
    5: R5_CANDIDATE,
}
R5_SOURCE_ARCHIVE_SHA256 = "8cc6b1d9d903c606600395e3b3d80ad9f15bd3ab78f0f19b0f49041b1df82013"
R5_INVENTORY_SHA256 = "ef6be335479f9f09f5f69ad8cd838cdca724a49da630eb7b3532f42e2a3cc2a4"
R5_PARENT_INTEGRITY_SHA256 = "09b7f5fab30b68b38cca5dc5c8037e511fc2dc1a2788c19a2c044c4cb169ee58"
R5_LINEAGE_ARTIFACTS = {
    "parent_integrity_sha256": "09b7f5fab30b68b38cca5dc5c8037e511fc2dc1a2788c19a2c044c4cb169ee58",
    "parent_integrity_path": "evidence/corpus-revision5-parent-integrity.json",
    "revision3_changes_sha256": "d5e1ad09cf7ffe295dc408a8d2424e1ea373872da7fa45b0e23a983d6f5e45cc",
    "revision3_source_review_sha256": "ce81c79ddf58d43d8f6ae5bda3a0770f7a72a24ce91463fa70f9c12b13255436",
    "revision4_changes_sha256": "c06282c6c0e2c58f058dd4291b9750f709feabb2387c92e84b0430199b7fe39f",
    "revision4_source_review_sha256": "fc3c52462eca202ac6618ece58398d22bc747e7bed5bbcfd027f0df3eebf3f19",
    "revision5_changes_sha256": "068283c9873346549790e07216ccdc84404bf91cba2b9b0c165e503c70aae65e",
    "revision5_full_candidate_screen_sha256": "a214749020c178bee43fcc06fdd2b78eb8136a4e6c330a8dd04be250e1d8508b",
    "revision5_proposal_sha256": "910f4f5899cb9f4036507e244272316f58c70aeaf74fb9a7988946f64e9ec442",
    "revision5_screen_binding_sha256": "f9b0f64ae06431444e562a7f0da0a2601e6e5b3a81508e225e9fd2f493970d60",
    "revision5_source_review_sha256": "f853b698920ec6c6622bb2db1d3f2ebcd34ab3d0c53bc80dd1671eca9c54a10d",
}
R5_CHANGE_SHA256 = "068283c9873346549790e07216ccdc84404bf91cba2b9b0c165e503c70aae65e"
R5_REVIEW_SHA256 = "f853b698920ec6c6622bb2db1d3f2ebcd34ab3d0c53bc80dd1671eca9c54a10d"
R5_REPLACEMENTS = [
    {
        "task_id": "omarchy-001", "removed_script_id": "omarchy-audio-output-switch",
        "removed_sha256": "d696d6fc6627338453524560a80c45d00a037b0c2356e43d43673a5bcbf9d165",
        "added_script_id": "omarchy-hyprland-focus-app",
        "added_sha256": "391d3f4d6b9e791265e1a64031e5a96f18bde29577fa9f09640f51c5c8c20c2c",
        "testability_stratum": "desktop_cli_simulation",
    },
    {
        "task_id": "omarchy-022", "removed_script_id": "omarchy-cmd-terminal-cwd",
        "removed_sha256": "64ef9d88a1e462f2e44522d1e0c9c436537237de92b360892c9d4ed1e4521f0f",
        "added_script_id": "omarchy-update-confirm",
        "added_sha256": "ed94d094d903513c24d66c5d90267276e6e02e54df5b092c47ab968f05d8dbff",
        "testability_stratum": "command_fixture",
    },
    {
        "task_id": "omarchy-065", "removed_script_id": "omarchy-powerprofiles-list",
        "removed_sha256": "3d2e4b3d5ad974aa78c97a653d3fe937bd263063369a57f14918748502d1f609",
        "added_script_id": "omarchy-hw-hybrid-gpu",
        "added_sha256": "fcbfa66ba577110e11c73c8d18501e4c130c60b08d1256d48f8bb933cb50a70c",
        "testability_stratum": "desktop_cli_simulation",
    },
    {
        "task_id": "omarchy-091", "removed_script_id": "omarchy-tui-remove",
        "removed_sha256": "6a5330eac6f9bdf4c2680c2d87e62e13f2bac0e45db66707872fee51cd10dd5e",
        "added_script_id": "omarchy-theme-colors-from-alacritty",
        "added_sha256": "b7fef9bcf13ae32fc82e366fa17191a68181b8e96a21fd4cf3cb418492fd4e9e",
        "testability_stratum": "filesystem_fixture",
    },
]


def _load_runner_module():
    path = HERE / "real_bash_100_runner.py"
    spec = importlib.util.spec_from_file_location("real_bash_100_runner_for_import", path)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"cannot load companion observation runner from {path}")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


RUNNER = _load_runner_module()


class StudyError(ValueError):
    """Input is incomplete or inconsistent with the focused study protocol."""


def _reject_duplicate_json_keys(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            raise StudyError(f"duplicate JSON object key {key!r}")
        result[key] = value
    return result


def _reject_json_constant(value: str) -> None:
    raise StudyError(f"non-standard JSON constant {value!r} is not permitted")


def strict_json_loads(raw: bytes | str, label: str) -> Any:
    try:
        return json.loads(
            raw,
            object_pairs_hook=_reject_duplicate_json_keys,
            parse_constant=_reject_json_constant,
        )
    except (UnicodeDecodeError, json.JSONDecodeError, StudyError) as error:
        raise StudyError(f"{label} is not unambiguous strict JSON: {error}") from error


def canonical_json_bytes(value: Any) -> bytes:
    try:
        return json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=False, allow_nan=False).encode("utf-8")
    except (TypeError, ValueError) as error:
        raise StudyError(f"value cannot be encoded as canonical JSON: {error}") from error


def digest_bytes(raw: bytes) -> str:
    return hashlib.sha256(raw).hexdigest()


def digest_file(path: Path) -> str:
    return digest_bytes(path.read_bytes())


def read_json(path: Path, label: str) -> tuple[Any, bytes, str]:
    try:
        raw = path.read_bytes()
    except OSError as error:
        raise StudyError(f"cannot read {label} {path}: {error}") from error
    value = strict_json_loads(raw, label)
    return value, raw, digest_bytes(raw)


def _corpus_file(root: Path, relative: Any, label: str) -> Path:
    rel = _relative_source_path(relative, label)
    root = root.resolve(strict=True)
    candidate = root
    for part in PurePosixPath(rel).parts:
        candidate = candidate / part
        if candidate.is_symlink():
            raise StudyError(f"{label} may not traverse a symbolic link")
    try:
        resolved = candidate.resolve(strict=True)
        resolved.relative_to(root)
    except (OSError, ValueError) as error:
        raise StudyError(f"{label} must resolve to a file inside the corpus directory") from error
    if not resolved.is_file():
        raise StudyError(f"{label} must name a regular file")
    return resolved


def _physical_lf_lines(raw: bytes) -> int:
    if not raw:
        return 0
    return raw.count(b"\n") + (0 if raw.endswith(b"\n") else 1)


def _require_bytes_hash(raw: bytes, expected_sha256: str, label: str) -> str:
    _hash(expected_sha256, f"{label} expected SHA-256")
    actual = digest_bytes(raw)
    if actual != expected_sha256:
        raise StudyError(f"{label} bytes do not match their frozen SHA-256")
    return actual


def _validate_r5_protocol_lineage(protocol: dict[str, Any]) -> dict[str, Any]:
    source = protocol.get("source_corpus")
    history = source.get("r5_lineage") if isinstance(source, dict) else None
    if not isinstance(history, dict):
        raise StudyError("R5 protocol must bind the preserved R2/R3/R4/R5 history chain")
    for revision, pinned in R5_HISTORY.items():
        if (
            history.get(f"revision{revision}_snapshot_path") != pinned["snapshot_path"]
            or history.get(f"revision{revision}_snapshot_sha256") != pinned["snapshot_sha256"]
        ):
            raise StudyError(f"R5 protocol revision-{revision} history snapshot binding is stale")
        for field in ("manifest_sha256", "receipt_sha256", "source_map_sha256"):
            expected = pinned.get(field)
            present = history.get(f"revision{revision}_{field}")
            if expected is not None and present != expected:
                raise StudyError(f"R5 protocol revision-{revision} {field} binding is stale")
    for field, expected in R5_LINEAGE_ARTIFACTS.items():
        if history.get(field) != expected:
            raise StudyError(f"R5 protocol lineage artifact binding is stale: {field}")
    linkage = history.get("purpose_exclusion_linkage")
    if (
        not isinstance(linkage, str)
        or "preserved R3 manifest/revision3-changes.json" not in linkage
        or "do not require active R5 membership" not in linkage
    ):
        raise StudyError("R5 protocol must keep purpose-only replacement IDs explicitly historical")
    return history


def _load_r5_protocol() -> tuple[dict[str, Any], str]:
    protocol, raw, protocol_sha = read_json(PROTOCOL_R5, "R5 focused preregistration amendment")
    _require_bytes_hash(raw, PROTOCOL_R5_SHA256, "R5 protocol amendment")
    if (
        not isinstance(protocol, dict)
        or protocol.get("schema") != "execute.real-bash-preregistration/2"
        or protocol.get("version") != 2
        or protocol.get("status") != "draft_revision5_candidate_unsealed_trials_and_original_behavior_proofs_not_run"
        or protocol.get("supersedes") != "preregistration.real-bash-100.json"
    ):
        raise StudyError("R5 protocol amendment identity or candidate-only status is unsupported")
    source = protocol.get("source_corpus")
    candidate = source.get("r5_candidate_identity") if isinstance(source, dict) else None
    if candidate != {**R5_CANDIDATE, "status": "candidate_unsealed"}:
        raise StudyError("R5 protocol candidate identity differs from the pinned manifest/receipt/source-map/snapshot bytes")
    frame = source.get("r5_frame") if isinstance(source, dict) else None
    if not isinstance(frame, dict) or frame != R5_FRAME:
        raise StudyError("R5 protocol frame differs from the pinned 397-eligible/100-selected/26-family frame")
    history = _validate_r5_protocol_lineage(protocol)
    if digest_file(PROTOCOL_R3) != protocol.get("prior_revision3_history", {}).get("source_protocol_sha256"):
        raise StudyError("R5 amendment does not preserve the exact historical R3 protocol bytes")
    if protocol.get("analysis", {}).get("seed") != 20261015:
        raise StudyError("R5 protocol changed the pre-existing randomized-plan seed without a separate amendment")
    if protocol.get("scoring", {}).get("human_ratings", {}).get("sha256") != digest_file(RUBRIC):
        raise StudyError("R5 protocol rubric hash differs from the unchanged masked-rating rubric")
    return protocol, protocol_sha


def _validate_r5_manifest_frame(
    manifest: dict[str, Any],
    protocol: dict[str, Any],
    manifest_sha256: str,
    receipt_sha256: str,
    source_map_sha256: str | None = None,
) -> None:
    identity = protocol["source_corpus"]["r5_candidate_identity"]
    if (
        manifest_sha256 != identity["manifest_sha256"]
        or manifest.get("corpus_id") != identity["corpus_id"]
        or manifest.get("status") != "candidate_unsealed"
        or manifest.get("proposed_split") != "held_out"
        or manifest.get("selection", {}).get("revision") != 5
    ):
        raise StudyError("R5 candidate manifest is mixed, stale, or no longer candidate_unsealed")
    if receipt_sha256 != identity["receipt_sha256"]:
        raise StudyError("R5 candidate receipt is mixed or stale")
    if source_map_sha256 is not None and source_map_sha256 != identity["source_map_sha256"]:
        raise StudyError("R5 canonical source-map digest is mixed or stale")

    source = manifest.get("source_population", {})
    if (
        source.get("inventory_bash_script_count") != 439
        or source.get("repository_url") != "https://github.com/omacom/omarchy"
        or source.get("release_tag") != "v4.0.4"
        or source.get("commit") != "c668141e9c42b13c80c9ca4ea108e11708c5e8a5"
        or source.get("archive_sha256") != R5_SOURCE_ARCHIVE_SHA256
        or source.get("inventory_sha256") != R5_INVENTORY_SHA256
    ):
        raise StudyError("R5 source population is not the pinned Omarchy v4.0.4 inventory")

    selection = manifest.get("selection", {})
    expected_eligible = {
        "published_bash_inventory_count": 439,
        "known_exact_exposed_sources_removed": 20,
        "material_adjacency_sources_removed": 14,
        "purpose_only_campaign_scope_sources_removed": 8,
        "remaining_before_curation": 397,
    }
    if (
        selection.get("eligible_frame") != expected_eligible
        or selection.get("selected_task_count") != 100
        or selection.get("target_task_count") != 100
        or selection.get("testability_stratum_counts") != R5_FRAME["fixture_feasibility_quotas"]
        or selection.get("size_band_counts") != protocol["source_corpus"]["r5_frame"].get("size_bands")
        or selection.get("low_complexity_count") != protocol["source_corpus"]["r5_frame"].get("low_complexity_count")
    ):
        raise StudyError("R5 manifest selection frame, quotas, or descriptive size metadata is stale")

    expected_families = protocol["source_corpus"]["r5_frame"].get("family_counts")
    scope = manifest.get("statistical_scope", {})
    if (
        not isinstance(expected_families, dict)
        or sum(expected_families.values()) != 100
        or scope.get("distinct_script_task_count") != 100
        or scope.get("functional_family_count") != 26
        or scope.get("family_counts") != expected_families
        or scope.get("independent_repository_family_claim") is not False
        or scope.get("source_repository_count") != 1
        or scope.get("source_release_count") != 1
    ):
        raise StudyError("R5 26-family composition or finite-corpus scope differs from the pinned protocol")


def _validate_historical_r3_scope_linkage(
    r5_scope_rows: list[dict[str, Any]],
    r3_scope_rows: list[dict[str, Any]],
    r3_manifest: dict[str, Any],
    r3_changes: dict[str, Any],
) -> None:
    """Validate R3 replacement links against R3 itself, never R5 membership."""
    if len(r3_scope_rows) != 8 or len(r5_scope_rows) != 8:
        raise StudyError("R5 must preserve the eight R3 purpose-only source exclusions")
    historical_by_path = {row.get("path"): row for row in r3_scope_rows}
    current_by_path = {row.get("path"): row for row in r5_scope_rows}
    if len(historical_by_path) != 8 or set(historical_by_path) != set(current_by_path):
        raise StudyError("R5 purpose-only exclusion paths differ from the preserved R3 ledger")
    r3_selected_ids = {row.get("script_id") for row in r3_manifest.get("scripts", [])}
    if len(r3_selected_ids) != 100:
        raise StudyError("preserved R3 manifest does not contain 100 historical selected source IDs")
    changes_by_removed_path = {}
    for change in r3_changes.get("swaps", []):
        removed, added = change.get("remove"), change.get("add")
        if isinstance(removed, dict) and isinstance(added, dict):
            changes_by_removed_path[removed.get("source_path")] = added.get("script_id")
    replacement_ids = set()
    for path, prior in historical_by_path.items():
        current = current_by_path[path]
        for field in ("sha256", "bytes", "lines", "mode_octal"):
            if current.get(field) != prior.get(field):
                raise StudyError(f"R5 purpose-only exclusion metadata differs from R3 history: {path}")
        replacement_id = prior.get("replacement_script_id")
        if not isinstance(replacement_id, str) or replacement_id in replacement_ids:
            raise StudyError("R3 purpose-only replacement links must be eight unique historical script IDs")
        if replacement_id not in r3_selected_ids or changes_by_removed_path.get(path) != replacement_id:
            raise StudyError(f"R3 purpose-only replacement link does not resolve within the R3 snapshot: {path}")
        replacement_ids.add(replacement_id)
    # These eight IDs name R3's historical replacements. Whether any is still
    # active in R5 is deliberately not a validity condition: R5 identity and
    # membership are validated independently against its own manifest.


def _read_lineage_json(root: Path, relative: str, expected_sha256: str, label: str) -> dict[str, Any]:
    path = _corpus_file(root, relative, label)
    document, raw, _ = read_json(path, label)
    _require_bytes_hash(raw, expected_sha256, label)
    if not isinstance(document, dict):
        raise StudyError(f"{label} must be a JSON object")
    return document


def _verify_r5_snapshot_entries(root: Path, snapshot: dict[str, Any]) -> int:
    """Verify every committed snapshot file and the exact current corpus file set.

    The snapshot descriptor is self-excluded from its own inventory; its bytes
    are separately pinned by the R5 lineage protocol before this function runs.
    No other path is exempt from the snapshot's regular-file inventory.
    """
    files = snapshot.get("files")
    count = snapshot.get("file_count")
    if not isinstance(files, list) or type(count) is not int or len(files) != count or count != 593:
        raise StudyError("R5 snapshot must enumerate its exact 593 preserved regular files")
    seen = set()
    for index, entry in enumerate(files):
        label = f"R5 snapshot.files[{index}]"
        _keys(entry, {"path", "sha256", "bytes", "mode_octal"}, label)
        relative = _relative_source_path(entry["path"], f"{label}.path")
        if relative in seen:
            raise StudyError("R5 snapshot file inventory contains a duplicate path")
        seen.add(relative)
        _hash(entry["sha256"], f"{label}.sha256")
        if type(entry["bytes"]) is not int or entry["bytes"] < 0 or not re.fullmatch(r"[0-7]{4}", entry["mode_octal"]):
            raise StudyError(f"{label} byte count or mode is malformed")
        path = _corpus_file(root, relative, f"{label}.path")
        raw = path.read_bytes()
        _require_bytes_hash(raw, entry["sha256"], f"{label}.path")
        if len(raw) != entry["bytes"]:
            raise StudyError(f"{label}.path byte count differs from the R5 snapshot")
        mode = format(stat.S_IMODE(path.stat().st_mode) & 0o777, "04o")
        if mode != entry["mode_octal"]:
            raise StudyError(f"{label}.path mode differs from the R5 snapshot")
    if "history/revision5/SNAPSHOT.json" in seen:
        raise StudyError("R5 snapshot must not recursively include its own descriptor")
    actual = _enumerate_corpus_regular_files(root)
    self_descriptor = "history/revision5/SNAPSHOT.json"
    if self_descriptor not in actual:
        raise StudyError("R5 snapshot self-excluded descriptor is missing from the corpus tree")
    actual.remove(self_descriptor)
    if actual != seen:
        unlisted = sorted(actual - seen)
        missing = sorted(seen - actual)
        raise StudyError(
            "actual R5 corpus file inventory differs from the committed snapshot "
            f"(unlisted={unlisted[:3]!r}, missing={missing[:3]!r})"
        )
    return len(seen)


def _enumerate_corpus_regular_files(root: Path) -> set[str]:
    """Walk without following links and reject non-regular entries."""
    root = root.resolve(strict=True)
    pending = [root]
    files: set[str] = set()
    while pending:
        directory = pending.pop()
        try:
            entries = list(os.scandir(directory))
        except OSError as error:
            raise StudyError(f"cannot enumerate R5 corpus directory {directory}: {error}") from error
        for entry in entries:
            path = Path(entry.path)
            relative = path.relative_to(root).as_posix()
            try:
                mode = entry.stat(follow_symlinks=False).st_mode
            except OSError as error:
                raise StudyError(f"cannot inspect R5 corpus entry {relative}: {error}") from error
            if stat.S_ISLNK(mode):
                raise StudyError(f"R5 corpus tree contains a symbolic link: {relative}")
            if stat.S_ISDIR(mode):
                pending.append(path)
            elif stat.S_ISREG(mode):
                files.add(relative)
            else:
                raise StudyError(f"R5 corpus tree contains a non-regular entry: {relative}")
    return files


def _validate_r5_lineage(
    root: Path,
    manifest: dict[str, Any],
    receipt: dict[str, Any],
    ledger: dict[str, Any],
    manifest_sha256: str,
    receipt_sha256: str,
    source_map_sha256: str,
    family_counts: Counter[str],
    testability_counts: Counter[str],
) -> dict[str, Any]:
    protocol, _ = _load_r5_protocol()
    _validate_r5_manifest_frame(manifest, protocol, manifest_sha256, receipt_sha256, source_map_sha256)
    candidate = protocol["source_corpus"]["r5_candidate_identity"]

    expected_ledger_counts = {
        "candidate_selected": 100,
        "excluded_campaign_scope": 8,
        "excluded_known_exposure": 20,
        "excluded_material_adjacency": 14,
        "excluded_non_bash": 5,
        "not_selected_in_fixed_size_slice": 297,
    }
    if ledger.get("counts") != expected_ledger_counts:
        raise StudyError("R5 ledger does not match the pinned 20/14/8/397/100/297 frame")
    if dict(sorted(family_counts.items())) != protocol["source_corpus"]["r5_frame"]["family_counts"]:
        raise StudyError("R5 active manifest family counts differ from the 26-family protocol amendment")
    if dict(sorted(testability_counts.items())) != R5_FRAME["fixture_feasibility_quotas"]:
        raise StudyError("R5 fixture labels differ from the descriptive 30/40/30 quotas")

    lineage = protocol["source_corpus"]["r5_lineage"]
    snapshots = {}
    for revision in (2, 3, 4, 5):
        pinned = R5_HISTORY[revision]
        snap = _read_lineage_json(root, pinned["snapshot_path"], pinned["snapshot_sha256"], f"revision-{revision} history snapshot")
        if snap.get("revision") != revision:
            raise StudyError(f"revision-{revision} snapshot content has a mixed revision marker")
        snapshots[revision] = snap
    r3_snapshot, r4_snapshot, r5_snapshot = snapshots[3], snapshots[4], snapshots[5]
    if (
        r5_snapshot.get("schema") != "execute.real-bash-corpus.snapshot/1"
        or r5_snapshot.get("corpus_id") != candidate["corpus_id"]
        or r5_snapshot.get("status") != "immutable_candidate_snapshot_candidate_unsealed"
        or r5_snapshot.get("basis_manifest_sha256") != candidate["manifest_sha256"]
        or r5_snapshot.get("basis_receipt_sha256") != candidate["receipt_sha256"]
        or r5_snapshot.get("source_map_sha256") != source_map_sha256
    ):
        raise StudyError("R5 immutable snapshot does not bind the active candidate bytes")
    if _verify_r5_snapshot_entries(root, r5_snapshot) != r5_snapshot["file_count"]:
        raise StudyError("R5 snapshot inventory did not verify completely")

    r3_manifest = _read_lineage_json(root, "history/revision3/manifest.json", R5_HISTORY[3]["manifest_sha256"], "preserved R3 manifest")
    r3_receipt = _read_lineage_json(root, "history/revision3/receipt.json", R5_HISTORY[3]["receipt_sha256"], "preserved R3 receipt")
    r3_changes = _read_lineage_json(root, "history/revision3/revision3-changes.json", lineage["revision3_changes_sha256"], "preserved R3 change record")
    r3_review = _read_lineage_json(root, "history/revision3/revision3-independent-review.json", lineage["revision3_source_review_sha256"], "preserved R3 source-only review")
    r4_manifest = _read_lineage_json(root, "history/revision4/manifest.json", R5_HISTORY[4]["manifest_sha256"], "preserved R4 manifest")
    r4_receipt = _read_lineage_json(root, "history/revision4/receipt.json", R5_HISTORY[4]["receipt_sha256"], "preserved R4 receipt")
    r4_changes = _read_lineage_json(root, "history/revision4/revision4-changes.json", lineage["revision4_changes_sha256"], "preserved R4 change record")
    r4_review = _read_lineage_json(root, "history/revision4/revision4-independent-review.json", lineage["revision4_source_review_sha256"], "preserved R4 source-only review")
    r5_changes = _read_lineage_json(root, "revision5-changes.json", R5_CHANGE_SHA256, "R5 change record")
    r5_review = _read_lineage_json(root, "revision5-bundled-independent-review.json", R5_REVIEW_SHA256, "R5 source-only review")

    r2 = snapshots[2]
    if (
        r2.get("schema") != "execute.real-bash-corpus.history-snapshot/1"
        or r2.get("corpus_id") != "omarchy-v4.0.4-bin-bash-candidate-100-v2"
        or r2.get("manifest_sha256") != R5_HISTORY[2]["manifest_sha256"]
        or r2.get("receipt_sha256") != R5_HISTORY[2]["receipt_sha256"]
        or r2.get("source_map_sha256") != R5_HISTORY[2]["source_map_sha256"]
    ):
        raise StudyError("R3's preserved R2 predecessor snapshot has mixed identity or digests")
    if (
        r3_snapshot.get("schema") != "execute.real-bash-corpus.snapshot/1"
        or r3_snapshot.get("corpus_id") != "omarchy-v4.0.4-bin-bash-candidate-100-v3"
        or r3_snapshot.get("manifest_sha256") != R5_HISTORY[3]["manifest_sha256"]
        or r3_snapshot.get("receipt_sha256") != R5_HISTORY[3]["receipt_sha256"]
        or r3_snapshot.get("source_map_sha256") != R5_HISTORY[3]["source_map_sha256"]
        or r3_manifest.get("selection", {}).get("revision") != 3
    ):
        raise StudyError("preserved R3 snapshot/manifest do not match the historical R3 frame")
    if (
        r4_snapshot.get("schema") != "execute.real-bash-corpus.immutable-revision-snapshot/1"
        or r4_snapshot.get("corpus_id") != "omarchy-v4.0.4-bin-bash-candidate-100-v4"
        or r4_snapshot.get("basis_manifest_sha256") != R5_HISTORY[4]["manifest_sha256"]
        or r4_snapshot.get("basis_receipt_sha256") != R5_HISTORY[4]["receipt_sha256"]
        or r4_snapshot.get("source_map_sha256") != R5_HISTORY[4]["source_map_sha256"]
        or r4_manifest.get("selection", {}).get("revision") != 4
    ):
        raise StudyError("preserved R4 snapshot/manifest do not match their R4 identity")

    r3_supersedes = r3_changes.get("supersedes", {})
    r4_supersedes = r4_changes.get("supersedes", {})
    if (
        r3_changes.get("revision") != 3
        or r3_changes.get("manifest_sha256") != R5_HISTORY[3]["manifest_sha256"]
        or r3_changes.get("source_map_sha256") != R5_HISTORY[3]["source_map_sha256"]
        or r3_supersedes.get("snapshot_path") != R5_HISTORY[2]["snapshot_path"]
        or r3_supersedes.get("snapshot_sha256") != R5_HISTORY[2]["snapshot_sha256"]
        or r3_supersedes.get("manifest_sha256") != R5_HISTORY[2]["manifest_sha256"]
        or r3_supersedes.get("receipt_sha256") != R5_HISTORY[2]["receipt_sha256"]
        or r3_supersedes.get("source_map_sha256") != R5_HISTORY[2]["source_map_sha256"]
    ):
        raise StudyError("R3 historical change record is not linked to the preserved R2 snapshot")
    if (
        r4_changes.get("revision") != 4
        or r4_changes.get("manifest_sha256") != R5_HISTORY[4]["manifest_sha256"]
        or r4_supersedes.get("snapshot_path") != R5_HISTORY[3]["snapshot_path"]
        or r4_supersedes.get("snapshot_sha256") != R5_HISTORY[3]["snapshot_sha256"]
        or r4_supersedes.get("manifest_sha256") != R5_HISTORY[3]["manifest_sha256"]
        or r4_supersedes.get("receipt_sha256") != R5_HISTORY[3]["receipt_sha256"]
        or r4_supersedes.get("source_map_sha256") != R5_HISTORY[3]["source_map_sha256"]
    ):
        raise StudyError("R4 historical change record is not linked to the preserved R3 snapshot")
    if (
        r3_review.get("decision") != "accept_all_eight_proposed_additions_for_curation; no replacements recommended"
        or r4_review.get("decision") != "accept_all_five_additions_for_next_candidate_curation_iteration_pending_metadata_correction; keep_candidate_unsealed"
        or r5_review.get("verdict") != "accept_four_swaps_for_unsealed_source_curation; no additional material-exposure replacement identified"
    ):
        raise StudyError("R3/R4/R5 source-review lineage changed or is overstated as final admission")
    for review in (r3_review, r4_review, r5_review):
        if "limits" not in review or not review["limits"]:
            raise StudyError("historical source-only reviews must retain their limits")

    r5_basis = r5_changes.get("basis", {})
    manifest_history = manifest["selection"].get("revision_history", {})
    change_frame = {
        "bash_sources": R5_FRAME["bash_sources"],
        "eligible_before_curation": R5_FRAME["eligible_before_curation"],
        "exact_exposures": R5_FRAME["exact_exposure_exclusions"],
        "family_count": R5_FRAME["functional_family_count"],
        "family_counts": R5_FRAME["family_counts"],
        "low_complexity_count": R5_FRAME["low_complexity_count"],
        "material_adjacency_exclusions": R5_FRAME["material_adjacency_exclusions"],
        "non_bash_files": R5_FRAME["non_bash_files"],
        "purpose_only_campaign_scope_exclusions": R5_FRAME["purpose_only_campaign_scope_exclusions"],
        "selected": R5_FRAME["selected_distinct_sources"],
        "size_bands": R5_FRAME["size_bands"],
        "strata": R5_FRAME["fixture_feasibility_quotas"],
        "unselected_eligible_bash": R5_FRAME["unselected_eligible_bash_sources"],
    }
    if (
        r5_changes.get("schema") != "execute.real-bash-corpus.revision5-changes/1"
        or r5_changes.get("status") != "candidate_unsealed"
        or r5_changes.get("selection_frame") != change_frame
        or r5_basis.get("r4_snapshot_sha256") != R5_HISTORY[4]["snapshot_sha256"]
        or r5_basis.get("supersedes") != R5_HISTORY[4]["snapshot_path"]
        or manifest_history.get("supersedes") != R5_HISTORY[4]["snapshot_path"]
        or manifest_history.get("snapshot_sha256") != R5_HISTORY[4]["snapshot_sha256"]
        or manifest_history.get("r5_changes_path") != "revision5-changes.json"
        or manifest_history.get("r5_review_path") != "revision5-bundled-independent-review.json"
    ):
        raise StudyError("R5 change record or manifest does not preserve the exact R4-to-R5 lineage/frame")

    current_by_task = {row["task_id"]: row for row in manifest["scripts"]}
    r4_by_task = {row["task_id"]: row for row in r4_manifest["scripts"]}
    expected_swaps = protocol["source_corpus"].get("r5_replacements")
    if expected_swaps != R5_REPLACEMENTS or len(r5_changes.get("swaps", [])) != 4:
        raise StudyError("R5 protocol must preserve the four source-candidate swaps")
    for expected, swap in zip(expected_swaps, r5_changes["swaps"]):
        old, new = r4_by_task.get(expected["task_id"]), current_by_task.get(expected["task_id"])
        if (
            swap.get("task_id") != expected["task_id"]
            or swap.get("remove_script_id") != expected["removed_script_id"]
            or swap.get("remove_sha256") != expected["removed_sha256"]
            or swap.get("add_script_id") != expected["added_script_id"]
            or swap.get("add_sha256") != expected["added_sha256"]
            or swap.get("testability_stratum") != expected["testability_stratum"]
            or old is None or new is None
            or old["script_id"] != expected["removed_script_id"]
            or old["source"]["sha256"] != expected["removed_sha256"]
            or new["script_id"] != expected["added_script_id"]
            or new["source"]["sha256"] != expected["added_sha256"]
            or new["testability_stratum"] != expected["testability_stratum"]
        ):
            raise StudyError(f"R5 task {expected['task_id']} swap does not link the preserved R4 row to the active R5 row")

    r3_ledger = read_json(_corpus_file(root, "history/revision3/candidate-exclusion-ledger.json", "preserved R3 exclusion ledger"), "preserved R3 exclusion ledger")[0]
    r3_scope = [row for row in r3_ledger["rows"] if row.get("disposition") == "excluded_campaign_scope"]
    r5_scope = [row for row in ledger["rows"] if row.get("disposition") == "excluded_campaign_scope"]
    _validate_historical_r3_scope_linkage(r5_scope, r3_scope, r3_manifest, r3_changes)

    binding = _read_lineage_json(
        root, "revision5-final-screen-binding.json", lineage["revision5_screen_binding_sha256"], "R5 final screen binding"
    )
    screen_path = _corpus_file(root, "revision5-full-candidate-token-screen.json", "R5 full candidate screen")
    _require_bytes_hash(screen_path.read_bytes(), lineage["revision5_full_candidate_screen_sha256"], "R5 full candidate screen")
    row_lines = [
        "\t".join((row["task_id"], row["script_id"], row["source"]["path"], row["source"]["sha256"]))
        for row in sorted(manifest["scripts"], key=lambda item: (item["task_id"], item["script_id"]))
    ]
    row_map_sha = digest_bytes(("\n".join(row_lines) + "\n").encode("utf-8"))
    if (
        binding.get("screen_sha256") != lineage["revision5_full_candidate_screen_sha256"]
        or binding.get("bundled_four_swap_proposal_sha256") != lineage["revision5_proposal_sha256"]
        or binding.get("independent_review_sha256") != lineage["revision5_source_review_sha256"]
        or binding.get("screened_roster", {}).get("rows") != 100
        or binding.get("screened_roster", {}).get("row_mapping_sha256") != row_map_sha
        or binding.get("screened_roster", {}).get("matches_bundled_proposal") is not True
    ):
        raise StudyError("R5 exposure-screen binding does not cover the exact active 100-row roster")

    parent_path = HERE.parents[2] / "evidence" / "corpus-revision5-parent-integrity.json"
    parent_integrity_status = "external_receipt_not_present"
    if parent_path.is_file():
        parent_doc, parent_raw, _ = read_json(parent_path, "R5 parent byte/count integrity receipt")
        _require_bytes_hash(parent_raw, R5_PARENT_INTEGRITY_SHA256, "R5 parent byte/count integrity receipt")
        if (
            parent_doc.get("status") != "candidate_unsealed"
            or parent_doc.get("manifest_sha256") != candidate["manifest_sha256"]
            or parent_doc.get("receipt_sha256") != candidate["receipt_sha256"]
            or parent_doc.get("source_map_sha256") != source_map_sha256
            or parent_doc.get("archive_inventory_rows_verified") != 444
            or parent_doc.get("selected_sources_verified") != 100
            or parent_doc.get("companion_files_verified") != 27
            or parent_doc.get("ledger_counts") != expected_ledger_counts
            or parent_doc.get("strata") != R5_FRAME["fixture_feasibility_quotas"]
            or parent_doc.get("families") != 26
        ):
            raise StudyError("R5 parent byte/count receipt does not reconcile to the exact candidate frame")
        parent_integrity_status = "candidate_byte_count_integrity_checked_no_admission_claim"
    return {
        "protocol_sha256": digest_file(PROTOCOL_R5),
        "parent_integrity_status": parent_integrity_status,
        "snapshot_file_count": r5_snapshot["file_count"],
        "snapshot_self_excluded_path": "history/revision5/SNAPSHOT.json",
    }


def verify_corpus_candidate(
    corpus_dir: Path,
    *,
    include_source_records: bool = False,
    historical_evidence_root: Path | None = None,
) -> dict[str, Any]:
    """Hash-check the curator's unsealed candidate package without accepting it."""
    root = corpus_dir.resolve(strict=True)
    manifest_path = _corpus_file(root, "manifest.json", "candidate manifest")
    receipt_path = _corpus_file(root, "receipt.json", "candidate receipt")
    manifest, _, manifest_sha = read_json(manifest_path, "candidate corpus manifest")
    receipt, _, receipt_sha = read_json(receipt_path, "candidate digest receipt")
    _keys(
        manifest,
        {"schema", "corpus_id", "status", "proposed_split", "source_population", "selection", "statistical_scope", "integrity", "scripts"},
        "candidate corpus manifest",
        {"exclusion_summary", "exposure_limitations"},
    )
    if manifest["schema"] != "execute.real-bash-corpus.candidate/1":
        raise StudyError("unsupported candidate corpus manifest schema")
    if manifest["status"] != "candidate_unsealed" or manifest["proposed_split"] != "held_out":
        raise StudyError("candidate verification requires an unsealed proposed held-out corpus")
    for field in ("source_population", "selection", "statistical_scope", "integrity"):
        if not isinstance(manifest[field], dict):
            raise StudyError(f"candidate manifest {field} must be an object")
    if not isinstance(manifest["integrity"].get("companion_evidence"), list):
        raise StudyError("candidate integrity must list its companion evidence paths")
    line_definition = manifest["integrity"].get("line_count_definition")
    if not isinstance(line_definition, str) or not line_definition:
        raise StudyError("candidate integrity must declare the physical-line counting rule")
    mode_definition = manifest["integrity"].get("mode_definition")
    if not isinstance(mode_definition, str) or not mode_definition:
        raise StudyError("candidate integrity must declare the source permission-bit counting rule")
    companion_paths = manifest["integrity"]["companion_evidence"]
    normalized_companions = {_relative_source_path(path, "integrity.companion_evidence entry") for path in companion_paths}
    revision = manifest["selection"].get("revision")
    if type(revision) is not int or revision not in {3, 5}:
        raise StudyError("the versioned candidate verifier supports only historical R3 or candidate R5")
    if revision == 3:
        required_companions = {
            "candidate-exclusion-ledger.json", "exclusions.json", "exposure-log.jsonl",
            "exposure-adjacency.json", "similarity-audit.json", "README.md",
            "independent-review.json", "revision2-changes.json", "receipt.json",
            "revision3-proposal.json", "revision3-independent-review.json", "revision3-changes.json",
        }
        if not required_companions.issubset(normalized_companions):
            raise StudyError("historical R3 integrity companion list is incomplete")
    elif normalized_companions != R5_COMPANIONS:
        raise StudyError("R5 integrity companion list is mixed, stale, incomplete, or contains unexpected paths")
    if len(companion_paths) != len(normalized_companions):
        raise StudyError("candidate integrity companion list contains duplicate paths")

    scripts = manifest["scripts"]
    if not isinstance(scripts, list) or len(scripts) != 100:
        raise StudyError("candidate corpus must contain exactly 100 source records")
    _keys(
        receipt,
        {
            "schema", "manifest_path", "manifest_sha256", "source_count", "unique_script_ids",
            "source_map_sha256", "unique_source_paths", "unique_source_hashes", "family_counts",
            "license_sha256", "sources", "verification", "companion_files", "created_utc",
        },
        "candidate digest receipt",
    )
    if receipt["schema"] != "execute.real-bash-corpus.receipt/1":
        raise StudyError("unsupported candidate corpus receipt schema")
    _utc(receipt["created_utc"], "receipt.created_utc")
    if _relative_source_path(receipt["manifest_path"], "receipt.manifest_path") != "manifest.json":
        raise StudyError("receipt must name the sibling manifest.json bytes")
    if receipt["manifest_sha256"] != manifest_sha:
        raise StudyError("candidate manifest bytes do not match the digest receipt")
    _hash(receipt["source_map_sha256"], "receipt.source_map_sha256")
    for field in ("source_count", "unique_script_ids", "unique_source_paths", "unique_source_hashes"):
        if type(receipt[field]) is not int or receipt[field] != 100:
            raise StudyError(f"receipt.{field} must equal 100")
    _hash(receipt["license_sha256"], "receipt.license_sha256")
    if not isinstance(receipt["verification"], dict):
        raise StudyError("receipt.verification must be an object")

    receipt_sources = receipt["sources"]
    if not isinstance(receipt_sources, list) or len(receipt_sources) != 100:
        raise StudyError("receipt must contain exactly 100 source-copy commitments")
    receipt_by_id: dict[str, dict[str, Any]] = {}
    for index, row in enumerate(receipt_sources):
        label = f"receipt.sources[{index}]"
        _keys(row, {"script_id", "path", "sha256", "bytes", "lines", "mode_octal"}, label)
        if not isinstance(row["script_id"], str) or row["script_id"] in receipt_by_id:
            raise StudyError(f"{label}.script_id must be unique")
        _hash(row["sha256"], f"{label}.sha256")
        _relative_source_path(row["path"], f"{label}.path")
        if type(row["bytes"]) is not int or row["bytes"] < 1 or type(row["lines"]) is not int or row["lines"] < 1:
            raise StudyError(f"{label} bytes and lines must be positive integers")
        if not isinstance(row["mode_octal"], str) or not re.fullmatch(r"[0-7]{4}", row["mode_octal"]):
            raise StudyError(f"{label}.mode_octal must contain four octal digits")
        receipt_by_id[row["script_id"]] = row

    companion_hashes = receipt["companion_files"]
    expected_companion_hashes = normalized_companions - {"receipt.json"}
    if not isinstance(companion_hashes, dict) or set(companion_hashes) != expected_companion_hashes:
        raise StudyError("receipt.companion_files must hash every declared companion except receipt.json")
    verified_companion_hashes = {}
    for relative, expected_sha in companion_hashes.items():
        _relative_source_path(relative, "receipt.companion_files path")
        _hash(expected_sha, f"receipt.companion_files[{relative!r}]")
        path = _corpus_file(root, relative, f"candidate companion {relative}")
        actual_sha = digest_file(path)
        if actual_sha != expected_sha:
            raise StudyError(f"candidate companion bytes do not match receipt: {relative}")
        verified_companion_hashes[relative] = actual_sha

    ledger, _, _ = read_json(
        _corpus_file(root, "candidate-exclusion-ledger.json", "candidate exclusion ledger"),
        "candidate exclusion ledger",
    )
    _keys(ledger, {"schema", "status", "scope_limit", "source_population", "counts", "rows"}, "candidate exclusion ledger")
    if ledger["schema"] != "execute.real-bash-corpus.population-ledger/1" or ledger["status"] != "candidate_unsealed":
        raise StudyError("candidate exclusion ledger must retain its unsealed population-ledger status")
    expected_ledger_counts = (
        {
            "candidate_selected": 100,
            "excluded_campaign_scope": 8,
            "excluded_known_exposure": 20,
            "excluded_material_adjacency": 5,
            "excluded_non_bash": 5,
            "not_selected_in_fixed_size_slice": 306,
        }
        if revision == 3 else
        {
            "candidate_selected": 100,
            "excluded_campaign_scope": 8,
            "excluded_known_exposure": 20,
            "excluded_material_adjacency": 14,
            "excluded_non_bash": 5,
            "not_selected_in_fixed_size_slice": 297,
        }
    )
    if ledger["counts"] != expected_ledger_counts:
        raise StudyError("candidate exclusion ledger counts differ from the frozen candidate revision arithmetic")
    ledger_rows = ledger["rows"]
    if not isinstance(ledger_rows, list) or len(ledger_rows) != 444:
        raise StudyError("candidate exclusion ledger must cover all 444 direct-bin inventory rows")
    observed_ledger_counts: Counter[str] = Counter()
    ledger_by_path: dict[str, dict[str, Any]] = {}
    for index, row in enumerate(ledger_rows):
        label = f"candidate exclusion ledger.rows[{index}]"
        optional_ledger_fields = {"replacement_script_id"}
        if revision == 5:
            optional_ledger_fields.update({"review_artifact", "candidate_script_id", "candidate_task_id"})
        _keys(
            row,
            {"path", "sha256", "bytes", "lines", "mode_octal", "disposition", "reason", "shebang"},
            label,
            optional_ledger_fields,
        )
        path = _relative_source_path(row["path"], f"{label}.path")
        if path in ledger_by_path:
            raise StudyError("candidate exclusion ledger must have unique source paths")
        _hash(row["sha256"], f"{label}.sha256")
        if type(row["bytes"]) is not int or row["bytes"] < 1 or type(row["lines"]) is not int or row["lines"] < 1:
            raise StudyError(f"{label} bytes and lines must be positive integers")
        if not isinstance(row["mode_octal"], str) or not re.fullmatch(r"[0-7]{4}", row["mode_octal"]):
            raise StudyError(f"{label}.mode_octal must contain four octal digits")
        if not isinstance(row["disposition"], str) or row["disposition"] not in expected_ledger_counts:
            raise StudyError(f"{label}.disposition is unsupported")
        if not isinstance(row["reason"], str) or not isinstance(row["shebang"], str):
            raise StudyError(f"{label}.reason and shebang must be strings")
        if "replacement_script_id" in row and (
            not isinstance(row["replacement_script_id"], str)
            or not ID_RE.fullmatch(row["replacement_script_id"])
        ):
            raise StudyError(f"{label}.replacement_script_id must be a script identity")
        if "review_artifact" in row and (
            not isinstance(row["review_artifact"], str) or not row["review_artifact"]
        ):
            raise StudyError(f"{label}.review_artifact must name a preserved review artifact")
        if ("candidate_script_id" in row) != ("candidate_task_id" in row):
            raise StudyError(f"{label} candidate membership fields must appear together")
        if "candidate_script_id" in row and (
            not isinstance(row["candidate_script_id"], str)
            or not ID_RE.fullmatch(row["candidate_script_id"])
            or not isinstance(row["candidate_task_id"], str)
            or not ID_RE.fullmatch(row["candidate_task_id"])
            or row["disposition"] != "candidate_selected"
        ):
            raise StudyError(f"{label} candidate membership fields are malformed or attached to an exclusion")
        ledger_by_path[path] = row
        observed_ledger_counts[row["disposition"]] += 1
    if dict(observed_ledger_counts) != expected_ledger_counts or len(ledger_by_path) != 444:
        raise StudyError("candidate exclusion ledger rows do not match their declared disposition counts")

    task_ids, script_ids, source_paths, source_hashes = set(), set(), set(), set()
    family_counts: Counter[str] = Counter()
    testability_counts: Counter[str] = Counter()
    repositories = set()
    license_hashes = set()
    source_records_by_task: dict[str, dict[str, Any]] = {}
    r5_adjacency_review_tasks: set[str] = set()
    for index, record in enumerate(scripts):
        label = f"manifest.scripts[{index}]"
        optional_record_fields = {"source_only_adjacency_review"} if revision == 5 else set()
        _keys(
            record,
            {
                "task_id", "script_id", "family_id", "family_label", "family_rationale",
                "analysis_stratum", "proposed_split", "status", "testability_stratum",
                "testability_status", "fixture_plan", "dependency_inventory", "source", "license",
                "inventory_signals", "size_band", "low_complexity_flag",
            },
            label,
            optional_record_fields,
        )
        for field in ("task_id", "script_id", "family_id"):
            if not isinstance(record[field], str) or not ID_RE.fullmatch(record[field]):
                raise StudyError(f"{label}.{field} is malformed")
        if record["task_id"] in task_ids or record["script_id"] in script_ids:
            raise StudyError(f"{label} duplicates a task or script identity")
        task_ids.add(record["task_id"])
        script_ids.add(record["script_id"])
        if record["analysis_stratum"] != "workflow_cli" or record["proposed_split"] != "held_out" or record["status"] != "candidate_unsealed":
            raise StudyError(f"{label} is not an unsealed proposed workflow_cli held-out candidate")
        for field in ("family_label", "family_rationale", "testability_stratum", "testability_status", "fixture_plan", "size_band"):
            if not isinstance(record[field], str) or not record[field]:
                raise StudyError(f"{label}.{field} must be a nonempty descriptive string")
        if "source_only_adjacency_review" in record and (
            not isinstance(record["source_only_adjacency_review"], str)
            or not record["source_only_adjacency_review"]
        ):
            raise StudyError(f"{label}.source_only_adjacency_review must be a nonempty review note")
        if "source_only_adjacency_review" in record:
            r5_adjacency_review_tasks.add(record["task_id"])
        dependency_inventory = record["dependency_inventory"]
        optional_dependency_fields = {"manual_callsite_corrections"} if revision == 5 else set()
        _keys(
            dependency_inventory,
            {
                "absolute_path_literal_candidates", "dynamic_or_unresolved_dependencies",
                "environment_variable_candidates", "external_command_candidates", "method",
                "omarchy_helper_candidates", "source_operator_observed", "source_operator_tokens",
                "source_path_candidates",
            },
            f"{label}.dependency_inventory",
            optional_dependency_fields,
        )
        for field in (
            "absolute_path_literal_candidates", "environment_variable_candidates",
            "external_command_candidates", "omarchy_helper_candidates",
            "source_operator_tokens", "source_path_candidates",
        ):
            if not isinstance(dependency_inventory[field], list) or any(not isinstance(item, str) for item in dependency_inventory[field]):
                raise StudyError(f"{label}.dependency_inventory.{field} must be a string array")
        for field in ("dynamic_or_unresolved_dependencies", "method"):
            if not isinstance(dependency_inventory[field], str) or not dependency_inventory[field]:
                raise StudyError(f"{label}.dependency_inventory.{field} must be a nonempty string")
        if type(dependency_inventory["source_operator_observed"]) is not bool:
            raise StudyError(f"{label}.dependency_inventory.source_operator_observed must be boolean")
        if "manual_callsite_corrections" in dependency_inventory:
            corrections = dependency_inventory["manual_callsite_corrections"]
            if not isinstance(corrections, list):
                raise StudyError(f"{label}.dependency_inventory.manual_callsite_corrections must be an array")
            for correction_index, correction in enumerate(corrections):
                correction_label = f"{label}.dependency_inventory.manual_callsite_corrections[{correction_index}]"
                _keys(correction, {"helper", "line", "reason", "rpc_method"}, correction_label)
                if (
                    not isinstance(correction["helper"], str)
                    or not correction["helper"]
                    or type(correction["line"]) is not int
                    or correction["line"] < 1
                    or not isinstance(correction["reason"], str)
                    or not correction["reason"]
                    or not isinstance(correction["rpc_method"], str)
                    or not correction["rpc_method"]
                ):
                    raise StudyError(f"{correction_label} must preserve the manual callsite audit note")
        if not isinstance(record["inventory_signals"], dict) or type(record["low_complexity_flag"]) is not bool:
            raise StudyError(f"{label} inventory signals and complexity flag are malformed")
        source = record["source"]
        _keys(source, {"repository_url", "release_tag", "commit", "path", "permalink", "copy_path", "sha256", "bytes", "lines", "mode_octal"}, f"{label}.source")
        if not isinstance(source["repository_url"], str) or not source["repository_url"].startswith(("https://", "http://")):
            raise StudyError(f"{label}.source.repository_url must be HTTP(S)")
        if not isinstance(source["release_tag"], str) or not source["release_tag"]:
            raise StudyError(f"{label}.source.release_tag must be nonempty")
        _git_oid(source["commit"], f"{label}.source.commit")
        _hash(source["sha256"], f"{label}.source.sha256")
        source_path = _relative_source_path(source["path"], f"{label}.source.path")
        copy_path = _relative_source_path(source["copy_path"], f"{label}.source.copy_path")
        source_key = (source["repository_url"], source["commit"], source_path)
        if source_key in source_paths or source["sha256"] in source_hashes:
            raise StudyError("candidate source paths and bytes must be unique across all 100 tasks")
        source_paths.add(source_key)
        source_hashes.add(source["sha256"])
        repositories.add((source["repository_url"], source["commit"]))
        if type(source["bytes"]) is not int or source["bytes"] < 1 or type(source["lines"]) is not int or source["lines"] < 1:
            raise StudyError(f"{label}.source bytes and lines must be positive integers")
        if not isinstance(source["mode_octal"], str) or not re.fullmatch(r"[0-7]{4}", source["mode_octal"]):
            raise StudyError(f"{label}.source.mode_octal must contain four octal digits")
        copy_file = _corpus_file(root, copy_path, f"{label}.source.copy_path")
        raw = copy_file.read_bytes()
        if digest_bytes(raw) != source["sha256"] or len(raw) != source["bytes"] or _physical_lf_lines(raw) != source["lines"]:
            raise StudyError(f"candidate source-copy content, byte count, or physical line count differs: {copy_path}")
        ledger_row = ledger_by_path.get(source_path)
        if ledger_row is None or ledger_row["disposition"] != "candidate_selected" or any(
            ledger_row[field] != source[source_field]
            for field, source_field in (
                ("sha256", "sha256"), ("bytes", "bytes"), ("lines", "lines"), ("mode_octal", "mode_octal"),
            )
        ):
            raise StudyError(f"selected source does not match the candidate disposition ledger: {source_path}")
        if revision == 5 and (
            ledger_row.get("candidate_script_id") != record["script_id"]
            or ledger_row.get("candidate_task_id") != record["task_id"]
        ):
            raise StudyError(f"R5 selected ledger membership does not match its task/source row: {source_path}")
        actual_mode = format(stat.S_IMODE(copy_file.stat().st_mode) & 0o777, "04o")
        if actual_mode != source["mode_octal"]:
            raise StudyError(f"candidate source-copy mode differs from provenance: {copy_path}")
        receipt_row = receipt_by_id.get(record["script_id"])
        if receipt_row is None or any(
            receipt_row[key] != expected
            for key, expected in (
                ("path", copy_path), ("sha256", source["sha256"]), ("bytes", source["bytes"]),
                ("lines", source["lines"]), ("mode_octal", source["mode_octal"]),
            )
        ):
            raise StudyError(f"receipt source map disagrees with manifest record: {record['script_id']}")
        license_record = record["license"]
        _keys(license_record, {"SPDX", "path", "sha256"}, f"{label}.license")
        if not isinstance(license_record["SPDX"], str) or not license_record["SPDX"] or not isinstance(license_record["path"], str) or not license_record["path"]:
            raise StudyError(f"{label}.license identifier and upstream path must be nonempty")
        _hash(license_record["sha256"], f"{label}.license.sha256")
        license_hashes.add(license_record["sha256"])
        family_counts[record["family_id"]] += 1
        testability_counts[record["testability_stratum"]] += 1
        canonical_record = json.dumps(
            record, ensure_ascii=False, sort_keys=True, separators=(",", ":"), allow_nan=False
        ).encode("utf-8")
        source_records_by_task[record["task_id"]] = {
            "family_id": record["family_id"],
            "testability_stratum": record["testability_stratum"],
            "repo_url": source["repository_url"],
            "source_commit": source["commit"],
            "source_path": source_path,
            "source_sha256": source["sha256"],
            "manifest_record_sha256": digest_bytes(canonical_record),
        }
    if set(receipt_by_id) != script_ids or len(source_paths) != 100 or len(source_hashes) != 100:
        raise StudyError("manifest and receipt do not attest exactly 100 distinct source files")
    if revision == 5 and r5_adjacency_review_tasks != {row["task_id"] for row in R5_REPLACEMENTS}:
        raise StudyError("R5 source-only adjacency notes must bind exactly the four reviewed replacement task slots")
    scope_replacement_ids = {
        row["replacement_script_id"]
        for row in ledger_by_path.values()
        if row["disposition"] == "excluded_campaign_scope" and "replacement_script_id" in row
    }
    if revision == 3 and (len(scope_replacement_ids) != 8 or not scope_replacement_ids.issubset(script_ids)):
        raise StudyError("historical R3 purpose-only links must resolve to its selected R3 rows")
    if revision == 5 and scope_replacement_ids:
        raise StudyError("R5 purpose-only exclusions must not carry superseded R3 replacement IDs as active links")
    if revision == 5:
        adjacency_rows = [
            row for row in ledger_by_path.values()
            if row["disposition"] == "excluded_material_adjacency"
        ]
        adjacency_replacements = [row.get("replacement_script_id") for row in adjacency_rows]
        if (
            len(adjacency_rows) != 14
            or any(not isinstance(script_id, str) for script_id in adjacency_replacements)
            or len(set(adjacency_replacements)) != 14
            or not set(adjacency_replacements).issubset(script_ids)
            or any(not row.get("review_artifact") for row in adjacency_rows)
            or sum("candidate_script_id" in row for row in ledger_by_path.values()) != 100
        ):
            raise StudyError("R5 selected membership or material-adjacency replacement links do not match the frozen candidate set")
    source_map_lines = [
        "\t".join(
            str(row[field])
            for field in ("script_id", "path", "sha256", "bytes", "lines", "mode_octal")
        )
        for row in sorted(receipt_by_id.values(), key=lambda item: item["script_id"])
    ]
    source_map_sha = digest_bytes(("\n".join(source_map_lines) + "\n").encode("utf-8"))
    if source_map_sha != receipt["source_map_sha256"]:
        raise StudyError("receipt source-map digest differs from its canonical sorted source rows")
    if len(repositories) != 1:
        raise StudyError("candidate sample must come from exactly one pinned repository commit")
    if len(license_hashes) != 1 or next(iter(license_hashes)) != receipt["license_sha256"]:
        raise StudyError("candidate records and receipt must agree on the pinned license file hash")
    if receipt["family_counts"] != dict(sorted(family_counts.items())):
        raise StudyError("receipt functional-family composition differs from the candidate manifest")
    r5_verification = None
    if revision == 3:
        eligible_frame = manifest["selection"].get("eligible_frame")
        expected_frame = {
            "published_bash_inventory_count": 439,
            "known_exact_exposed_sources_removed": 20,
            "material_adjacency_sources_removed": 5,
            "campaign_scope_sources_removed": 8,
            "remaining_before_curation": 406,
        }
        if eligible_frame != expected_frame or manifest["selection"].get("selected_task_count") != 100:
            raise StudyError("candidate manifest selection frame differs from revision-3 exclusion arithmetic")
        changes, _, _ = read_json(
            _corpus_file(root, "revision3-changes.json", "revision-3 change record"),
            "revision-3 change record",
        )
        if (
            changes.get("schema") != "execute.real-bash-corpus.revision-changes/1"
            or changes.get("revision") != 3
            or changes.get("status") != "candidate_unsealed"
            or changes.get("manifest_sha256") != manifest_sha
            or changes.get("source_map_sha256") != source_map_sha
            or changes.get("count_arithmetic") != {
                "bash_sources": 439,
                "campaign_scope_exclusions": 8,
                "direct_bin_inventory_files": 444,
                "eligible_before_curation": 406,
                "known_exact_exposures": 20,
                "material_adjacency_rejections": 5,
                "non_bash_files": 5,
                "remaining_unselected_bash": 306,
                "selected_distinct_tasks": 100,
            }
            or changes.get("selection_counts") != {
                "command_fixture": 30,
                "desktop_cli_simulation": 30,
                "filesystem_fixture": 40,
            }
            or len(changes.get("swaps", [])) != 8
        ):
            raise StudyError("revision-3 change record does not match the verified candidate and ledger arithmetic")
        protocol_doc, _, _ = read_json(PROTOCOL_R3, "historical R3 preregistration")
        prior_candidates = protocol_doc.get("evidence_state", {}).get("superseded_candidate_evidence", [])
        prior_r2 = next((row for row in prior_candidates if row.get("revision") == 2), None) if isinstance(prior_candidates, list) else None
        revision_history = manifest["selection"].get("revision_history")
        supersedes = changes.get("supersedes")
        if (
            not isinstance(prior_r2, dict)
            or not isinstance(revision_history, dict)
            or not isinstance(supersedes, dict)
            or prior_r2.get("manifest_sha256") != supersedes.get("manifest_sha256")
            or prior_r2.get("receipt_sha256") != supersedes.get("receipt_sha256")
            or prior_r2.get("source_map_sha256") != supersedes.get("source_map_sha256")
            or revision_history.get("snapshot_sha256") != supersedes.get("snapshot_sha256")
            or revision_history.get("supersedes") != supersedes.get("snapshot_path")
        ):
            raise StudyError("revision-3 supersession link differs from the preserved revision-2 preregistration history")
        try:
            snapshot_path = _corpus_file(root, supersedes["snapshot_path"], "revision-2 history snapshot")
        except StudyError:
            if historical_evidence_root is None:
                raise
            snapshot_path = _corpus_file(
                historical_evidence_root.resolve(strict=True), supersedes["snapshot_path"], "revision-2 history snapshot"
            )
        if digest_file(snapshot_path) != supersedes["snapshot_sha256"]:
            raise StudyError("preserved revision-2 history snapshot does not match the revision-3 commitment")
        proposal, _, _ = read_json(
            _corpus_file(root, "revision3-proposal.json", "revision-3 proposal"),
            "revision-3 proposal",
        )
        if (
            proposal.get("schema") != "execute.real-bash-corpus.revision-proposal/1"
            or proposal.get("candidate_revision") != 3
            or proposal.get("status") != "proposed_for_independent_review_not_frozen"
            or proposal.get("basis_manifest_sha256") != prior_r2["manifest_sha256"]
            or proposal.get("basis_receipt_sha256") != prior_r2["receipt_sha256"]
            or proposal.get("r2_snapshot_sha256") != revision_history["snapshot_sha256"]
            or len(proposal.get("proposed_scope_exclusions_from_purpose_only", [])) != 8
            or len(proposal.get("proposed_swaps", [])) != 8
        ):
            raise StudyError("revision-3 proposal does not preserve the recorded revision-2 candidate lineage")
        source_only_review, _, _ = read_json(
            _corpus_file(root, "revision3-independent-review.json", "revision-3 source-only review"),
            "revision-3 source-only review",
        )
        if (
            source_only_review.get("schema") != "execute.real-bash-corpus.revision3-independent-review/1"
            or source_only_review.get("decision") != "accept_all_eight_proposed_additions_for_curation; no replacements recommended"
            or len(source_only_review.get("candidate_decisions", [])) != 8
        ):
            raise StudyError("revision-3 source-only review does not document the eight curation additions")
        expected_testability = {"command_fixture": 30, "filesystem_fixture": 40, "desktop_cli_simulation": 30}
        if dict(sorted(testability_counts.items())) != expected_testability:
            raise StudyError("candidate testability fixture quotas differ from the frozen 30/40/30 selection rule")
    else:
        r5_verification = _validate_r5_lineage(
            root, manifest, receipt, ledger, manifest_sha, receipt_sha, source_map_sha,
            family_counts, testability_counts,
        )
    result = {
        "schema": "execute.ai-productivity.real-bash-100-candidate-verification/1",
        "status": "candidate_local_integrity_verified_review_pending",
        "manifest_sha256": manifest_sha,
        "receipt_sha256": receipt_sha,
        "source_map_sha256": source_map_sha,
        "candidate_revision": revision,
        "source_count": len(scripts),
        "distinct_task_ids": len(task_ids),
        "distinct_script_ids": len(script_ids),
        "distinct_source_paths": len(source_paths),
        "distinct_source_hashes": len(source_hashes),
        "distinct_functional_clusters": len(family_counts),
        "candidate_task_ids": sorted(task_ids),
        "family_counts": dict(sorted(family_counts.items())),
        "candidate_ledger_counts": dict(sorted(expected_ledger_counts.items())),
        "testability_counts": dict(sorted(testability_counts.items())),
        "source_review_scope": (
            "R3 source review is historical and rejects held-out admission"
            if revision == 3 else "R5 source-only review accepts four swaps for continued unsealed curation only"
        ),
        "corpus_acceptance": (
            "historical_R3_rejected_for_held_out_admission"
            if revision == 3 else "pending_not_inferred_by_local_integrity_verification"
        ),
        "line_count_definition": line_definition,
        "mode_definition": mode_definition,
        "companion_sha256": verified_companion_hashes,
        "receipt_verification_attestation": receipt["verification"],
        "integrity_basis": (
            (
                "Checked the pinned R5 manifest, receipt, selected source copies, 444-row ledger, 27 receipt-bound "
                "companions, historical R2/R3/R4 lineage, and exact current-tree equality with the 593-file R5 "
                "snapshot (excluding only its separately pinned history/revision5/SNAPSHOT.json descriptor). Did not independently "
                "fetch the upstream archive, verify licensing, reproduce the inventory/exposure search, re-review "
                "source curation, run behavior tests, or accept a held-out split."
            )
            if revision == 5 else
            (
                "Checked the historical R3 candidate manifest, receipt, source copies, ledger arithmetic, and R3-to-R2 "
                "lineage. R3 has an external held-out-admission rejection; this verification does not overturn it or "
                "independently reproduce upstream provenance or behavior."
            )
        ),
        "independent_review": (
            "R5 source-only review artifact hash checked; independent trial-corpus acceptance pending"
            if revision == 5 else "historical R3 review rejects held-out admission"
        ),
        "claim": "none",
    }
    if include_source_records:
        result["_source_records_by_task"] = source_records_by_task
    if r5_verification is not None:
        result["r5_lineage_verification"] = r5_verification
    return result


def validate_corpus_review(
    review: Any,
    review_sha256: str,
    freeze: dict[str, Any],
    candidate: dict[str, Any],
    tasks: dict[str, dict[str, Any]],
) -> dict[str, Any]:
    _require_r5_candidate_for_admission(candidate, freeze)
    _keys(
        review,
        {
            "schema", "decision", "reviewed_utc", "reviewer_id", "independence",
            "corpus_manifest_sha256", "corpus_receipt_sha256", "candidate_source_count",
            "accepted_task_ids", "exact_source_overlap_count", "license_status",
            "selection_status", "family_adjacency_review", "review_notes_sha256",
        },
        "independent corpus review",
    )
    if review["schema"] != "execute.ai-productivity.real-bash-100-corpus-review/1" or review["decision"] != "accepted_held_out":
        raise StudyError("independent corpus review has not accepted a held-out corpus")
    _utc(review["reviewed_utc"], "corpus_review.reviewed_utc")
    if not isinstance(review["reviewer_id"], str) or not review["reviewer_id"]:
        raise StudyError("corpus_review.reviewer_id must be nonempty")
    _keys(review["independence"], {"not_corpus_curator", "not_programmer_operator"}, "corpus_review.independence")
    if review["independence"] != {"not_corpus_curator": True, "not_programmer_operator": True}:
        raise StudyError("corpus acceptance must be recorded by an independent reviewer")
    _hash(review_sha256, "corpus review byte hash")
    if review_sha256 != freeze["corpus_review_sha256"]:
        raise StudyError("review record bytes do not match the pre-dispatch freeze")
    if review["corpus_manifest_sha256"] != candidate["manifest_sha256"] or review["corpus_manifest_sha256"] != freeze["corpus_manifest_sha256"]:
        raise StudyError("review record references different candidate manifest bytes")
    if review["corpus_receipt_sha256"] != candidate["receipt_sha256"] or review["corpus_receipt_sha256"] != freeze["corpus_digest_receipt_sha256"]:
        raise StudyError("review record references different candidate receipt bytes")
    if type(review["candidate_source_count"]) is not int or review["candidate_source_count"] != 100:
        raise StudyError("review record must accept exactly 100 unique source tasks")
    accepted_ids = review["accepted_task_ids"]
    if not isinstance(accepted_ids, list) or len(accepted_ids) != 100 or any(not isinstance(item, str) or not ID_RE.fullmatch(item) for item in accepted_ids) or len(set(accepted_ids)) != 100:
        raise StudyError("review record must enumerate 100 unique accepted task IDs")
    if set(accepted_ids) != set(candidate["candidate_task_ids"]) or set(accepted_ids) != set(tasks):
        raise StudyError("accepted task IDs must exactly match the verified corpus and frozen roster")
    if type(review["exact_source_overlap_count"]) is not int or review["exact_source_overlap_count"] != 0:
        raise StudyError("held-out corpus review must report zero exact-source overlap")
    if review["license_status"] != "accepted" or review["selection_status"] != "accepted_curated_sample_as_described":
        raise StudyError("independent reviewer must accept the license and declared curated selection")
    adjacency = review["family_adjacency_review"]
    _keys(adjacency, {"ledger_sha256", "rows_reviewed", "material_overlap_rows", "unresolved_rows"}, "corpus_review.family_adjacency_review")
    _hash(adjacency["ledger_sha256"], "corpus_review.family_adjacency_review.ledger_sha256")
    expected_adjacency_hash = candidate["companion_sha256"].get("exposure-adjacency.json")
    if adjacency["ledger_sha256"] != expected_adjacency_hash:
        raise StudyError("family-adjacency review must cite the verified exposure adjacency ledger")
    if type(adjacency["rows_reviewed"]) is not int or adjacency["rows_reviewed"] < 0:
        raise StudyError("family-adjacency review row count must be a nonnegative integer")
    if type(adjacency["material_overlap_rows"]) is not int or adjacency["material_overlap_rows"] != 0:
        raise StudyError("material prior-exposure family overlap must be removed before held-out acceptance")
    if type(adjacency["unresolved_rows"]) is not int or adjacency["unresolved_rows"] != 0:
        raise StudyError("all prior-exposure family-adjacency rows must be resolved before acceptance")
    _hash(review["review_notes_sha256"], "corpus_review.review_notes_sha256")
    reviewed_at = datetime.fromisoformat(review["reviewed_utc"].replace("Z", "+00:00"))
    frozen_at = datetime.fromisoformat(freeze["frozen_utc"].replace("Z", "+00:00"))
    if reviewed_at > frozen_at:
        raise StudyError("independent corpus acceptance must precede the pre-dispatch freeze")
    return review


def _require_r5_candidate_for_admission(candidate: Any, freeze: Any) -> None:
    """Keep historical candidate verification separate from R5 trial admission."""
    if not isinstance(candidate, dict) or candidate.get("candidate_revision") != 5:
        raise StudyError("held-out admission requires a verified revision-5 candidate")
    for field in ("manifest_sha256", "receipt_sha256", "source_map_sha256"):
        if candidate.get(field) != R5_CANDIDATE[field]:
            raise StudyError(f"held-out admission candidate {field} is not the pinned R5 identity")
    if candidate.get("corpus_acceptance") != "pending_not_inferred_by_local_integrity_verification":
        raise StudyError("R5 local integrity verification cannot self-assert held-out acceptance")
    if not isinstance(freeze, dict):
        raise StudyError("held-out admission requires an R5 pre-dispatch freeze")
    if (
        freeze.get("corpus_manifest_sha256") != R5_CANDIDATE["manifest_sha256"]
        or freeze.get("corpus_digest_receipt_sha256") != R5_CANDIDATE["receipt_sha256"]
    ):
        raise StudyError("held-out admission freeze does not bind the exact pinned R5 manifest and receipt")


def _keys(value: Any, required: set[str], label: str, optional: set[str] | None = None) -> None:
    if not isinstance(value, dict):
        raise StudyError(f"{label} must be an object")
    optional = optional or set()
    missing = required - value.keys()
    extra = value.keys() - required - optional
    if missing or extra:
        details = []
        if missing:
            details.append("missing " + ", ".join(sorted(missing)))
        if extra:
            details.append("unexpected " + ", ".join(sorted(extra)))
        raise StudyError(f"{label}: " + "; ".join(details))


def _hash(value: Any, label: str) -> None:
    if not isinstance(value, str) or not HASH_RE.fullmatch(value):
        raise StudyError(f"{label} must be a lowercase SHA-256 digest")


def _git_oid(value: Any, label: str) -> None:
    if not isinstance(value, str) or not re.fullmatch(r"(?:[a-f0-9]{40}|[a-f0-9]{64})", value):
        raise StudyError(f"{label} must be a lowercase 40- or 64-digit Git object ID")


def _utc(value: Any, label: str) -> None:
    if not isinstance(value, str):
        raise StudyError(f"{label} must be an ISO-8601 timestamp")
    try:
        parsed = datetime.fromisoformat(value.replace("Z", "+00:00"))
    except ValueError as error:
        raise StudyError(f"{label} is not an ISO-8601 timestamp: {error}") from error
    if parsed.tzinfo is None or parsed.utcoffset().total_seconds() != 0:
        raise StudyError(f"{label} must specify UTC")


def _relative_source_path(value: Any, label: str) -> str:
    if not isinstance(value, str) or not value or "\\" in value or "\x00" in value:
        raise StudyError(f"{label} must be a nonempty POSIX relative path")
    path = PurePosixPath(value)
    if path.is_absolute() or any(part in {"", ".", ".."} for part in path.parts):
        raise StudyError(f"{label} may not be absolute or traverse directories")
    return path.as_posix()


def validate_freeze(freeze: Any) -> dict[str, Any]:
    _keys(freeze, FREEZE_REQUIRED_FIELDS, "freeze")
    if freeze["schema"] != "execute.ai-productivity.real-bash-100-freeze/1":
        raise StudyError("unsupported 100-script freeze schema")
    if freeze["status"] != "frozen_before_dispatch":
        raise StudyError("study freeze must precede every programmer dispatch")
    if freeze["corpus_review_status"] != "accepted":
        raise StudyError("candidate_unsealed corpus cannot be frozen for trial dispatch")
    _utc(freeze["frozen_utc"], "freeze.frozen_utc")
    if not isinstance(freeze["randomization_seed"], int) or isinstance(freeze["randomization_seed"], bool) or freeze["randomization_seed"] < 0:
        raise StudyError("freeze.randomization_seed must be a nonnegative integer")
    if not isinstance(freeze["comparison_block"], str) or not freeze["comparison_block"]:
        raise StudyError("freeze.comparison_block must be nonempty")
    for field in HASHED_FREEZE_FIELDS:
        _hash(freeze[field], f"freeze.{field}")
    protocol, _, protocol_hash = read_json(PROTOCOL, "checked-in focused preregistration")
    if freeze["protocol_sha256"] != protocol_hash:
        raise StudyError("freeze protocol hash does not match the focused preregistration bytes")
    r5_identity = protocol.get("source_corpus", {}).get("r5_candidate_identity", {})
    if (
        freeze["corpus_manifest_sha256"] != r5_identity.get("manifest_sha256")
        or freeze["corpus_digest_receipt_sha256"] != r5_identity.get("receipt_sha256")
    ):
        raise StudyError("freeze corpus identity does not match the R5 candidate pinned by the preregistration")
    rubric_hash = digest_file(RUBRIC)
    if freeze["rubric_sha256"] != rubric_hash:
        raise StudyError("freeze rubric hash does not match the focused rubric bytes")
    if protocol["scoring"]["human_ratings"]["sha256"] != rubric_hash:
        raise StudyError("focused preregistration rubric hash does not match the rubric bytes")
    if freeze["randomization_seed"] != protocol["analysis"]["seed"]:
        raise StudyError("freeze randomization seed differs from the preregistered seed")
    return freeze


def validate_roster(roster: Any, freeze: dict[str, Any], roster_sha256: str) -> dict[str, dict[str, Any]]:
    required = {
        "schema", "status", "frozen_utc", "protocol_sha256", "corpus_manifest_sha256",
        "digest_receipt_sha256", "task_package_sha256", "tasks",
    }
    _keys(roster, required, "roster")
    if roster["schema"] != "execute.ai-productivity.real-bash-100-roster/1":
        raise StudyError("unsupported 100-script roster schema")
    if roster["status"] != "frozen_before_generation":
        raise StudyError("roster must be frozen before the first generation attempt")
    _utc(roster["frozen_utc"], "roster.frozen_utc")
    _hash(roster_sha256, "roster byte hash")
    if roster_sha256 != freeze["task_roster_sha256"]:
        raise StudyError("roster bytes do not match the pre-dispatch freeze")
    if roster["protocol_sha256"] != freeze["protocol_sha256"]:
        raise StudyError("roster protocol hash differs from the freeze")
    if roster["corpus_manifest_sha256"] != freeze["corpus_manifest_sha256"]:
        raise StudyError("roster corpus manifest hash differs from the freeze")
    if roster["digest_receipt_sha256"] != freeze["corpus_digest_receipt_sha256"]:
        raise StudyError("roster corpus receipt hash differs from the freeze")
    if roster["task_package_sha256"] != freeze["task_package_sha256"]:
        raise StudyError("roster task-package hash differs from the freeze")
    if not isinstance(roster["tasks"], list) or len(roster["tasks"]) != 100:
        raise StudyError("focused roster must contain exactly 100 unique-source tasks")
    tasks: dict[str, dict[str, Any]] = {}
    source_keys = set()
    record_hashes = set()
    repositories = set()
    for index, task in enumerate(roster["tasks"]):
        label = f"roster.tasks[{index}]"
        fields = {
            "task_id", "family_id", "split", "stratum", "testability_stratum", "repo_url",
            "source_commit", "source_path", "source_sha256", "manifest_record_sha256",
            "prompt_sha256", "behavior_brief_sha256", "oracle_sha256", "oracle_ref",
            "criteria", "timing_case_set_sha256", "timing_case_ref",
        }
        _keys(task, fields, label)
        for field in ("task_id", "family_id"):
            if not isinstance(task[field], str) or not ID_RE.fullmatch(task[field]):
                raise StudyError(f"{label}.{field} is malformed")
        if task["task_id"] in tasks:
            raise StudyError(f"duplicate task ID {task['task_id']!r}")
        if task["split"] != "held_out" or task["stratum"] != "workflow_cli":
            raise StudyError(f"{label} must be in the focused held_out/workflow_cli sample")
        if not isinstance(task["testability_stratum"], str) or not task["testability_stratum"]:
            raise StudyError(f"{label}.testability_stratum must be a descriptive label")
        if not isinstance(task["repo_url"], str) or not task["repo_url"].startswith(("https://", "http://")):
            raise StudyError(f"{label}.repo_url must be an absolute HTTP(S) URL")
        _git_oid(task["source_commit"], f"{label}.source_commit")
        path = _relative_source_path(task["source_path"], f"{label}.source_path")
        source_key = (task["repo_url"], task["source_commit"], path)
        if source_key in source_keys:
            raise StudyError(f"source file occurs more than once in the task roster: {source_key}")
        source_keys.add(source_key)
        repositories.add((task["repo_url"], task["source_commit"]))
        for field in (
            "source_sha256", "manifest_record_sha256", "prompt_sha256",
            "behavior_brief_sha256", "oracle_sha256", "timing_case_set_sha256",
        ):
            _hash(task[field], f"{label}.{field}")
        if task["manifest_record_sha256"] in record_hashes:
            raise StudyError(f"{label} duplicates a corpus-manifest record commitment")
        record_hashes.add(task["manifest_record_sha256"])
        for field in ("oracle_ref", "timing_case_ref"):
            if not isinstance(task[field], str) or not task[field]:
                raise StudyError(f"{label}.{field} must be an opaque evaluator reference")
        criteria = task["criteria"]
        if not isinstance(criteria, dict) or not criteria or any(
            not isinstance(item, str) or not CRITERION_RE.fullmatch(item)
            or not isinstance(operation, str) or operation not in CRITERION_OPERATIONS
            for item, operation in criteria.items()
        ):
            raise StudyError(f"{label}.criteria must map unique criterion IDs to supported observation operations")
        if criteria.get("all_cases_observed") != "all_cases_observed" or list(criteria.values()).count("all_cases_observed") != 1:
            raise StudyError(f"{label}.criteria must contain the reserved all_cases_observed ID/op exactly once")
        tasks[task["task_id"]] = task
    if len(repositories) != 1:
        raise StudyError("the frozen corpus must come from exactly one repository commit")
    return tasks


def validate_oracle_package_against_roster(
    document: Any,
    raw: bytes,
    freeze: dict[str, Any],
    tasks: dict[str, dict[str, Any]],
) -> tuple[dict[str, dict[str, Any]], str]:
    """Bind the exact evaluator package and every criterion operation to the frozen roster."""
    package_sha256 = digest_bytes(raw)
    if package_sha256 != freeze["oracle_package_sha256"]:
        raise StudyError("evaluator-only oracle package bytes do not match the pre-dispatch freeze")
    if not isinstance(document, dict) or document.get("task_package_sha256") != freeze["task_package_sha256"]:
        raise StudyError("oracle package is not bound to the frozen shared task-package bytes")
    try:
        oracle_tasks = RUNNER.validate_oracle_package(
            document,
            freeze["task_package_sha256"],
            set(tasks),
            allow_fixture=False,
        )
    except (RUNNER.RunnerError, TypeError, KeyError) as error:
        raise StudyError(f"frozen oracle package is invalid: {error}") from error
    if set(oracle_tasks) != set(tasks):
        raise StudyError("oracle package task IDs must exactly match the frozen roster")
    for task_id, roster_task in tasks.items():
        oracle_task = oracle_tasks[task_id]
        if oracle_task["oracle_sha256"] != roster_task["oracle_sha256"]:
            raise StudyError(f"task {task_id!r} oracle digest differs from the roster")
        if oracle_task["criteria"] != roster_task["criteria"]:
            raise StudyError(f"task {task_id!r} criterion IDs or operations differ between roster and oracle package")
        if roster_task["criteria"].get("all_cases_observed") != "all_cases_observed":
            raise StudyError(f"task {task_id!r} lacks the mandatory completion criterion")
    return oracle_tasks, package_sha256


def validate_task_package_against_roster(
    document: Any,
    raw: bytes,
    roster_document: Any,
    roster_raw: bytes,
    freeze: dict[str, Any],
    oracle_tasks: dict[str, dict[str, Any]],
) -> tuple[dict[str, dict[str, Any]], str]:
    """Check the exact shared brief bytes and their per-task roster commitments."""
    package_sha256 = digest_bytes(raw)
    if package_sha256 != freeze["task_package_sha256"]:
        raise StudyError("shared task package bytes do not match the pre-dispatch freeze")
    try:
        brief_tasks = RUNNER.validate_brief_package(document, allow_fixture=False)
        bound_roster_sha = RUNNER.validate_roster_binding(
            roster_document, roster_raw, raw, brief_tasks, oracle_tasks,
        )
    except (RUNNER.RunnerError, TypeError, KeyError) as error:
        raise StudyError(f"frozen task package does not match its roster/oracle bindings: {error}") from error
    if bound_roster_sha != freeze["task_roster_sha256"]:
        raise StudyError("runner task-package binding returned a different roster digest")
    return brief_tasks, package_sha256


def _archive_relative_path(value: Any, label: str) -> str:
    if not isinstance(value, str) or not value or "\\" in value or "\x00" in value:
        raise StudyError(f"{label} must be a nonempty POSIX relative path")
    path = PurePosixPath(value)
    if path.is_absolute() or any(part in {"", ".", ".."} for part in value.split("/")):
        raise StudyError(f"{label} may not be absolute or traverse directories")
    if path.as_posix() != value:
        raise StudyError(f"{label} must use its normalized POSIX spelling")
    return value


def _archive_child(root: Path, relative: Any, label: str, *, directory: bool = False) -> Path:
    rel = _archive_relative_path(relative, label)
    candidate = root
    for part in PurePosixPath(rel).parts:
        candidate = candidate / part
        if candidate.is_symlink():
            raise StudyError(f"{label} may not traverse a symbolic link")
    try:
        resolved = candidate.resolve(strict=True)
        resolved.relative_to(root)
    except (OSError, ValueError) as error:
        raise StudyError(f"{label} must resolve inside the attempt archive") from error
    if directory:
        if not resolved.is_dir():
            raise StudyError(f"{label} must name an attempt archive directory")
    elif not resolved.is_file():
        raise StudyError(f"{label} must name a regular archive file")
    return resolved


def _read_archive_json(path: Path, label: str, *, newline: bool) -> tuple[Any, bytes]:
    try:
        raw = path.read_bytes()
    except OSError as error:
        raise StudyError(f"cannot read archived {label}: {error}") from error
    document = strict_json_loads(raw, f"archived {label}")
    canonical = canonical_json_bytes(document)
    expected = canonical + (b"\n" if newline else b"")
    if raw != expected:
        raise StudyError(f"archived {label} is not in the runner's canonical JSON encoding")
    return document, raw


def _archive_inventory(root: Path) -> tuple[set[str], set[str]]:
    files: set[str] = set()
    directories: set[str] = set()
    for directory, dirnames, filenames in os.walk(root, topdown=True, followlinks=False):
        current = Path(directory)
        for dirname in list(dirnames):
            child = current / dirname
            if child.is_symlink():
                raise StudyError(f"attempt archive contains a symbolic-link directory: {child.relative_to(root)}")
            if not child.is_dir():
                raise StudyError(f"attempt archive contains a non-directory path: {child.relative_to(root)}")
            directories.add(child.relative_to(root).as_posix())
        for filename in filenames:
            child = current / filename
            relative = child.relative_to(root).as_posix()
            if child.is_symlink() or not child.is_file():
                raise StudyError(f"attempt archive contains a non-regular file: {relative}")
            files.add(relative)
    return files, directories


def _observed_tree_nodes(value: Any, label: str) -> list[dict[str, Any]]:
    """Validate the runner's raw workspace snapshot, including unrepresentable nodes."""
    if not isinstance(value, list) or len(value) > RUNNER.MAX_TREE_NODES:
        raise StudyError(f"{label}.nodes must be a bounded array")
    paths: set[str] = set()
    allowed = {"file", "directory", "symlink", "other"}
    for index, row in enumerate(value):
        item_label = f"{label}.nodes[{index}]"
        if not isinstance(row, dict) or not isinstance(row.get("type"), str):
            raise StudyError(f"{item_label} must be a node object")
        kind = row["type"]
        if kind not in allowed:
            raise StudyError(f"{item_label}.type is unsupported")
        if kind == "file":
            _keys(row, {"path", "type", "mode", "data_base64"}, item_label)
            try:
                data = base64.b64decode(row["data_base64"], validate=True)
            except (ValueError, TypeError) as error:
                raise StudyError(f"{item_label}.data_base64 is malformed") from error
            if base64.b64encode(data).decode("ascii") != row["data_base64"]:
                raise StudyError(f"{item_label}.data_base64 is not canonical base64")
        elif kind == "directory" or kind == "other":
            _keys(row, {"path", "type", "mode"}, item_label)
        else:
            _keys(row, {"path", "type", "target"}, item_label)
        path = _archive_relative_path(row["path"], f"{item_label}.path")
        if path == RUNNER.RESERVED_WORKSPACE_NAME or path.startswith(RUNNER.RESERVED_WORKSPACE_NAME + "/"):
            raise StudyError(f"{item_label} exposes the runner's reserved workspace directory")
        if path in paths:
            raise StudyError(f"{label}.nodes repeats path {path!r}")
        paths.add(path)
        if "mode" in row and (not isinstance(row["mode"], str) or not re.fullmatch(r"[0-7]{4}", row["mode"])):
            raise StudyError(f"{item_label}.mode must contain four octal digits")
        if kind == "symlink" and (not isinstance(row["target"], str) or not row["target"] or "\x00" in row["target"]):
            raise StudyError(f"{item_label}.target must be nonempty text")
    if [row["path"] for row in value] != sorted(paths):
        raise StudyError(f"{label}.nodes must be sorted by path")
    return value


def _archive_jsonl(path: Path, label: str) -> list[Any]:
    try:
        raw = path.read_bytes()
    except OSError as error:
        raise StudyError(f"cannot read archived {label}: {error}") from error
    if raw and not raw.endswith(b"\n"):
        raise StudyError(f"archived {label} must end with LF")
    rows = []
    for line_number, line in enumerate(raw.splitlines(keepends=True), 1):
        if not line.endswith(b"\n") or not line[:-1]:
            raise StudyError(f"archived {label} line {line_number} is blank or unterminated")
        row = strict_json_loads(line[:-1], f"archived {label} line {line_number}")
        if canonical_json_bytes(row) + b"\n" != line:
            raise StudyError(f"archived {label} line {line_number} is not canonical JSON")
        rows.append(row)
    return rows


def verify_attempt_archive(
    attempt: dict[str, Any],
    record: dict[str, Any],
    roster_task: dict[str, Any],
    oracle_task: dict[str, Any],
    archive_root: Path,
    freeze: dict[str, Any],
    roster_sha256: str,
    *,
    line_number: int,
    attempt_number: int,
) -> dict[str, Any]:
    """Verify one archive and recompute its criteria/status from raw case evidence.

    This establishes byte-level consistency between the row, frozen packages, source
    bundle and retained observation files. It is not a signature or write-once proof.
    """
    try:
        archive_base = archive_root.resolve(strict=True)
    except OSError as error:
        raise StudyError(f"attempt archive root is unavailable: {error}") from error
    if not archive_base.is_dir():
        raise StudyError("attempt archive root must be a directory")
    archive = _archive_child(archive_base, attempt["archive_ref"], f"results line {line_number} attempt {attempt_number} archive_ref", directory=True)
    manifest_path = _archive_child(archive, attempt["archive_manifest_ref"], "archive manifest")
    if attempt["archive_manifest_ref"] != "archive-manifest.json":
        raise StudyError("attempt archive_manifest_ref must be archive-manifest.json")
    manifest, manifest_raw = _read_archive_json(manifest_path, "attempt archive manifest", newline=True)
    _keys(manifest, {"schema", "files"}, "attempt archive manifest")
    if manifest["schema"] != "execute.real-bash-100-attempt-archive-manifest/1" or not isinstance(manifest["files"], list):
        raise StudyError("unsupported attempt archive manifest schema or files list")
    manifest_canonical = canonical_json_bytes(manifest)
    manifest_sha = digest_bytes(manifest_canonical)
    _hash(attempt["archive_manifest_sha256"], "attempt.archive_manifest_sha256")
    if manifest_sha != attempt["archive_manifest_sha256"]:
        raise StudyError("attempt archive manifest does not match the result row commitment")
    archive_files, archive_dirs = _archive_inventory(archive)
    if "archive-manifest.json" not in archive_files:
        raise StudyError("attempt archive lacks archive-manifest.json")
    expected_files: set[str] = set()
    for index, row in enumerate(manifest["files"]):
        label = f"attempt archive manifest.files[{index}]"
        _keys(row, {"path", "sha256", "bytes"}, label)
        relative = _archive_relative_path(row["path"], f"{label}.path")
        if relative == "archive-manifest.json" or relative in expected_files:
            raise StudyError(f"{label}.path is duplicated or names the manifest itself")
        _hash(row["sha256"], f"{label}.sha256")
        if type(row["bytes"]) is not int or row["bytes"] < 0:
            raise StudyError(f"{label}.bytes must be a nonnegative integer")
        expected_files.add(relative)
        file_path = _archive_child(archive, relative, label)
        raw = file_path.read_bytes()
        if len(raw) != row["bytes"] or digest_bytes(raw) != row["sha256"]:
            raise StudyError(f"archived file {relative!r} differs from the attempt manifest")
    if [row["path"] for row in manifest["files"]] != sorted(expected_files):
        raise StudyError("attempt archive manifest files must be uniquely sorted POSIX paths")
    actual_without_manifest = archive_files - {"archive-manifest.json"}
    if actual_without_manifest != expected_files:
        raise StudyError("attempt archive manifest does not enumerate every archived regular file exactly once")
    ancestor_dirs = {
        PurePosixPath(relative).parent.as_posix()
        for relative in expected_files
        if PurePosixPath(relative).parent.as_posix() != "."
    }
    all_ancestor_dirs: set[str] = set()
    for dirname in ancestor_dirs:
        parent = PurePosixPath(dirname)
        while parent.as_posix() != ".":
            all_ancestor_dirs.add(parent.as_posix())
            parent = parent.parent
    if archive_dirs != all_ancestor_dirs:
        raise StudyError("attempt archive contains an unmanifested empty or unexpected directory")

    observation_path = _archive_child(archive, "observation.json", "attempt observation")
    observation, observation_raw = _read_archive_json(observation_path, "attempt observation", newline=True)
    _hash(attempt["observation_sha256"], "attempt.observation_sha256")
    if digest_bytes(observation_raw) != attempt["observation_sha256"]:
        raise StudyError("attempt observation bytes differ from the result row commitment")
    observation_fields = {
        "schema", "trial_type", "run_id", "session_id", "task_id", "blind_arm_id", "dispatch_index",
        "attempt", "task_package_sha256", "oracle_package_sha256", "task_roster_sha256", "oracle_sha256",
        "criteria", "runner_source_sha256", "runtime", "limits", "sandbox_wrapper_sha256",
        "sandbox_policy_verified", "started_utc", "cases", "criteria_results", "ended_utc",
    }
    _keys(observation, observation_fields, "attempt observation")
    expected_identity = {
        "trial_type": "candidate_observation_not_trial",
        "run_id": record["run_id"], "session_id": record["session_id"],
        "task_id": record["task_id"], "blind_arm_id": record["blind_arm_id"],
        "dispatch_index": record["dispatch_index"],
        "task_package_sha256": freeze["task_package_sha256"],
        "oracle_package_sha256": freeze["oracle_package_sha256"],
        "task_roster_sha256": roster_sha256,
        "oracle_sha256": roster_task["oracle_sha256"],
        "criteria": roster_task["criteria"],
        "runner_source_sha256": freeze["runner_source_sha256"],
    }
    if observation["schema"] != "execute.real-bash-100-case-observation/1":
        raise StudyError("unsupported runner observation schema")
    for field, expected in expected_identity.items():
        if observation[field] != expected:
            raise StudyError(f"archived observation {field} does not match the frozen trial binding")
    for field in ("started_utc", "ended_utc"):
        _utc(observation[field], f"observation.{field}")
    started_at = datetime.fromisoformat(observation["started_utc"].replace("Z", "+00:00"))
    ended_at = datetime.fromisoformat(observation["ended_utc"].replace("Z", "+00:00"))
    if ended_at < started_at:
        raise StudyError("archived runner observation ends before it starts")
    runner_attempt = observation["attempt"]
    _keys(runner_attempt, {"stage", "attempt_index", "artifact_sha256", "artifact_ref"}, "observation.attempt")
    if runner_attempt["stage"] != attempt["stage"] or runner_attempt["attempt_index"] != attempt_number:
        raise StudyError("archived observation attempt stage/index differs from the result row")
    if runner_attempt["artifact_sha256"] != attempt["artifact_sha256"] or runner_attempt["artifact_ref"] != attempt["artifact_ref"]:
        raise StudyError("archived observation candidate binding differs from the result row")
    _utc(attempt["started_utc"], "result attempt.started_utc")
    if observation["started_utc"] != attempt["started_utc"]:
        raise StudyError("result attempt start timestamp differs from the archived runner observation")
    candidate_path = _archive_child(archive, runner_attempt["artifact_ref"], "observation candidate artifact")
    candidate_raw = candidate_path.read_bytes()
    if digest_bytes(candidate_raw) != runner_attempt["artifact_sha256"]:
        raise StudyError("archived candidate bytes do not match their observation digest")
    _hash(attempt["artifact_sha256"], "result attempt artifact_sha256")
    if digest_bytes(candidate_raw) != attempt["artifact_sha256"]:
        raise StudyError("result candidate digest differs from archived candidate bytes")
    if len(oracle_task["cases"]) == 0 or not isinstance(observation["cases"], list) or len(observation["cases"]) != len(oracle_task["cases"]):
        raise StudyError("observation cases do not match the frozen oracle case count")
    if not isinstance(observation["criteria_results"], dict) or set(observation["criteria_results"]) != set(roster_task["criteria"]):
        raise StudyError("observation criterion IDs do not exactly match the frozen roster")
    runtime = observation["runtime"]
    _keys(
        runtime,
        {"version_label", "version_label_sha256", "runtime_argv_sha256", "runtime_binary_sha256",
         "sandbox_wrapper_executable_sha256", "machine", "machine_sha256", "python_executable_sha256",
         "runner_source_files", "runner_source_sha256"},
        "observation.runtime",
    )
    if not isinstance(runtime["version_label"], str) or not runtime["version_label"]:
        raise StudyError("observation runtime version label must be nonempty")
    for field in ("version_label_sha256", "runtime_argv_sha256", "runtime_binary_sha256", "machine_sha256", "python_executable_sha256", "runner_source_sha256"):
        _hash(runtime[field], f"observation.runtime.{field}")
    if runtime["sandbox_wrapper_executable_sha256"] is not None:
        _hash(runtime["sandbox_wrapper_executable_sha256"], "observation.runtime.sandbox_wrapper_executable_sha256")
    if digest_bytes(runtime["version_label"].encode("utf-8")) != runtime["version_label_sha256"]:
        raise StudyError("runtime version label digest does not match its text")
    if not isinstance(runtime["machine"], dict) or digest_bytes(canonical_json_bytes(runtime["machine"])) != runtime["machine_sha256"]:
        raise StudyError("runtime machine receipt digest does not match its content")
    if runtime["runner_source_sha256"] != freeze["runner_source_sha256"] or runtime["runner_source_sha256"] != observation["runner_source_sha256"]:
        raise StudyError("archived runtime runner-source digest differs from the frozen runner bundle")
    sources = runtime["runner_source_files"]
    if not isinstance(sources, dict) or set(sources) != {"real_bash_100_runner.py", "evaluate.py"}:
        raise StudyError("runtime runner-source file set is incomplete or unexpected")
    for filename, source_sha in sources.items():
        _hash(source_sha, f"observation.runtime.runner_source_files.{filename}")
        source_file = _archive_child(archive, f"runner-source/{filename}", "archived runner source")
        if digest_file(source_file) != source_sha:
            raise StudyError(f"archived runner source {filename!r} differs from the observation receipt")
    if digest_bytes(canonical_json_bytes(sources)) != runtime["runner_source_sha256"]:
        raise StudyError("runner source aggregate digest does not match the per-file source receipt")
    if type(observation["sandbox_policy_verified"]) is not bool or observation["sandbox_policy_verified"]:
        raise StudyError("runner observation must not assert a sandbox policy it cannot independently attest")
    if observation["sandbox_wrapper_sha256"] is not None:
        _hash(observation["sandbox_wrapper_sha256"], "observation.sandbox_wrapper_sha256")
    limits = observation["limits"]
    if not isinstance(limits, dict) or set(limits) != {"timeout_seconds", "cpu_seconds", "memory_bytes", "output_bytes", "file_bytes"} or any(type(value) is not int or value <= 0 for value in limits.values()):
        raise StudyError("observation limits must contain positive frozen runner limits")

    derived_by_criterion: dict[str, list[bool | None]] = {key: [] for key in roster_task["criteria"]}
    statuses = {"completed", "timeout", "output_limit", "resource_limit", "launch_error", "infrastructure_error"}
    for index, (case_observation, oracle_case) in enumerate(zip(observation["cases"], oracle_task["cases"])):
        label = f"observation.cases[{index}]"
        required_case_fields = {
            "case_id", "status", "returncode", "process_status", "runner_error", "path_bindings",
            "filesystem_observation_status", "filesystem_observation_limit", "case_template", "case_materialized",
            "stdout", "stderr", "filesystem_before", "filesystem_after", "mock_events", "criteria_results",
        }
        _keys(case_observation, required_case_fields, label)
        case_id = oracle_case["case_id"]
        if case_observation["case_id"] != case_id:
            raise StudyError(f"{label}.case_id does not match the frozen oracle ordering")
        process_status = case_observation["process_status"]
        final_status = case_observation["status"]
        if not isinstance(process_status, str) or process_status not in statuses or not isinstance(final_status, str) or final_status not in statuses:
            raise StudyError(f"{label} has an unsupported candidate/runner status")
        filesystem_status = case_observation["filesystem_observation_status"]
        if not isinstance(filesystem_status, str) or filesystem_status not in {"complete", "candidate_resource_limit", "infrastructure_error"}:
            raise StudyError(f"{label}.filesystem_observation_status is unsupported")
        if filesystem_status == "complete":
            if final_status != process_status or case_observation["filesystem_observation_limit"] is not None:
                raise StudyError(f"{label} completion status contradicts its filesystem observation")
        elif filesystem_status == "candidate_resource_limit":
            if final_status != "resource_limit" or not isinstance(case_observation["filesystem_observation_limit"], dict):
                raise StudyError(f"{label} candidate snapshot limit metadata is inconsistent")
        elif final_status != "infrastructure_error" or case_observation["filesystem_observation_limit"] is not None:
            raise StudyError(f"{label} infrastructure snapshot metadata is inconsistent")
        if case_observation["runner_error"] is not None and not isinstance(case_observation["runner_error"], str):
            raise StudyError(f"{label}.runner_error must be text or null")
        returncode = case_observation["returncode"]
        if process_status in {"launch_error", "infrastructure_error"}:
            if returncode is not None:
                raise StudyError(f"{label} unavailable process observation must have null returncode")
        elif type(returncode) is not int or not -255 <= returncode <= 255:
            raise StudyError(f"{label}.returncode must be an observed process status")

        bindings = case_observation["path_bindings"]
        _keys(bindings, {"workspace", "home", "tmp"}, f"{label}.path_bindings")
        if any(not isinstance(bindings[key], str) or not Path(bindings[key]).is_absolute() for key in bindings):
            raise StudyError(f"{label}.path_bindings must contain absolute paths")
        workspace = Path(bindings["workspace"])
        if bindings["home"] != str(workspace / "home") or bindings["tmp"] != str(workspace / "tmp"):
            raise StudyError(f"{label}.path_bindings do not have the runner's exact home/tmp relation")
        template = oracle_case["_runner_template"]
        template_bytes = canonical_json_bytes(template)
        template_ref = case_observation["case_template"]
        _keys(template_ref, {"artifact_ref", "sha256"}, f"{label}.case_template")
        template_path = _archive_child(archive, template_ref["artifact_ref"], f"{label}.case_template ref")
        stored_template, stored_template_raw = _read_archive_json(template_path, f"{label} case template", newline=True)
        if stored_template_raw != template_bytes + b"\n" or digest_bytes(template_bytes) != template_ref["sha256"]:
            raise StudyError(f"{label} case template differs from the frozen oracle bytes")
        try:
            expanded = RUNNER._expand_case_template(template, workspace)
            RUNNER._refresh_materialized_case_expectations(expanded, workspace)
            materialized = RUNNER._validate_case(expanded, f"materialized case {case_id}")
            RUNNER._refresh_materialized_case_expectations(materialized, workspace)
        except (RUNNER.RunnerError, TypeError, KeyError) as error:
            raise StudyError(f"{label} cannot be independently materialized from the frozen oracle: {error}") from error
        materialized_ref = case_observation["case_materialized"]
        _keys(materialized_ref, {"artifact_ref", "sha256"}, f"{label}.case_materialized")
        materialized_path = _archive_child(archive, materialized_ref["artifact_ref"], f"{label}.case_materialized ref")
        stored_materialized, stored_materialized_raw = _read_archive_json(materialized_path, f"{label} materialized case", newline=True)
        materialized_bytes = canonical_json_bytes(materialized)
        if stored_materialized_raw != materialized_bytes + b"\n" or digest_bytes(materialized_bytes) != materialized_ref["sha256"]:
            raise StudyError(f"{label} materialized case does not reproduce from the frozen template and path bindings")

        raw_artifacts: dict[str, bytes] = {}
        for field in ("stdout", "stderr"):
            ref = case_observation[field]
            _keys(ref, {"artifact_ref", "sha256", "bytes"}, f"{label}.{field}")
            artifact_path = _archive_child(archive, ref["artifact_ref"], f"{label}.{field} ref")
            raw = artifact_path.read_bytes()
            if type(ref["bytes"]) is not int or ref["bytes"] != len(raw) or digest_bytes(raw) != ref["sha256"]:
                raise StudyError(f"{label}.{field} bytes do not match the observation reference")
            raw_artifacts[field] = raw
        snapshot_nodes: dict[str, list[dict[str, Any]]] = {}
        for field in ("filesystem_before", "filesystem_after"):
            ref = case_observation[field]
            _keys(ref, {"artifact_ref", "sha256"}, f"{label}.{field}")
            snapshot_path = _archive_child(archive, ref["artifact_ref"], f"{label}.{field} ref")
            snapshot, snapshot_raw = _read_archive_json(snapshot_path, f"{label} {field} snapshot", newline=False)
            _keys(snapshot, {"schema", "nodes"}, f"{label}.{field} snapshot")
            if snapshot["schema"] != "execute.real-bash-100-filesystem-snapshot/1" or digest_bytes(snapshot_raw) != ref["sha256"]:
                raise StudyError(f"{label}.{field} snapshot schema or digest is invalid")
            snapshot_nodes[field] = _observed_tree_nodes(snapshot["nodes"], f"{label}.{field}")
        expected_before = list(materialized["filesystem_initial"])
        initial_by_path = {node["path"]: node for node in expected_before}
        for needed in ("home", "tmp"):
            if needed in initial_by_path:
                if initial_by_path[needed]["type"] != "directory":
                    raise StudyError(f"{label} oracle initial tree cannot provide runner {needed} directory")
            else:
                expected_before.append({"path": needed, "type": "directory", "mode": "0755"})
        try:
            expected_before = RUNNER._validate_tree(expected_before, f"{label} expected initial tree")
        except RUNNER.RunnerError as error:
            raise StudyError(f"{label} has an invalid initial tree: {error}") from error
        if snapshot_nodes["filesystem_before"] != expected_before:
            raise StudyError(f"{label} initial workspace snapshot differs from the frozen fixture")

        mock_ref = case_observation["mock_events"]
        _keys(mock_ref, {"artifact_ref", "sha256", "count"}, f"{label}.mock_events")
        mock_path = _archive_child(archive, mock_ref["artifact_ref"], f"{label}.mock_events ref")
        observed_mock_events = _archive_jsonl(mock_path, f"{label} mock events")
        if digest_file(mock_path) != mock_ref["sha256"] or type(mock_ref["count"]) is not int or mock_ref["count"] != len(observed_mock_events):
            raise StudyError(f"{label} mock event stream differs from its observation reference")

        expected = materialized["expected"]
        available = process_status not in {"launch_error", "infrastructure_error"}
        process_match = None if not available else (
            returncode == expected["returncode"]
            and raw_artifacts["stdout"] == RUNNER._b64(expected["stdout_base64"], f"{label} expected stdout")
            and raw_artifacts["stderr"] == RUNNER._b64(expected["stderr_base64"], f"{label} expected stderr")
        )
        fs_status = case_observation["filesystem_observation_status"]
        if fs_status == "infrastructure_error":
            filesystem_match = None
        elif fs_status == "candidate_resource_limit":
            filesystem_match = False
        else:
            filesystem_match = snapshot_nodes["filesystem_after"] == expected["filesystem_after"]
        mock_match = None if not available else observed_mock_events == expected["mock_events"]
        case_criteria = {
            "process_io_exact": process_match,
            "filesystem_final_exact": filesystem_match,
            "mock_command_effects_exact": mock_match,
        }
        if case_observation["criteria_results"] != case_criteria:
            raise StudyError(f"{label} self-reported comparison differs from independently recomputed raw evidence")
        all_observed: bool | None
        if process_status in {"timeout", "output_limit", "resource_limit"} or fs_status == "candidate_resource_limit":
            all_observed = False
        elif process_status in {"launch_error", "infrastructure_error"} or fs_status == "infrastructure_error":
            all_observed = None
        else:
            all_observed = process_status == "completed" and fs_status == "complete"
        for criterion_id, operation in roster_task["criteria"].items():
            if operation == "all_cases_observed":
                value = all_observed
            else:
                value = case_criteria[operation]
            derived_by_criterion[criterion_id].append(value)
    derived = {
        criterion: (False if False in values else None if any(value is None for value in values) else True)
        for criterion, values in derived_by_criterion.items()
    }
    if observation["criteria_results"] != derived:
        raise StudyError("top-level runner criteria do not match the importer-recomputed case outcomes")
    if attempt["criteria_results"] != derived:
        raise StudyError("result row criteria are self-reported incorrectly; archive-derived values differ")
    status = "fail" if False in derived.values() else "infrastructure_error" if any(value is None for value in derived.values()) else "pass"
    if attempt["oracle_status"] != status:
        raise StudyError("result row oracle_status differs from archive-derived criteria")
    return {"oracle_status": status, "criteria_results": derived}


def validate_roster_against_candidate(tasks: dict[str, dict[str, Any]], candidate: dict[str, Any]) -> None:
    source_records = candidate.get("_source_records_by_task")
    if not isinstance(source_records, dict) or set(source_records) != set(tasks):
        raise StudyError("frozen roster task IDs must exactly match the verified 100-source manifest")
    for task_id, task in tasks.items():
        source = source_records[task_id]
        for roster_field, candidate_field in (
            ("family_id", "family_id"), ("repo_url", "repo_url"),
            ("source_commit", "source_commit"), ("source_path", "source_path"),
            ("source_sha256", "source_sha256"),
            ("manifest_record_sha256", "manifest_record_sha256"),
            ("testability_stratum", "testability_stratum"),
        ):
            if task[roster_field] != source[candidate_field]:
                raise StudyError(f"roster task {task_id!r} {roster_field} differs from its candidate manifest record")


def make_run_plan(roster: dict[str, Any], roster_sha256: str, blind_arm_ids: list[str], seed: int) -> dict[str, Any]:
    """Create and freeze a balanced randomized order without exposing arm labels."""
    if (
        roster.get("schema") != "execute.ai-productivity.real-bash-100-roster/1"
        or roster.get("status") != "frozen_before_generation"
        or len(roster.get("tasks", [])) != 100
        or len(blind_arm_ids) != 3
    ):
        raise StudyError("a run plan requires 100 tasks and exactly three blind arm IDs")
    if any(not isinstance(item, str) for item in blind_arm_ids) or len(set(blind_arm_ids)) != 3 or any(not ARM_RE.fullmatch(item) for item in blind_arm_ids):
        raise StudyError("blind arm IDs must be three distinct opaque arm_ IDs")
    protocol, _, _ = read_json(PROTOCOL, "focused preregistration")
    if type(seed) is not int or seed != protocol["analysis"]["seed"]:
        raise StudyError("run-plan seed must equal the preregistered seed")
    _hash(roster_sha256, "run-plan roster hash")
    rng = random.Random(seed)
    tasks = [task["task_id"] for task in roster["tasks"]]
    rng.shuffle(tasks)
    blind_arm_ids = sorted(blind_arm_ids)
    cells = []
    for task_id in tasks:
        order = list(blind_arm_ids)
        rng.shuffle(order)
        for blind_id in order:
            cells.append({
                "dispatch_index": len(cells) + 1,
                "task_id": task_id,
                "blind_arm_id": blind_id,
                "replicate": 1,
            })
    return {
        "schema": "execute.ai-productivity.real-bash-100-run-plan/1",
        "status": "frozen_before_dispatch",
        "randomization_seed": seed,
        "task_roster_sha256": roster_sha256,
        "blind_arm_ids": sorted(blind_arm_ids),
        "cells": cells,
    }


def validate_run_plan(plan: Any, freeze: dict[str, Any], plan_sha256: str, roster_sha256: str, tasks: dict[str, dict[str, Any]], arms: dict[str, Any]) -> dict[int, dict[str, Any]]:
    fields = {"schema", "status", "randomization_seed", "task_roster_sha256", "blind_arm_ids", "cells"}
    _keys(plan, fields, "run plan")
    if plan["schema"] != "execute.ai-productivity.real-bash-100-run-plan/1" or plan["status"] != "frozen_before_dispatch":
        raise StudyError("run plan is not a frozen focused-study plan")
    if plan_sha256 != freeze["randomized_run_plan_sha256"]:
        raise StudyError("run-plan bytes do not match the pre-dispatch freeze")
    if plan["task_roster_sha256"] != roster_sha256 or plan["randomization_seed"] != freeze["randomization_seed"]:
        raise StudyError("run plan roster or seed differs from the freeze")
    arm_ids = set(arms)
    plan_arm_ids = plan["blind_arm_ids"]
    if (
        not isinstance(plan_arm_ids, list)
        or len(plan_arm_ids) != 3
        or any(not isinstance(item, str) or not ARM_RE.fullmatch(item) for item in plan_arm_ids)
        or len(set(plan_arm_ids)) != 3
        or set(plan_arm_ids) != arm_ids
    ):
        raise StudyError("run plan must contain exactly the three sealed arm IDs")
    cells = plan["cells"]
    if not isinstance(cells, list) or len(cells) != 300:
        raise StudyError("run plan must contain exactly 300 task/arm cells")
    expected = {(task_id, arm_id) for task_id in tasks for arm_id in arm_ids}
    seen = set()
    by_dispatch: dict[int, dict[str, Any]] = {}
    for index, cell in enumerate(cells, 1):
        _keys(cell, {"dispatch_index", "task_id", "blind_arm_id", "replicate"}, f"run plan cell {index}")
        if type(cell["dispatch_index"]) is not int or cell["dispatch_index"] != index or type(cell["replicate"]) is not int or cell["replicate"] != 1:
            raise StudyError("run-plan dispatch indices must be contiguous and replicate must equal one")
        if not isinstance(cell["task_id"], str) or not isinstance(cell["blind_arm_id"], str):
            raise StudyError(f"run plan cell {index} task and arm IDs must be strings")
        key = (cell["task_id"], cell["blind_arm_id"])
        if key not in expected or key in seen:
            raise StudyError(f"run plan contains an unknown or duplicate cell: {key}")
        if cell["task_id"] not in tasks or cell["blind_arm_id"] not in arm_ids:
            raise StudyError(f"run plan contains an unknown task or arm: {key}")
        seen.add(key)
        by_dispatch[index] = cell
    if seen != expected:
        raise StudyError(f"run plan omits {len(expected - seen)} of the 300 required task/arm cells")
    for start in range(0, 300, 3):
        block = cells[start:start + 3]
        if len({cell["task_id"] for cell in block}) != 1 or {cell["blind_arm_id"] for cell in block} != arm_ids:
            raise StudyError("each script block must contain its three arms in a randomized contiguous order")
    return by_dispatch


def validate_arm_map(document: Any, raw_sha256: str, freeze: dict[str, Any]) -> dict[str, dict[str, Any]]:
    _keys(document, {"schema", "arms"}, "arm map")
    if document["schema"] != "execute.ai-productivity.real-bash-100-arms/1":
        raise StudyError("unsupported focused arm-map schema")
    if raw_sha256 != freeze["arm_map_sha256"]:
        raise StudyError("arm-map bytes do not match the pre-dispatch freeze")
    arms = document["arms"]
    if not isinstance(arms, dict) or len(arms) != 3:
        raise StudyError("focused arm map must contain exactly three arms")
    variants = set()
    common = None
    for blind_id, arm in arms.items():
        if not ARM_RE.fullmatch(blind_id):
            raise StudyError("arm-map keys must be opaque arm_ identifiers")
        fields = {
            "variant", "provider", "model_alias", "model_snapshot_status", "model_snapshot",
            "comparison_block", "generation_parameters", "language_version",
            "language_reference_sha256", "programmer_prompt_sha256",
            "language_instruction_sha256", "tool_configuration_sha256",
        }
        _keys(arm, fields, f"arm map {blind_id}")
        if not isinstance(arm["variant"], str) or arm["variant"] not in VARIANTS or arm["variant"] in variants:
            raise StudyError("arm map must map one blind ID to each supported language exactly once")
        variants.add(arm["variant"])
        if not isinstance(arm["provider"], str) or not arm["provider"]:
            raise StudyError(f"arm map {blind_id}.provider must be nonempty")
        if arm["model_alias"] != "gpt-6-luna":
            raise StudyError("all focused arms must use the preregistered gpt-6-luna selection")
        if not isinstance(arm["model_snapshot_status"], str) or arm["model_snapshot_status"] not in {"recorded", "unavailable"}:
            raise StudyError("model_snapshot_status must disclose whether an immutable snapshot is exposed")
        snapshot = arm["model_snapshot"]
        if arm["model_snapshot_status"] == "recorded" and (not isinstance(snapshot, str) or not snapshot):
            raise StudyError("recorded model snapshots need a nonempty identifier")
        if arm["model_snapshot_status"] == "unavailable" and snapshot is not None:
            raise StudyError("an unavailable model snapshot must be null")
        if arm["comparison_block"] != freeze["comparison_block"]:
            raise StudyError("arm comparison block differs from the freeze")
        if not isinstance(arm["generation_parameters"], dict):
            raise StudyError("generation_parameters must be an object with explicit nulls for unavailable settings")
        if not isinstance(arm["language_version"], str) or not arm["language_version"]:
            raise StudyError("language_version must be recorded")
        for field in (
            "language_reference_sha256", "programmer_prompt_sha256",
            "language_instruction_sha256", "tool_configuration_sha256",
        ):
            _hash(arm[field], f"arm map {blind_id}.{field}")
        shared = (
            arm["provider"], arm["model_alias"], arm["model_snapshot_status"], arm["model_snapshot"],
            arm["comparison_block"],
            arm["generation_parameters"], arm["programmer_prompt_sha256"],
            arm["tool_configuration_sha256"],
        )
        if common is None:
            common = shared
        elif common != shared:
            raise StudyError("provider, model selection and snapshot, generation settings, task prompt, and tool config must match across arms")
        if arm["tool_configuration_sha256"] != freeze["tool_configuration_sha256"]:
            raise StudyError("arm tool configuration differs from the frozen tool configuration")
    if variants != VARIANTS:
        raise StudyError("arm map is missing a preregistered language")
    return arms


def _validate_unavailable_metrics(attempt: Any, label: str) -> None:
    _keys(attempt["usage"], {"basis", "input_tokens", "output_tokens", "reasoning_tokens", "cached_input_tokens"}, f"{label}.usage")
    if attempt["usage"] != {
        "basis": "unavailable", "input_tokens": None, "output_tokens": None,
        "reasoning_tokens": None, "cached_input_tokens": None,
    }:
        raise StudyError(f"{label}.usage must remain unavailable with null token counters")
    _keys(attempt["tool_calls"], {"basis", "count"}, f"{label}.tool_calls")
    if attempt["tool_calls"] != {"basis": "unavailable", "count": None}:
        raise StudyError(f"{label}.tool_calls must remain explicitly unavailable")
    _keys(attempt["cost"], {"basis", "amount", "currency"}, f"{label}.cost")
    if attempt["cost"] != {"basis": "unavailable", "amount": None, "currency": None}:
        raise StudyError(f"{label}.cost must remain unavailable with null amount/currency")


def _attempt_passes(attempt: dict[str, Any], task: dict[str, Any]) -> bool:
    return attempt["oracle_status"] == "pass" and all(
        attempt["criteria_results"][criterion] is True for criterion in task["criteria"]
    )


def _behavior_complete(attempt: dict[str, Any]) -> bool:
    return attempt["oracle_status"] in {"pass", "fail", "invalid_artifact"}


def validate_trial(
    record: Any,
    tasks: dict[str, dict[str, Any]],
    freeze: dict[str, Any],
    freeze_sha256: str,
    roster_sha256: str,
    plan_by_dispatch: dict[int, dict[str, Any]],
    *,
    production: bool = False,
    verified_attempt_indices: set[int] | None = None,
) -> dict[str, Any]:
    _keys(record, RESULT_KEYS, "trial result")
    if record["schema"] != "execute.ai-productivity.real-bash-100-run/1":
        raise StudyError("unsupported focused run-record schema")
    if production and record["trial_type"] != "evaluation":
        raise StudyError("synthetic fixtures cannot be imported as evaluation trials")
    if not isinstance(record["run_id"], str):
        raise StudyError("run_id must be a UUID")
    try:
        uuid.UUID(record["run_id"])
    except ValueError as error:
        raise StudyError("run_id must be a UUID") from error
    if not isinstance(record["session_id"], str) or not record["session_id"]:
        raise StudyError("session_id must identify one fresh programmer session")
    if not isinstance(record["task_id"], str) or not ID_RE.fullmatch(record["task_id"]):
        raise StudyError("task_id is malformed")
    if record["task_id"] not in tasks:
        raise StudyError(f"unknown task ID {record['task_id']!r}")
    if not isinstance(record["blind_arm_id"], str) or not ARM_RE.fullmatch(record["blind_arm_id"]):
        raise StudyError("blind_arm_id must be an opaque arm_ identifier")
    if type(record["replicate"]) is not int or record["replicate"] != 1:
        raise StudyError("the preregistered design has one session per script/language cell")
    _hash(record["freeze_sha256"], "trial.freeze_sha256")
    if record["freeze_sha256"] != freeze_sha256:
        raise StudyError("trial references different freeze bytes")
    for result_field, freeze_field in RESULT_FREEZE_FIELDS.items():
        _hash(record[result_field], f"trial.{result_field}")
        if record[result_field] != freeze[freeze_field]:
            raise StudyError(f"trial.{result_field} differs from the frozen commitment")
    dispatch = record["dispatch_index"]
    if type(dispatch) is not int or dispatch not in plan_by_dispatch:
        raise StudyError("dispatch_index must reference a frozen schedule cell")
    planned = plan_by_dispatch[dispatch]
    if (record["task_id"], record["blind_arm_id"]) != (planned["task_id"], planned["blind_arm_id"]):
        raise StudyError("trial task/arm does not match its frozen dispatch index")
    attempts = record["attempts"]
    if not isinstance(attempts, list) or not 1 <= len(attempts) <= 3:
        raise StudyError("a cell needs one generation attempt and may have at most two repairs")
    task = tasks[record["task_id"]]
    criterion_ids = set(task["criteria"])
    previous = None
    for index, attempt in enumerate(attempts):
        label = f"trial.attempts[{index}]"
        _keys(attempt, ATTEMPT_KEYS, label)
        expected_stage = "generate" if index == 0 else "repair"
        if attempt["stage"] != expected_stage:
            raise StudyError(f"{label}.stage must be {expected_stage!r}")
        _hash(attempt["artifact_sha256"], f"{label}.artifact_sha256")
        if not isinstance(attempt["artifact_ref"], str) or not attempt["artifact_ref"]:
            raise StudyError(f"{label}.artifact_ref must be an immutable archive reference")
        _archive_relative_path(attempt["artifact_ref"], f"{label}.artifact_ref")
        _archive_relative_path(attempt["archive_ref"], f"{label}.archive_ref")
        if attempt["archive_manifest_ref"] != "archive-manifest.json":
            raise StudyError(f"{label}.archive_manifest_ref must be archive-manifest.json")
        _hash(attempt["archive_manifest_sha256"], f"{label}.archive_manifest_sha256")
        _hash(attempt["observation_sha256"], f"{label}.observation_sha256")
        _utc(attempt["started_utc"], f"{label}.started_utc")
        elapsed, basis = attempt["conductor_elapsed_ms"], attempt["elapsed_basis"]
        if basis == "dispatch_to_response_wall_clock":
            if type(elapsed) is not int or elapsed < 0:
                raise StudyError(f"{label} needs elapsed milliseconds for the declared clock basis")
        elif basis == "unavailable":
            if elapsed is not None:
                raise StudyError(f"{label} must use null elapsed time when unavailable")
        else:
            raise StudyError(f"{label}.elapsed_basis is unsupported")
        _validate_unavailable_metrics(attempt, label)
        status = attempt["oracle_status"]
        if not isinstance(status, str) or status not in {"pass", "fail", "invalid_artifact", "infrastructure_error", "provider_refusal", "provider_timeout"}:
            raise StudyError(f"{label}.oracle_status is unsupported")
        if production and status not in {"pass", "fail", "infrastructure_error"}:
            raise StudyError(f"{label} status requires a separate provider-event evidence adapter; this importer accepts only archived candidate observations")
        results = attempt["criteria_results"]
        if not isinstance(results, dict) or results.keys() != criterion_ids:
            raise StudyError(f"{label}.criteria_results must match the task criteria exactly")
        if not all(value is None or type(value) is bool for value in results.values()):
            raise StudyError(f"{label}.criteria_results accepts booleans or null only")
        failed = {criterion for criterion, value in results.items() if value is False}
        if status == "pass" and any(value is not True for value in results.values()):
            raise StudyError(f"{label} cannot pass with null or failed mandatory criteria")
        if status == "fail" and not failed:
            raise StudyError(f"{label} behavioral failure must identify at least one failed mandatory criterion")
        if status == "invalid_artifact" and not failed:
            raise StudyError(f"{label} invalid artifact must fail at least one declared criterion")
        if status == "infrastructure_error" and (failed or not any(value is None for value in results.values())):
            raise StudyError(f"{label} infrastructure outcome may retain known passes but needs unevaluated criteria and no candidate failure")
        if status in {"provider_refusal", "provider_timeout"} and any(value is not None for value in results.values()):
            raise StudyError(f"{label} provider refusal/timeout must leave criteria unevaluated (null)")
        feedback = attempt["diagnostic_feedback_sent"]
        if not isinstance(feedback, list) or any(not isinstance(item, str) or not CRITERION_RE.fullmatch(item) for item in feedback):
            raise StudyError(f"{label}.diagnostic_feedback_sent must contain criterion IDs")
        if index == 0:
            if feedback:
                raise StudyError("generation cannot receive repair feedback")
        else:
            if previous["oracle_status"] not in {"fail", "invalid_artifact"}:
                raise StudyError("repair is permitted only after a behavioral failure or invalid artifact")
            prior_failed = sorted(key for key, value in previous["criteria_results"].items() if value is False)
            if sorted(feedback) != prior_failed:
                raise StudyError("repair feedback must equal the prior attempt's failed criterion IDs")
        previous = attempt

    if production and verified_attempt_indices != set(range(len(attempts))):
        raise StudyError("production results require every attempt to be derived from its verified archive and frozen oracle")

    runtime = record["runtime"]
    _keys(runtime, {"status", "timing_case_set_sha256", "warmup_runs", "repetitions_ms"}, "trial.runtime")
    _hash(runtime["timing_case_set_sha256"], "trial.runtime.timing_case_set_sha256")
    if runtime["timing_case_set_sha256"] != task["timing_case_set_sha256"]:
        raise StudyError("runtime timing-case hash differs from its frozen task")
    final = attempts[-1]
    runtime_status = runtime["status"]
    if not isinstance(runtime_status, str):
        raise StudyError("runtime.status must be a string")
    repetitions = runtime["repetitions_ms"]
    if not isinstance(repetitions, list) or any(type(value) is not int or value < 0 for value in repetitions):
        raise StudyError("runtime repetitions must be nonnegative integer milliseconds")
    if runtime_status == "measured":
        if not _attempt_passes(final, task):
            raise StudyError("runtime is measured only for a final artifact passing every mandatory criterion")
        if type(runtime["warmup_runs"]) is not int or runtime["warmup_runs"] != 1 or len(repetitions) != 10:
            raise StudyError("measured runtime requires one warm-up and ten timed repetitions")
    else:
        if repetitions:
            raise StudyError("unmeasured runtime must not contain numeric timings")
        if runtime["warmup_runs"] is not None:
            raise StudyError("unmeasured runtime must use a null warmup_runs value")
        if _attempt_passes(final, task) and runtime_status not in {"measurement_error", "unavailable"}:
            raise StudyError("a passing artifact has an invalid runtime status")
        if not _attempt_passes(final, task) and _behavior_complete(final) and runtime_status != "not_measured_functional_failure":
            raise StudyError("a final functional failure must be explicitly excluded from runtime")
        if not _behavior_complete(final) and runtime_status != "not_measured_incomplete_outcome":
            raise StudyError("an incomplete outcome must be explicitly excluded from runtime")
        if runtime_status not in {"measurement_error", "unavailable", "not_measured_functional_failure", "not_measured_incomplete_outcome"}:
            raise StudyError("runtime status is unsupported")

    review = record["review"]
    _keys(review, {"status", "ratings", "adjudication", "score_locked_utc"}, "trial.review")
    if not isinstance(review["status"], str):
        raise StudyError("review.status must be a string")
    ratings = review["ratings"]
    if not isinstance(ratings, list):
        raise StudyError("review.ratings must be an array")
    if review["status"] == "pending":
        if ratings or review["adjudication"] is not None or review["score_locked_utc"] is not None:
            raise StudyError("pending review must not contain scores")
    elif review["status"] == "not_rateable":
        if _behavior_complete(final) or ratings or review["adjudication"] is not None or review["score_locked_utc"] is not None:
            raise StudyError("not_rateable is only for a final nonbehavior outcome with no code to rate")
    elif review["status"] == "locked":
        _utc(review["score_locked_utc"], "trial.review.score_locked_utc")
        if len(ratings) != 2:
            raise StudyError("locked review needs two independent ratings")
        first_scores = []
        rater_ids = set()
        for index, rating in enumerate(ratings):
            _validate_rating(rating, f"trial.review.ratings[{index}]", rater_ids)
            rater_ids.add(rating["rater_id"])
            first_scores.append(tuple(rating[key] for key in RATING_DIMENSIONS))
        disagreement = first_scores[0] != first_scores[1]
        adjudication = review["adjudication"]
        if disagreement and adjudication is None:
            raise StudyError("disagreeing masked ratings require a third-rater adjudication")
        if not disagreement and adjudication is not None:
            raise StudyError("adjudication must be null when the first two ratings agree exactly")
        if adjudication is not None:
            _validate_rating(adjudication, "trial.review.adjudication", rater_ids)
    else:
        raise StudyError("review.status must be pending, locked, or not_rateable")
    if production and record["trial_type"] != "evaluation":
        raise StudyError("non-evaluation records are not importable")
    return record


def _validate_rating(rating: Any, label: str, existing_raters: set[str]) -> None:
    _keys(rating, {"rater_id", "rubric_sha256", *RATING_DIMENSIONS}, label)
    if not isinstance(rating["rater_id"], str) or not rating["rater_id"]:
        raise StudyError(f"{label}.rater_id must be nonempty")
    if rating["rater_id"] in existing_raters:
        raise StudyError("human ratings must have distinct raters")
    if rating["rubric_sha256"] != digest_file(RUBRIC):
        raise StudyError(f"{label}.rubric_sha256 differs from the frozen focused rubric")
    for dimension in RATING_DIMENSIONS:
        if type(rating[dimension]) is not int or not 1 <= rating[dimension] <= 5:
            raise StudyError(f"{label}.{dimension} must be an integer from 1 to 5")


def read_trial_rows(
    path: Path,
    tasks: dict[str, dict[str, Any]],
    freeze: dict[str, Any],
    freeze_sha256: str,
    roster_sha256: str,
    plan_by_dispatch: dict[int, dict[str, Any]],
    *,
    oracle_tasks: dict[str, dict[str, Any]] | None = None,
    attempt_archive_root: Path | None = None,
    production: bool = True,
) -> list[dict[str, Any]]:
    records = []
    seen_cells, seen_dispatch, seen_runs, seen_sessions = set(), set(), set(), set()
    try:
        source = path.open(encoding="utf-8")
    except OSError as error:
        raise StudyError(f"cannot read trial rows {path}: {error}") from error
    try:
        with source:
            for line_number, line in enumerate(source, 1):
                if not line.strip():
                    raise StudyError(f"results line {line_number} is blank; JSONL rows must be explicit")
                record = strict_json_loads(line, f"results line {line_number}")
                if production and (oracle_tasks is None or attempt_archive_root is None):
                    raise StudyError("production import requires frozen oracle tasks and an attempt archive root")
                validate_trial(record, tasks, freeze, freeze_sha256, roster_sha256, plan_by_dispatch, production=False)
                verified_indices: set[int] = set()
                if production:
                    assert oracle_tasks is not None and attempt_archive_root is not None
                    task_id = record["task_id"]
                    if task_id not in oracle_tasks:
                        raise StudyError(f"results line {line_number} task has no frozen oracle package")
                    for attempt_number, attempt in enumerate(record["attempts"], 1):
                        verify_attempt_archive(
                            attempt, record, tasks[task_id], oracle_tasks[task_id], attempt_archive_root,
                            freeze, roster_sha256, line_number=line_number, attempt_number=attempt_number,
                        )
                        verified_indices.add(attempt_number - 1)
                validate_trial(
                    record, tasks, freeze, freeze_sha256, roster_sha256, plan_by_dispatch,
                    production=production, verified_attempt_indices=verified_indices if production else None,
                )
                cell = (record["task_id"], record["blind_arm_id"], record["replicate"])
                if cell in seen_cells:
                    raise StudyError(f"duplicate script/arm/session result at line {line_number}: {cell}")
                if record["dispatch_index"] in seen_dispatch:
                    raise StudyError(f"duplicate dispatch index at line {line_number}")
                if record["run_id"] in seen_runs or record["session_id"] in seen_sessions:
                    raise StudyError(f"run and programmer session IDs must be unique (line {line_number})")
                seen_cells.add(cell)
                seen_dispatch.add(record["dispatch_index"])
                seen_runs.add(record["run_id"])
                seen_sessions.add(record["session_id"])
                records.append(record)
    except UnicodeDecodeError as error:
        raise StudyError(f"trial rows {path} are not valid UTF-8 JSONL: {error}") from error
    return records


def _runtime_median(record: dict[str, Any]) -> float | None:
    if record["runtime"]["status"] != "measured":
        return None
    return float(statistics.median(record["runtime"]["repetitions_ms"]))


def _repair_count(record: dict[str, Any]) -> int | None:
    if not _behavior_complete(record["attempts"][-1]):
        return None
    return len(record["attempts"]) - 1


def _turn_wall_ms(record: dict[str, Any]) -> int | None:
    if not _behavior_complete(record["attempts"][-1]):
        return None
    values = [item["conductor_elapsed_ms"] for item in record["attempts"]]
    if any(value is None for value in values):
        return None
    return sum(values)


def summarize(
    roster: dict[str, Any],
    roster_sha256: str,
    freeze: dict[str, Any],
    freeze_sha256: str,
    plan: dict[str, Any],
    plan_sha256: str,
    tasks: dict[str, dict[str, Any]],
    records: list[dict[str, Any]],
    arm_map: dict[str, dict[str, Any]] | None = None,
) -> dict[str, Any]:
    heldout_ids = set(tasks)
    expected_arm_ids = plan["blind_arm_ids"]
    grouped = {arm_id: [record for record in records if record["blind_arm_id"] == arm_id] for arm_id in expected_arm_ids}
    per_arm = []
    by_cell = {(row["task_id"], row["blind_arm_id"]): row for row in records}
    for arm_id in sorted(expected_arm_ids):
        rows = grouped[arm_id]
        completed = [row for row in rows if _behavior_complete(row["attempts"][-1])]
        initial_observed = [
            row for row in rows if _behavior_complete(row["attempts"][0])
        ]
        successes = [row for row in completed if _attempt_passes(row["attempts"][-1], tasks[row["task_id"]])]
        initial_successes = [
            row for row in initial_observed if _attempt_passes(row["attempts"][0], tasks[row["task_id"]])
        ]
        repair_values = [value for row in rows if (value := _repair_count(row)) is not None]
        runtimes = [value for row in rows if (value := _runtime_median(row)) is not None]
        elapsed = [value for row in rows if (value := _turn_wall_ms(row)) is not None]
        rating_rows = [row for row in rows if row["review"]["status"] == "locked"]
        review_summary = {
            "locked_artifacts": len(rating_rows),
            "pending_artifacts": sum(row["review"]["status"] == "pending" for row in rows),
            "not_rateable": sum(row["review"]["status"] == "not_rateable" for row in rows),
            "adjudications": sum(row["review"]["adjudication"] is not None for row in rating_rows),
            "mean_by_dimension": {},
            "rating_observations_by_dimension": {},
        }
        if rating_rows:
            agreement_count = sum(
                tuple(row["review"]["ratings"][0][key] for key in RATING_DIMENSIONS)
                == tuple(row["review"]["ratings"][1][key] for key in RATING_DIMENSIONS)
                for row in rating_rows
            )
            raw_histograms = {}
            resolved_histograms = {}
            for dimension in RATING_DIMENSIONS:
                scores = []
                raw_scores = []
                for row in rating_rows:
                    ratings = row["review"]["ratings"]
                    raw_scores.extend(rating[dimension] for rating in ratings)
                    resolved = ratings[0] if all(ratings[0][key] == ratings[1][key] for key in RATING_DIMENSIONS) else row["review"]["adjudication"]
                    scores.append(resolved[dimension])
                review_summary["mean_by_dimension"][dimension] = statistics.mean(scores)
                review_summary["rating_observations_by_dimension"][dimension] = len(scores)
                raw_histograms[dimension] = {str(value): raw_scores.count(value) for value in range(1, 6)}
                resolved_histograms[dimension] = {str(value): scores.count(value) for value in range(1, 6)}
            review_summary["first_pair_exact_agreement_count"] = agreement_count
            review_summary["first_pair_exact_agreement_rate"] = agreement_count / len(rating_rows)
            review_summary["raw_score_histograms_by_dimension"] = raw_histograms
            review_summary["resolved_score_histograms_by_dimension"] = resolved_histograms
        entry = {
            "blind_arm_id": arm_id,
            "expected_tasks": len(heldout_ids),
            "observed_tasks": len({row["task_id"] for row in rows}),
            "missing_task_ids": sorted(heldout_ids - {row["task_id"] for row in rows}),
            "initial_oracle_status_counts": dict(sorted(Counter(row["attempts"][0]["oracle_status"] for row in rows).items())),
            "initial_behavioral_outcomes": len(initial_observed),
            "initial_passes": len(initial_successes),
            "initial_passes_over_expected_tasks": len(initial_successes) / len(heldout_ids) if heldout_ids else None,
            "initial_success_rate_among_behavioral_outcomes": len(initial_successes) / len(initial_observed) if initial_observed else None,
            "final_oracle_status_counts": dict(sorted(Counter(row["attempts"][-1]["oracle_status"] for row in rows).items())),
            "final_behavioral_outcomes": len(completed),
            "final_passes": len(successes),
            "final_passes_over_expected_tasks": len(successes) / len(heldout_ids) if heldout_ids else None,
            "final_success_rate_among_behavioral_outcomes": len(successes) / len(completed) if completed else None,
            "incomplete_final_outcomes": len(rows) - len(completed),
            "repair_count_observations": len(repair_values),
            "mean_repairs_among_behavioral_outcomes": statistics.mean(repair_values) if repair_values else None,
            "median_repairs_among_behavioral_outcomes": statistics.median(repair_values) if repair_values else None,
            "generation_to_final_success_after_repair": sum(
                not _attempt_passes(row["attempts"][0], tasks[row["task_id"]])
                and _attempt_passes(row["attempts"][-1], tasks[row["task_id"]])
                for row in completed
            ),
            "measured_runtime_cells": len(runtimes),
            "median_of_per_cell_runtime_medians_ms": statistics.median(runtimes) if runtimes else None,
            "conductor_elapsed_observations": len(elapsed),
            "median_total_dispatch_to_response_wall_ms": statistics.median(elapsed) if elapsed else None,
            "telemetry": {
                "token_counter_observations": 0,
                "tool_call_counter_observations": 0,
                "cost_observations": 0,
                "claim": "unavailable; no economic comparison",
            },
            "masked_review": review_summary,
        }
        if arm_map is not None:
            entry["variant"] = arm_map[arm_id]["variant"]
        per_arm.append(entry)

    summary: dict[str, Any] = {
        "schema": "execute.ai-productivity.real-bash-100-summary/1",
        "status": "pending_no_trials" if not records else "descriptive_import_no_claim",
        "evidence_hashes": {
            "protocol_sha256": freeze["protocol_sha256"],
            "freeze_sha256": freeze_sha256,
            "corpus_review_sha256": freeze["corpus_review_sha256"],
            "corpus_manifest_sha256": freeze["corpus_manifest_sha256"],
            "corpus_digest_receipt_sha256": freeze["corpus_digest_receipt_sha256"],
            "task_roster_sha256": roster_sha256,
            "run_plan_sha256": plan_sha256,
            "results_file_sha256": None,
            "arm_map_sha256": freeze["arm_map_sha256"] if arm_map is not None else None,
        },
        "expected_cells": 300,
        "observed_cells": len(records),
        "missing_cells": 300 - len(records),
        "cluster_composition": dict(sorted(Counter(task["family_id"] for task in roster["tasks"]).items())),
        "arms": per_arm,
        "paired_analysis": None,
        "interpretation_limits": [
            "The unit is a real source file, not an assertion of independent functional families.",
            "The 100 scripts come from one upstream repository and release; results describe only this finite corpus.",
            "Missing and nonbehavior outcomes are shown separately and are never imputed as success or zero repairs.",
            "Runtime is candidate process wall time including interpreter startup, only among matched functional passes.",
            "Conductor dispatch-to-response time includes tools and scheduling; it is not model inference time.",
            "Tokens, model tool calls, and task-level cost are unavailable and are not inferred.",
            "Masked review hides the arm map, but source syntax may reveal the language.",
        ],
        "claim": "none",
    }
    if arm_map is None:
        summary["status"] = "arm_mapping_sealed" if records else "pending_no_trials"
        return summary
    if any(row["review"]["status"] == "pending" for row in records):
        raise StudyError("arm map cannot be applied before all submitted artifacts have locked masked reviews")
    comparisons = {}
    execute_arm = next(key for key, value in arm_map.items() if value["variant"] == "execute_program")
    for comparator_variant in ("bash", "nushell"):
        comparator_arm = next(key for key, value in arm_map.items() if value["variant"] == comparator_variant)
        matched_behavior = []
        matched_repairs = []
        matched_runtime = []
        matched_elapsed = []
        paired_task_evidence = []
        correctness_counts = Counter()
        absent_cell_tasks = []
        incomplete_behavior_tasks = []
        runtime_omissions = Counter()
        runtime_omission_tasks: dict[str, list[str]] = defaultdict(list)
        for task_id in sorted(heldout_ids):
            execute_row = by_cell.get((task_id, execute_arm))
            comparator_row = by_cell.get((task_id, comparator_arm))
            execute_final = execute_row["attempts"][-1] if execute_row is not None else None
            other_final = comparator_row["attempts"][-1] if comparator_row is not None else None
            execute_complete = execute_final is not None and _behavior_complete(execute_final)
            comparator_complete = other_final is not None and _behavior_complete(other_final)
            paired_task_evidence.append({
                "task_id": task_id,
                "family_id": tasks[task_id]["family_id"],
                "execute_final_status": execute_final["oracle_status"] if execute_final is not None else "missing_cell",
                "comparator_final_status": other_final["oracle_status"] if other_final is not None else "missing_cell",
                "execute_final_pass": _attempt_passes(execute_final, tasks[task_id]) if execute_complete else None,
                "comparator_final_pass": _attempt_passes(other_final, tasks[task_id]) if comparator_complete else None,
                "execute_repair_count": _repair_count(execute_row) if execute_row is not None else None,
                "comparator_repair_count": _repair_count(comparator_row) if comparator_row is not None else None,
                "execute_runtime_median_ms": _runtime_median(execute_row) if execute_row is not None else None,
                "comparator_runtime_median_ms": _runtime_median(comparator_row) if comparator_row is not None else None,
            })
            if execute_row is None or comparator_row is None:
                absent_cell_tasks.append(task_id)
            elif not (execute_complete and comparator_complete):
                incomplete_behavior_tasks.append(task_id)
            if execute_complete and comparator_complete:
                execute_ok = _attempt_passes(execute_final, tasks[task_id])
                other_ok = _attempt_passes(other_final, tasks[task_id])
                matched_behavior.append((task_id, int(execute_ok), int(other_ok)))
                if execute_ok and not other_ok:
                    correctness_counts["execute_pass_comparator_fail"] += 1
                    correctness_counts["execute_wins"] += 1
                elif other_ok and not execute_ok:
                    correctness_counts["execute_fail_comparator_pass"] += 1
                    correctness_counts["comparator_wins"] += 1
                else:
                    correctness_counts["ties"] += 1
                correctness_counts["both_pass" if execute_ok and other_ok else "both_fail"] += 1
                matched_repairs.append((task_id, _repair_count(execute_row), _repair_count(comparator_row)))
            execute_runtime = _runtime_median(execute_row) if execute_row is not None else None
            comparator_runtime = _runtime_median(comparator_row) if comparator_row is not None else None
            if execute_runtime is not None and comparator_runtime is not None:
                matched_runtime.append((task_id, execute_runtime, comparator_runtime))
            else:
                reasons = []
                for label, row in (("execute", execute_row), ("comparator", comparator_row)):
                    if row is None:
                        reasons.append(f"{label}_cell_missing")
                    elif not _behavior_complete(row["attempts"][-1]):
                        reasons.append(f"{label}_behavior_outcome_incomplete")
                    elif not _attempt_passes(row["attempts"][-1], tasks[task_id]):
                        reasons.append(f"{label}_final_artifact_failed")
                    elif _runtime_median(row) is None:
                        reasons.append(f"{label}_runtime_unavailable")
                for reason in reasons:
                    runtime_omissions[reason] += 1
                    runtime_omission_tasks[reason].append(task_id)
            execute_elapsed = _turn_wall_ms(execute_row) if execute_row is not None else None
            comparator_elapsed = _turn_wall_ms(comparator_row) if comparator_row is not None else None
            if execute_elapsed is not None and comparator_elapsed is not None:
                matched_elapsed.append((task_id, execute_elapsed, comparator_elapsed))
        repair_differences = [execute_n - comparator_n for _, execute_n, comparator_n in matched_repairs]
        runtime_differences = [execute_ms - comparator_ms for _, execute_ms, comparator_ms in matched_runtime]
        runtime_ratios = [execute_ms / comparator_ms for _, execute_ms, comparator_ms in matched_runtime if comparator_ms > 0]
        elapsed_differences = [execute_ms - comparator_ms for _, execute_ms, comparator_ms in matched_elapsed]
        comparisons[f"execute_program_vs_{comparator_variant}"] = {
            "expected_pairs": 100,
            "matched_behavior_pairs": len(matched_behavior),
            "behavior_unmatched_or_incomplete": 100 - len(matched_behavior),
            "missing_cell_task_ids": absent_cell_tasks,
            "behavior_incomplete_task_ids": incomplete_behavior_tasks,
            "paired_task_evidence": paired_task_evidence,
            "family_counts": {
                "eligible_tasks": dict(sorted(Counter(tasks[task_id]["family_id"] for task_id in heldout_ids).items())),
                "missing_cell_pairs": dict(sorted(Counter(tasks[task_id]["family_id"] for task_id in absent_cell_tasks).items())),
                "behavior_incomplete_pairs": dict(sorted(Counter(tasks[task_id]["family_id"] for task_id in incomplete_behavior_tasks).items())),
                "matched_behavior_pairs": dict(sorted(Counter(tasks[task_id]["family_id"] for task_id, _, _ in matched_behavior).items())),
                "matched_runtime_pairs": dict(sorted(Counter(tasks[task_id]["family_id"] for task_id, _, _ in matched_runtime).items())),
            },
            "correctness": {
                "both_pass": correctness_counts["both_pass"],
                "execute_wins": correctness_counts["execute_pass_comparator_fail"],
                "comparator_wins": correctness_counts["execute_fail_comparator_pass"],
                "both_fail": correctness_counts["both_fail"],
                "ties": correctness_counts["ties"],
            },
            "repair_efficiency": {
                "matched_behavior_pairs": len(matched_repairs),
                "mean_execute_repairs": statistics.mean(pair[1] for pair in matched_repairs) if matched_repairs else None,
                "mean_comparator_repairs": statistics.mean(pair[2] for pair in matched_repairs) if matched_repairs else None,
                "mean_execute_minus_comparator_repairs": statistics.mean(repair_differences) if repair_differences else None,
                "execute_used_fewer_repairs": sum(value < 0 for value in repair_differences),
                "comparator_used_fewer_repairs": sum(value > 0 for value in repair_differences),
                "repair_count_ties": sum(value == 0 for value in repair_differences),
            },
            "runtime_on_matched_functional_passes": {
                "matched_pairs": len(matched_runtime),
                "omitted_from_expected_100": 100 - len(matched_runtime),
                "omission_reason_events": dict(sorted(runtime_omissions.items())),
                "omission_task_ids_by_reason": dict(sorted(runtime_omission_tasks.items())),
                "median_paired_execute_minus_comparator_ms": statistics.median(runtime_differences) if runtime_differences else None,
                "median_paired_execute_over_comparator_ratio": statistics.median(runtime_ratios) if runtime_ratios else None,
                "ratio_observations_with_nonzero_comparator": len(runtime_ratios),
            },
            "dispatch_to_response_wall_time": {
                "matched_complete_pairs": len(matched_elapsed),
                "median_paired_execute_minus_comparator_ms": statistics.median(elapsed_differences) if elapsed_differences else None,
                "interpretation": "conductor wall time, including tools/scheduling, not inference time",
            },
        }
    summary["paired_analysis"] = comparisons
    return summary


def load_inputs(args: argparse.Namespace) -> tuple[Any, str, dict[str, Any], str, Any, str, dict[str, dict[str, Any]], dict[int, dict[str, Any]], list[dict[str, Any]], dict[str, dict[str, Any]] | None]:
    freeze_doc, _, freeze_sha = read_json(args.freeze, "freeze receipt")
    freeze = validate_freeze(freeze_doc)
    candidate = verify_corpus_candidate(args.corpus_dir, include_source_records=True)
    _require_r5_candidate_for_admission(candidate, freeze)
    if candidate["manifest_sha256"] != freeze["corpus_manifest_sha256"]:
        raise StudyError("verified candidate manifest bytes do not match the pre-dispatch freeze")
    if candidate["receipt_sha256"] != freeze["corpus_digest_receipt_sha256"]:
        raise StudyError("verified candidate digest receipt bytes do not match the pre-dispatch freeze")
    roster_doc, roster_raw, roster_sha = read_json(args.roster, "task roster")
    tasks = validate_roster(roster_doc, freeze, roster_sha)
    validate_roster_against_candidate(tasks, candidate)
    brief_doc, brief_raw, _ = read_json(args.task_package, "shared task package")
    oracle_doc, oracle_raw, _ = read_json(args.oracle_package, "evaluator-only oracle package")
    oracle_tasks, _ = validate_oracle_package_against_roster(oracle_doc, oracle_raw, freeze, tasks)
    validate_task_package_against_roster(brief_doc, brief_raw, roster_doc, roster_raw, freeze, oracle_tasks)
    review_doc, _, review_sha = read_json(args.corpus_review, "independent corpus review")
    validate_corpus_review(review_doc, review_sha, freeze, candidate, tasks)
    arms_doc, _, arms_sha = read_json(args.arms, "arm map") if args.arms else (None, b"", None)
    arm_map = validate_arm_map(arms_doc, arms_sha, freeze) if arms_doc is not None else None
    plan_doc, _, plan_sha = read_json(args.plan, "randomized run plan")
    if arm_map is not None:
        sealed_arms = arm_map
    else:
        # The arm map can remain sealed during blind validation. The plan exposes
        # opaque IDs only, so its ID list is sufficient to validate cell pairing.
        sealed_arms = {arm_id: {} for arm_id in plan_doc.get("blind_arm_ids", [])}
    plan_by_dispatch = validate_run_plan(plan_doc, freeze, plan_sha, roster_sha, tasks, sealed_arms)
    records = read_trial_rows(
        args.results, tasks, freeze, freeze_sha, roster_sha, plan_by_dispatch,
        oracle_tasks=oracle_tasks, attempt_archive_root=args.attempt_archive_root,
    )
    return roster_doc, roster_sha, freeze, freeze_sha, plan_doc, plan_sha, tasks, plan_by_dispatch, records, arm_map


def _make_plan_command(args: argparse.Namespace) -> dict[str, Any]:
    roster_doc, _, roster_sha = read_json(args.roster, "task roster")
    arms_doc, _, _ = read_json(args.arms, "sealed arm map")
    if not isinstance(arms_doc, dict) or arms_doc.get("schema") != "execute.ai-productivity.real-bash-100-arms/1" or not isinstance(arms_doc.get("arms"), dict):
        raise StudyError("arm map must use execute.ai-productivity.real-bash-100-arms/1")
    plan = make_run_plan(roster_doc, roster_sha, list(arms_doc["arms"]), args.seed)
    args.out.write_text(json.dumps(plan, sort_keys=True, indent=2) + "\n", encoding="utf-8")
    return {"status": "schedule_written_not_dispatched", "plan_sha256": digest_file(args.out), "cells": len(plan["cells"]), "claim": "none"}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    candidate_parser = commands.add_parser("verify-candidate")
    candidate_parser.add_argument("--corpus-dir", required=True, type=Path)
    plan_parser = commands.add_parser("make-plan")
    plan_parser.add_argument("--roster", required=True, type=Path)
    plan_parser.add_argument("--arms", required=True, type=Path, help="sealed arm map; schedule output contains only opaque IDs")
    plan_parser.add_argument("--seed", required=True, type=int)
    plan_parser.add_argument("--out", required=True, type=Path)
    for name in ("validate", "summarize"):
        command = commands.add_parser(name)
        command.add_argument("--freeze", required=True, type=Path)
        command.add_argument("--corpus-dir", required=True, type=Path)
        command.add_argument("--corpus-review", required=True, type=Path)
        command.add_argument("--roster", required=True, type=Path)
        command.add_argument("--task-package", required=True, type=Path, help="exact frozen programmer-visible task brief package")
        command.add_argument("--oracle-package", required=True, type=Path, help="exact frozen evaluator-only case oracle package")
        command.add_argument("--plan", required=True, type=Path)
        command.add_argument("--results", required=True, type=Path)
        command.add_argument("--attempt-archive-root", required=True, type=Path, help="root containing every result-row attempt archive")
        if name == "summarize":
            command.add_argument("--arms", type=Path, help="open only after masked review lock")
            command.add_argument("--out", type=Path)
    args = parser.parse_args()
    try:
        if args.command == "verify-candidate":
            result = verify_corpus_candidate(args.corpus_dir)
        elif args.command == "make-plan":
            result = _make_plan_command(args)
        else:
            roster, roster_sha, freeze, freeze_sha, plan, plan_sha, tasks, _, records, arms = load_inputs(args)
            if args.command == "validate":
                result = {
                    "schema": "execute.ai-productivity.real-bash-100-validation/1",
                    "status": "pending_no_trials" if not records else "partial_or_complete_import_validated",
                    "expected_cells": 300,
                    "observed_cells": len(records),
                    "missing_cells": 300 - len(records),
                    "freeze_sha256": freeze_sha,
                    "corpus_review_sha256": freeze["corpus_review_sha256"],
                    "roster_sha256": roster_sha,
                    "results_sha256": digest_file(args.results),
                    "claim": "none",
                }
            else:
                result = summarize(roster, roster_sha, freeze, freeze_sha, plan, plan_sha, tasks, records, arms)
                result["evidence_hashes"]["results_file_sha256"] = digest_file(args.results)
            encoded = json.dumps(result, sort_keys=True, indent=2) + "\n"
            output_path = getattr(args, "out", None)
            if output_path:
                output_path.write_text(encoded, encoding="utf-8")
            else:
                sys.stdout.write(encoded)
            return 0
        sys.stdout.write(json.dumps(result, sort_keys=True, indent=2) + "\n")
        return 0
    except (StudyError, OSError) as error:
        parser.error(str(error))
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
