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

DIGEST = "ab" * 32


def meta(run_id, machine="x86_64", platform="Linux-6.11.0", manifest_dir=None):
    return {
        "run_id": run_id,
        "platform": platform,
        "machine": machine,
        "source_revision": "c" * 40,
        "binary_sha256": DIGEST,
        "python": "3.11.0",
        "manifest_dir": manifest_dir,
    }


def triple(medians, rss=None):
    """Three same-config samples with distinct runs for direct regenerate() calls."""
    return [
        (meta(str(n)), {"one_line": (median, (rss or {}).get(n))})
        for n, median in enumerate(medians)
    ]


def sample(run_id, medians, rss=None, machine="x86_64", platform="Linux-6.11.0"):
    results = {}
    for key, median in medians.items():
        results[key] = {
            "median_ms": median,
            "p95_ms": median,
            "runs": 20,
            "max_rss_kb": (rss or {}).get(key),
        }
    return {
        "metadata": {
            "run_id": run_id,
            "platform": platform,
            "machine": machine,
            "source_revision": "c" * 40,
            "binary_sha256": DIGEST,
            "python": "3.11.0",
            "manifest_dir": None,
        },
        "results": results,
    }


class RegenerationFormulaTests(unittest.TestCase):
    def test_time_ceiling_adds_fifteen_percent(self):
        budget = regenerate.regenerate(triple((10.0, 10.0, 10.0)))
        self.assertEqual(budget["one_line_median_ms"], 11.5)

    def test_time_floor_of_03ms_wins_for_fast_workloads(self):
        samples = [
            (meta(str(n)), {"version": (1.0, None)}) for n in range(3)
        ]
        budget = regenerate.regenerate(samples)
        self.assertEqual(budget["version_median_ms"], 1.3)

    def test_time_uses_median_of_retained_medians(self):
        samples = [
            (meta(str(n)), {"one_line": (median, None)})
            for n, median in enumerate((10.0, 12.0, 11.0))
        ]
        budget = regenerate.regenerate(samples)
        # Baseline 11.0 ms + 15% headroom, rounded up to one decimal.
        self.assertEqual(budget["one_line_median_ms"], 12.7)

    def test_rss_ceiling_adds_ten_percent_to_retained_peak(self):
        samples = [
            (meta(str(n)), {"hot_loop": (100.0, peak)})
            for n, peak in enumerate((1000.0, 1100.0, 1050.0))
        ]
        budget = regenerate.regenerate(samples)
        self.assertEqual(budget["hot_loop_max_rss_kb"], 1210)

    def test_rss_ceiling_is_omitted_when_never_measured(self):
        budget = regenerate.regenerate(triple((10.0, 10.0, 10.0)))
        self.assertNotIn("one_line_max_rss_kb", budget)

    def test_note_records_runs_arch_formula_and_measurement(self):
        samples = [
            (meta(run_id), {"one_line": (10.0, 1000.0)})
            for run_id in ("111", "222", "333")
        ]
        budget = regenerate.regenerate(samples)
        self.assertIn("111", budget["note"])
        self.assertIn("222", budget["note"])
        self.assertIn("x86_64", budget["note"])
        self.assertIn("measured", budget["note"])
        non_note = [value for key, value in budget.items() if key != "note"]
        self.assertTrue(non_note)
        self.assertTrue(all(isinstance(value, (int, float)) for value in non_note))

    def test_ceiling_rounds_time_up_to_one_decimal(self):
        # 10.01 ms + 15% = 11.5115 ms, which must round up, never down.
        budget = regenerate.regenerate(triple((10.01, 10.01, 10.01)))
        self.assertEqual(budget["one_line_median_ms"], 11.6)


