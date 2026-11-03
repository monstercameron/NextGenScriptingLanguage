#!/usr/bin/env python3
"""Regression checks for the pinned Arch package smoke test."""

from pathlib import Path
import unittest


ROOT = Path(__file__).resolve().parents[1]


class ArchWorkflowTests(unittest.TestCase):
    def test_arch_packages_use_dated_snapshot_and_non_blocking_until_verified(self):
        workflow = (ROOT / ".github/workflows/packages.yml").read_text(
            encoding="utf-8"
        )
        script = (ROOT / "tools/arch_test.sh").read_text(encoding="utf-8")

        arch_job = workflow.split("  arch:\n", maxsplit=1)[1].split(
            "  sbom:\n", maxsplit=1
        )[0]
        self.assertIn("snapshot 2026-11-01", arch_job)
        self.assertIn("continue-on-error: true", arch_job)
        self.assertIn(
            "https://archive.archlinux.org/repos/2026/11/01/$repo/os/$arch",
            script,
        )
        self.assertIn("pacman -Sy --noconfirm", script)
        self.assertNotIn("pacman -Syu", script)


if __name__ == "__main__":
    unittest.main()
