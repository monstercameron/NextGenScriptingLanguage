"""Tests for tools/omarchy_upstream_map.py. Run with: python tools/test_omarchy_upstream_map.py"""

from __future__ import annotations

import os
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import omarchy_upstream_map as mapper  # noqa: E402


FIXTURE_A = """#!/bin/bash
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
bash "$ROOT/bin/omarchy-a" --check
bash "$ROOT/bin/omarchy-a" --other
bash "$ROOT/bin/omarchy-b"
"""

FIXTURE_B = """#!/bin/bash
bash $ROOT/bin/omarchy-a
bash "${ROOT}/bin/omarchy-c" arg
source "$ROOT/bin/omarchy-lib"
. "$ROOT/bin/omarchy-lib-two"
"""


class CallScanTests(unittest.TestCase):
    def test_quoted_calls_are_counted_per_command(self):
        calls, sourced = mapper.scan_text(FIXTURE_A)
        self.assertEqual(calls["omarchy-a"], 2)
        self.assertEqual(calls["omarchy-b"], 1)
        self.assertEqual(sourced, set())

    def test_unquoted_and_braced_root_forms_are_recognised(self):
        calls, _ = mapper.scan_text(FIXTURE_B)
        self.assertEqual(calls["omarchy-a"], 1)
        self.assertEqual(calls["omarchy-c"], 1)

    def test_sourced_bin_files_are_reported_not_counted_as_calls(self):
        calls, sourced = mapper.scan_text(FIXTURE_B)
        self.assertEqual(sourced, {"omarchy-lib", "omarchy-lib-two"})
        self.assertNotIn("omarchy-lib", calls)


class MapUpstreamTests(unittest.TestCase):
    def test_fixture_tree_maps_commands_to_files_and_counts(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp) / "test" / "shell.d"
            (root / "apps").mkdir(parents=True)
            (root / "omarchy-a-test.sh").write_text(FIXTURE_A, encoding="utf-8")
            (root / "apps" / "mixed-test.sh").write_text(FIXTURE_B, encoding="utf-8")

            result = mapper.map_upstream(root)

        self.assertEqual(result["files_scanned"], 2)
        self.assertEqual(
            result["commands"]["omarchy-a"],
            {
                "total_calls": 3,
                "files": {"apps/mixed-test.sh": 1, "omarchy-a-test.sh": 2},
            },
        )
        self.assertEqual(
            result["commands"]["omarchy-b"],
            {"total_calls": 1, "files": {"omarchy-a-test.sh": 1}},
        )
        self.assertEqual(result["commands"]["omarchy-c"]["total_calls"], 1)
        self.assertEqual(
            result["sourced"],
            {
                "omarchy-lib": ["apps/mixed-test.sh"],
                "omarchy-lib-two": ["apps/mixed-test.sh"],
            },
        )

    def test_text_rendering_lists_each_file(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "omarchy-a-test.sh").write_text(FIXTURE_A, encoding="utf-8")
            text = mapper.render_text(mapper.map_upstream(root))
        self.assertIn("omarchy-a  (2 calls in 1 files)", text)
        self.assertIn("omarchy-a-test.sh  x2", text)
        self.assertIn("omarchy-b  (1 calls in 1 files)", text)


if __name__ == "__main__":
    unittest.main()
