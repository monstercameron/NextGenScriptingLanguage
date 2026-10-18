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
        self.assertIn("cargo test --locked --no-default-features --lib --tests", self.workflow)
        self.assertIn("cargo test --locked --lib --tests", self.workflow)

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
