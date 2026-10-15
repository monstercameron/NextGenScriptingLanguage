#!/usr/bin/env python3
"""Exercise staged-snapshot checks in disposable repositories without building."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

REPOSITORY = Path(__file__).resolve().parents[1]
CARGO_TOOL = shutil.which(os.environ.get("CARGO", "cargo"))
RUSTFMT = shutil.which(os.environ.get("RUSTFMT", "rustfmt"))


class StagedRust(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="execute hook test ")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name) / "repo with spaces"
        self.root.mkdir()
        self.git("init", "--quiet")
        for directory in [
            "src/runtime",
            "browser/src",
            "tools",
            ".githooks",
            "examples",
            "vendor/dependency",
            "docs/playground",
        ]:
            (self.root / directory).mkdir(parents=True, exist_ok=True)
        for path in [
            ".githooks/pre-commit",
            "tools/check_staged_rust.py",
            "tools/install_hooks.py",
        ]:
            shutil.copy2(REPOSITORY / path, self.root / path)
        for path in [
            "src/lib.rs",
            "src/runtime/shared.rs",
            "browser/src/lib.rs",
            "examples/space name.rs",
        ]:
            (self.root / path).write_text("fn main() {}\n")
        (self.root / "Cargo.toml").write_text("# fixture manifest\n")
        (self.root / "browser/Cargo.toml").write_text("# fixture manifest\n")
        self.log = Path(self.temporary.name) / "tool calls.jsonl"
        self.cargo = Path(self.temporary.name) / "cargo fixture"
        self.cargo.write_text(
            """#!/usr/bin/env python3
import json, os, pathlib, subprocess, sys
root = pathlib.Path.cwd()
with open(os.environ["HOOK_TEST_LOG"], "a") as log:
    log.write(json.dumps({"args":sys.argv[1:], "source":(root/"src/lib.rs").read_text(), "shared":(root/"src/runtime/shared.rs").read_text(), "space":(root/"examples/space name.rs").read_text()})+"\\n")
if sys.argv[1] == "fmt":
    paths = [root/"browser/src/lib.rs"] if "--manifest-path" in sys.argv else [root/"src/lib.rs",root/"examples/space name.rs"]
    sys.exit(subprocess.call([os.environ["RUSTFMT"], "--edition", "2021", "--check", *map(str, paths)]))
