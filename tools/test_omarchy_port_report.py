"""Tests for tools/omarchy_port_report.py. Run with: python -m unittest tools/test_omarchy_port_report.py"""

from __future__ import annotations

import json
import os
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import omarchy_port_report as report  # noqa: E402


def features_of(source: str) -> set[str]:
    return {item["feature"] for item in report.analyze_source(source)["features"]}


class LexerFeatureTests(unittest.TestCase):
    def test_here_document_is_detected_with_its_body(self):
        source = "cat <<EOF\nline one $(not_run)\nEOF\necho done\n"
        result = report.analyze_source(source)
        self.assertIn("heredoc", {f["feature"] for f in result["features"]})
        self.assertEqual(result["verdict"], "blocked")
        # `cat` is an external command; `echo` is a builtin and is not listed.
        self.assertIn("cat", result["external_commands"])
        self.assertNotIn("echo", result["external_commands"])

    def test_double_angle_inside_quotes_or_comments_is_not_a_here_document(self):
        self.assertNotIn("heredoc", features_of('echo "a << b"\n'))
        self.assertNotIn("heredoc", features_of("# cat <<EOF\necho ok\n"))

    def test_arrays_and_array_expansion(self):
        features = features_of('files=(a b c)\nfor f in "${files[@]}"; do echo "$f"; done\n')
        self.assertIn("array_assign", features)
        self.assertIn("array_expansion", features)
        self.assertIn("loop", features)

    def test_regex_match_in_double_bracket_condition(self):
        self.assertIn("regex_match", features_of('if [[ "$x" =~ ^a+$ ]]; then echo y; fi\n'))

    def test_read_getopts_local_and_process_substitution(self):
        source = (
            "f() {\n  local name=$1\n  while read -r line; do echo \"$line\"; done < <(printf 'x\\n')\n}\n"
            "while getopts 'ab:' opt; do echo \"$opt\"; done\n"
        )
        features = features_of(source)
        self.assertIn("function_definition", features)
        self.assertIn("local_variable", features)
        self.assertIn("read_builtin", features)
        self.assertIn("process_substitution", features)
        self.assertIn("getopts", features)

    def test_trap_and_eval_block_a_direct_port(self):
        result = report.analyze_source("trap 'rm -f x' EXIT\neval \"$cmd\"\n")
        self.assertEqual(result["verdict"], "blocked")
        self.assertIn("trap", result["feature_names"])
        self.assertIn("eval", result["feature_names"])

    def test_command_substitution_and_positional_arguments_inside_quotes(self):
        features = features_of('name="$(basename "$1")"\necho "$name"\n')
        self.assertIn("command_substitution", features)
        self.assertIn("positional_arguments", features)

    def test_arithmetic_condition_after_if(self):
        result = report.analyze_source("if (($# == 0)); then exit 1; fi\n")
        self.assertIn("arithmetic", result["feature_names"])
        self.assertNotIn("$#", result["external_commands"])

    def test_exec_names_the_following_command(self):
        result = report.analyze_source("exec setsid uwsm-app -- nautilus\n")
        names = result["external_commands"]
        self.assertIn("setsid", names)
        self.assertEqual(result["verdict"], "partial")

    def test_verdict_levels(self):
        self.assertEqual(report.analyze_source("echo hi\n")["verdict"], "portable")
        self.assertEqual(report.analyze_source("x=$((1 + 2))\n")["verdict"], "partial")
        self.assertEqual(report.analyze_source("cat <<EOF\nx\nEOF\n")["verdict"], "blocked")


class FileAnalysisTests(unittest.TestCase):
    def test_crlf_input_is_normalized_and_reported(self):
        with tempfile.TemporaryDirectory() as directory:
            path = os.path.join(directory, "script")
            with open(path, "wb") as handle:
                handle.write(b"#!/bin/bash\r\nif (($# == 0)); then\r\n  exit 1\r\nfi\r\n")
            result = report.analyze_file(path, None, set())
            self.assertEqual(result["line_endings"], "crlf_normalized")
            self.assertEqual(result["verdict"], "partial")
            self.assertNotIn("$#", [c["name"] for c in result["external_commands"]])

    def test_cli_writes_json_and_summary_over_the_fixtures(self):
        with tempfile.TemporaryDirectory() as directory:
            out = os.path.join(directory, "report.json")
            code = report.main(["--json", out])
            self.assertEqual(code, 0)
            with open(out, encoding="utf-8") as handle:
                data = json.load(handle)
            self.assertEqual(data["coverage"]["inventory_commands"], 444)
            self.assertGreaterEqual(data["coverage"]["analyzed"], 1)
            self.assertIn("bin/omarchy-launch-nautilus", data["scripts"])
            self.assertEqual(data["scripts"]["bin/omarchy-launch-nautilus"]["verdict"], "partial")


if __name__ == "__main__":
    unittest.main()
