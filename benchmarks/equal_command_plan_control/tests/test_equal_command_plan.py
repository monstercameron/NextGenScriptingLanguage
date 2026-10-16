"""Non-launching checks for the campaign-local equal-plan controls."""
from __future__ import annotations

import importlib.util
import json
import os
from pathlib import Path
import shutil
import tempfile
import unittest
from types import SimpleNamespace
from unittest import mock


CONTROL_ROOT = Path(__file__).resolve().parents[1]
REPO_ROOT = Path(os.environ["FLOW_THESIS_REPO_ROOT"]).resolve(strict=True)
spec = importlib.util.spec_from_file_location("equal_command_plan", CONTROL_ROOT / "equal_command_plan.py")
control = importlib.util.module_from_spec(spec)
assert spec.loader is not None
spec.loader.exec_module(control)
_, source_pair = control.shared(REPO_ROOT)


class EqualCommandPlanTests(unittest.TestCase):
    def test_sources_use_same_external_plan_and_preserve_shell_references(self) -> None:
        paths = control.check_sources(CONTROL_ROOT, REPO_ROOT, source_pair)
        self.assertEqual(control.direct_commands(paths["filesystem_inventory"]["execute"].read_text()),
                         ("find", "sort", "tee"))
        for task in control.TASKS[1:]:
            self.assertEqual(control.direct_commands(paths[task]["execute"].read_text()), ("jq", "tee"))
        self.assertNotIn("cat", sum((control.direct_commands(paths[t]["execute"].read_text()) for t in control.TASKS), ()))

    def test_control_task_ids_separate_catalog_modes(self) -> None:
        scenarios = control.shared(REPO_ROOT)[0].load_scenarios()[1]
        cases = control.control_cases(scenarios)
        names = [scenario.name for scenario in cases]
        self.assertEqual(len(names), len(set(names)))
        self.assertTrue(all(name.startswith("equal_command_plan_control_") for name in names))

    def test_catalog_argument_is_the_only_execute_argv_difference(self) -> None:
        paths = control.check_sources(CONTROL_ROOT, REPO_ROOT, source_pair)["layered_config_merge"]
        scenarios = control.shared(REPO_ROOT)[0].load_scenarios()[1]
        scenario = next(s for s in scenarios if s.task_id == "layered_config_merge")
        shell = {"bash": Path("bash"), "nushell": Path("nu")}
        catalogs = {"full": Path("catalog/full"), "minimal_inert": Path("catalog/inert")}
        arms = control.commands(Path("execute"), shell, paths, catalogs, scenario, 25000)
        self.assertEqual(arms["execute_full"][:-2], arms["execute_minimal_inert"][:-2])
        self.assertEqual(arms["execute_full"][-2:], ["--manifest-dir", "catalog/full"])
        self.assertEqual(arms["execute_minimal_inert"][-2:], ["--manifest-dir", "catalog/inert"])
        self.assertEqual(arms["bash"][0], "bash")
        self.assertEqual(arms["nushell"][0], "nu")
        orders = [control.arm_order(i) for i in range(24)]
        self.assertEqual(len(set(orders)), 24)
        self.assertTrue(all(set(order) == set(control.ARMS) for order in orders))
        self.assertEqual(orders, list(control.ARM_PERMUTATIONS))

    def test_official_nushell_asset_url_uses_parent_version_segment(self) -> None:
        url = "https://github.com/nushell/nushell/releases/download/0.116.0/nu-aarch64-apple-darwin.tar.gz"
        self.assertEqual(control.nushell_version_from_release_url(url), "0.116.0")

    def test_full_dispatch_smoke_uses_fake_binaries_and_runner(self) -> None:
        mh, sp = control.shared(REPO_ROOT)
        scenarios = mh.load_scenarios()[1]
        expected = {(s.task_id, s.case_id): s for s in scenarios}
        with tempfile.TemporaryDirectory(prefix="equal-plan-smoke-") as temp:
            temp_root = Path(temp)
            binary_dir = temp_root / "binaries"
            binary_dir.mkdir()
            binaries = {}
            for name in ("execute", "bash", "nu", "nu-archive"):
                path = binary_dir / name
                path.write_text("synthetic smoke fixture; never executed\n")
                binaries[name] = path
            tools_dir = temp_root / "tools"
            tools_dir.mkdir()
            tools = {}
            for name in sorted({tool for group in control.TOOLS.values() for tool in group}):
                path = tools_dir / name
                path.write_text("synthetic tool marker; never executed\n")
                tools[name] = path
            calls = []

            def fake_process(command, cwd, environment, timeout):
                task = cwd.parent.name.replace("equal_command_plan_control_", "")
                scenario = expected[(task, cwd.name)]
                output = cwd / scenario.output_path
                output.write_bytes(scenario.expected_stdout)
                calls.append((task, command[0]))
                return SimpleNamespace(status=0, stdout=scenario.expected_stdout, stderr=b"", elapsed_ns=0)

            args = SimpleNamespace(
                confirm_idle_window=True, samples=24, warmups=5, timeout_seconds=2.0,
                command_timeout_ms=1500, repo_root=REPO_ROOT,
                execute_source_root=REPO_ROOT, execute_binary=binaries["execute"],
                manifest_dir=REPO_ROOT / "manifests", inert_catalog=CONTROL_ROOT / "catalogs/minimal-unreferenced",
                execute_revision="synthetic-smoke-source", bash_binary=binaries["bash"],
                nushell_binary=binaries["nu"], nushell_archive=binaries["nu-archive"],
                nushell_archive_sha256="0" * 64,
                nushell_release_url="https://github.com/nushell/nushell/releases/download/0.116.0/nu-aarch64-apple-darwin.tar.gz",
                report=temp_root / "smoke.json",
            )

            with (mock.patch.object(control.shutil, "which", side_effect=lambda name, path=None: str(tools[name])),
                  mock.patch.object(sp, "verify_executable", side_effect=lambda path, label: Path(path).resolve()),
                  mock.patch.object(sp, "helper_path", return_value=None),
                  mock.patch.object(sp, "run_process", side_effect=fake_process),
                  mock.patch.object(mh, "_verify_nushell_provenance", return_value=binaries["nu-archive"]),
                  mock.patch.object(mh, "_nearest_version", return_value="0.116.0")):
                report = control.run(args)

            self.assertEqual(report["status"], "passed")
            self.assertEqual(len(report["workloads"]), 6)
            self.assertEqual(len(calls), 6 * 4 * (1 + 5 + 24))
            for workload in report["workloads"]:
                self.assertEqual(len(workload["raw_samples"]), 4 * 24)
                self.assertEqual(len(workload["warmup_observations"]), 4 * 5)
                self.assertEqual(workload["paired_execute_catalog_delta"]["summary"]["sample_count"], 24)
                for side in ("bash", "nushell"):
                    for mode in ("full", "minimal_inert"):
                        self.assertEqual(workload["paired_language_comparisons"][side][mode]["summary"]["sample_count"], 24)

    def test_inert_catalog_is_exactly_one_unreferenced_cat_manifest(self) -> None:
        catalog = CONTROL_ROOT / "catalogs/minimal-unreferenced"
        manifest = catalog / "cat.json"
        self.assertEqual([manifest], list(catalog.iterdir()))
        self.assertEqual(source_pair.sha256_file(manifest), source_pair.sha256_file(REPO_ROOT / "manifests/cat.json"))
        self.assertEqual("cat", json.loads(manifest.read_text())["name"])
        self.assertNotIn("cat", sum((control.direct_commands(
            (CONTROL_ROOT / "sources/execute" / (task + ".program")).read_text()) for task in control.TASKS), ()))

    def test_internal_save_or_cat_stage_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory(prefix="equal-plan-test-") as temp:
            copied = Path(temp) / "control"
            shutil.copytree(CONTROL_ROOT / "sources", copied / "sources")
            program = copied / "sources/execute/layered_config_merge.program"
            program.write_text(program.read_text().replace('then "tee"', 'then "cat"'))
            with self.assertRaisesRegex(ValueError, "direct command plan"):
                control.check_sources(copied, REPO_ROOT, source_pair)


if __name__ == "__main__":
    unittest.main()
