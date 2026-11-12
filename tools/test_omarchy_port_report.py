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
        # Heredocs map to raw blocks with use value, so they are partial, not blocked.
        self.assertEqual(result["verdict"], "partial")
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
        self.assertEqual(report.analyze_source("cat <<EOF\nx\nEOF\n")["verdict"], "partial")


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


class ExternalCommandTests(unittest.TestCase):
    def test_function_call_is_not_an_external_command(self):
        source = "usage() {\n  ls\n}\nusage\nfunction helper {\n  true\n}\nhelper\n"
        result = report.analyze_source(source)
        self.assertEqual(result["external_commands"], ["ls"])
        self.assertEqual(result["functions"], ["helper", "usage"])

    def test_case_labels_are_not_external_commands(self):
        source = (
            'case "$1" in\n'
            "  start|on) systemctl start x ;;\n"
            "  --clear) rm -f y ;;\n"
            "  *) cat z ;;\n"
            "esac\n"
        )
        result = report.analyze_source(source)
        self.assertEqual(result["external_commands"], ["cat", "rm", "systemctl"])

    def test_python_script_is_not_lexed_as_bash(self):
        with tempfile.TemporaryDirectory() as directory:
            path = os.path.join(directory, "helper")
            with open(path, "wb") as handle:
                handle.write(b"#!/usr/bin/python3\ndef main():\n    import json\n    print(json.dumps({}))\n")
            result = report.analyze_file(path, None, set())
            self.assertEqual(result["language"], "python")
            self.assertEqual(result["analysis"], "not_shell")
            self.assertEqual(result["verdict"], "out_of_scope")
            self.assertEqual(result["external_commands"], [])

    def test_aggregate_ranking_has_no_function_label_or_python_token(self):
        with tempfile.TemporaryDirectory() as directory:
            bin_dir = os.path.join(directory, "bin")
            os.mkdir(bin_dir)
            with open(os.path.join(bin_dir, "tool"), "wb") as handle:
                handle.write(b"usage() { ls; }\ncase \"$1\" in\n  fail) cat x ;;\n  *) usage ;;\nesac\n")
            with open(os.path.join(bin_dir, "helper"), "wb") as handle:
                handle.write(b"#!/usr/bin/python3\ndef main():\n    import json\n")
            out = os.path.join(directory, "report.json")
            manifests = os.path.join(directory, "manifests")
            os.mkdir(manifests)
            self.assertEqual(report.main(["--fixtures", bin_dir, "--manifests", manifests, "--json", out]), 0)
            with open(out, encoding="utf-8") as handle:
                data = json.load(handle)
            names = [entry["name"] for entry in data["external_command_ranking"]]
            self.assertEqual(names, ["cat", "ls"])
            self.assertEqual(data["verdicts"]["out_of_scope"], 1)


class TopCommandTableTests(unittest.TestCase):
    def test_call_lines_count_distinct_source_lines_per_external_command(self):
        result = report.analyze_source("cat a\ncat b\ncat c\nusage() { cat d; }\nusage\n")
        self.assertEqual(result["external_commands"], ["cat"])
        self.assertEqual(result["external_call_lines"], {"cat": 4})
        with tempfile.TemporaryDirectory() as directory:
            path = os.path.join(directory, "script")
            with open(path, "wb") as handle:
                handle.write(b"#!/bin/bash\ncat a\ncat b\ncat c\nusage() { cat d; }\nusage\n")
            analyzed = report.analyze_file(path, None, set())
            self.assertEqual(analyzed["external_commands"], [{"name": "cat", "manifest": False, "call_lines": 4}])

    def test_top_fifty_table_has_no_function_case_label_or_python_token(self):
        with tempfile.TemporaryDirectory() as directory:
            bin_dir = os.path.join(directory, "bin")
            os.mkdir(bin_dir)
            commands = [f"tool{index:02d}" for index in range(60)]
            body = "usage() { ls; }\nfail() { false; }\ncase \"$1\" in\n  start|on) usage ;;\n  --clear) fail ;;\n  *) usage ;;\nesac\n"
            body += "".join(f"{name} x\n" for name in commands)
            with open(os.path.join(bin_dir, "tool"), "wb") as handle:
                handle.write(("#!/bin/bash\n" + body).encode("utf-8"))
            with open(os.path.join(bin_dir, "helper"), "wb") as handle:
                handle.write(b"#!/usr/bin/python3\ndef main():\n    import json\n    except ValueError:\n        raise\n")
            manifests = os.path.join(directory, "manifests")
            os.mkdir(manifests)
            out = os.path.join(directory, "report.json")
            markdown = os.path.join(directory, "top.md")
            code = report.main([
                "--fixtures", bin_dir, "--manifests", manifests,
                "--json", out, "--top-markdown", markdown,
            ])
            self.assertEqual(code, 0)
            with open(markdown, encoding="utf-8") as handle:
                rows = [line for line in handle.read().splitlines() if line.startswith("| ")]
            body_rows = rows[2:]  # header and separator
            self.assertEqual(len(body_rows), 50)
            # `ls` sorts before `tool00` when every command has one script.
            self.assertEqual(body_rows[0], "| 1 | `ls` | 1 | 1 | no |")
            self.assertEqual(body_rows[1], "| 2 | `tool00` | 1 | 1 | no |")
            table = "\n".join(body_rows)
            for token in ("usage", "fail", "start", "on", "--clear", "def", "import", "except", "raise", "json.dumps"):
                self.assertNotIn(f"`{token}`", table)
            with open(out, encoding="utf-8") as handle:
                data = json.load(handle)
            names = {entry["name"] for entry in data["external_command_ranking"]}
            self.assertEqual(names, set(commands) | {"ls"})


