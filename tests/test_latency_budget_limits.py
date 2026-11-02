"""Budget lookup contracts for default and explicit manifest rows."""
import contextlib
import importlib.util
import io
import json
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location("check_latency", ROOT / "tools" / "check_latency.py")
check_latency = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(check_latency)


class LatencyBudgetLimitTests(unittest.TestCase):
    def test_explicit_manifest_row_inherits_existing_limit(self):
        budget = {"one_line_median_ms": 5.0, "one_line_max_rss_kb": 20000}
        self.assertEqual(check_latency.budget_limit(budget, "one_line_manifest_dir", "median_ms"), 5.0)
        self.assertEqual(check_latency.budget_limit(budget, "one_line_manifest_dir", "max_rss_kb"), 20000)

    def test_row_specific_limit_takes_precedence(self):
        budget = {"one_line_median_ms": 5.0, "one_line_manifest_dir_median_ms": 4.0}
        self.assertEqual(check_latency.budget_limit(budget, "one_line_manifest_dir", "median_ms"), 4.0)


FAKE_EXECUTE = """#!/usr/bin/env python3
import pathlib
import sys

arguments = sys.argv[1:]
if arguments == ["--version"]:
    print("synthetic-execute 0.0.0")
    sys.exit(0)
if len(arguments) >= 2 and arguments[0] == "run":
    rest = arguments[2:]
    if "--manifest-dir" in rest:
        manifest_dir = pathlib.Path(rest[rest.index("--manifest-dir") + 1])
    else:
        # A fresh build tree has no installed manifests next to the binary,
        # so the default row fails exactly like the real release binary does
        # without a staged installed layout (empty registry, unknown applet).
        manifest_dir = pathlib.Path(sys.argv[0]).resolve().parent.parent / "manifests"
    if manifest_dir.is_dir() and (manifest_dir / "printf.json").is_file():
        sys.exit(0)
    print('execute: line 1: unknown applet "printf"', file=sys.stderr)
    sys.exit(1)
print("usage error", file=sys.stderr)
sys.exit(2)
"""


class StagedInstallTests(unittest.TestCase):
    def test_stage_copies_byte_identical_binary_next_to_manifests(self):
        with tempfile.TemporaryDirectory(prefix="execute-staging-test-") as directory:
            root = Path(directory)
            binary = root / "fake-execute"
            binary.write_bytes(b"#!/bin/sh\necho hi\n")
            with check_latency.stage_installed_binary(binary) as staged:
                staged_manifests = staged.parent.parent / "manifests"
                self.assertTrue((staged_manifests / "printf.json").is_file())
                self.assertEqual(
                    (staged_manifests / "printf.json").read_bytes(),
                    (check_latency.repo_manifests() / "printf.json").read_bytes(),
                )
                self.assertEqual(staged.read_bytes(), binary.read_bytes())
                self.assertTrue(staged.stat().st_mode & 0o111)

    def test_default_rows_run_through_staged_layout_not_the_raw_binary(self):
        with tempfile.TemporaryDirectory(prefix="execute-staging-test-") as directory:
            root = Path(directory)
            binary = root / "fake-execute"
            binary.write_text(FAKE_EXECUTE, encoding="utf-8")
            binary.chmod(0o755)
            explicit = root / "explicit-manifests"
            explicit.mkdir()
            (explicit / "printf.json").write_text("{}", encoding="utf-8")
            budget = root / "budget.json"
            budget.write_text(json.dumps({
                "version_median_ms": 60000,
                "one_line_median_ms": 60000,
                "ten_step_median_ms": 60000,
                "hot_loop_median_ms": 60000,
            }), encoding="utf-8")
            # Without staging this raises RuntimeError: the raw fake binary
            # has no installed manifests next to it, so its default `run`
            # rows exit 1 with unknown applet, just like a fresh build tree.
            output = io.StringIO()
            with contextlib.redirect_stdout(output):
                self.assertEqual(check_latency.main([
                    str(binary), str(budget),
                    "--manifest-dir", str(explicit),
                    "--runs", "1", "--warmup", "0",
                ]), 0)
            text = output.getvalue()
            self.assertIn("one-line program: median", text)
            self.assertIn("(--manifest-dir)", text)

    def test_stage_rejects_unrunnable_copy(self):
        with tempfile.TemporaryDirectory(prefix="execute-staging-test-") as directory:
            root = Path(directory)
            binary = root / "fake-execute"
            binary.write_text("#!/usr/bin/env python3\nimport sys\nsys.exit(3)\n", encoding="utf-8")
            binary.chmod(0o755)
            with self.assertRaisesRegex(RuntimeError, "refusing to time it"):
                with check_latency.stage_installed_binary(binary):
                    pass

if __name__ == "__main__":
    unittest.main()
