"""Keeps docs/migrating-from-bash.md in step with the FEATURES table in tools/omarchy_port_report.py.

Rows of the guide's idiom tables that map to a port-report feature carry a marker comment,
for example `<!-- feature: eval -->`, at the end of the row. The status cell of a marked row
must agree with that feature's category. The table is read through the CLI flag
`--features-json`, the same path CI uses.

Run from the repository root with: python tools/test_omarchy_port_features.py
"""

from __future__ import annotations

import copy
import json
import os
import re
import subprocess
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
GUIDE = os.path.join(ROOT, "docs", "migrating-from-bash.md")
REPORT = os.path.join(HERE, "omarchy_port_report.py")

sys.path.insert(0, HERE)
import omarchy_port_report as report  # noqa: E402

# Guide status words accepted for each FEATURES category. "partial" accepts "applet"
# because a partial feature is usually solved by an applet.
ALLOWED = {
    "supported": {"supported"},
    "partial": {"partial", "applet"},
    "unsupported": {"missing"},
}
HEADER = "| Bash idiom |"
TAG = re.compile(r"<!-- feature: ([a-z_]+) -->")
# A pipe inside a cell is written as \| and does not end the cell.
CELL_SPLIT = re.compile(r"(?<!\\)\|")


def read_guide(path: str = GUIDE) -> str:
    with open(path, encoding="utf-8") as handle:
        return handle.read()


def idiom_rows(text: str):
    """Yield (line number, idiom, status, feature or None) for each row of an idiom table."""
    in_table = False
    for number, line in enumerate(text.splitlines(), 1):
        if line.startswith(HEADER):
            in_table = True
            continue
        if not in_table:
            continue
        if not line.startswith("|"):
            in_table = False
            continue
        if line.startswith("| ---"):
            continue
        cells = CELL_SPLIT.split(line.strip())[1:-1]
        if len(cells) != 4:
            raise AssertionError(f"line {number}: idiom row has {len(cells)} cells, expected 4")
        tag = TAG.search(cells[3])
        yield number, cells[0].strip(), cells[2].strip(), tag[1] if tag else None


def drift_errors(text: str, features: dict) -> list[str]:
    """Return one message per guide row whose status disagrees with its feature."""
    errors = []
    for number, idiom, status, feature in idiom_rows(text):
        if feature is None:
            continue
        if feature not in features:
            errors.append(f"line {number}: {idiom}: marker names unknown feature {feature!r}")
            continue
        category = features[feature]["category"]
        allowed = ALLOWED[category]
        if status not in allowed:
            errors.append(
                f"line {number}: {idiom}: status {status!r} does not match feature {feature!r} "
                f"({category}); expected one of {sorted(allowed)}"
            )
    return errors


def feature_table_from_cli() -> dict:
    with tempfile.TemporaryDirectory() as scratch:
        out = os.path.join(scratch, "features.json")
        subprocess.run(
            [sys.executable, REPORT, "--features-json", out],
            check=True,
            capture_output=True,
            text=True,
        )
        with open(out, encoding="utf-8") as handle:
            return json.load(handle)


class FeatureTableTests(unittest.TestCase):
    def test_cli_emits_the_features_table(self):
        table = feature_table_from_cli()
        self.assertEqual(table, report.feature_table())
        self.assertEqual(set(table), set(report.FEATURES))
        for name, entry in table.items():
            self.assertIn(entry["category"], ALLOWED, name)


class GuideStatusTests(unittest.TestCase):
    def setUp(self):
        self.text = read_guide()
        self.features = feature_table_from_cli()

    def test_guide_status_matches_feature_table(self):
        errors = drift_errors(self.text, self.features)
        self.assertEqual(errors, [], "\n".join(errors))

    def test_guide_covers_at_least_40_idioms(self):
        self.assertGreaterEqual(len(list(idiom_rows(self.text))), 40)

    def test_changed_feature_category_fails_with_the_idiom_name(self):
        changed = copy.deepcopy(self.features)
        changed["heredoc"]["category"] = "supported"
        errors = drift_errors(self.text, changed)
        self.assertTrue(errors, "a changed feature category must fail the guide check")
        self.assertTrue(any("`cat <<EOF`" in error for error in errors), "\n".join(errors))

    def test_changed_guide_status_fails_with_the_idiom_name(self):
        edited = self.text.replace("| `eval` | None. | missing |", "| `eval` | None. | supported |", 1)
        self.assertNotEqual(edited, self.text, "the eval row was not found")
        errors = drift_errors(edited, self.features)
        self.assertEqual(len(errors), 1, "\n".join(errors))
        self.assertIn("`eval`", errors[0])

    def test_unknown_feature_marker_fails(self):
        edited = self.text.replace("<!-- feature: eval -->", "<!-- feature: no_such_feature -->", 1)
        errors = drift_errors(edited, self.features)
        self.assertTrue(any("no_such_feature" in error for error in errors), "\n".join(errors))


if __name__ == "__main__":
    unittest.main()
