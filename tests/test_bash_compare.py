"""Contract tests for the Bash/dash comparison workload inventory."""
import importlib.util
import os
import shlex
import shutil
import tempfile
import time
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location("bash_compare", ROOT / "tools" / "bash_compare.py")
bash_compare = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(bash_compare)


class BashCompareContractTests(unittest.TestCase):
    def test_inventory_is_broad_grouped_and_dash_paired(self):
        scenarios = bash_compare.SCENARIOS
        self.assertGreaterEqual(len(scenarios), 20)
        self.assertTrue(all(scenario.dash for scenario in scenarios), "all documented workloads must have a dash arm")
        self.assertEqual(len({scenario.key for scenario in scenarios}), len(scenarios))
        self.assertEqual(
            {scenario.group for scenario in scenarios},
            {"startup", "processes", "text", "files", "data"},
        )
        self.assertTrue(all(isinstance(bash_compare.expected_output(scenario), bytes) for scenario in scenarios))

    def test_relative_binary_and_manifest_arguments_survive_temp_cwd(self):
        scenario = bash_compare.SCENARIOS[0]
        command = bash_compare.execute_command(
            Path("target/release/execute"),
            Path("/tmp/startup.program"),
            Path("manifests"),
            scenario,
        )
        self.assertTrue(Path(command[0]).is_absolute())
        manifest_index = command.index("--manifest-dir") + 1
        self.assertTrue(Path(command[manifest_index]).is_absolute())

    def test_bash_and_dash_arms_match_the_independent_output_oracles(self):
        bash = shutil.which("bash")
        if not bash:
            self.skipTest("bash is unavailable")
        dash = shutil.which("dash")
        with tempfile.TemporaryDirectory(prefix="bash-compare-contract-") as tmp:
            root = Path(tmp)
            inputs = {}
            for name, make_input in bash_compare.INPUTS.items():
                inputs[name] = root / f"{name}.input"
                inputs[name].write_bytes(make_input())
            files = root / "files"
            files.mkdir()
            for n in range(1000):
                (files / f"item-{n:04}.dat").write_text(f"record-{n}\n", encoding="utf-8")
            (files / "directory-guard.dat").mkdir()
            for scenario in bash_compare.SCENARIOS:
                if any(not shutil.which(tool) for tool in scenario.requires):
                    continue
                stdin_path = inputs[scenario.stdin] if scenario.stdin else None
                expected = bash_compare.expected_output(scenario)
                shells = [(bash, ["--norc", "--noprofile", "-c", scenario.bash])]
                if scenario.dash and dash:
                    shells.append((dash, ["-c", scenario.dash_command or scenario.bash]))
                for shell, options in shells:
                    if scenario.reset_output_dir:
                        bash_compare.reset_output_dir(root)
                    _, status, stderr, output = bash_compare.run_once(
                        [shell, *options], stdin_path, cwd=root, capture=True
                    )
                    self.assertEqual(status, 0, f"{scenario.key} failed: {stderr}")
                    self.assertEqual(output, expected, f"{shell} output oracle mismatch for {scenario.key}")
                    if scenario.expected_file_count:
                        paths = sorted((root / "out").glob("*"))
                        expected_names = [
                            f"file-{n:03}.txt" for n in range(scenario.expected_file_count)
                        ]
                        self.assertEqual([path.name for path in paths], expected_names)
                        self.assertTrue(all(path.read_bytes() == b"written\n" for path in paths))

    @unittest.skipUnless(os.name == "posix", "process-group timeout handling is POSIX-specific")
    def test_timeout_kills_background_descendant(self):
        bash = shutil.which("bash")
        if not bash:
            self.skipTest("bash is unavailable")
        with tempfile.TemporaryDirectory(prefix="bash-compare-timeout-") as tmp:
            marker = Path(tmp) / "descendant-survived"
            script = f"(sleep 1; printf survived > {shlex.quote(str(marker))}) & wait"
            command = [bash, "--norc", "--noprofile", "-c", script]
            with self.assertRaises(TimeoutError):
                bash_compare.run_once(command, None, cwd=tmp, deadline=time.monotonic() + 0.2)
            time.sleep(1.2)
            self.assertFalse(marker.exists(), "timeout must kill the shell's background descendant")


if __name__ == "__main__":
    unittest.main()