class InternalHelperRankingTests(unittest.TestCase):
    def run_report(self, scripts: dict[str, str], inventory_paths: list[str]) -> dict:
        with tempfile.TemporaryDirectory() as directory:
            bin_dir = os.path.join(directory, "bin")
            os.mkdir(bin_dir)
            for name, body in scripts.items():
                with open(os.path.join(bin_dir, name), "wb") as handle:
                    handle.write(body.encode("utf-8"))
            inventory = {
                "release_tag": "test",
                "commit": "0" * 40,
                "command_count": len(inventory_paths),
                "commands": [{"path": path} for path in inventory_paths],
            }
            inventory_file = os.path.join(directory, "inventory.json")
            with open(inventory_file, "w", encoding="utf-8") as handle:
                json.dump(inventory, handle)
            manifests = os.path.join(directory, "manifests")
            os.mkdir(manifests)
            out = os.path.join(directory, "report.json")
            code = report.main([
                "--inventory", inventory_file, "--fixtures", bin_dir,
                "--manifests", manifests, "--json", out,
            ])
            self.assertEqual(code, 0)
            with open(out, encoding="utf-8") as handle:
                return json.load(handle)

    def test_helpers_are_ranked_by_distinct_calling_scripts(self):
        scripts = {
            # Calls notify twice: still one calling script.
            "omarchy-menu": "#!/bin/bash\nomarchy-notify a\nomarchy-notify b\nomarchy-pkg-add x\nls\n",
            "omarchy-launch": "#!/bin/bash\nomarchy-notify c\nomarchy-launch again\n",
            "omarchy-sync": "#!/bin/bash\nomarchy-pkg-add y\nomarchy-remote-only\n",
            "omarchy-notify": "#!/bin/bash\necho sent\n",
            "omarchy-pkg-add": "#!/bin/bash\necho added\n",
            # A Python script is not lexed, so its mention of notify is not a call.
            "omarchy-py": "#!/usr/bin/python3\nprint('omarchy-notify')\n",
        }
        inventory = [f"bin/{name}" for name in scripts] + ["bin/omarchy-remote-only"]
        data = self.run_report(scripts, inventory)
        ranking = data["internal_helper_ranking"]
        self.assertEqual(
            [(item["name"], item["scripts"]) for item in ranking],
            [("omarchy-notify", 2), ("omarchy-pkg-add", 2), ("omarchy-remote-only", 1)],
        )
        notify = ranking[0]
        self.assertEqual(notify["path"], "bin/omarchy-notify")
        self.assertEqual(notify["callers"], ["bin/omarchy-launch", "bin/omarchy-menu"])
        self.assertFalse(notify["manifest"])

    def test_self_calls_and_external_commands_are_not_helpers(self):
        scripts = {
            "omarchy-loop": "#!/bin/bash\nomarchy-loop again\ncurl -s x\n",
            "omarchy-notify": "#!/bin/bash\necho sent\n",
        }
        data = self.run_report(scripts, ["bin/omarchy-loop", "bin/omarchy-notify"])
        self.assertEqual(data["internal_helper_ranking"], [])
        # The external ranking still lists curl; only the helper ranking excludes it.
        self.assertIn("curl", [item["name"] for item in data["external_command_ranking"]])


if __name__ == "__main__":
    unittest.main()