class RegenerationGuardTests(unittest.TestCase):
    def test_rejects_fewer_than_three_samples(self):
        with self.assertRaisesRegex(ValueError, "at least 3"):
            regenerate.regenerate([])
        with self.assertRaisesRegex(ValueError, "at least 3"):
            regenerate.regenerate(triple((10.0, 10.0, 10.0))[:2])

    def test_rejects_reused_run_ids(self):
        samples = [(meta("7"), {"one_line": (10.0, None)}) for _ in range(3)]
        with self.assertRaisesRegex(ValueError, "distinct runs"):
            regenerate.regenerate(samples)

    def test_rejects_missing_run_id(self):
        incomplete = meta("7")
        del incomplete["run_id"]
        samples = [
            (metadata, {"one_line": (10.0, None)})
            for metadata in (incomplete, meta("8"), meta("9"))
        ]
        with self.assertRaisesRegex(ValueError, "run_id"):
            regenerate.regenerate(samples)

    def test_rejects_mixed_architectures(self):
        samples = [
            (meta("1", machine="x86_64"), {"one_line": (10.0, None)}),
            (meta("2", machine="x86_64"), {"one_line": (10.0, None)}),
            (meta("3", machine="aarch64"), {"one_line": (10.0, None)}),
        ]
        with self.assertRaisesRegex(ValueError, "architectures"):
            regenerate.regenerate(samples)

    def test_rejects_mixed_os_families(self):
        samples = [
            (meta("1", platform="Linux-6.11"), {"one_line": (10.0, None)}),
            (meta("2", platform="Linux-6.11"), {"one_line": (10.0, None)}),
            (meta("3", platform="macOS-15.0"), {"one_line": (10.0, None)}),
        ]
        with self.assertRaisesRegex(ValueError, "OS families"):
            regenerate.regenerate(samples)

    def test_rejects_mixed_manifest_configurations(self):
        samples = [
            (meta("1"), {"one_line": (10.0, None)}),
            (meta("2"), {"one_line": (10.0, None)}),
            (meta("3", manifest_dir="manifests"), {"one_line": (10.0, None)}),
        ]
        with self.assertRaisesRegex(ValueError, "manifest configurations"):
            regenerate.regenerate(samples)

    def test_rejects_diverging_workload_keys(self):
        samples = [
            (meta("1"), {"one_line": (10.0, None)}),
            (meta("2"), {"one_line": (10.0, None)}),
            (meta("3"), {"one_line": (10.0, None), "ten_step": (12.0, None)}),
        ]
        with self.assertRaisesRegex(ValueError, "same workload keys"):
            regenerate.regenerate(samples)

    def test_rejects_zero_median(self):
        with self.assertRaisesRegex(ValueError, "non-positive"):
            regenerate.regenerate(triple((10.0, 0.0, 10.0)))

    def test_rejects_unreadable_sample(self):
        with self.assertRaisesRegex(ValueError, "readable JSON"):
            regenerate.load_sample(ROOT / "does-not-exist.json")

    def test_rejects_sample_missing_provenance(self):
        with tempfile.TemporaryDirectory(prefix="execute-regenerate-") as directory:
            root = Path(directory)
            document = sample("111", {"one_line": 10.0})
            del document["metadata"]["source_revision"]
            path = root / "noprov.json"
            path.write_text(json.dumps(document), encoding="utf-8")
            with self.assertRaisesRegex(ValueError, "source_revision"):
                regenerate.load_sample(path)

    def test_rejects_sample_with_bad_digest(self):
        with tempfile.TemporaryDirectory(prefix="execute-regenerate-") as directory:
            root = Path(directory)
            document = sample("111", {"one_line": 10.0})
            document["metadata"]["binary_sha256"] = "not-a-digest"
            path = root / "baddigest.json"
            path.write_text(json.dumps(document), encoding="utf-8")
            with self.assertRaisesRegex(ValueError, "binary_sha256"):
                regenerate.load_sample(path)

    def test_rejects_sample_with_zero_median(self):
        with tempfile.TemporaryDirectory(prefix="execute-regenerate-") as directory:
            root = Path(directory)
            document = sample("111", {"one_line": 0.0})
            document["results"]["one_line"]["p95_ms"] = 0.0
            path = root / "zero.json"
            path.write_text(json.dumps(document), encoding="utf-8")
            with self.assertRaisesRegex(ValueError, "non-positive"):
                regenerate.load_sample(path)

    def test_rejects_sample_with_inconsistent_p95(self):
        with tempfile.TemporaryDirectory(prefix="execute-regenerate-") as directory:
            root = Path(directory)
            document = sample("111", {"one_line": 10.0})
            document["results"]["one_line"]["p95_ms"] = 5.0
            path = root / "p95.json"
            path.write_text(json.dumps(document), encoding="utf-8")
            with self.assertRaisesRegex(ValueError, "p95_ms"):
                regenerate.load_sample(path)

    def test_rejects_sample_missing_runs(self):
        with tempfile.TemporaryDirectory(prefix="execute-regenerate-") as directory:
            root = Path(directory)
            document = sample("111", {"one_line": 10.0})
            del document["results"]["one_line"]["runs"]
            path = root / "noruns.json"
            path.write_text(json.dumps(document), encoding="utf-8")
            with self.assertRaisesRegex(ValueError, "runs"):
                regenerate.load_sample(path)

    def test_end_to_end_write_and_check(self):
        with tempfile.TemporaryDirectory(prefix="execute-regenerate-") as directory:
            root = Path(directory)
            paths = []
            for run_id, median, peak in (("111", 10.0, 1000.0), ("222", 12.0, 1100.0), ("333", 11.0, 1050.0)):
                path = root / f"{run_id}.json"
                path.write_text(json.dumps(sample(run_id, {"one_line": median}, {"one_line": peak})), encoding="utf-8")
                paths.append(path)
            budget_path = root / "budget.json"
            self.assertEqual(regenerate.main([str(path) for path in paths] + ["--output", str(budget_path)]), 0)
            budget = json.loads(budget_path.read_text(encoding="utf-8"))
            # Median 11.0 ms + 15% headroom; peak 1100 KiB + 10%.
            self.assertEqual(budget["one_line_median_ms"], 12.7)
            self.assertEqual(budget["one_line_max_rss_kb"], 1210)
            self.assertEqual(regenerate.main([str(path) for path in paths] + ["--check", str(budget_path)]), 0)
            budget["one_line_median_ms"] = 999.9
            budget_path.write_text(json.dumps(budget), encoding="utf-8")
            self.assertEqual(regenerate.main([str(path) for path in paths] + ["--check", str(budget_path)]), 1)

    def test_check_ignores_note_provenance(self):
        with tempfile.TemporaryDirectory(prefix="execute-regenerate-") as directory:
            root = Path(directory)
            first = [root / f"a{n}.json" for n in range(3)]
            second = [root / f"b{n}.json" for n in range(3)]
            for n, path in enumerate(first):
                path.write_text(json.dumps(sample(f"a{n}", {"one_line": 10.0}, {"one_line": 1000.0})), encoding="utf-8")
            for n, path in enumerate(second):
                path.write_text(json.dumps(sample(f"b{n}", {"one_line": 10.0}, {"one_line": 1000.0})), encoding="utf-8")
            budget_path = root / "budget.json"
            self.assertEqual(regenerate.main([str(path) for path in first] + ["--output", str(budget_path)]), 0)
            # Same ceilings from different runs must still match: only the note differs.
            self.assertEqual(regenerate.main([str(path) for path in second] + ["--check", str(budget_path)]), 0)


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
            retained = [root / f"retained{n}.json" for n in range(3)]
            seen = []
            for n, path in enumerate(retained):
                self.assertEqual(
                    check_latency.main(
                        [str(binary), str(budget), "--runs", "1", "--warmup", "0",
                         "--json-out", str(path), "--run-id", f"424{n}"]
                    ),
                    0,
                )
                metadata, parsed = regenerate.load_sample(path)
                seen.append((metadata, parsed))
            self.assertEqual({metadata["run_id"] for metadata, _ in seen}, {"4240", "4241", "4242"})
            self.assertEqual(set(seen[0][1]), {"version", "one_line", "ten_step", "hot_loop"})
            regenerated = regenerate.regenerate(seen)
            self.assertIn("4240", regenerated["note"])

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
