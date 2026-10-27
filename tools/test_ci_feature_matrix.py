#!/usr/bin/env python3
"""Regression checks for the cross-platform CI feature matrix."""

from __future__ import annotations

import os
import pathlib
import re
import hashlib
import subprocess
import sys
import tempfile
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[1]
FEATURES = ("graph", "screens", "records", "tooling", "module-tools", "agent-advisory")


class FeatureMatrixTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        ci_file = pathlib.Path(os.environ.get("LANE_W3_CI_FILE", ROOT / ".github/workflows/ci.yml"))
        cls.workflow = ci_file.read_text(encoding="utf-8")

    def test_every_feature_combination_is_linted_on_linux_and_windows(self) -> None:
        self.assertIn("feature-matrix:", self.workflow)
        self.assertIn("os: [ubuntu-24.04, windows-2022]", self.workflow)
        self.assertIn('"--no-default-features" \\', self.workflow)
        self.assertIn("cargo clippy --locked $args --all-targets -- -D warnings", self.workflow)
        for feature in FEATURES:
            with self.subTest(feature=feature):
                self.assertIn(f'"--no-default-features --features {feature}"', self.workflow)

    def test_core_and_default_are_tested_on_both_platforms(self) -> None:
        self.assertIn("cargo test --locked --no-default-features --features test-fixtures --lib --tests", self.workflow)
        self.assertIn("cargo test --locked --features test-fixtures --lib --tests", self.workflow)

    def test_fixture_is_test_only_and_built_for_ci(self) -> None:
        manifest = (ROOT / "Cargo.toml").read_text(encoding="utf-8")
        full = re.search(r'(?m)^full\s*=\s*\[(.*?)\]', manifest)
        self.assertIsNotNone(full)
        self.assertNotIn('"test-fixtures"', full.group(1))
        nightly = (ROOT / ".github/workflows/flaky.yml").read_text(encoding="utf-8")
        self.assertIn("cargo nextest archive --locked --release --features full,test-fixtures", nightly)
        self.assertIn("cargo build --locked --release --features full,test-fixtures --bins", nightly)
        self.assertIn("target/release/native-fixture*", nightly)
        self.assertIn("--release --features full,test-fixtures --bins", self.workflow)
        self.assertIn("--release --features full,test-fixtures --lib", self.workflow)
        makefile = (ROOT / "Makefile").read_text(encoding="utf-8")
        self.assertIn("test --locked --features full,test-fixtures --release", makefile)

    def test_helper_and_subcommand_requirements(self) -> None:
        manifest_path = pathlib.Path(os.environ.get("FEATURE_MATRIX_MANIFEST", ROOT / "Cargo.toml"))
        manifest = manifest_path.read_text(encoding="utf-8")
        requirements = {}
        for section in re.split(r"(?m)^\[\[test\]\]\s*$", manifest)[1:]:
            section = re.split(r"(?m)^\[", section, maxsplit=1)[0]
            name = re.search(r'(?m)^name\s*=\s*"([^"]+)"', section)
            features = re.search(r'(?ms)^required-features\s*=\s*\[(.*?)\]', section)
            if name:
                requirements[name.group(1)] = set(re.findall(r'"([^"]+)"', features.group(1))) if features else set()
        for path in (ROOT / "tests").glob("*.rs"):
            source = path.read_text(encoding="utf-8")
            needed = set()
            if 'CARGO_BIN_EXE_execute-model' in source:
                needed.add("model")
            if 'CARGO_BIN_EXE_native-fixture' in source:
                needed.add("test-fixtures")
            if re.search(r'"checkpoint"\s*,\s*"status"|"--what-if"|"--confirm"|"explain"', source):
                # Only dedicated advisory suites need this requirement; other tests
                # may deliberately check behavior when the feature is disabled.
                if (path.name.startswith("agent_") or path.name == "what_if_confirm.rs"
                        or path.name in {"gate_effects.rs", "graph_checkpoint_cli.rs"}):
                    needed.add("agent-advisory")
            with self.subTest(test=path.name):
                self.assertFalse(needed - requirements.get(path.stem, set()),
                                 f"{path.name} missing required-features: {needed - requirements.get(path.stem, set())}")
            self.assertNotRegex(source, r'examples[/\\]native-fixture|join\("examples"\)\s*\.join\(name\)',
                                f"{path.name} must use the Cargo fixture binary")

    def test_arch_desktop_image_requires_an_independently_supplied_digest(self) -> None:
        workflow_path = pathlib.Path(
            os.environ.get("LANE_W3_DESKTOP_WORKFLOW", ROOT / ".github/workflows/desktop-live-acceptance.yml")
        )
        workflow = workflow_path.read_text(encoding="utf-8")
        script = ROOT / "tools/desktop_ci/verify_image_sha256.py"
        self.assertIn("arch_image_sha256:", workflow)
        self.assertIn("ARCH_IMAGE_SHA256: ${{ inputs.arch_image_sha256 }}", workflow)
        with tempfile.TemporaryDirectory() as directory:
            image = pathlib.Path(directory) / "image.raw"
            image.write_bytes(b"trusted fixture image")
            expected = hashlib.sha256(image.read_bytes()).hexdigest()
            valid = subprocess.run(
                [sys.executable, str(script), str(image), expected], capture_output=True, text=True, check=False
            )
            mismatch = subprocess.run(
                [sys.executable, str(script), str(image), "0" * 64], capture_output=True, text=True, check=False
            )
            missing = subprocess.run(
                [sys.executable, str(script), str(image), ""], capture_output=True, text=True, check=False
            )
        self.assertEqual(valid.returncode, 0, valid.stderr)
        self.assertEqual(mismatch.returncode, 1)
        self.assertEqual(missing.returncode, 1)


if __name__ == "__main__":
    unittest.main()