"""
        )
        self.cargo.chmod(0o755)
        self.env = os.environ | {
            "CARGO": str(self.cargo),
            "RUSTFMT": RUSTFMT or "rustfmt",
            "HOOK_TEST_LOG": str(self.log),
            "PYTHON": shutil.which("python3"),
        }
        self.git("add", ".")

    def git(self, *arguments):
        return subprocess.run(
            ["git", *arguments], cwd=self.root, check=True, capture_output=True
        )

    def hook(self):
        return subprocess.run(
            [str(self.root / ".githooks/pre-commit")],
            cwd=self.root,
            env=self.env,
            capture_output=True,
            text=True,
        )

    def calls(self):
        return (
            [json.loads(line) for line in self.log.read_text().splitlines()]
            if self.log.exists()
            else []
        )

    @unittest.skipUnless(
        RUSTFMT, "rustfmt is required for real formatting rejection checks"
    )
    def test_clean_snapshot_checks_native_browser_shared_and_paths_with_spaces(self):
        before = self.git("diff", "--cached", "--binary").stdout
        result = self.hook()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        calls = self.calls()
        self.assertEqual(
            [call["args"][0] for call in calls], ["fmt", "fmt", "clippy", "clippy"]
        )
        self.assertNotIn("--all", calls[0]["args"])
        self.assertNotIn("--all", calls[1]["args"])
        self.assertIn("wasm32-unknown-unknown", calls[-1]["args"])
        self.assertIn("--all-targets", calls[-2]["args"])
        self.assertTrue(all("--offline" in call["args"] for call in calls[2:]))
        self.assertEqual(before, self.git("diff", "--cached", "--binary").stdout)

    @unittest.skipUnless(
        RUSTFMT, "rustfmt is required for real formatting rejection checks"
    )
    def test_unstaged_fix_cannot_hide_bad_staged_rust(self):
        source = self.root / "src/lib.rs"
        source.write_text("fn main(){ }\n")
        self.git("add", "src/lib.rs")
        source.write_text("fn main() {}\n")
        before = self.git("diff", "--cached", "--binary").stdout
        result = self.hook()
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.calls()[0]["source"], "fn main(){ }\n")
        self.assertEqual(source.read_text(), "fn main() {}\n")
        self.assertEqual(before, self.git("diff", "--cached", "--binary").stdout)

    @unittest.skipUnless(
        RUSTFMT, "rustfmt is required for real formatting rejection checks"
    )
    def test_unstaged_bad_content_does_not_fail_clean_staged_snapshot(self):
        (self.root / "src/lib.rs").write_text("fn main(){ }\n")
        self.assertEqual(self.hook().returncode, 0)
        self.assertEqual(self.calls()[0]["source"], "fn main() {}\n")

    @unittest.skipUnless(
        RUSTFMT, "rustfmt is required for real formatting rejection checks"
    )
    def test_shared_include_file_is_checked_explicitly(self):
        (self.root / "src/runtime/shared.rs").write_text("fn main(){ }\n")
        self.git("add", "src/runtime/shared.rs")
        result = self.hook()
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual([call["args"][0] for call in self.calls()], ["fmt", "fmt"])

    def test_vendor_and_generated_changes_do_not_invoke_tools(self):
        # Empty the initially added index so only the excluded paths are staged.
        self.git("read-tree", "--empty")
        for path in ["vendor/dependency/lib.rs", "docs/playground/generated.rs"]:
            (self.root / path).write_text("unformatted generated bytes\n")
            self.git("add", path)
        self.env["CARGO"] = "missing-cargo-fixture"
        self.assertEqual(self.hook().returncode, 0)
        self.assertEqual(self.calls(), [])

    @unittest.skipUnless(CARGO_TOOL and RUSTFMT, "cargo and rustfmt are required")
    def test_real_cargo_fmt_excludes_nonworkspace_vendor_dependency(self):
        (self.root / "Cargo.toml").write_text(
            '[package]\nname = "hook-fixture"\nversion = "0.1.0"\nedition = "2021"\n'
            '[dependencies]\nvendored-fixture = { path = "vendor/dependency" }\n'
        )
        (self.root / "vendor/dependency/Cargo.toml").write_text(
            '[package]\nname = "vendored-fixture"\nversion = "0.1.0"\nedition = "2021"\n[lib]\npath = "lib.rs"\n'
        )
        (self.root / "vendor/dependency/lib.rs").write_text("pub fn vendor(){ }\n")
        result = subprocess.run(
            [CARGO_TOOL, "fmt", "--check"],
            cwd=self.root,
            env=self.env,
            capture_output=True,
            text=True,
        )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(
            (self.root / "vendor/dependency/lib.rs").read_text(), "pub fn vendor(){ }\n"
        )

    def test_formatter_and_lint_configuration_changes_trigger_checks(self):
        for path in ["rustfmt.toml", ".rustfmt.toml", "clippy.toml", ".clippy.toml"]:
            with self.subTest(path=path):
                self.git("read-tree", "--empty")
                (self.root / path).write_text("# policy fixture\n")
                self.git("add", path)
                self.env["CARGO"] = "missing-cargo-fixture"
                result = self.hook()
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("missing-cargo-fixture", result.stderr)

    def test_missing_tool_has_actionable_failure(self):
        self.env["CARGO"] = "missing-cargo-fixture"
        result = self.hook()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("missing-cargo-fixture", result.stderr)
        self.assertIn("install Rust", result.stderr)

    def test_installer_sets_repository_local_config_only(self):
        result = subprocess.run(
            [self.env["PYTHON"], "tools/install_hooks.py"],
            cwd=self.root,
            capture_output=True,
            text=True,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            self.git("config", "--local", "--get", "core.hooksPath").stdout.strip(),
            b".githooks",
        )


if __name__ == "__main__":
    unittest.main()
