#!/usr/bin/env python3
"""Contract tests for latency budget regeneration from retained CI samples."""
import importlib.util
import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


def load_tool(name):
    spec = importlib.util.spec_from_file_location(name, ROOT / "tools" / f"{name}.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


regenerate = load_tool("regenerate_latency_budget")
check_latency = load_tool("check_latency")


def sample(run_id, medians, rss=None, machine="x86_64", platform="Linux-6.11.0"):
    results = {}
    for key, median in medians.items():
        results[key] = {"median_ms": median, "max_rss_kb": (rss or {}).get(key)}
    return {
        "metadata": {"run_id": run_id, "platform": platform, "machine": machine},
        "results": results,
    }


class RegenerationFormulaTests(unittest.TestCase):
    def test_time_ceiling_adds_fifteen_percent(self):
        budget = regenerate.regenerate(
            [({"platform": "Linux-6.11", "machine": "x86_64", "run_id": "1"}, {"one_line": (10.0, None)})]
        )
        self.assertEqual(budget["one_line_median_ms"], 11.5)

    def test_time_floor_of_03ms_wins_for_fast_workloads(self):
        budget = regenerate.regenerate(
            [({"platform": "Linux-6.11", "machine": "x86_64", "run_id": "1"}, {"version": (1.0, None)})]
        )
        self.assertEqual(budget["version_median_ms"], 1.3)

    def test_time_uses_median_of_retained_medians(self):
        samples = [
            ({"platform": "Linux-6.11", "machine": "x86_64", "run_id": str(n)}, {"one_line": (median, None)})
            for n, median in enumerate((10.0, 12.0, 11.0))
        ]
        budget = regenerate.regenerate(samples)
        # Baseline 11.0 ms + 15% headroom, rounded up to one decimal.
        self.assertEqual(budget["one_line_median_ms"], 12.7)

    def test_rss_ceiling_adds_ten_percent_to_retained_peak(self):
        samples = [
            ({"platform": "Linux-6.11", "machine": "x86_64", "run_id": str(n)}, {"hot_loop": (100.0, peak)})
            for n, peak in enumerate((1000.0, 1100.0))
        ]
        budget = regenerate.regenerate(samples)
        self.assertEqual(budget["hot_loop_max_rss_kb"], 1210)

    def test_rss_ceiling_is_omitted_when_never_measured(self):
        budget = regenerate.regenerate(
            [({"platform": "Linux-6.11", "machine": "x86_64", "run_id": "1"}, {"one_line": (10.0, None)})]
        )
        self.assertNotIn("one_line_max_rss_kb", budget)

    def test_note_records_runs_arch_formula_and_measurement(self):
        budget = regenerate.regenerate(
            [
                ({"platform": "Linux-6.11", "machine": "x86_64", "run_id": "111"}, {"one_line": (10.0, 1000.0)}),
                ({"platform": "Linux-6.11", "machine": "x86_64", "run_id": "222"}, {"one_line": (10.0, 1000.0)}),
            ]
        )
        self.assertIn("111", budget["note"])
        self.assertIn("222", budget["note"])
        self.assertIn("x86_64", budget["note"])
        self.assertIn("measured", budget["note"])
        non_note = [value for key, value in budget.items() if key != "note"]
        self.assertTrue(non_note)
        self.assertTrue(all(isinstance(value, (int, float)) for value in non_note))


class RegenerationGuardTests(unittest.TestCase):
    def test_rejects_empty_samples(self):
        with self.assertRaisesRegex(ValueError, "at least one"):
            regenerate.regenerate([])

    def test_rejects_mixed_architectures(self):
        samples = [
            ({"platform": "Linux-6.11", "machine": "x86_64", "run_id": "1"}, {"one_line": (10.0, None)}),
            ({"platform": "Linux-6.11", "machine": "aarch64", "run_id": "2"}, {"one_line": (10.0, None)}),
        ]
        with self.assertRaisesRegex(ValueError, "architectures"):
            regenerate.regenerate(samples)

    def test_rejects_mixed_os_families(self):
        samples = [
            ({"platform": "Linux-6.11", "machine": "x86_64", "run_id": "1"}, {"one_line": (10.0, None)}),
            ({"platform": "macOS-15.0", "machine": "x86_64", "run_id": "2"}, {"one_line": (10.0, None)}),
        ]
        with self.assertRaisesRegex(ValueError, "OS families"):
            regenerate.regenerate(samples)

    def test_rejects_diverging_workload_keys(self):
        samples = [
            ({"platform": "Linux-6.11", "machine": "x86_64", "run_id": "1"}, {"one_line": (10.0, None)}),
            (
                {"platform": "Linux-6.11", "machine": "x86_64", "run_id": "2"},
                {"one_line": (10.0, None), "ten_step": (12.0, None)},
            ),
        ]
        with self.assertRaisesRegex(ValueError, "same workload keys"):
            regenerate.regenerate(samples)

    def test_rejects_unreadable_sample(self):
        with self.assertRaisesRegex(ValueError, "readable JSON"):
            regenerate.load_sample(ROOT / "does-not-exist.json")

    def test_end_to_end_write_and_check(self):
        with tempfile.TemporaryDirectory(prefix="execute-regenerate-") as directory:
            root = Path(directory)
            first = root / "first.json"
            second = root / "second.json"
            first.write_text(json.dumps(sample("111", {"one_line": 10.0}, {"one_line": 1000.0})), encoding="utf-8")
            second.write_text(json.dumps(sample("222", {"one_line": 12.0}, {"one_line": 1100.0})), encoding="utf-8")
            budget_path = root / "budget.json"
            self.assertEqual(regenerate.main([str(first), str(second), "--output", str(budget_path)]), 0)
            budget = json.loads(budget_path.read_text(encoding="utf-8"))
            # Median 11.0 ms + 15% headroom; peak 1100 KiB + 10%.
            self.assertEqual(budget["one_line_median_ms"], 12.7)
            self.assertEqual(budget["one_line_max_rss_kb"], 1210)
            self.assertEqual(regenerate.main([str(first), str(second), "--check", str(budget_path)]), 0)
            budget["one_line_median_ms"] = 999.9
            budget_path.write_text(json.dumps(budget), encoding="utf-8")
            self.assertEqual(regenerate.main([str(first), str(second), "--check", str(budget_path)]), 1)


class RetainedSampleFormatTests(unittest.TestCase):
    def test_check_latency_json_out_feeds_regeneration(self):
        with tempfile.TemporaryDirectory(prefix="execute-retained-sample-") as directory:
            root = Path(directory)
            binary = root / "synthetic-execute"
            binary.write_text(
                "#!/usr/bin/env python3\n"
                "import time\n"
                "time.sleep(0.01)\n",
                encoding="utf-8",
            )
            binary.chmod(0o755)
            budget = root / "budget.json"
            budget.write_text(json.dumps({"one_line_median_ms": 10000}), encoding="utf-8")
            retained = root / "retained.json"
            self.assertEqual(
                check_latency.main(
                    [str(binary), str(budget), "--runs", "1", "--warmup", "0",
                     "--json-out", str(retained), "--run-id", "4242"]
                ),
                0,
            )
            document = json.loads(retained.read_text(encoding="utf-8"))
            metadata, parsed = regenerate.load_sample(retained)
            self.assertEqual(metadata["run_id"], "4242")
            self.assertEqual(set(parsed), {"version", "one_line", "ten_step", "hot_loop"})
            budget_text = regenerate.regenerate([(metadata, parsed)])
            self.assertIn("4242", budget_text["note"])

    def test_regeneration_cli_rejects_garbage(self):
        with tempfile.TemporaryDirectory(prefix="execute-regenerate-cli-") as directory:
            bad = Path(directory) / "bad.json"
            bad.write_text("not json", encoding="utf-8")
            result = subprocess.run(
                [sys.executable, str(ROOT / "tools/regenerate_latency_budget.py"),
                 str(bad), "--output", str(Path(directory) / "out.json")],
                cwd=ROOT, capture_output=True, text=True, check=False,
            )
            self.assertEqual(result.returncode, 2)
            self.assertIn("error:", result.stderr)


if __name__ == "__main__":
    unittest.main()
