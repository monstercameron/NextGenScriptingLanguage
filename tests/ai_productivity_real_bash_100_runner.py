#!/usr/bin/env python3
"""Harmless owned-fixture checks for the non-measuring 100-task runner."""

import base64
import hashlib
import importlib.util
import json
import os
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch


ROOT = Path(__file__).resolve().parents[1]
MODULE = ROOT / "benchmarks" / "ai_productivity" / "real_bash_100_runner.py"
SPEC = importlib.util.spec_from_file_location("real_bash_100_runner", MODULE)
RUNNER = importlib.util.module_from_spec(SPEC)
assert SPEC and SPEC.loader
SPEC.loader.exec_module(RUNNER)


def b64(value: bytes) -> str:
    return base64.b64encode(value).decode("ascii")


def sha(value: bytes) -> str:
    return hashlib.sha256(value).hexdigest()


def case_fixture():
    response_out = b"mocked\n"
    effect_bytes = b"configured-by-mock\n"
    expected_event = {
        "event_index": 0,
        "command": "fake-cli",
        "call_index": 0,
        "matched_step": 0,
        "argv": ["set", "theme"],
        "stdin_sha256": sha(b""),
        "stdin_base64": b64(b""),
        "cwd": ".",
        "env": {},
        "returncode": 0,
        "stdout_sha256": sha(response_out),
        "stderr_sha256": sha(b""),
        "matched": True,
        "applied_effects": [{"op": "write_file", "path": "settings/theme", "sha256": sha(effect_bytes), "bytes": len(effect_bytes)}],
    }
    return {
        "case_id": "c" * 100,
        "cwd": ".",
        "argv": [],
        "stdin_base64": b64(b""),
        "env": {},
        "filesystem_initial": [],
        "mock_commands": [{
            "name": "fake-cli",
            "steps": [{
                "match": {"argv": ["set", "theme"], "stdin_sha256": sha(b"")},
                "response": {"returncode": 0, "stdout_base64": b64(response_out), "stderr_base64": b64(b"")},
                "effects": [{"op": "write_file", "path": "settings/theme", "data_base64": b64(effect_bytes)}],
            }],
        }],
        "expected": {
            "returncode": 0,
            "stdout_base64": b64(response_out),
            "stderr_base64": b64(b""),
            "filesystem_after": [
                {"path": "home", "type": "directory", "mode": "0755"},
                {"path": "out.txt", "type": "file", "mode": "0644", "data_base64": b64(b"result\n")},
                {"path": "settings", "type": "directory", "mode": "0755"},
                {"path": "settings/theme", "type": "file", "mode": "0644", "data_base64": b64(effect_bytes)},
                {"path": "tmp", "type": "directory", "mode": "0755"},
            ],
            "mock_events": [expected_event],
        },
    }


def packages(case_override=None, criteria_override=None):
    prompt = "Create the requested small fixture program."
    behavior = "Write result\\n to out.txt and invoke fake-cli set theme once; copy its stdout to stdout and retain its status."
    task = {
        "task_id": "fixture_task",
        "family_id": "fixture_family",
        "split": "held_out",
        "stratum": "workflow_cli",
        "testability_stratum": "command_fixture",
        "prompt": prompt,
        "behavior_brief": behavior,
        "prompt_sha256": sha(prompt.encode()),
        "behavior_brief_sha256": sha(behavior.encode()),
    }
    brief = {
        "schema": RUNNER.BRIEF_SCHEMA,
        "status": "draft_fixture",
        "fixture_only": True,
        "protocol_sha256": "0" * 64,
        "corpus_manifest_sha256": "1" * 64,
        "tasks": [task],
    }
    brief_raw = RUNNER.canonical(brief)
    case = case_fixture() if case_override is None else case_override
    criteria = criteria_override or {"process_contract": "process_io_exact", "state_contract": "filesystem_final_exact", "cli_contract": "mock_command_effects_exact", "all_cases_observed": "all_cases_observed"}
    oracle_task = {"task_id": "fixture_task", "criteria": criteria, "cases": [case]}
    oracle = {
        "schema": RUNNER.ORACLE_SCHEMA,
        "status": "draft_fixture",
        "fixture_only": True,
        "task_package_sha256": sha(brief_raw),
        "tasks": [{**oracle_task, "oracle_sha256": sha(RUNNER.canonical(RUNNER.oracle_material(oracle_task)))}],
    }
    return brief, brief_raw, oracle


