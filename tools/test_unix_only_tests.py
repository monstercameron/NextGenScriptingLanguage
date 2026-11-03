#!/usr/bin/env python3
"""Guard for the Unix-only integration test list (#521).

`tools/unix_only_tests.json` is the checked list of test files that stay
Unix-only, each with the Unix-only mechanism that still blocks a Windows port.
This check refuses a new `#![cfg(...unix...)]` test file that is not on the
list, and refuses stale entries for files that no longer exist or are no
longer Unix-gated, so follow-up conversions keep the list accurate.
"""

from __future__ import annotations

import json
import os
import pathlib
import re
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[1]
LIST = ROOT / "tools" / "unix_only_tests.json"
TESTS = ROOT / "tests"
FILE_GATE = re.compile(r"^#\!\s*\[cfg\(.*unix.*\)\]")


def unix_only_tests() -> set[str]:
    gated: set[str] = set()
    for path in sorted(TESTS.glob("*.rs")):
        try:
            text = path.read_text(encoding="utf-8")
        except OSError:
            continue
        if any(FILE_GATE.match(line.strip()) for line in text.splitlines()):
            gated.add(path.name)
    return gated


def listed_tests() -> dict[str, str]:
    data = json.loads(LIST.read_text(encoding="utf-8"))
    return {name: reason for name, reason in data.items() if not name.startswith("_")}


class UnixOnlyListTests(unittest.TestCase):
    def test_every_unix_only_file_is_listed_with_a_reason(self) -> None:
        listed = listed_tests()
        for name in sorted(unix_only_tests() - set(listed)):
            with self.subTest(file=name):
                self.fail(
                    f"tests/{name} is Unix-only but not in tools/unix_only_tests.json: "
                    "convert it to the cross-platform fixture or add it with a reason"
                )

    def test_no_stale_list_entries(self) -> None:
        gated = unix_only_tests()
        listed = listed_tests()
        for name in sorted(set(listed) - gated):
            with self.subTest(file=name):
                self.fail(
                    f"tools/unix_only_tests.json lists tests/{name} but it is gone "
                    "or no longer Unix-only: remove the entry"
                )

    def test_every_entry_has_a_reason(self) -> None:
        for name, reason in sorted(listed_tests().items()):
            with self.subTest(file=name):
                self.assertIsInstance(reason, str)
                self.assertTrue(reason.strip(), f"tools/unix_only_tests.json entry for {name} needs a reason")

    def test_guard_runs_in_ci(self) -> None:
        ci_file = pathlib.Path(os.environ.get("LANE_W3_CI_FILE", ROOT / ".github/workflows/ci.yml"))
        self.assertIn("python3 tools/test_unix_only_tests.py", ci_file.read_text(encoding="utf-8"))

    # Every suite converted for #521 must run in the Native Windows job.
    CONVERTED_SUITES = [
        "adversarial",
        "cache_dependencies",
        "child_env_minimize",
        "cli",
        "conditions",
        "cpu",
        "direct_commands",
        "github_actions",
        "graph_composition_acceptance",
        "graph_result_events",
        "inspection",
        "invocation_io",
        "lane_a_cli",
        "lane_o_init",
        "loops",
        "manifest_catalog_cli",
        "model_cli",
        "model_control",
        "native_status",
        "opt_child_wakeup",
        "opt_diagnostics_poll",
        "opt_local_child_observer",
        "opt_services_checkpoint_listener",
        "opt_small_input",
        "opt_stdin_prefill",
        "process_inputs",
        "program_inputs",
        "program_module_accounting",
        "resume",
        "reusable_programs",
        "security_dataflow_runtime",
        "security_redaction",
        "service_environment",
        "stdin",
        "streaming",
        "structured_json",
        "terminal_output_escape",
        "timeout",
        "typed_contracts",
        "ui",
        "worker_backend",
    ]

    def test_converted_suites_run_on_windows(self) -> None:
        ci_file = pathlib.Path(os.environ.get("LANE_W3_CI_FILE", ROOT / ".github/workflows/ci.yml"))
        windows = ci_file.read_text(encoding="utf-8").split("  windows:\n", maxsplit=1)[1]
        command = next(
            line for line in windows.splitlines()
            if "cargo test --locked --target ${{ matrix.target }}" in line
            and "--test windows_native" in line
        )
        self.assertIn("--test robustness", command)
        self.assertIn("--test applet_boundaries_bench", command)
        for suite in self.CONVERTED_SUITES:
            with self.subTest(suite=suite):
                self.assertIn(f"--test {suite}", command)


if __name__ == "__main__":
    unittest.main()
