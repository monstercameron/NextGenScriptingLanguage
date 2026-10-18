import json
import tempfile
import unittest
from pathlib import Path
import sys

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "tools"))
import collect_flaky
import release
import sbom


class ReleaseToolRegressions(unittest.TestCase):
    def test_prerelease_patch_bump_releases_candidate_version(self):
        self.assertEqual(release.bumped_version("0.9.0-rc.5", "patch"), "0.9.0")

    def test_set_version_updates_and_checks_fuzz_lockfile(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "fuzz").mkdir()
            (root / "Cargo.toml").write_text('[package]\nname = "execute"\nversion = "0.9.0-rc.5"\n')
            lock = '[[package]]\nname = "execute"\nversion = "0.9.0-rc.5"\n'
            (root / "Cargo.lock").write_text(lock)
            (root / "fuzz/Cargo.lock").write_text(lock)
            release.set_version("0.9.0-rc.6", root)
            self.assertEqual(release.version(root), "0.9.0-rc.6")
            self.assertIn('version = "0.9.0-rc.6"', (root / "fuzz/Cargo.lock").read_text())


class FlakyToolRegressions(unittest.TestCase):
    def test_classification_is_per_os_and_missing_runs_are_reported(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for label, status in (("windows-2022-1", "failed"), ("ubuntu-24.04-1", "passed")):
                folder = root / label
                folder.mkdir()
                failure = '<failure message="boom" />' if status == "failed" else ""
                (folder / "junit.xml").write_text(
                    f'<testsuites><testsuite><testcase classname="s" name="case">{failure}</testcase></testsuite></testsuites>'
                )
            tests, runs, errors, missing = collect_flaky.collect(
                root, expected_os=["windows-2022", "ubuntu-24.04"], expected_runs=2
            )
            report = collect_flaky.summary_markdown(tests, runs, errors, missing)
            self.assertIn("**failing** on windows-2022", report)
            self.assertIn("junit-windows-2022-2", report)
            self.assertEqual(len(missing), 4)

    def test_sbom_uses_normal_closure_and_spdx_license_operators(self):
        metadata = {
            "resolve": {
                "root": "root",
                "nodes": [
                    {"id": "root", "deps": [
                        {"pkg": "runtime", "dep_kinds": [{"kind": "normal"}]},
                        {"pkg": "test-only", "dep_kinds": [{"kind": "dev"}]},
                    ]},
                    {"id": "runtime", "deps": []},
                    {"id": "test-only", "deps": []},
                ],
            },
            "packages": [
                {"id": "root", "name": "execute", "version": "1.0.0", "license": "MIT"},
                {"id": "runtime", "name": "runtime", "version": "1.0.0", "license": "MIT/Apache-2.0"},
                {"id": "test-only", "name": "test-only", "version": "1.0.0", "license": "MIT"},
            ],
        }
        result = sbom.build_sbom(metadata, "", "1.0.0", 1)
        components = {component["name"]: component for component in result["components"]}
        self.assertIn("runtime", components)
        self.assertNotIn("test-only", components)
        self.assertEqual(components["runtime"]["licenses"], [{"expression": "MIT OR Apache-2.0"}])

    def test_ci_budgets_are_enforceable_numbers(self):
        for name in (
            "linux-binary-budget.json",
            "linux-minimal-binary-budget.json",
            "linux-latency-budget.json",
            "linux-bench-budget.json",
        ):
            document = json.loads((ROOT / "tools" / name).read_text())
            limits = [value for key, value in document.items() if key != "note"]
            self.assertTrue(limits)
            self.assertTrue(all(isinstance(value, (int, float)) for value in limits), name)
            self.assertTrue(
                "measured" in document["note"] or "234c1fc" in document["note"], name
            )

    def test_fuzz_workflow_uses_validated_environment_input(self):
        workflow = (ROOT / ".github/workflows/fuzz.yml").read_text()
        self.assertIn("FUZZ_SECONDS: ${{ github.event.inputs.seconds || 1200 }}", workflow)
        self.assertIn("[[ \"$FUZZ_SECONDS\" =~ ^[0-9]+$ ]]", workflow)
        self.assertNotIn("-max_total_time=${{ github.event.inputs.seconds", workflow)

    def test_release_and_reproducible_builds_share_remap_configuration(self):
        workflow = (ROOT / ".github/workflows/packages.yml").read_text()
        self.assertEqual(workflow.count("--remap-path-prefix=$PWD=/build"), 2)
        self.assertIn('SOURCE_DATE_EPOCH="$(git log -1 --format=%ct)"', workflow)
        self.assertIn('SOURCE_DATE_EPOCH="$(git -C build-a log -1 --format=%ct)"', workflow)

    def test_shell_completions_include_files_subcommands_flags_and_safe_bash_candidates(self):
        bash = (ROOT / "completions/execute.bash").read_text()
        zsh = (ROOT / "completions/_execute").read_text()
        fish = (ROOT / "completions/execute.fish").read_text()
        for completion in (bash, zsh):
            for command in ("run-parts", "show-run", "repair", "compile", "attach", "dap"):
                self.assertIn(command, completion)
            for flag in ("--what-if", "--json-errors", "--max-record-bytes", "--daemon"):
                self.assertIn(flag, completion)
        for command in ("run-parts", "show-run", "repair", "compile", "attach", "dap"):
            self.assertIn(command, fish)
        for flag in ("what-if", "json-errors", "max-record-bytes", "daemon"):
            self.assertIn(f"-l {flag}", fish)
        self.assertIn("mapfile -t COMPREPLY", bash)
        check_options = bash.split("check)", 1)[1].split(";;", 1)[0]
        self.assertNotIn("--headless", check_options)
        self.assertIn("programs:program:_files", zsh)


if __name__ == "__main__":
    unittest.main()