def production_packages():
    base_brief, _, _ = packages()
    base_case = case_fixture()
    criteria = {"process_contract": "process_io_exact", "all_cases_observed": "all_cases_observed"}
    tasks = []
    oracle_tasks = []
    roster_tasks = []
    for index in range(100):
        task_id = f"task_{index:03d}"
        prompt = f"Create behavior recreation fixture {index}."
        behavior = f"Follow the language-neutral behavior for fixture {index}."
        brief_task = {
            "task_id": task_id,
            "family_id": f"family_{index:03d}",
            "split": "held_out",
            "stratum": "workflow_cli",
            "testability_stratum": "command_fixture",
            "prompt": prompt,
            "behavior_brief": behavior,
            "prompt_sha256": sha(prompt.encode()),
            "behavior_brief_sha256": sha(behavior.encode()),
        }
        tasks.append(brief_task)
        oracle_task = {"task_id": task_id, "criteria": criteria, "cases": [dict(base_case)]}
        oracle_tasks.append({**oracle_task, "oracle_sha256": sha(RUNNER.canonical(RUNNER.oracle_material(oracle_task)))})
        roster_tasks.append({
            "task_id": task_id,
            "family_id": brief_task["family_id"],
            "split": brief_task["split"],
            "stratum": brief_task["stratum"],
            "testability_stratum": brief_task["testability_stratum"],
            "repo_url": "https://example.invalid/owned-fixture",
            "source_commit": "0" * 40,
            "source_path": f"scripts/task-{index:03d}.sh",
            "source_sha256": "2" * 64,
            "manifest_record_sha256": f"{index + 1:064x}",
            "prompt_sha256": brief_task["prompt_sha256"],
            "behavior_brief_sha256": brief_task["behavior_brief_sha256"],
            "oracle_sha256": oracle_tasks[-1]["oracle_sha256"],
            "oracle_ref": f"evaluator://oracle/{task_id}",
            "criteria": dict(criteria),
            "timing_case_set_sha256": "3" * 64,
            "timing_case_ref": f"evaluator://timing/{task_id}",
        })
    brief = {**base_brief, "status": "frozen_before_generation", "fixture_only": False, "tasks": tasks}
    brief_raw = RUNNER.canonical(brief)
    oracle = {
        "schema": RUNNER.ORACLE_SCHEMA,
        "status": "frozen_before_generation",
        "fixture_only": False,
        "task_package_sha256": sha(brief_raw),
        "tasks": oracle_tasks,
    }
    roster = {
        "schema": RUNNER.ROSTER_SCHEMA,
        "status": "frozen_before_generation",
        "frozen_utc": "2026-10-15T00:00:00Z",
        "protocol_sha256": "4" * 64,
        "corpus_manifest_sha256": "5" * 64,
        "digest_receipt_sha256": "6" * 64,
        "task_package_sha256": sha(brief_raw),
        "tasks": roster_tasks,
    }
    return brief, brief_raw, oracle, roster


