"""Tests for tools/example_lint.py. Run with: python tools/test_example_lint.py"""

from __future__ import annotations

import contextlib
import io
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import example_lint as gate  # noqa: E402


def rules_for(text: str) -> list[str]:
    return [f["rule"] for f in gate.analyze_text("fixture.program", text)]


class RuleTests(unittest.TestCase):
    def test_jq_object_build_with_bindings_is_flagged(self):
        text = (
            'then jq with flags ["-n", "-c", "--arg", "label", label] '
            'and filter "{label:$label}" as out\n'
        )
        self.assertIn("example.jq_object_build", rules_for(text))

    def test_jq_without_object_build_is_not_flagged(self):
        text = 'run jq with flags ["-n", "-r", "--arg", "draft", draft] and filter "$draft" as out\n'
        self.assertNotIn("example.jq_object_build", rules_for(text))

    def test_jq_object_filter_without_arg_is_not_flagged(self):
        text = 'run jq with flags ["-c"] and filter "{a:.b}" as out\n'
        self.assertNotIn("example.jq_object_build", rules_for(text))

    def test_printf_only_to_bind_is_flagged(self):
        text = 'then "printf" with arguments ["%s", evidence] as bounded\n'
        self.assertIn("example.printf_bind", rules_for(text))

    def test_printf_that_prints_is_not_flagged(self):
        text = 'run printf with message "all records processed"\n'
        self.assertNotIn("example.printf_bind", rules_for(text))

    def test_text_concat_building_json_is_flagged(self):
        text = 'run text.concat with parts ["{\\"label\\":\\"", label, "\\"}"] as body\n'
        self.assertIn("example.text_concat_json", rules_for(text))

    def test_text_concat_plain_text_is_not_flagged(self):
        text = 'run text.concat with parts ["hello", "graph"] and separator " " as joined\n'
        self.assertNotIn("example.text_concat_json", rules_for(text))

    def test_unread_binding_is_flagged_and_read_binding_is_not(self):
        text = (
            'run tool.a with path p as used\n'
            'run tool.b with path used as other\n'
            'run tool.c with path other as never\n'
        )
        findings = gate.analyze_text("fixture.program", text)
        unread = [f["line"] for f in findings if f["rule"] == "example.unread_binding"]
        self.assertEqual(unread, [3])

    def test_accept_type_words_are_not_bindings(self):
        text = (
            "accept workspace_path as text with default \"workspace.txt\"\n"
            "read workspace_path as workspace\n"
            "then encode score as json\n"
        )
        self.assertEqual(
            [f["line"] for f in gate.analyze_text("fixture.program", text)
             if f["rule"] == "example.unread_binding"],
            [2],
        )

    def test_hash_inside_string_is_not_a_comment(self):
        code, comment = gate.split_comment('run tool with text "a # b" as x\n')
        self.assertEqual(comment, "")
        self.assertIn('"a # b"', code)


class AllowTests(unittest.TestCase):
    def test_own_allow_comment_suppresses_the_finding(self):
        text = 'then "printf" with arguments ["%s", e] as bounded # execute-allow: example.printf_bind keep\n'
        self.assertNotIn("example.printf_bind", rules_for(text))

    def test_existing_unused_binding_allow_suppresses_unread(self):
        text = 'run tool with path p as bounded # execute-allow: lint.unused_binding intentional\n'
        self.assertNotIn("example.unread_binding", rules_for(text))

    def test_allow_for_another_rule_does_not_suppress(self):
        text = 'run tool with path p as bounded # execute-allow: example.printf_bind\n'
        self.assertIn("example.unread_binding", rules_for(text))


class TreeAndRatchetTests(unittest.TestCase):
    def make_root(self, tmp: str, program: str) -> Path:
        root = Path(tmp)
        (root / "examples").mkdir()
        (root / "tools").mkdir()
        (root / "examples" / "one.program").write_text(program, encoding="utf-8")
        return root

    def run_main(self, args: list[str]) -> int:
        with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
            return gate.main(args)

    def test_one_jq_object_build_fixture_reports_one_finding(self):
        program = (
            'accept label as text with default "x"\n'
            'run jq with flags ["-n", "-c", "--arg", "label", label] '
            'and filter "{label:$label}" as out\n'
            "then encode out as json\n"
        )
        with tempfile.TemporaryDirectory() as tmp:
            root = self.make_root(tmp, program)
            findings = gate.lint_tree(root)
            self.assertEqual(gate.count_by_rule(findings)["example.jq_object_build"], 1)
            self.assertEqual(findings[0]["path"], "examples/one.program")
            self.assertEqual(findings[0]["line"], 2)

    def test_update_then_check_passes_and_growth_fails(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = self.make_root(tmp, 'then "printf" with arguments ["%s", e] as bounded\n')
            baseline = root / "tools" / "example_lint_baseline.json"
            self.assertEqual(self.run_main(["--root", str(root), "--update-baseline"]), 0)
            self.assertEqual(self.run_main(["--root", str(root)]), 0)
            (root / "examples" / "two.program").write_text(
                'then "printf" with arguments ["%s", e] as bounded\n', encoding="utf-8"
            )
            self.assertEqual(self.run_main(["--root", str(root)]), 1)
            self.assertTrue(baseline.exists())

    def test_missing_baseline_is_a_usage_error(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = self.make_root(tmp, "run tool with path p as q\n")
            self.assertEqual(self.run_main(["--root", str(root)]), 2)

    def test_committed_baseline_matches_examples_corpus(self):
        repo = Path(__file__).resolve().parent.parent
        counts = gate.count_by_rule(gate.lint_tree(repo))
        baseline = gate.load_baseline(repo / "tools" / "example_lint_baseline.json")
        for rule in gate.RULES:
            self.assertLessEqual(counts[rule], baseline[rule], rule)


if __name__ == "__main__":
    unittest.main()
