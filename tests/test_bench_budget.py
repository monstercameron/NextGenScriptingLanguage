"""Command construction tests for default and explicit manifest benchmarks."""
import importlib.util
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location("bench_budget", ROOT / "tools" / "bench_budget.py")
bench_budget = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(bench_budget)


class BenchBudgetCommandTests(unittest.TestCase):
    def test_default_manifest_path_omits_explicit_selector(self):
        command = bench_budget.execute_command("execute", "run.program", None)
        self.assertEqual(command, ["execute", "run", "run.program"])
        self.assertNotIn("None", command)

    def test_explicit_manifest_directory_is_preserved(self):
        command = bench_budget.execute_command("execute", "run.program", "manifests")
        self.assertEqual(
            command,
            ["execute", "run", "run.program", "--manifest-dir", "manifests"],
        )


if __name__ == "__main__":
    unittest.main()
