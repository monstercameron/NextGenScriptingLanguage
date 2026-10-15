#!/usr/bin/env python3
"""Synthetic and public-fixture tests for the AI productivity evaluator."""

from __future__ import annotations

import hashlib
import json
import os
import platform
import shutil
import socket
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
EVAL = ROOT / "benchmarks" / "ai_productivity"
sys.path.insert(0, str(EVAL))
import evaluate  # noqa: E402
import harness  # noqa: E402


def make_external_package() -> dict:
    tasks = []
    for split in ("development", "calibration", "held_out"):
        for stratum in ("workflow_cli", "desktop_composition", "maintenance_change"):
            for family_index in range(30):
                task_id = f"{split}_{stratum}_{family_index:02}"
                tasks.append({
                    "task_id": task_id,
                    "family_id": f"{split}_{stratum}_family_{family_index:02}",
                    "split": split,
                    "stratum": stratum,
                    "prompt": f"Public synthetic prompt for {task_id}.",
                    "oracle": {"kind": "utf8_exact", "expected": "ok\n"},
                    "criteria": ["expected_stdout_and_exit_status"],
                    "optional_criteria": [],
                    "inputs": [],
                    "output_path": "reports/result.txt",
                    "cases": [{
                        "case_id": "case_1",
                        "files": {"fixtures/input.txt": "fixture\n"},
                        "expected": "ok\n",
                    }],
                })
    prereg = json.loads(evaluate.PREREG.read_text())
    return {
        "schema": evaluate.PACKAGE_SCHEMA,
        "preregistration_sha256": evaluate.digest_file(evaluate.PREREG),
        "randomization_seed": prereg["analysis"]["randomization_seed"],
        "frozen_utc": "2026-10-14T12:00:00Z",
        "tasks": tasks,
    }


def make_arm_map(path: Path) -> None:
    variants = [
        "bash", "nushell", "execute_program",
        "bash+qml", "nushell+qml", "execute_program+qml",
    ]
    arms = {}
    for index, variant in enumerate(variants):
        blind = f"arm_{index:08d}"
        arms[blind] = {
            "variant": variant,
            "provider": "fixture-provider",
            "model_snapshot": "fixture-snapshot-1",
            "comparison_block": "fixture-block-1",
            "generation_parameters": {"temperature": 0, "seed": 20261014},
            "api_version": "fixture-api-1",
            "language_version": "fixture-language-1",
            "system_prompt_sha256": "a" * 64,
            "tool_config_sha256": "b" * 64,
        }
    path.write_text(json.dumps({"schema": "execute.ai-productivity.arms/1", "arms": arms}))


