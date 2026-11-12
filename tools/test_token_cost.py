"""Tests for tools/token_cost.py. Run with: python tools/test_token_cost.py"""

from __future__ import annotations

import contextlib
import io
import json
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import token_cost as tc  # noqa: E402


def write_pair(root: Path, name: str, original: str, port: str) -> None:
    folder = root / name
    folder.mkdir(parents=True)
    (folder / "original.sh").write_text(original, encoding="utf-8", newline="\n")
    (folder / "port.program").write_text(port, encoding="utf-8", newline="\n")


class ArithmeticTests(unittest.TestCase):
    def test_strip_drops_comments_and_blank_lines(self):
        text = "#!/bin/bash\n\n# note\n  # indented note\nq w\n"
        self.assertEqual(tc.strip_text(text), "q w\n")

    def test_measure_counts_words_and_punctuation(self):
        # "x ( y ) ;\n": words x, y; punctuation (, ), ;  -> 5 tokens; 10 bytes
        self.assertEqual(tc.measure("x ( y ) ;\n"), (10, 5))

    def test_fixture_ratios_and_median(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            # token ratio 4/2 = 2.0, byte ratio 8/4 = 2.0
            write_pair(root, "a-two", "a b\n", "a b c d\n")
            # token ratio 5/1 = 5.0, byte ratio 10/2 = 5.0
            write_pair(root, "b-five", "x\n", "x ( y ) ;\n")
            # comments and blanks removed: 2 tokens/4 bytes vs 3 tokens/6 bytes
            write_pair(
                root,
                "c-comments",
                "#!/bin/bash\n\n# note\nq w\n",
                "# port note\n\nq w e\n",
            )
            rows = tc.report(root)
            self.assertEqual([r["name"] for r in rows], ["a-two", "b-five", "c-comments"])
            self.assertEqual(rows[0]["token_ratio"], 2.0)
            self.assertEqual(rows[1]["token_ratio"], 5.0)
            self.assertEqual(rows[2]["token_ratio"], 1.5)
            self.assertEqual(rows[2]["byte_ratio"], 1.5)

            summary = tc.summarize(rows)
            self.assertEqual(summary["scripts"], 3)
            self.assertEqual(summary["median_token_ratio"], 2.0)
            self.assertEqual(summary["median_byte_ratio"], 2.0)
            # port tokens 4+5+3 = 12 over original tokens 2+1+2 = 5
            self.assertEqual(summary["aggregate_token_ratio"], 2.4)

    def test_folder_without_port_is_skipped(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            write_pair(root, "a-pair", "a\n", "a\n")
            (root / "b-missing-port").mkdir()
            (root / "b-missing-port" / "original.sh").write_text("b\n", encoding="utf-8")
            self.assertEqual([r["name"] for r in tc.report(root)], ["a-pair"])


class OutputTests(unittest.TestCase):
    def run_main(self, argv: list[str]) -> tuple[int, str]:
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            code = tc.main(argv)
        return code, out.getvalue()

    def test_same_fixed_inputs_give_identical_output(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp) / "pairs"
            write_pair(root, "p-one", "a b\n", "a b c\n")
            write_pair(root, "p-two", "# c\nx\n", "x ( y )\n")
            first = self.run_main(["--root", str(root), "--json", str(Path(tmp) / "one.json")])
            second = self.run_main(["--root", str(root), "--json", str(Path(tmp) / "two.json")])
            self.assertEqual(first, second)
            self.assertEqual(
                (Path(tmp) / "one.json").read_bytes(),
                (Path(tmp) / "two.json").read_bytes(),
            )

    def test_real_ports_report_one_row_each_and_are_deterministic(self):
        self.assertEqual(self.run_main(["--root", str(tc.DEFAULT_ROOT)])[0], 0)
        rows = tc.report(tc.DEFAULT_ROOT)
        self.assertEqual(len(rows), 20)
        self.assertEqual(tc.report(tc.DEFAULT_ROOT), rows)
        self.assertEqual(
            self.run_main(["--root", str(tc.DEFAULT_ROOT)]),
            self.run_main(["--root", str(tc.DEFAULT_ROOT)]),
        )

    def test_json_summary_has_median_token_ratio(self):
        with tempfile.TemporaryDirectory() as tmp:
            out_path = Path(tmp) / "report.json"
            self.assertEqual(self.run_main(["--json", str(out_path)])[0], 0)
            payload = json.loads(out_path.read_text(encoding="utf-8"))
            self.assertEqual(payload["summary"]["scripts"], 20)
            self.assertIn("median_token_ratio", payload["summary"])


class RatchetTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.baseline = Path(self.tmp.name) / "baseline.json"
        self.baseline.write_text(json.dumps({"median_token_ratio": 2.0}), encoding="utf-8")

    def tearDown(self):
        self.tmp.cleanup()

    def summary(self, median: float) -> dict:
        return {"median_token_ratio": median}

    def test_growth_past_slack_fails(self):
        self.assertIsNotNone(tc.check_baseline(self.summary(2.5), self.baseline))

    def test_equal_or_smaller_median_passes(self):
        self.assertIsNone(tc.check_baseline(self.summary(2.0), self.baseline))
        self.assertIsNone(tc.check_baseline(self.summary(1.9), self.baseline))

    def test_growth_within_slack_passes(self):
        self.assertIsNone(tc.check_baseline(self.summary(2.004), self.baseline))


if __name__ == "__main__":
    unittest.main()
