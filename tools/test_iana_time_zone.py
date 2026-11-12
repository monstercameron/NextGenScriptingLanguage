#!/usr/bin/env python3
"""Tests for the vendored iana-time-zone verification and SBOM hash."""
import hashlib
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from sbom import build_sbom, directory_sha256  # noqa: E402
from verify_iana_time_zone import PATCH_RECORD, check_patch, compare_files  # noqa: E402


def _sha(data):
    return hashlib.sha256(data).hexdigest()


class VendoredIanaTimeZoneTests(unittest.TestCase):
    def setUp(self):
        self.upstream = {
            "Cargo.lock": b"lock",
            "src/lib.rs": b"upstream lib",
            "README.md": b"readme",
        }
        self.record = {
            "missing": frozenset({"Cargo.lock"}),
            "vendored_only": {"src/extra.rs": _sha(b"extra")},
            "changed": {"src/lib.rs": _sha(b"patched lib")},
        }

    def test_expected_patch_passes(self):
        vendored = {
            "README.md": b"readme",
            "src/lib.rs": b"patched lib",
            "src/extra.rs": b"extra",
        }
        self.assertEqual(check_patch(self.upstream, vendored, self.record), [])

    def test_unexpected_changed_file_fails_naming_path(self):
        vendored = {
            "README.md": b"edited readme",
            "src/lib.rs": b"patched lib",
            "src/extra.rs": b"extra",
        }
        problems = check_patch(self.upstream, vendored, self.record)
        self.assertEqual(problems, ["unexpected content difference: README.md"])

    def test_changed_patched_file_hash_mismatch_fails_naming_path(self):
        vendored = {
            "README.md": b"readme",
            "src/lib.rs": b"patched lib, edited again",
            "src/extra.rs": b"extra",
        }
        problems = check_patch(self.upstream, vendored, self.record)
        self.assertEqual(len(problems), 1)
        self.assertIn("patched file changed: src/lib.rs", problems[0])

    def test_unexpected_vendored_only_file_fails_naming_path(self):
        vendored = {
            "README.md": b"readme",
            "src/lib.rs": b"patched lib",
            "src/extra.rs": b"extra",
            "src/rogue.rs": b"rogue",
        }
        problems = check_patch(self.upstream, vendored, self.record)
        self.assertEqual(problems, ["unexpected vendored-only file: src/rogue.rs"])

    def test_recorded_patch_identical_to_upstream_is_stale(self):
        vendored = {
            "README.md": b"readme",
            "src/lib.rs": b"upstream lib",
            "src/extra.rs": b"extra",
        }
        problems = check_patch(self.upstream, vendored, self.record)
        self.assertEqual(
            problems, ["recorded patch no longer differs from upstream: src/lib.rs"]
        )

    def test_shipped_record_hashes_are_sha256_hex(self):
        self.assertEqual(
            set(PATCH_RECORD["changed"]),
            {"Cargo.toml", "Cargo.toml.orig", "src/lib.rs"},
        )
        self.assertEqual(
            set(PATCH_RECORD["vendored_only"]), {"src/tz_darwin_macos.rs"}
        )
        self.assertEqual(PATCH_RECORD["missing"], frozenset({"Cargo.lock"}))
        for digest in list(PATCH_RECORD["changed"].values()) + list(
            PATCH_RECORD["vendored_only"].values()
        ):
            self.assertRegex(digest, r"^[0-9a-f]{64}$")

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
