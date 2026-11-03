#!/usr/bin/env python3
"""Contract tests for release publication-asset validation (issue #815)."""
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from release import (  # noqa: E402
    CHECKSUMS,
    SIGNATURE,
    documented_artifacts,
    publication_assets,
    verify_publication_assets,
)

VERSION = "0.9.0-rc.5"
SBOM = "execute-" + VERSION + ".cdx.json"


def write_assets(root, names):
    for name in names:
        (root / name).write_bytes(b"x" * 128)


class VerifyPublicationAssetsTests(unittest.TestCase):
    def test_documented_artifacts_are_the_three_guide_names(self):
        self.assertEqual(
            documented_artifacts(VERSION), {CHECKSUMS, SIGNATURE, SBOM}
        )

    def test_complete_publication_set_passes(self):
        with tempfile.TemporaryDirectory(prefix="execute-release-") as directory:
            root = Path(directory)
            write_assets(root, publication_assets(VERSION))
            verify_publication_assets(root, VERSION)

    def test_missing_documented_artifact_is_named(self):
        for missing in (CHECKSUMS, SIGNATURE, SBOM):
            with self.subTest(missing=missing):
                with tempfile.TemporaryDirectory(
                    prefix="execute-release-"
                ) as directory:
                    root = Path(directory)
                    write_assets(
                        root, publication_assets(VERSION) - {missing}
                    )
                    with self.assertRaisesRegex(ValueError, missing):
                        verify_publication_assets(root, VERSION)

    def test_rc4_shape_without_bundle_or_sbom_fails(self):
        # v0.9.0-rc.4 published the packages and SHA256SUMS only; the future
        # publish gate must refuse that shape instead of repeating it.
        with tempfile.TemporaryDirectory(prefix="execute-release-") as directory:
            root = Path(directory)
            write_assets(
                root, publication_assets(VERSION) - {SIGNATURE, SBOM}
            )
            with self.assertRaisesRegex(ValueError, SIGNATURE):
                verify_publication_assets(root, VERSION)


class WorkflowWiringTests(unittest.TestCase):
    def test_release_workflow_checks_publication_set_before_publish(self):
        root = Path(__file__).resolve().parents[1]
        workflow = (root / ".github/workflows/release.yml").read_text(
            encoding="utf-8"
        )
        self.assertIn(
            "python3 tools/release.py verify-publication-assets dist", workflow
        )
        check = workflow.index("verify-publication-assets dist")
        self.assertLess(workflow.index("cosign sign-blob"), check)
        self.assertLess(check, workflow.index("tools/publish_release.py"))


if __name__ == "__main__":
    unittest.main()
