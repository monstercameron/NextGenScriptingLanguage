#!/usr/bin/env python3
"""Validation regressions for the offline model-workflow runner."""
from __future__ import annotations

import importlib.util
import os
from pathlib import Path
import tempfile
import time
import unittest


PROJECT = Path(__file__).resolve().parents[1]
RUNNER_PATH = PROJECT / "examples" / "models" / "run.py"
SPEC = importlib.util.spec_from_file_location("model_workflow_runner", RUNNER_PATH)
if SPEC is None or SPEC.loader is None:
    raise RuntimeError(f"unable to load runner: {RUNNER_PATH}")
runner = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(runner)


class ModelCatalogTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory(prefix="model-catalog-tests-")
        self.root = Path(self.temp.name)
        self.program_dir = self.root / "programs"
        self.fixtures_dir = self.root / "fixtures"
        self.program_dir.mkdir()
        fixture = self.fixtures_dir / "001"
        fixture.mkdir(parents=True)
        (fixture / "input.json").write_text('{"message":"fixture"}\n', encoding="utf-8")
        self.program = self.program_dir / "001-fixture-probe.program"
        self.program.write_text(
            'accept source_path as text with default "fixtures/001/input.json"\n'
            'accept report_path as text with default "out/report.txt"\n'
            'accept live as boolean with default false\n'
            'if live is true\n'
            '  then llm.ask with model "fixture-test" and instruction "fixture test" as live_raw\n'
            'otherwise\n'
            '  then llm.ask with model "fixture-test" and instruction "fixture test" and fixture source_path as raw\n',
            encoding="utf-8",
        )
        self.document = {
            "version": 1,
            "workflows": [
                {
                    "id": 1,
                    "title": "Fixture probe",
                    "description": "Reads one local fixture and writes an exact report.",
                    "file": self.program.name,
                    "cases": [self.case("base"), self.case("alternate")],
                }
            ],
        }

    def tearDown(self) -> None:
        self.temp.cleanup()

    @staticmethod
    def case(name: str, *, expected: str = "done\n", output: str = "report\n") -> dict:
        return {
            "name": name,
            "inputs": {
                "source_path": "fixtures/001/input.json",
                "report_path": "out/report.txt",
            },
            "expected_stdout": expected,
            "outputs": {"out/report.txt": output},
        }

    def validate(self, document: dict | None = None) -> list[dict]:
        return runner.validate_catalog(
            self.document if document is None else document,
            program_dir=self.program_dir,
            fixtures_dir=self.fixtures_dir,
        )

    def test_catalog_accepts_nonempty_fixture_contract_and_preserves_numeric_id(self) -> None:
        workflows = self.validate()
        self.assertEqual(workflows[0]["id"], 1)
        self.assertEqual(workflows[0]["ordinal"], "001")
        self.assertEqual(len(workflows[0]["cases"]), 2)
        runner._validate_fixture_backed_model_calls(
            self.program.read_text(encoding="utf-8"), "001", 1, require_live_control=True
        )

    def test_every_model_call_requires_a_fixture_and_rejects_empty_live_fallbacks(self) -> None:
        source = self.program.read_text(encoding="utf-8")
        live_case = self.case("live-in-fixture-catalog")
        live_case["inputs"]["live"] = "true"
        workflow = dict(self.document["workflows"][0], cases=[live_case, self.case("base")])
        with self.assertRaisesRegex(runner.ContractError, "fixture cases must set live to false"):
            self.validate({"version": 1, "workflows": [workflow]})

        unsafe_env = source.replace(
            'accept live as boolean with default false\n',
            'accept env_file_path as text with default "/Users/test/.env"\n'
            'accept live as boolean with default false\n',
        )
        self.program.write_text(unsafe_env, encoding="utf-8")
        with self.assertRaisesRegex(runner.ContractError, "must default to the private .env"):
            self.validate()

        no_fixture = source.replace(" and fixture source_path as raw", " as raw")
        self.program.write_text(no_fixture, encoding="utf-8")
        with self.assertRaisesRegex(runner.ContractError, "must pass a fixture"):
            self.validate()
        self.program.write_text(source + 'if classify_fixture_path is ""\n', encoding="utf-8")
        with self.assertRaisesRegex(runner.ContractError, "empty-fixture live fallback"):
            self.validate()

    def test_full_catalog_mode_requires_exactly_one_hundred_workflows(self) -> None:
        with self.assertRaisesRegex(runner.ContractError, "exactly 100 workflows"):
            runner.validate_catalog(
                self.document,
                program_dir=self.program_dir,
                fixtures_dir=self.fixtures_dir,
                strict_total=True,
            )

    def test_duplicate_numeric_ids_are_rejected(self) -> None:
        duplicate = {
            "version": 1,
            "workflows": [self.document["workflows"][0], self.document["workflows"][0]],
        }
        with self.assertRaisesRegex(runner.ContractError, "duplicate workflow id"):
            self.validate(duplicate)

    def test_program_path_traversal_is_rejected(self) -> None:
        changed = {"version": 1, "workflows": [dict(self.document["workflows"][0], file="../escape.program")]}
        with self.assertRaisesRegex(runner.ContractError, "escapes its owned directory"):
            self.validate(changed)

    def test_output_path_traversal_is_rejected(self) -> None:
        workflow = dict(self.document["workflows"][0])
        workflow["cases"] = [self.case("base"), self.case("alternate")]
        workflow["cases"][0]["outputs"] = {"out/../outside.txt": "unsafe\n"}
        with self.assertRaisesRegex(runner.ContractError, "escapes its owned directory"):
            self.validate({"version": 1, "workflows": [workflow]})

    def test_missing_and_empty_fixtures_are_rejected_before_execution(self) -> None:
        (self.fixtures_dir / "001" / "input.json").unlink()
        with self.assertRaisesRegex(runner.ContractError, "no non-empty files"):
            self.validate()
        (self.fixtures_dir / "001" / "input.json").write_bytes(b"")
        with self.assertRaisesRegex(runner.ContractError, "empty fixture input"):
            self.validate()

    def test_fixture_path_must_resolve_inside_its_numbered_bundle(self) -> None:
        workflow = dict(self.document["workflows"][0])
        workflow["cases"] = [
            dict(self.case("base"), inputs={"source_path": "fixtures/002/input.json", "report_path": "out/report.txt"}),
            self.case("alternate"),
        ]
        with self.assertRaisesRegex(runner.ContractError, "another workflow's fixture"):
            self.validate({"version": 1, "workflows": [workflow]})

    def test_wrong_expected_stdout_is_a_runtime_failure(self) -> None:
        workflows = self.validate()
        fake_binary = self.root / "fake-execute"
        fake_binary.write_text(
            "#!/bin/sh\n"
            "mkdir -p out\n"
            "printf 'report\\n' > out/report.txt\n"
            "printf 'done\\n'\n",
            encoding="utf-8",
        )
        fake_binary.chmod(0o755)
        wrong = self.case("wrong", expected="wrong\n")
        with self.assertRaisesRegex(runner.ContractError, "stdout mismatch"):
            runner.run_case(fake_binary, self.root, workflows[0], wrong, self.fixtures_dir)

    def test_wrong_expected_output_file_is_a_runtime_failure(self) -> None:
        workflows = self.validate()
        fake_binary = self.root / "fake-execute"
        fake_binary.write_text(
            "#!/bin/sh\n"
            "mkdir -p out\n"
            "printf 'report\\n' > out/report.txt\n"
            "printf 'done\\n'\n",
            encoding="utf-8",
        )
        fake_binary.chmod(0o755)
        normalized = workflows[0]["cases"][0]
        wrong = dict(normalized, outputs={"out/report.txt": b"wrong\n"})
        with self.assertRaisesRegex(runner.ContractError, "output file mismatch"):
            runner.run_case(fake_binary, self.root, workflows[0], wrong, self.fixtures_dir)

    def test_each_run_gets_a_private_empty_env_file(self) -> None:
        workflows = self.validate()
        fake_binary = self.root / "fake-execute"
        fake_binary.write_text(
            "#!/bin/sh\n"
            "test -f .env && test ! -s .env || exit 4\n"
            "test -z \"${OPENAI_API_KEY:-}\" || exit 5\n"
            "test \"${EXECUTE_MODELS_OFFLINE:-}\" = 1 || exit 6\n"
            "mkdir -p out\n"
            "printf 'report\\n' > out/report.txt\n"
            "printf 'done\\n'\n",
            encoding="utf-8",
        )
        fake_binary.chmod(0o755)
        previous = os.environ.get("OPENAI_API_KEY")
        os.environ["OPENAI_API_KEY"] = "not-for-the-child"
        try:
            result = runner.run_case(fake_binary, self.root, workflows[0], workflows[0]["cases"][0], self.fixtures_dir)
        finally:
            if previous is None:
                os.environ.pop("OPENAI_API_KEY", None)
            else:
                os.environ["OPENAI_API_KEY"] = previous
        self.assertTrue(result["passed"])

    def test_runner_environment_has_no_inherited_credentials_or_proxy(self) -> None:
        keys = (
            "OPENAI_API_KEY",
            "ANTHROPIC_API_KEY",
            "AWS_SECRET_ACCESS_KEY",
            "HTTPS_PROXY",
            "BASH_ENV",
        )
        all_keys = (*keys, "EXECUTE_MODELS_OFFLINE")
        previous = {key: os.environ.get(key) for key in all_keys}
        try:
            for key in all_keys:
                os.environ[key] = "must-not-propagate"
            env = runner.safe_environment(self.root, self.root / "execute")
            for key in keys:
                self.assertNotIn(key, env)
            self.assertEqual(env["HOME"], str(self.root))
            self.assertEqual(env["LC_ALL"], "C")
            self.assertEqual(env["EXECUTE_MODELS_OFFLINE"], "1")
        finally:
            for key, value in previous.items():
                if value is None:
                    os.environ.pop(key, None)
                else:
                    os.environ[key] = value

    @unittest.skipUnless(
        runner.discover_network_sandbox() is not None
        and runner.discover_network_sandbox()[0] == "macos-sandbox-exec",
        "macOS network-denial assertion is not portable to this sandbox",
    )
    def test_launcher_blocks_network_access(self) -> None:
        fake_binary = self.root / "try-network"
        fake_binary.write_text(
            "#!/usr/bin/python3\n"
            "import socket\n"
            "socket.socket().bind(('127.0.0.1', 0))\n",
            encoding="utf-8",
        )
        fake_binary.chmod(0o755)
        result = runner.invoke(fake_binary, self.root, [])
        self.assertNotEqual(result["code"], 0)
        self.assertIn(b"Operation not permitted", result["stderr"])

    def test_timeout_kills_descendants_after_process_group_leader_exits(self) -> None:
        fake_binary = self.root / "spawn-descendant"
        fake_binary.write_text(
            "#!/bin/sh\n"
            "(sleep 1; : > descendant-ran) &\n"
            "exit 0\n",
            encoding="utf-8",
        )
        fake_binary.chmod(0o755)
        with self.assertRaises(TimeoutError):
            runner.invoke(fake_binary, self.root, [], timeout=0.1)
        time.sleep(1.1)
        self.assertFalse((self.root / "descendant-ran").exists())


if __name__ == "__main__":
    unittest.main(verbosity=2)
