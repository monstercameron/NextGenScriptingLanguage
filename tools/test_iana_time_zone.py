#!/usr/bin/env python3
"""Tests for the vendored iana-time-zone verification and SBOM hash."""
import hashlib
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from sbom import build_sbom, directory_sha256  # noqa: E402
from verify_iana_time_zone import compare_files  # noqa: E402


class VendoredIanaTimeZoneTests(unittest.TestCase):
    def test_upstream_comparison_detects_changed_added_and_missing_files(self):
        upstream = {"src/lib.rs": b"upstream", "README.md": b"readme"}
        vendored = {"src/lib.rs": b"changed", "extra.txt": b"extra"}
        self.assertEqual(
            compare_files(upstream, vendored),
            [
                "missing from vendor: README.md",
                "vendored-only: extra.txt",
                "content differs: src/lib.rs",
            ],
        )

    def test_tree_hash_includes_relative_paths_and_file_content(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "a.txt").write_bytes(b"abc")
            expected = hashlib.sha256()
            name = b"a.txt"
            content = b"abc"
            expected.update(len(name).to_bytes(8, "big"))
            expected.update(name)
            expected.update(len(content).to_bytes(8, "big"))
            expected.update(content)
            self.assertEqual(directory_sha256(root), expected.hexdigest())

    def test_sbom_marks_vendored_crate_and_hashes_its_tree(self):
        root = Path(__file__).resolve().parents[1]
        metadata = {
            "workspace_members": ["path+file:///workspace#execute@1.0.0"],
            "resolve": {
                "root": "path+file:///workspace#execute@1.0.0",
                "nodes": [
                    {
                        "id": "path+file:///workspace#execute@1.0.0",
                        "deps": [
                            {
                                "pkg": "path+file://"
                                + str(root / "vendor/iana-time-zone")
                                + "#iana-time-zone@0.1.65",
                                "dep_kinds": [{"kind": None}],
                            }
                        ],
                    },
                    {
                        "id": "path+file://"
                        + str(root / "vendor/iana-time-zone")
                        + "#iana-time-zone@0.1.65",
                        "deps": [],
                    },
                ],
            },
            "packages": [
                {
                    "id": "path+file:///workspace#execute@1.0.0",
                    "name": "execute",
                    "version": "1.0.0",
                    "manifest_path": "/workspace/Cargo.toml",
                },
                {
                    "id": "path+file://"
                    + str(root / "vendor/iana-time-zone")
                    + "#iana-time-zone@0.1.65",
                    "name": "iana-time-zone",
                    "version": "0.1.65",
                    "manifest_path": str(root / "vendor/iana-time-zone/Cargo.toml"),
                    "license": "MIT OR Apache-2.0",
                },
            ],
        }
        sbom = build_sbom(metadata, "", "1.0.0", 0)
        component = next(
            item for item in sbom["components"] if item["name"] == "iana-time-zone"
        )
        self.assertEqual(
            component["hashes"],
            [
                {
                    "alg": "SHA-256",
                    "content": directory_sha256(root / "vendor/iana-time-zone"),
                }
            ],
        )
        self.assertEqual(
            {item["name"]: item["value"] for item in component["properties"]},
            {
                "vendored": "true",
                "patched": "true",
                "patch-source": "vendor/iana-time-zone",
            },
        )

    def test_security_workflow_runs_upstream_verification(self):
        root = Path(__file__).resolve().parents[1]
        workflow = (root / ".github/workflows/security.yml").read_text(
            encoding="utf-8"
        )
        self.assertIn("python3 tools/verify_iana_time_zone.py", workflow)


if __name__ == "__main__":
    unittest.main()
