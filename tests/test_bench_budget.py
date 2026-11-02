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


class BenchWorkflowTests(unittest.TestCase):
    def test_bench_baseline_is_pinned_validated_and_cleaned_up(self):
        workflow = (ROOT / ".github/workflows/ci.yml").read_text(encoding="utf-8")
        bench = workflow.split("Measure hot-path benchmarks", 1)[1].split(
            "Check Linux minimal build size", 1
        )[0]
        self.assertIn(
            "BENCH_BASELINE_SHA: 35483ce354f9481cbe2c6878d48e9f48b37d5a5c",
            bench,
        )
        self.assertNotIn("origin 35483ce\n", bench)
        self.assertIn('=~ ^[0-9a-f]{40}$', bench)
        self.assertIn("invalid bench baseline revision", bench)
        self.assertIn("trap cleanup EXIT", bench)
        self.assertIn('git worktree remove --force "$baseline_src"', bench)
        self.assertIn("worktree_added=1", bench)

    def test_bench_guards_missing_executable_without_weakening_gate(self):
        workflow = (ROOT / ".github/workflows/ci.yml").read_text(encoding="utf-8")
        bench = workflow.split("Measure hot-path benchmarks", 1)[1].split(
            "Check Linux minimal build size", 1
        )[0]
        self.assertIn("[[ ! -x target/release/execute ]]", bench)
        self.assertIn("preserve the original failure", bench)
        self.assertIn("tools/bench_budget.py", bench)
        self.assertIn("tools/linux-bench-budget.json", bench)
        self.assertNotIn("continue-on-error", bench)


if __name__ == "__main__":
    unittest.main()