class RunnerContractTests(unittest.TestCase):
    def test_mocked_cli_effects_and_raw_observations_are_exact_and_fixture_only(self):
        brief, brief_raw, oracle = packages()
        with tempfile.TemporaryDirectory(prefix="rb100-runner-test-") as temp:
            root = Path(temp)
            candidate = root / "candidate.py"
            candidate.write_text(
                "from pathlib import Path\n"
                "import subprocess, sys\n"
                "Path('out.txt').write_bytes(b'result\\n')\n"
                "p = subprocess.run(['fake-cli', 'set', 'theme'], stdout=subprocess.PIPE, stderr=subprocess.PIPE)\n"
                "sys.stdout.buffer.write(p.stdout)\n"
                "sys.stderr.buffer.write(p.stderr)\n"
                "raise SystemExit(p.returncode)\n",
                encoding="utf-8",
            )
            output = root / "attempt-1"
            result = RUNNER.run_attempt(
                brief_package=brief,
                brief_raw=brief_raw,
                oracle_package=oracle,
                task_id="fixture_task",
                blind_arm_id="arm_fixture01",
                dispatch_index=1,
                stage="generate",
                attempt_index=1,
                candidate=candidate,
                runtime_argv=[sys.executable, "{candidate}"],
                runtime_version=sys.version,
                output_dir=output,
                sandbox_wrapper=None,
                fixture_only=True,
            )
            self.assertEqual(result["trial_type"], "fixture")
            self.assertEqual(result["criteria_results"], {"process_contract": True, "state_contract": True, "cli_contract": True, "all_cases_observed": True})
            observation = result["cases"][0]
            self.assertEqual(observation["status"], "completed")
            self.assertEqual((output / observation["stdout"]["artifact_ref"]).read_bytes(), b"mocked\n")
            self.assertEqual((output / observation["stderr"]["artifact_ref"]).read_bytes(), b"")
            self.assertTrue((output / observation["filesystem_after"]["artifact_ref"]).exists())
            self.assertTrue((output / observation["mock_events"]["artifact_ref"]).exists())
            self.assertTrue((output / "candidate.py").exists())
            self.assertTrue((output / "observation.json").exists())
            manifest_path = output / result["archive_manifest_ref"]
            manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
            self.assertEqual(result["archive_manifest_sha256"], sha(RUNNER.canonical(manifest)))
            self.assertEqual(result["observation_sha256"], sha((output / "observation.json").read_bytes()))
            manifest_paths = {entry["path"] for entry in manifest["files"]}
            self.assertEqual(len(manifest_paths), len(manifest["files"]))
            self.assertIn("observation.json", manifest_paths)
            self.assertIn("candidate.py", manifest_paths)
            for entry in manifest["files"]:
                archived = (output / entry["path"]).read_bytes()
                self.assertEqual(entry["bytes"], len(archived))
                self.assertEqual(entry["sha256"], sha(archived))
            for name, expected_hash in result["runtime"]["runner_source_files"].items():
                self.assertEqual(sha((output / "runner-source" / name).read_bytes()), expected_hash)
                self.assertIn(f"runner-source/{name}", manifest_paths)

    def test_production_roster_binds_every_task_and_criterion_id(self):
        brief, brief_raw, oracle, roster = production_packages()
        brief_tasks, oracle_tasks = RUNNER.validate_packages(brief, brief_raw, oracle)
        roster_raw = RUNNER.canonical(roster)
        self.assertEqual(RUNNER.validate_roster_binding(roster, roster_raw, brief_raw, brief_tasks, oracle_tasks), sha(roster_raw))
        roster["tasks"][0]["criteria"].pop("all_cases_observed")
        with self.assertRaisesRegex(RUNNER.RunnerError, "reserved all_cases_observed ID/op exactly once"):
            RUNNER.validate_roster_binding(roster, RUNNER.canonical(roster), brief_raw, brief_tasks, oracle_tasks)

        roster["tasks"][0]["criteria"]["all_cases_observed"] = "process_io_exact"
        with self.assertRaisesRegex(RUNNER.RunnerError, "reserved all_cases_observed ID/op exactly once"):
            RUNNER.validate_roster_binding(roster, RUNNER.canonical(roster), brief_raw, brief_tasks, oracle_tasks)

        roster["tasks"][0]["criteria"]["all_cases_observed"] = "all_cases_observed"
        roster["tasks"][0]["criteria"]["process_contract"] = "filesystem_final_exact"
        with self.assertRaisesRegex(RUNNER.RunnerError, "criterion ID-to-operation map does not exactly match"):
            RUNNER.validate_roster_binding(roster, RUNNER.canonical(roster), brief_raw, brief_tasks, oracle_tasks)

    def test_exact_path_placeholders_bind_in_argv_stdin_responses_effects_and_env(self):
        response = b"home={home}\ntmp={tmp}\nenv={workspace}/marker\n"
        effect_data = b"workspace={workspace}\ntmp={tmp}\n"
        event = {
            "event_index": 0,
            "command": "fake-cli",
            "call_index": 0,
            "matched_step": 0,
            "argv": ["--home", "{home}"],
            "stdin_sha256": sha(b"{tmp}\n"),
            "stdin_base64": b64(b"{tmp}\n"),
            "cwd": ".",
            "env": {"OBSERVED_WORKSPACE": "{workspace}/marker"},
            "returncode": 0,
            "stdout_sha256": sha(response),
            "stderr_sha256": sha(b""),
            "matched": True,
            "applied_effects": [{"op": "write_file", "path": "settings/theme", "sha256": sha(effect_data), "bytes": len(effect_data)}],
        }
        case = {
            "case_id": "placeholder_paths",
            "cwd": ".",
            "argv": [],
            "stdin_base64": b64(b""),
            "env": {"OBSERVED_WORKSPACE": "{workspace}/marker"},
            "filesystem_initial": [],
            "mock_commands": [{
                "name": "fake-cli",
                "observe_env": ["OBSERVED_WORKSPACE"],
                "steps": [{
                    "match": {"argv": ["--home", "{home}"], "stdin_base64": b64(b"{tmp}\n"), "stdin_sha256": sha(b"{tmp}\n")},
                    "response": {"returncode": 0, "stdout_base64": b64(response), "stderr_base64": b64(b"")},
                    "effects": [{"op": "write_file", "path": "settings/theme", "data_base64": b64(effect_data)}],
                }],
            }],
            "expected": {
                "returncode": 0,
                "stdout_base64": b64(response),
                "stderr_base64": b64(b""),
                "filesystem_after": [
                    {"path": "home", "type": "directory", "mode": "0755"},
                    {"path": "out.txt", "type": "file", "mode": "0644", "data_base64": b64(response)},
                    {"path": "settings", "type": "directory", "mode": "0755"},
                    {"path": "settings/theme", "type": "file", "mode": "0644", "data_base64": b64(effect_data)},
                    {"path": "tmp", "type": "directory", "mode": "0755"},
                ],
                "mock_events": [event],
            },
        }
        brief, brief_raw, oracle = packages(case)
        raw_case = oracle["tasks"][0]["cases"][0]
        original_case_hash = sha(RUNNER.canonical(raw_case))
        with tempfile.TemporaryDirectory(prefix="rb100-runner-test-") as temp:
            root = Path(temp)
            candidate = root / "candidate.py"
            candidate.write_text(
                "import os, subprocess, sys\n"
                "from pathlib import Path\n"
                "p = subprocess.run(['fake-cli', '--home', os.environ['HOME']], input=(os.environ['TMPDIR']+'\\n').encode(), stdout=subprocess.PIPE, stderr=subprocess.PIPE)\n"
                "Path('out.txt').write_bytes(p.stdout)\n"
                "sys.stdout.buffer.write(p.stdout)\n"
                "sys.stderr.buffer.write(p.stderr)\n"
                "raise SystemExit(p.returncode)\n",
                encoding="utf-8",
            )
            output = root / "attempt-path-placeholders"
            result = RUNNER.run_attempt(
                brief_package=brief, brief_raw=brief_raw, oracle_package=oracle,
                task_id="fixture_task", blind_arm_id="arm_fixture01", dispatch_index=1,
                stage="generate", attempt_index=1, candidate=candidate,
                runtime_argv=[sys.executable, "{candidate}"], runtime_version=sys.version,
                output_dir=output, sandbox_wrapper=None, fixture_only=True,
            )
            observation = result["cases"][0]
            self.assertEqual(set(result["criteria_results"].values()), {True})
            self.assertEqual(observation["case_template"]["sha256"], original_case_hash)
            template_path = output / observation["case_template"]["artifact_ref"]
            materialized_path = output / observation["case_materialized"]["artifact_ref"]
            archived_template = json.loads(template_path.read_text(encoding="utf-8"))
            archived_materialized = json.loads(materialized_path.read_text(encoding="utf-8"))
            self.assertEqual(sha(RUNNER.canonical(archived_template)), original_case_hash)
            self.assertEqual(observation["case_materialized"]["sha256"], sha(RUNNER.canonical(archived_materialized)))
            self.assertIn("{home}", archived_template["mock_commands"][0]["steps"][0]["match"]["argv"][1])
            self.assertNotIn("{home}", archived_materialized["mock_commands"][0]["steps"][0]["match"]["argv"][1])
            self.assertEqual(archived_materialized["expected"]["mock_events"][0]["env"]["OBSERVED_WORKSPACE"], "$WORKSPACE/marker")
            materialized_event = archived_materialized["expected"]["mock_events"][0]
            materialized_match = archived_materialized["mock_commands"][0]["steps"][0]["match"]
            self.assertEqual(materialized_event["argv"], materialized_match["argv"])
            self.assertEqual(materialized_event["stdin_base64"], materialized_match["stdin_base64"])
            self.assertEqual(materialized_event["stdin_sha256"], sha(RUNNER._b64(materialized_match["stdin_base64"], "test stdin")))
            self.assertIn("/home", materialized_event["argv"][1])
            self.assertIn("/tmp", RUNNER._b64(materialized_match["stdin_base64"], "test stdin").decode())

    def test_production_runner_refuses_to_execute_without_external_wrapper(self):
        brief, brief_raw, oracle = packages()
        with tempfile.TemporaryDirectory(prefix="rb100-runner-test-") as temp:
            root = Path(temp)
            candidate = root / "candidate.py"
            candidate.write_text("raise SystemExit(0)\n", encoding="utf-8")
            with self.assertRaisesRegex(RUNNER.RunnerError, "requires the external reviewed sandbox wrapper"):
                RUNNER.run_attempt(
                    brief_package={**brief, "fixture_only": False, "status": "frozen_before_generation"},
                    brief_raw=RUNNER.canonical({**brief, "fixture_only": False, "status": "frozen_before_generation"}),
                    oracle_package={**oracle, "fixture_only": False, "status": "frozen_before_generation", "task_package_sha256": sha(RUNNER.canonical({**brief, "fixture_only": False, "status": "frozen_before_generation"}))},
                    task_id="fixture_task", blind_arm_id="arm_fixture01", dispatch_index=1,
                    stage="generate", attempt_index=1, candidate=candidate,
                    runtime_argv=[sys.executable, "{candidate}"], runtime_version=sys.version,
                    output_dir=root / "attempt", sandbox_wrapper=None, fixture_only=False,
                )

    def test_production_attempts_are_refused_on_non_linux_hosts(self):
        if RUNNER.platform.system() == "Linux":
            self.skipTest("production Linux workers are the only eligible host")
        with self.assertRaisesRegex(RUNNER.RunnerError, "requires a disposable Linux worker"):
            RUNNER.run_attempt(
                brief_package={}, brief_raw=b"{}", oracle_package={},
                task_id="fixture_task", blind_arm_id="arm_fixture01", dispatch_index=1,
                stage="generate", attempt_index=1, candidate=Path("unused"),
                runtime_argv=[sys.executable, "{candidate}"], runtime_version=sys.version,
                output_dir=Path("unused-output"), sandbox_wrapper=["/bin/true", "{workspace}", "{command}"],
                fixture_only=False,
            )

    def test_oracle_package_must_be_bound_to_exact_shared_brief_bytes(self):
        brief, brief_raw, oracle = packages()
        oracle["task_package_sha256"] = "f" * 64
        with self.assertRaisesRegex(RUNNER.RunnerError, "bound to different shared brief package bytes"):
            RUNNER.validate_packages(brief, brief_raw, oracle, allow_fixture=True)

    def test_shared_brief_object_must_match_the_exact_hashed_bytes(self):
        brief, brief_raw, oracle = packages()
        altered = dict(brief)
        altered["tasks"] = [dict(brief["tasks"][0], prompt="changed after hashing")]
        with self.assertRaisesRegex(RUNNER.RunnerError, "does not match its exact bound bytes"):
            RUNNER.validate_packages(altered, brief_raw, oracle, allow_fixture=True)

    def test_output_limit_bounds_combined_retained_stdout_and_stderr(self):
        with tempfile.TemporaryDirectory(prefix="rb100-runner-test-") as temp:
            root = Path(temp)
            captured = RUNNER._run_capture(
                [sys.executable, "-c", "import sys; sys.stdout.write('x'*10000); sys.stderr.write('y'*10000)"],
                root, dict(os.environ), b"", {**RUNNER.DEFAULT_LIMITS, "output_bytes": 128},
            )
            self.assertEqual(captured["status"], "output_limit")
            self.assertLessEqual(len(captured["stdout"]) + len(captured["stderr"]), 128)

    def test_malformed_mock_transport_is_retained_as_an_explicit_failed_event(self):
        with tempfile.TemporaryDirectory(prefix="rb100-runner-test-") as temp:
            root = Path(temp)
            server = RUNNER._MockServer(str(root / "s"), root, [])
            try:
                response = server._transport_error("malformed request")
                self.assertEqual(response["returncode"], 125)
                self.assertEqual(len(server.events), 1)
                self.assertEqual(server.events[0]["command"], "mock_protocol_error")
                self.assertFalse(server.events[0]["matched"])
            finally:
                server.server_close()

    def test_candidate_timeout_is_a_failed_observation_not_missing_infrastructure(self):
        response = b"done\n"
        case = {
            "case_id": "timeout_after_valid_output",
            "cwd": ".",
            "argv": [],
            "stdin_base64": b64(b""),
            "env": {},
            "filesystem_initial": [],
            "mock_commands": [],
            "expected": {
                "returncode": 0,
                "stdout_base64": b64(response),
                "stderr_base64": b64(b""),
                "filesystem_after": [
                    {"path": "home", "type": "directory", "mode": "0755"},
                    {"path": "out.txt", "type": "file", "mode": "0644", "data_base64": b64(response)},
                    {"path": "tmp", "type": "directory", "mode": "0755"},
                ],
                "mock_events": [],
            },
        }
        criteria = {
            "state_contract": "filesystem_final_exact",
            "command_contract": "mock_command_effects_exact",
            "all_cases_observed": "all_cases_observed",
        }
        brief, brief_raw, oracle = packages(case, criteria)
        with tempfile.TemporaryDirectory(prefix="rb100-runner-test-") as temp:
            root = Path(temp)
            candidate = root / "candidate.py"
            candidate.write_text(
                "from pathlib import Path\n"
                "import sys, time\n"
                "Path('out.txt').write_bytes(b'done\\n')\n"
                "sys.stdout.buffer.write(b'done\\n')\n"
                "sys.stdout.flush()\n"
                "time.sleep(4)\n",
                encoding="utf-8",
            )
            result = RUNNER.run_attempt(
                brief_package=brief, brief_raw=brief_raw, oracle_package=oracle,
                task_id="fixture_task", blind_arm_id="arm_fixture01", dispatch_index=1,
                stage="generate", attempt_index=1, candidate=candidate,
                runtime_argv=[sys.executable, "{candidate}"], runtime_version=sys.version,
                output_dir=root / "attempt", sandbox_wrapper=None, fixture_only=True,
                limits={**RUNNER.DEFAULT_LIMITS, "timeout_seconds": 1, "cpu_seconds": 5},
            )
            self.assertEqual(result["cases"][0]["status"], "timeout")
            self.assertEqual(result["cases"][0]["process_status"], "timeout")
            self.assertEqual(result["criteria_results"], {"state_contract": True, "command_contract": True, "all_cases_observed": False})

    def test_production_oracle_rejects_missing_completion_criterion(self):
        brief, brief_raw, oracle = packages()
        oracle["fixture_only"] = False
        oracle["status"] = "frozen_before_generation"
        oracle["tasks"][0]["criteria"].pop("all_cases_observed")
        material = {"criteria": oracle["tasks"][0]["criteria"], "cases": oracle["tasks"][0]["cases"]}
        oracle["tasks"][0]["oracle_sha256"] = sha(RUNNER.canonical(material))
        with self.assertRaisesRegex(RUNNER.RunnerError, "reserved all_cases_observed ID/op"):
            RUNNER.validate_oracle_package(oracle, sha(brief_raw), {"fixture_task"})

    def test_snapshot_infrastructure_error_does_not_erase_known_timeout(self):
        response = b"done\n"
        case = {
            "case_id": "timeout_snapshot_error",
            "cwd": ".",
            "argv": [],
            "stdin_base64": b64(b""),
            "env": {},
            "filesystem_initial": [],
            "mock_commands": [],
            "expected": {
                "returncode": 0,
                "stdout_base64": b64(response),
                "stderr_base64": b64(b""),
                "filesystem_after": [
                    {"path": "home", "type": "directory", "mode": "0755"},
                    {"path": "out.txt", "type": "file", "mode": "0644", "data_base64": b64(response)},
                    {"path": "tmp", "type": "directory", "mode": "0755"},
                ],
                "mock_events": [],
            },
        }
        criteria = {
            "process_contract": "process_io_exact",
            "state_contract": "filesystem_final_exact",
            "all_cases_observed": "all_cases_observed",
        }
        brief, brief_raw, oracle = packages(case, criteria)
        with tempfile.TemporaryDirectory(prefix="rb100-runner-test-") as temp:
            root = Path(temp)
            candidate = root / "candidate.py"
            candidate.write_text(
                "from pathlib import Path\n"
                "import sys, time\n"
                "Path('out.txt').write_bytes(b'done\\n')\n"
                "sys.stdout.buffer.write(b'done\\n')\n"
                "sys.stdout.flush()\n"
                "time.sleep(3)\n",
                encoding="utf-8",
            )
            original_snapshot = RUNNER._tree_snapshot
            calls = 0

            def fail_second_snapshot(path):
                nonlocal calls
                calls += 1
                if calls == 2:
                    raise OSError("fixture observer failure")
                return original_snapshot(path)

            with patch.object(RUNNER, "_tree_snapshot", side_effect=fail_second_snapshot):
                result = RUNNER.run_attempt(
                    brief_package=brief, brief_raw=brief_raw, oracle_package=oracle,
                    task_id="fixture_task", blind_arm_id="arm_fixture01", dispatch_index=1,
                    stage="generate", attempt_index=1, candidate=candidate,
                    runtime_argv=[sys.executable, "{candidate}"], runtime_version=sys.version,
                    output_dir=root / "attempt", sandbox_wrapper=None, fixture_only=True,
                    limits={**RUNNER.DEFAULT_LIMITS, "timeout_seconds": 1, "cpu_seconds": 5},
                )
            observation = result["cases"][0]
            self.assertEqual(observation["status"], "infrastructure_error")
            self.assertEqual(observation["process_status"], "timeout")
            self.assertEqual(observation["filesystem_observation_status"], "infrastructure_error")
            self.assertFalse(result["criteria_results"]["process_contract"])
            self.assertIsNone(result["criteria_results"]["state_contract"])
            self.assertFalse(result["criteria_results"]["all_cases_observed"])

            candidate.write_text(
                "from pathlib import Path\n"
                "import sys\n"
                "Path('out.txt').write_bytes(b'done\\n')\n"
                "sys.stdout.buffer.write(b'done\\n')\n",
                encoding="utf-8",
            )
            calls = 0
            with patch.object(RUNNER, "_tree_snapshot", side_effect=fail_second_snapshot):
                completed = RUNNER.run_attempt(
                    brief_package=brief, brief_raw=brief_raw, oracle_package=oracle,
                    task_id="fixture_task", blind_arm_id="arm_fixture01", dispatch_index=2,
                    stage="generate", attempt_index=1, candidate=candidate,
                    runtime_argv=[sys.executable, "{candidate}"], runtime_version=sys.version,
                    output_dir=root / "attempt-complete", sandbox_wrapper=None, fixture_only=True,
                )
            completed_observation = completed["cases"][0]
            self.assertEqual(completed_observation["process_status"], "completed")
            self.assertEqual(completed_observation["filesystem_observation_status"], "infrastructure_error")
            self.assertTrue(completed["criteria_results"]["process_contract"])
            self.assertIsNone(completed["criteria_results"]["state_contract"])
            self.assertIsNone(completed["criteria_results"]["all_cases_observed"])

    def test_candidate_workspace_node_overflow_is_a_resource_failure(self):
        case = {
            "case_id": "node_overflow",
            "cwd": ".",
            "argv": [],
            "stdin_base64": b64(b""),
            "env": {},
            "filesystem_initial": [],
            "mock_commands": [],
            "expected": {
                "returncode": 0,
                "stdout_base64": b64(b""),
                "stderr_base64": b64(b""),
                "filesystem_after": [
                    {"path": "home", "type": "directory", "mode": "0755"},
                    {"path": "tmp", "type": "directory", "mode": "0755"},
                ],
                "mock_events": [],
            },
        }
        criteria = {"all_cases_observed": "all_cases_observed"}
        brief, brief_raw, oracle = packages(case, criteria)
        with tempfile.TemporaryDirectory(prefix="rb100-runner-test-") as temp:
            root = Path(temp)
            candidate = root / "candidate.py"
            candidate.write_text(
                "from pathlib import Path\n"
                "for i in range(520): Path(f'node-{i:04}.txt').write_bytes(b'x')\n",
                encoding="utf-8",
            )
            result = RUNNER.run_attempt(
                brief_package=brief, brief_raw=brief_raw, oracle_package=oracle,
                task_id="fixture_task", blind_arm_id="arm_fixture01", dispatch_index=1,
                stage="generate", attempt_index=1, candidate=candidate,
                runtime_argv=[sys.executable, "{candidate}"], runtime_version=sys.version,
                output_dir=root / "attempt-node-limit", sandbox_wrapper=None, fixture_only=True,
            )
            observation = result["cases"][0]
            self.assertEqual(observation["status"], "resource_limit")
            self.assertEqual(observation["filesystem_observation_status"], "candidate_resource_limit")
            self.assertEqual(observation["filesystem_observation_limit"]["code"], "node_limit")
            self.assertEqual(result["criteria_results"], {"all_cases_observed": False})

    def test_candidate_workspace_byte_overflow_is_a_resource_failure(self):
        case = {
            "case_id": "byte_overflow",
            "cwd": ".",
            "argv": [],
            "stdin_base64": b64(b""),
            "env": {},
            "filesystem_initial": [],
            "mock_commands": [],
            "expected": {
                "returncode": 0,
                "stdout_base64": b64(b""),
                "stderr_base64": b64(b""),
                "filesystem_after": [
                    {"path": "home", "type": "directory", "mode": "0755"},
                    {"path": "tmp", "type": "directory", "mode": "0755"},
                ],
                "mock_events": [],
            },
        }
        brief, brief_raw, oracle = packages(case, {"all_cases_observed": "all_cases_observed"})
        with tempfile.TemporaryDirectory(prefix="rb100-runner-test-") as temp:
            root = Path(temp)
            candidate = root / "candidate.py"
            candidate.write_text("from pathlib import Path; Path('too-large').write_bytes(b'four')\n", encoding="utf-8")
            original_limit = RUNNER.MAX_FIXTURE_BYTES
            RUNNER.MAX_FIXTURE_BYTES = 3
            try:
                result = RUNNER.run_attempt(
                    brief_package=brief, brief_raw=brief_raw, oracle_package=oracle,
                    task_id="fixture_task", blind_arm_id="arm_fixture01", dispatch_index=1,
                    stage="generate", attempt_index=1, candidate=candidate,
                    runtime_argv=[sys.executable, "{candidate}"], runtime_version=sys.version,
                    output_dir=root / "attempt-byte-limit", sandbox_wrapper=None, fixture_only=True,
                )
            finally:
                RUNNER.MAX_FIXTURE_BYTES = original_limit
            observation = result["cases"][0]
            self.assertEqual(observation["status"], "resource_limit")
            self.assertEqual(observation["filesystem_observation_status"], "candidate_resource_limit")
            self.assertEqual(observation["filesystem_observation_limit"]["code"], "file_byte_limit")
            self.assertEqual(observation["filesystem_observation_limit"]["observed_at_least"], 4)
            self.assertEqual(result["criteria_results"], {"all_cases_observed": False})

    def test_only_root_runner_directory_is_hidden_from_workspace_snapshot(self):
        with tempfile.TemporaryDirectory(prefix="rb100-runner-test-") as temp:
            root = Path(temp)
            (root / RUNNER.RESERVED_WORKSPACE_NAME).mkdir()
            (root / RUNNER.RESERVED_WORKSPACE_NAME / "internal").write_text("runner", encoding="utf-8")
            (root / "work" / RUNNER.RESERVED_WORKSPACE_NAME).mkdir(parents=True)
            nested_file = root / "work" / RUNNER.RESERVED_WORKSPACE_NAME / "candidate-output"
            nested_file.write_text("visible", encoding="utf-8")
            paths = {row["path"] for row in RUNNER._tree_snapshot(root)}
            self.assertNotIn(RUNNER.RESERVED_WORKSPACE_NAME, paths)
            self.assertNotIn(f"{RUNNER.RESERVED_WORKSPACE_NAME}/internal", paths)
            self.assertIn(f"work/{RUNNER.RESERVED_WORKSPACE_NAME}", paths)
            self.assertIn(f"work/{RUNNER.RESERVED_WORKSPACE_NAME}/candidate-output", paths)
            RUNNER._validate_tree([{"path": f"work/{RUNNER.RESERVED_WORKSPACE_NAME}/candidate-output", "type": "file", "data_base64": b64(b"visible")}], "nested reserved-name fixture")

    def test_symlink_fixture_cannot_escape_owned_workspace(self):
        with self.assertRaisesRegex(RUNNER.RunnerError, "escapes the owned fixture workspace"):
            RUNNER._validate_tree([{"path": "link", "type": "symlink", "target": "../outside"}], "fixture")


if __name__ == "__main__":
    unittest.main()
