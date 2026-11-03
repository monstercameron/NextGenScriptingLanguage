"""Tests for tools/replacement_gates.py. Run with: python -m unittest tools/test_replacement_gates.py"""

from __future__ import annotations

import json
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import replacement_gates as gates  # noqa: E402


class ReplacementGateSummaryTests(unittest.TestCase):
    def test_page_summary_is_generated_from_ledger(self):
        ledger = {
            "release": "1.2.3",
            "commit": "a" * 40,
            "gates": [
                {
                    "id": 1,
                    "priority": "P0",
                    "requirement": "Unix compatibility",
                    "status": "partial",
                    "evidence_and_gap": "pipes | bytes",
                }
            ],
        }
        page = f"before\n{gates.START}\nold\n{gates.END}\nafter\n"
        result = gates.render(ledger, page)
        self.assertIn("Execute `1.2.3`", result)
        self.assertIn("`" + "a" * 40 + "`", result)
        self.assertIn("pipes \\| bytes", result)
        self.assertNotIn("old", result)

    def test_committed_json_and_page_are_current_and_consistent(self):
        ledger = json.loads(gates.LEDGER.read_text(encoding="utf-8"))
        page = gates.PAGE.read_text(encoding="utf-8")
        self.assertEqual(ledger["release"], gates.current_release())
        self.assertRegex(ledger["commit"], r"^[0-9a-f]{40}$")
        self.assertEqual(gates.render(ledger, page), page)


if __name__ == "__main__":
    unittest.main()