class PublicDevelopmentRunnerTests(unittest.TestCase):
    def test_public_task_set_has_filesystem_config_and_event_tasks(self) -> None:
        tasks = evaluate.load_public_tasks()
        self.assertEqual(set(tasks), {"filesystem_inventory", "layered_config_merge", "typed_event_normalizer"})
        self.assertTrue(all(task["split"] == "development" for task in tasks.values()))

    def test_bash_references_execute_in_temp_fixtures_with_exact_effect_oracles(self) -> None:
        reference = EVAL / "public_development" / "reference_solutions" / "bash"
        for task_id in ("filesystem_inventory", "layered_config_merge", "typed_event_normalizer"):
            with self.subTest(task_id=task_id):
                report = evaluate.run_public_fixture(
                    task_id, {"bash": reference}, execute_binary=None,
                    trusted_reference_fixtures=True,
                )
                bash = next(arm for arm in report["arms"] if arm["variant"] == "bash")
                self.assertEqual(report["trial_type"], "fixture")
                self.assertEqual(bash["status"], "pass", bash)
                self.assertEqual(len(bash["cases"]), 2)
                self.assertTrue(all(all(case["criteria_results"].values()) for case in bash["cases"]))

    def test_external_candidate_is_refused_before_its_marker_can_run(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            candidate_dir = root / "candidate"
            candidate_dir.mkdir()
            marker = root / "executed"
            (candidate_dir / "filesystem_inventory.sh").write_text(
                "#!/bin/sh\nprintf executed > " + str(marker) + "\n",
                encoding="utf-8",
            )
            with self.assertRaisesRegex(evaluate.EvaluationError, "refusing to execute candidate artifacts by default"):
                evaluate.run_public_fixture(
                    "filesystem_inventory", {"bash": candidate_dir}, execute_binary=None,
                )
            self.assertFalse(marker.exists(), "default refusal must happen before candidate code is launched")

    def test_sandbox_wrapper_requires_direct_argv_and_exact_splice_tokens(self) -> None:
        with self.assertRaisesRegex(evaluate.EvaluationError, "JSON argv array"):
            evaluate.parse_sandbox_wrapper("/bin/sh -c 'echo unsafe'")
        with self.assertRaisesRegex(evaluate.EvaluationError, "exactly one"):
            evaluate.parse_sandbox_wrapper(json.dumps(["/usr/bin/env", "{command}"]))
        with self.assertRaisesRegex(evaluate.EvaluationError, "complete argv item"):
            evaluate.parse_sandbox_wrapper(json.dumps(["/usr/bin/env", "{workspace}", "{command} --bad"]))
        wrapper = evaluate.parse_sandbox_wrapper(json.dumps([
            "/usr/bin/env", "WORKSPACE={workspace}", "--", "{command}",
        ]))
        expanded = evaluate._wrapped_command(wrapper, Path("/tmp/work space"), ["/bin/echo", "a b"])
        self.assertEqual(expanded[-3:], ["--", "/bin/echo", "a b"])
        self.assertEqual(expanded[1], f"WORKSPACE={Path('/tmp/work space').resolve()}")

    def test_available_macos_sandbox_denies_home_reads_and_network(self) -> None:
        sandbox = shutil.which("sandbox-exec")
        if platform.system() != "Darwin" or not sandbox:
            self.skipTest("macOS sandbox-exec is not available on this host")
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            workspace = root / "workspace"
            host_home = root / "host-home"
            workspace.mkdir()
            host_home.mkdir()
            secret = host_home / "secret.txt"
            secret.write_text("must not be readable", encoding="utf-8")
            result_path = workspace / "result.json"
            probe = workspace / "probe.py"
            probe.write_text(
                "import json, socket, sys\n"
                "try:\n open(sys.argv[1]).read(); home_access=True\n"
                "except OSError:\n home_access=False\n"
                "try:\n socket.create_connection(('127.0.0.1', int(sys.argv[2])), timeout=.25).close(); network_access=True\n"
                "except OSError:\n network_access=False\n"
                "with open(sys.argv[3], 'w') as output:\n json.dump({'home_access': home_access, 'network_access': network_access}, output)\n",
                encoding="utf-8",
            )
            listener = socket.socket()
            listener.bind(("127.0.0.1", 0))
            listener.listen(1)
            profile = (
                '(version 1)(allow default)'
                f'(deny file-read* (subpath "{host_home.resolve()}"))'
                '(deny network*)'
            )
            try:
                completed = subprocess.run(
                    [sandbox, "-p", profile, sys.executable, str(probe), str(secret), str(listener.getsockname()[1]), str(result_path)],
                    cwd=workspace,
                    env={**os.environ, "HOME": str(host_home)},
                    stdin=subprocess.DEVNULL,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.PIPE,
                    timeout=10,
                    check=False,
                )
            finally:
                listener.close()
            self.assertEqual(completed.returncode, 0, completed.stderr.decode("utf-8", errors="replace"))
            result = json.loads(result_path.read_text(encoding="utf-8"))
            self.assertFalse(result["home_access"], result)
            self.assertFalse(result["network_access"], result)

    def test_program_references_execute_when_a_release_binary_is_supplied(self) -> None:
        binary = os.environ.get("EXECUTE_AI_PRODUCTIVITY_FIXTURE_BINARY")
        if not binary:
            self.skipTest("set EXECUTE_AI_PRODUCTIVITY_FIXTURE_BINARY after the release build is ready")
        reference = EVAL / "public_development" / "reference_solutions" / "execute_program"
        for task_id in ("filesystem_inventory", "layered_config_merge", "typed_event_normalizer"):
            with self.subTest(task_id=task_id):
                report = evaluate.run_public_fixture(
                    task_id, {"execute_program": reference}, execute_binary=Path(binary),
                    trusted_reference_fixtures=True,
                )
                program = next(arm for arm in report["arms"] if arm["variant"] == "execute_program")
                self.assertEqual(program["status"], "pass", program)

    def test_nushell_references_execute_when_a_nushell_binary_is_supplied(self) -> None:
        binary = os.environ.get("NUSHELL_AI_PRODUCTIVITY_FIXTURE_BINARY")
        if not binary:
            self.skipTest("set NUSHELL_AI_PRODUCTIVITY_FIXTURE_BINARY to test the Nushell references")
        reference = EVAL / "public_development" / "reference_solutions" / "nushell"
        for task_id in ("filesystem_inventory", "layered_config_merge", "typed_event_normalizer"):
            with self.subTest(task_id=task_id):
                report = evaluate.run_public_fixture(
                    task_id, {"nushell": reference}, execute_binary=None, nushell_binary=binary,
                    trusted_reference_fixtures=True,
                )
                nushell = next(arm for arm in report["arms"] if arm["variant"] == "nushell")
                self.assertEqual(nushell["status"], "pass", nushell)

    def test_external_package_freeze_binds_exact_bytes_and_seed(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            package_path = root / "sealed.json"
            freeze_path = root / "sealed.freeze.json"
            roster_path = root / "roster.json"
            package_path.write_text(json.dumps(make_external_package(), sort_keys=True))
            record = evaluate.freeze_external_package(package_path, 20261014, freeze_path)
            self.assertEqual(record["task_count"], 270)
            self.assertEqual(record["randomization_seed"], 20261014)
            _, tasks = evaluate.verify_external_package(package_path, freeze_path)
            self.assertEqual(len(tasks), 270)
            roster = evaluate.export_external_roster(package_path, freeze_path, roster_path)
            self.assertEqual(roster["task_package_sha256"], evaluate.digest_file(package_path))
            self.assertEqual(len(harness.validate_roster(roster, production=True)), 270)
            package_path.write_text(package_path.read_text() + "\n")
            with self.assertRaisesRegex(evaluate.EvaluationError, "bytes differ"):
                evaluate.verify_external_package(package_path, freeze_path)

    def test_external_package_rejects_unpreregistered_seed(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            package_path = root / "sealed.json"
            freeze_path = root / "sealed.freeze.json"
            document = make_external_package()
            document["randomization_seed"] += 1
            package_path.write_text(json.dumps(document, sort_keys=True))
            with self.assertRaisesRegex(evaluate.EvaluationError, "randomization_seed"):
                evaluate.freeze_external_package(package_path, 20261014, freeze_path)

    def test_seeded_schedule_is_repeatable_and_blind(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            package_path = root / "sealed.json"
            freeze_path = root / "sealed.freeze.json"
            arms_path = root / "arms.json"
            first_path = root / "plan-1.json"
            second_path = root / "plan-2.json"
            package_path.write_text(json.dumps(make_external_package(), sort_keys=True))
            evaluate.freeze_external_package(package_path, 20261014, freeze_path)
            make_arm_map(arms_path)
            first = evaluate.build_run_plan(package_path, freeze_path, arms_path, first_path)
            second = evaluate.build_run_plan(package_path, freeze_path, arms_path, second_path)
            self.assertEqual(first["schedule"], second["schedule"])
            self.assertEqual(first["task_arm_replicate_count"], 810)
            self.assertTrue(all("prompt" not in item and "oracle" not in item for item in first["schedule"]))
            for task in first["schedule"]:
                self.assertRegex(task["run_id"], r"^[0-9a-f-]{36}$")

    def test_fixture_runs_cannot_be_imported_as_results(self) -> None:
        tasks = {
            "task_1": {"task_id": "task_1", "family_id": "family_1", "split": "held_out", "stratum": "workflow_cli", "criteria": ["passes"]}
        }
        fixture_record = {
            "schema": "execute.ai-productivity.run/1",
            "trial_type": "fixture",
            "run_id": "00000000-0000-4000-8000-000000000001",
            "blind_arm_id": "arm_abcdefgh",
            "task_id": "task_1",
            "replicate": 1,
            "runner_image_sha256": "a" * 64,
            "attempts": [],
            "review": {"scoring_locked": False, "ratings": []},
        }
        with self.assertRaisesRegex(harness.EvalError, "fixture"):
            harness.validate_result(fixture_record, tasks, production=True)


if __name__ == "__main__":
    unittest.main(verbosity=2)
