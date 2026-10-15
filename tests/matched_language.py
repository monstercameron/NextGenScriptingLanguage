#!/usr/bin/env python3
"""Contract tests for the matched-language workflow harness."""
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "benchmarks"))
import matched_language as bench
import source_pair


class MatchedLanguageContracts(unittest.TestCase):
    def _campaign_fixture(self, root):
        bin_dir = root / "bin"
        bin_dir.mkdir()
        execute = bin_dir / "execute"
        helper = bin_dir / "execute-applet"
        bash = bin_dir / "bash"
        for path, body in (
            (execute, b"fixture execute binary"),
            (helper, b"fixture helper binary"),
            (bash, b"#!/bin/sh\nprintf 'fixture bash version\\n'\n"),
        ):
            path.write_bytes(body)
            path.chmod(0o755)
        source_root = root / "source"
        (source_root / "src").mkdir(parents=True)
        (source_root / "Cargo.toml").write_text("[package]\nname='fixture'\nversion='0.0.0'\n")
        (source_root / "src/main.rs").write_text("fn main() {}\n")
        catalog = root / "catalog"
        catalog.mkdir()
        (catalog / "unused.json").write_text("{}\n")
        fake_tools = {}
        for name in ("find", "basename", "sort", "tee", "cat", "jq"):
            path = bin_dir / name
            path.write_text("fixture tool\n")
            path.chmod(0o755)
            fake_tools[name] = path
        return execute, source_root, catalog, bash, fake_tools

    def test_public_reference_workloads_have_independent_expected_bytes(self):
        definition, scenarios = bench.load_scenarios()
        self.assertTrue(definition["fixture_only"])
        self.assertEqual(len(scenarios), 6)
        by_name = {scenario.name: scenario for scenario in scenarios}
        self.assertEqual(
            by_name["filesystem_inventory/names_with_spaces_and_hidden_files"].expected_stdout,
            b".env\nalpha.txt\nnotes final.md\n",
        )
        self.assertEqual(
            by_name["typed_event_normalizer/mixed_optional_actor_fields"].expected_stdout,
            b'{"event_id":"e1","occurred_at":"2026-10-01T12:00:00Z","kind":"push","source":"ci","actor":"sam"}\n'
            b'{"event_id":"e2","occurred_at":"2026-10-01T12:01:00Z","kind":"release","source":"registry","actor":null}\n',
        )
        self.assertEqual(
            by_name["layered_config_merge/empty_layers_are_an_empty_object"].expected_stdout,
            b"{}\n",
        )

    def test_interleaving_cycles_every_language_order_evenly(self):
        first_six = [bench.balanced_order(index) for index in range(6)]
        self.assertEqual(len(set(first_six)), 6)
        for position in range(3):
            counts = {side: sum(order[position] == side for order in first_six) for side in bench.SIDES}
            self.assertEqual(counts, {"execute": 2, "bash": 2, "nushell": 2})
        self.assertEqual(bench.balanced_order(6), first_six[0])

    def test_preflight_requires_exact_oracle_and_cross_language_parity(self):
        scenario = bench.Scenario(
            "fixture", "small", "EVAL_INPUT", "input_path", "fixtures/input.txt",
            {"fixtures/input.txt": b"source\n"}, "reports/output.txt", b"result\n",
        )
        with tempfile.TemporaryDirectory() as temporary:
            workspace = bench.fixture_workspace(Path(temporary), scenario)

            def runner(command, cwd, environment, timeout):
                (cwd / scenario.output_path).write_bytes(scenario.expected_stdout)
                return source_pair.Completed(0, scenario.expected_stdout, b"", 123)

            commands = {side: ["fake", side] for side in bench.SIDES}
            environments = {side: {} for side in commands}
            result = bench.preflight_scenario(scenario, commands, workspace, environments, 1.0, runner)
        self.assertEqual(set(result), set(bench.SIDES))
        self.assertTrue(all("elapsed_ns" not in observation for observation in result.values()))
        self.assertEqual({observation["stdout_base64"] for observation in result.values()}, {"cmVzdWx0Cg=="})

    def test_campaign_preflight_failure_carries_arm_and_creates_no_report(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            execute, source_root, catalog, bash, fake_tools = self._campaign_fixture(root)
            report = root / "report.json"

            def runner(command, cwd, environment, timeout):
                return source_pair.Completed(0, b"wrong\n", b"", 1)

            with patch.object(bench, "_tool_paths", return_value=fake_tools):
                with self.assertRaisesRegex(bench.BenchmarkError, r"side=execute phase=preflight.*stdout differs"):
                    bench.run_campaign(
                        execute, source_root, catalog, report,
                        execute_revision="fixture-source",
                        bash_binary=bash,
                        samples=1,
                        warmups=0,
                        timeout_seconds=2,
                        command_timeout_ms=1000,
                        process_runner=runner,
                    )
            self.assertFalse(report.exists())

    def test_campaign_dispatches_each_case_to_its_task_reference(self):
        _, scenarios = bench.load_scenarios()
        by_task_and_case = {(item.task_id, item.case_id): item for item in scenarios}
        observed = set()
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            execute, source_root, catalog, bash, fake_tools = self._campaign_fixture(root)

            def runner(command, cwd, environment, timeout):
                script = next(Path(item) for item in command[1:] if Path(item).suffix in (".program", ".sh"))
                # The executable protocol derives its task from the selected
                # artifact, so dispatching an inventory script into another
                # task's fixture cannot accidentally get that fixture's oracle.
                self.assertEqual(script.stem, cwd.parent.name)
                scenario = by_task_and_case[(script.stem, cwd.name)]
                side = "execute" if script.suffix == ".program" else "bash"
                observed.add((side, scenario.name))
                if side == "execute":
                    index = command.index("--input")
                    self.assertEqual(command[index + 1], scenario.program_input + "=" + scenario.input_value)
                else:
                    self.assertEqual(environment[scenario.environment_input], scenario.input_value)
                (cwd / scenario.output_path).write_bytes(scenario.expected_stdout)
                return source_pair.Completed(0, scenario.expected_stdout, b"", 123)

            with patch.object(bench, "_tool_paths", return_value=fake_tools):
                bench.run_campaign(
                    execute, source_root, catalog, root / "report.json",
                    execute_revision="fixture-source", bash_binary=bash,
                    samples=1, warmups=0, timeout_seconds=2,
                    command_timeout_ms=1000, process_runner=runner,
                )
        self.assertEqual(observed, {(side, item.name) for side in ("execute", "bash") for item in scenarios})

    def test_unexpected_filesystem_effect_fails_even_when_stdout_matches(self):
        scenario = bench.Scenario(
            "fixture", "small", "EVAL_INPUT", "input_path", "fixtures/input.txt",
            {"fixtures/input.txt": b"source\n"}, "reports/output.txt", b"result\n",
        )
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            workspace = bench.fixture_workspace(root, scenario)

            def runner(command, cwd, environment, timeout):
                (cwd / scenario.output_path).write_bytes(scenario.expected_stdout)
                (cwd / "unexpected.txt").write_bytes(b"forbidden\n")
                return source_pair.Completed(0, scenario.expected_stdout, b"", 1)

            with self.assertRaisesRegex(bench.BenchmarkError, "unexpected fixture or workspace effects"):
                bench.preflight_scenario(
                    scenario, {"execute": ["fake"]}, workspace, {"execute": {}}, 1.0, runner
                )

    def test_failure_contracts_and_execute_only_workloads_are_retained(self):
        cells = bench._unmatched_cells()
        keys = {(cell["workload"], cell["language"], cell["status"]) for cell in cells}
        for side in ("execute", "bash", "nushell"):
            self.assertIn(("external_failure_contracts", side, "not_timed"), keys)
        self.assertIn(("applet_match_1000_catalog_top_100", "bash,nushell", "not_matched"), keys)
        self.assertIn(("legacy_typed_graph_two_inprocess_nodes", "bash,nushell", "not_matched"), keys)

    def test_nushell_archive_is_verified_against_explicit_release_hash(self):
        with tempfile.TemporaryDirectory() as temporary:
            archive = Path(temporary) / "nu.tar.gz"
            payload = b"pinned fixture archive\n"
            archive.write_bytes(payload)
            expected_hash = source_pair.sha256_bytes(payload)
            url = "https://github.com/nushell/nushell/releases/download/0.116.0/nu-0.116.0-aarch64-apple-darwin.tar.gz"
            resolved = bench._verify_nushell_provenance(Path("nu"), archive, url, expected_hash)
            self.assertEqual(resolved, archive.resolve())
            with self.assertRaisesRegex(bench.BenchmarkError, "does not match"):
                bench._verify_nushell_provenance(Path("nu"), archive, url, "0" * 64)
            with self.assertRaisesRegex(ValueError, "Nushell release URL"):
                bench._verify_nushell_provenance(Path("nu"), archive, "https://example.com/nu.tar.gz", expected_hash)


if __name__ == "__main__":
    unittest.main()
