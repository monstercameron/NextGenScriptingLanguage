#!/usr/bin/env python3
"""Focused tests for interleaved latency comparison behavior."""
import hashlib
import io
import json
import sys
import tempfile
import unittest
from contextlib import redirect_stdout
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from compare_latency import (  # noqa: E402
    main,
    manifest_tree_metadata,
    paired_samples,
    summarize_pairs,
)


class PairedLatencyTests(unittest.TestCase):
    def test_workflow_measures_latency_after_size_failure_and_pairs_pushes_and_prs(self) -> None:
        root = Path(__file__).resolve().parents[1]
        workflow = (root / ".github/workflows/ci.yml").read_text(encoding="utf-8")
        self.assertIn("id: linux_latency", workflow)
        self.assertIn("id: paired_latency", workflow)
        # The latency gate must still collect samples when the size gate fails.
        measure = workflow.split("id: linux_latency", 1)[1]
        measure = measure.split("Save Linux latency samples", 1)[0]
        self.assertIn("if: success() || failure()", measure)
        self.assertIn("--require-rss", measure)
        self.assertIn("--json-out target/linux-latency-samples.json", measure)
        self.assertIn(
            "if: failure() && steps.linux_latency.outcome == 'failure'"
            " && ((github.event_name == 'push'",
            workflow,
        )
        self.assertIn("|| github.event_name == 'pull_request')", workflow)
        self.assertIn(
            "github.event.before != '0000000000000000000000000000000000000000'",
            workflow,
        )
        self.assertIn(
            "PR_BASE_SHA: ${{ github.event.pull_request.base.sha }}", workflow
        )
        self.assertIn('if [[ "$EVENT_NAME" == "pull_request" ]]; then', workflow)
        self.assertIn('--manifest-dir "$GITHUB_WORKSPACE/manifests"', workflow)
        self.assertIn("continue-on-error: true", workflow)
        self.assertIn("path: target/paired-latency.json", workflow)

    def test_paired_latency_stays_diagnostic_and_fail_closed(self) -> None:
        root = Path(__file__).resolve().parents[1]
        workflow = (root / ".github/workflows/ci.yml").read_text(encoding="utf-8")
        paired = workflow.split("id: paired_latency", 1)[1]
        paired = paired.split("Save paired latency samples", 1)[0]
        self.assertIn("invalid base revision", paired)
        self.assertIn('=~ ^[0-9a-f]{40}$', paired)
        self.assertIn("tools/compare_latency.py", paired)
        self.assertNotIn("tools/check_latency.py", paired)
        self.assertNotIn("linux-latency-budget.json", paired)
        self.assertIn("without", workflow)
        self.assertIn("moving the absolute ceiling", workflow)

    def test_historical_anchor_workflow_is_manual_and_does_not_change_budgets(self) -> None:
        root = Path(__file__).resolve().parents[1]
        workflow = (
            root / ".github/workflows/latency-anchor-diagnostic.yml"
        ).read_text(encoding="utf-8")
        budgets = json.loads(
            (root / "tools/linux-latency-budget.json").read_text(encoding="utf-8")
        )

        self.assertIn("workflow_dispatch:", workflow)
        self.assertIn("if: github.ref == 'refs/heads/main'", workflow)
        self.assertIn(
            "ANCHOR_SHA: 6fa84251ec359f22116ddd316b90ec95cd23491f", workflow
        )
        self.assertIn("toolchain: stable", workflow)
        self.assertIn('CARGO_BUILD_JOBS: "1"', workflow)
        self.assertIn(
            'git -C "$anchor_worktree" show "$ANCHOR_SHA:manifests/printf.json"',
            workflow,
        )
        self.assertIn(
            'git -C "$anchor_worktree" show "$ANCHOR_SHA:manifests/core.number_to_text.json"',
            workflow,
        )
        self.assertIn("tools/compare_latency.py", workflow)
        self.assertIn(
            'git -C "$anchor_worktree" archive "$ANCHOR_SHA" manifests', workflow
        )
        self.assertIn("--baseline-manifest-dir \"$historical_manifests\"", workflow)
        self.assertIn("--candidate-manifest-dir \"$current_manifests\"", workflow)
        self.assertIn('"$candidate_bin" run "$preflight_program"', workflow)
        self.assertIn("incompatible or execution failed", workflow)
        self.assertIn("Catalog preflight failed", workflow)
        self.assertIn("manifest_tree_metadata", workflow)
        self.assertNotIn("tools/check_latency.py", workflow)
        self.assertIn("diagnostic only", workflow)
        self.assertEqual(budgets["one_line_median_ms"], 5)
        # #518: the ten-step ceiling stays at the committed 8 ms cap until
        # retained-sample regeneration lands; the anchor workflow must not change it.
        self.assertEqual(budgets["ten_step_median_ms"], 8)

    def test_same_binary_can_be_paired_with_separate_argument_sets(self) -> None:
        commands = []

        def runner(command):
            commands.append(command)
            return 1.0, 100, 0, ""

        paired_samples(
            Path("/bin/execute"),
            Path("/bin/execute"),
            [],
            2,
            1,
            runner,
            baseline_arguments=["--manifest-dir", "historical"],
            candidate_arguments=["--manifest-dir", "current"],
        )

        self.assertEqual(
            [command[-1] for command in commands],
            ["historical", "current", "historical", "current", "current", "historical"],
        )

    def test_manifest_tree_metadata_fingerprints_sorted_direct_json_files(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            (directory / "b.json").write_bytes(b"{}\n")
            (directory / "a.json").write_bytes(b"{\"a\":1}\n")
            nested = directory / "nested"
            nested.mkdir()
            (nested / "ignored.json").write_bytes(b"ignored")

            metadata = manifest_tree_metadata(directory)

        digest = hashlib.sha256()
        for name, content in (("a.json", b'{"a":1}\n'), ("b.json", b"{}\n")):
            digest.update(name.encode("utf-8"))
            digest.update(b"\0")
            digest.update(hashlib.sha256(content).digest())
        self.assertEqual(metadata["json_files"], 2)
        self.assertEqual(metadata["bytes"], len(b'{"a":1}\n') + len(b"{}\n"))
        self.assertEqual(metadata["sha256"], digest.hexdigest())

    def test_manifest_tree_metadata_rejects_missing_or_empty_directories(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            empty = Path(temporary)
            with self.assertRaisesRegex(ValueError, "no JSON files"):
                manifest_tree_metadata(empty)
            with self.assertRaisesRegex(ValueError, "does not exist"):
                manifest_tree_metadata(empty / "missing")

    def test_cli_records_each_manifest_directory_and_fingerprint(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            baseline_manifests = root / "historical"
            candidate_manifests = root / "current"
            baseline_manifests.mkdir()
            candidate_manifests.mkdir()
            (baseline_manifests / "printf.json").write_text("{}\n", encoding="utf-8")
            (candidate_manifests / "printf.json").write_text(
                '{"name":"printf"}\n', encoding="utf-8"
            )
            output = root / "paired.json"
            commands = []

            def runner(command):
                commands.append(command)
                return 1.0, None, 0, ""

            with redirect_stdout(io.StringIO()):
                status = main(
                    [
                        "--baseline",
                        sys.executable,
                        "--candidate",
                        sys.executable,
                        "--baseline-manifest-dir",
                        str(baseline_manifests),
                        "--candidate-manifest-dir",
                        str(candidate_manifests),
                        "--only",
                        "version",
                        "--runs",
                        "1",
                        "--warmup",
                        "0",
                        "--json-out",
                        str(output),
                    ],
                    runner=runner,
                )
            report = json.loads(output.read_text(encoding="utf-8"))

        self.assertEqual(status, 0)
        self.assertEqual(
            commands,
            [
                [sys.executable, "--version"],
                [sys.executable, "--version"],
            ],
        )
        self.assertEqual(report["manifest_dir"], None)
        self.assertEqual(report["baseline_manifest_dir"]["json_files"], 1)
        self.assertEqual(report["candidate_manifest_dir"]["json_files"], 1)
        self.assertNotEqual(
            report["baseline_manifest_dir"]["sha256"],
            report["candidate_manifest_dir"]["sha256"],
        )

    def test_warmups_and_samples_alternate_balanced_order(self) -> None:
        calls = []
        elapsed = iter(range(1, 11))

        def runner(command):
            name = Path(command[0]).name
            calls.append(name)
            return next(elapsed), 100 if name == "baseline" else 120, 0, ""

        samples, rss = paired_samples(
            Path("/bin/baseline"), Path("/bin/candidate"), ["--version"], 3, 2, runner
        )

        self.assertEqual(
            calls,
            [
                "baseline", "candidate",
                "candidate", "baseline",
                "baseline", "candidate",
                "candidate", "baseline",
                "baseline", "candidate",
            ],
        )
        self.assertEqual(samples, {"baseline": [5, 8, 9], "candidate": [6, 7, 10]})
        self.assertEqual(rss, {"baseline": [100, 100, 100], "candidate": [120, 120, 120]})

    def test_reports_independent_medians_and_paired_deltas(self) -> None:
        result = summarize_pairs(
            {"baseline": [4.0, 6.0], "candidate": [6.0, 6.0]},
            {"baseline": [100, 110], "candidate": [120, 130]},
        )

        self.assertEqual(result["baseline_median_ms"], 5.0)
        self.assertEqual(result["candidate_median_ms"], 6.0)
        self.assertEqual(result["paired_delta_pct"], [50.0, 0.0])
        self.assertEqual(result["paired_delta_pct_median"], 25.0)
        self.assertEqual(result["baseline_max_rss_kb"], 110)
        self.assertEqual(result["candidate_max_rss_kb"], 130)

    def test_nonzero_child_status_names_the_binary_that_failed(self) -> None:
        def runner(command):
            return 1.0, None, 2, "bad run"

        with self.assertRaisesRegex(RuntimeError, "baseline failed with status 2: bad run"):
            paired_samples(
                Path("/bin/baseline"), Path("/bin/candidate"), ["--version"], 1, 0, runner
            )


if __name__ == "__main__":
    unittest.main()
