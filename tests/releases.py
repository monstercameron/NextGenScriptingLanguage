#!/usr/bin/env python3
"""Version, architecture and deterministic package contracts; no publishing."""
import hashlib
import contextlib
import io
import json
import os
import subprocess
from pathlib import Path
import struct
import sys
import tarfile
import tempfile
import unittest
from unittest import mock
import zipfile

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "tools"))
import release
import package
import publish_release
import package_smoke


class Releases(unittest.TestCase):
    def test_strict_semver_and_tag_source_versions(self):
        for value in ["0.0.0", "0.9.0-rc.1", "1.2.3", "1.2.3-a-1.0"]:
            self.assertEqual(release.validate(value), value)
        for value in [
            "v1.2.3",
            "1.2",
            "01.2.3",
            "1.02.3",
            "1.2.03",
            "1.2.3-01",
            "1.2.3-x..y",
            "1.2.3+a",
            "1.2.3\n",
            "../x",
        ]:
            with self.subTest(value=value), self.assertRaises(ValueError):
                release.validate(value)
        self.assertTrue(release.version())
        self.assertEqual(release.package_version("0.9.0-rc.1"), "0.9.0~rc.1")

    def test_version_edit_only_changes_the_root_package_and_rejects_mismatch(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "Cargo.toml").write_text(
                '[package]\nname = "execute"\nversion = "0.9.0-dev"\n\n[dependencies]\nother = "1"\n'
            )
            lock = 'version = 4\n\n[[package]]\nname = "execute"\nversion = "0.9.0-dev"\n\n[[package]]\nname = "other"\nversion = "1.2.3"\n'
            (root / "Cargo.lock").write_text(lock)
            release.set_version("0.9.0-rc.1", root)
            self.assertEqual(release.version(root), "0.9.0-rc.1")
            self.assertIn(
                'name = "other"\nversion = "1.2.3"', (root / "Cargo.lock").read_text()
            )
            before = (root / "Cargo.toml").read_bytes()
            (root / "Cargo.lock").write_text(lock)
            with self.assertRaises(ValueError):
                release.set_version("0.9.1", root)
            self.assertEqual((root / "Cargo.toml").read_bytes(), before)

    def test_complete_asset_gate_rejects_partial_extra_and_empty_then_checksums(self):
        with tempfile.TemporaryDirectory() as tmp:
            folder = Path(tmp)
            names = release.expected_assets("0.9.0-rc.1")
            self.assertEqual(len(names), 12)
            self.assertTrue(
                all(release.re.fullmatch(r"[A-Za-z0-9._-]+", name) for name in names)
            )
            self.assertIn("execute_0.9.0-rc.1_amd64.deb", names)
            self.assertIn("execute-0.9.0-rc.1-1.x86_64.rpm", names)
            with self.assertRaises(ValueError):
                release.verify_assets(folder, "0.9.0-rc.1")
            for name in names:
                (folder / name).write_bytes(b"x" * 200)
            extra = folder / "untrusted.exe"
            extra.write_bytes(b"bad")
            with self.assertRaises(ValueError):
                release.verify_assets(folder, "0.9.0-rc.1")
            extra.unlink()
            release.verify_assets(folder, "0.9.0-rc.1")
            lines = (folder / "SHA256SUMS").read_text().splitlines()
            self.assertEqual(len(lines), 12)
            self.assertEqual({line.split("  ")[1] for line in lines}, names)
            self.assertTrue(
                all(
                    line.startswith(hashlib.sha256(b"x" * 200).hexdigest())
                    for line in lines
                )
            )

    def test_architecture_checks_reject_wrong_platform_and_dynamic_linux(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "binary"
            for target in release.TARGETS:
                arm = target.startswith("aarch64")
                data = bytearray(256)
                if "windows" in target:
                    data[:2] = b"MZ"
                    struct.pack_into("<I", data, 0x3C, 128)
                    data[128:132] = b"PE\0\0"
                    struct.pack_into("<H", data, 132, 0xAA64 if arm else 0x8664)
                elif "linux" in target:
                    data[:6] = b"\x7fELF\x02\x01"
                    struct.pack_into("<H", data, 18, 183 if arm else 62)
                    struct.pack_into("<Q", data, 32, 64)
                    struct.pack_into("<HH", data, 54, 56, 1)
                    struct.pack_into("<I", data, 64, 1)
                else:
                    data[:4] = b"\xcf\xfa\xed\xfe"
                    struct.pack_into("<I", data, 4, 0x100000C if arm else 0x1000007)
                path.write_bytes(data)
                package.check_binary(path, target)
                if "linux" in target:
                    struct.pack_into("<I", data, 64, 3)
                    path.write_bytes(data)
                    with self.assertRaises(ValueError):
                        package.check_binary(path, target)
                path.write_bytes(b"wrong")
                with self.assertRaises((ValueError, struct.error)):
                    package.check_binary(path, target)

    def test_archive_bytes_reproducible_despite_input_mtime(self):
        with tempfile.TemporaryDirectory() as tmp:
            work = Path(tmp)
            root = work / "execute-1.0.0"
            (root / "bin").mkdir(parents=True)
            (root / "bin/execute").write_bytes(b"fixture")
            (root / "README.md").write_text("read me")
            for extension in [".tar.gz", ".zip"]:
                a, b = work / ("a" + extension), work / ("b" + extension)
                package.archive(root, a, 1700000000)
                (root / "README.md").touch()
                package.archive(root, b, 1700000000)
                self.assertEqual(a.read_bytes(), b.read_bytes())
                if extension == ".zip":
                    with zipfile.ZipFile(a) as z:
                        self.assertEqual(
                            z.read("execute-1.0.0/bin/execute"), b"fixture"
                        )
                else:
                    with tarfile.open(a) as t:
                        self.assertEqual(
                            t.extractfile("execute-1.0.0/bin/execute").read(),
                            b"fixture",
                        )


class ArchiveInstallation(unittest.TestCase):
    def test_extraction_is_fresh_versioned_and_rejects_traversal(self):
        with tempfile.TemporaryDirectory() as tmp:
            work = Path(tmp)
            root = work / ("execute-" + release.version())
            root.mkdir()
            (root / "fixture.txt").write_text("owned")
            for extension in [".zip", ".tar.gz"]:
                path = work / ("fixture" + extension)
                package.archive(root, path, 1700000000)
                with package_smoke.extracted_archive(path) as extracted:
                    self.assertNotEqual(extracted.parent, work)
                    self.assertEqual((extracted / "fixture.txt").read_text(), "owned")
                    previous = extracted
                self.assertFalse(previous.exists())
            malicious = work / "malicious.zip"
            with zipfile.ZipFile(malicious, "w") as archive:
                archive.writestr("../must-not-exist", b"escape")
            with self.assertRaisesRegex(ValueError, "unsafe archive path"):
                with package_smoke.extracted_archive(malicious):
                    self.fail("unsafe archive was accepted")
            self.assertFalse((work / "must-not-exist").exists())


class Publication(unittest.TestCase):
    @contextlib.contextmanager
    def payload(self):
        with tempfile.TemporaryDirectory() as tmp:
            previous = Path.cwd()
            os.chdir(tmp)
            try:
                folder = Path("dist")
                folder.mkdir()
                value = release.version()
                for name in release.expected_assets(value):
                    (folder / name).write_bytes(b"owned artifact fixture" * 20)
                release.verify_assets(folder, value)
                with mock.patch.dict(
                    os.environ,
                    {
                        "GITHUB_REPOSITORY": "owner/repo",
                        "GITHUB_SHA": "a" * 40,
                        "GITHUB_REF": "refs/tags/v" + value,
                    },
                ), contextlib.redirect_stdout(io.StringIO()):
                    yield folder, value
            finally:
                os.chdir(previous)

    def server(self, folder, value, draft=True):
        return {
            "id": 1,
            "tag_name": "v" + value,
            "draft": draft,
            "body": "<!-- execute-release:v" + value + ":" + "a" * 40 + " -->",
            "assets": [
                {
                    "name": p.name,
                    "size": p.stat().st_size,
                    "digest": "sha256:" + hashlib.sha256(p.read_bytes()).hexdigest(),
                }
                for p in folder.iterdir()
            ],
        }

    def test_new_draft_is_found_in_paginated_listing_and_verified_by_id(self):
        with self.payload() as (folder, value):
            server = self.server(folder, value)
            missing = subprocess.CompletedProcess([], 0, "[[]]", "")
            found = subprocess.CompletedProcess([], 0, json.dumps([[], [server]]), "")
            with mock.patch.object(
                publish_release.subprocess, "run", side_effect=[missing, found]
            ) as listing, mock.patch.object(
                publish_release.subprocess,
                "check_output",
                return_value=json.dumps(server),
            ) as by_id, mock.patch.object(
                publish_release, "gh"
            ) as gh:
                publish_release.main()
                self.assertEqual(listing.call_count, 2)
                self.assertIn("--paginate", listing.call_args.args[0])
                self.assertTrue(by_id.call_args.args[0][-1].endswith("/releases/1"))
                self.assertEqual(gh.call_args_list[0].args[:2], ("release", "create"))
                self.assertEqual(
                    gh.call_args_list[-1].args[:3], ("api", "--method", "PATCH")
                )

    def test_uploaded_server_digest_is_required_before_publication(self):
        for corrupt in [False, True]:
            with self.subTest(corrupt=corrupt), self.payload() as (folder, value):
                server = self.server(folder, value)
                if corrupt:
                    server["assets"][0]["digest"] = "sha256:" + "0" * 64
                lookup = subprocess.CompletedProcess([], 0, json.dumps([[server]]), "")
                with mock.patch.object(
                    publish_release.subprocess, "run", return_value=lookup
                ), mock.patch.object(
                    publish_release.subprocess,
                    "check_output",
                    return_value=json.dumps(server),
                ), mock.patch.object(
                    publish_release, "gh"
                ) as gh:
                    if corrupt:
                        with self.assertRaisesRegex(ValueError, "digest mismatch"):
                            publish_release.main()
                        self.assertFalse(
                            any(
                                c.args[:3] == ("api", "--method", "PATCH")
                                for c in gh.call_args_list
                            )
                        )
                    else:
                        publish_release.main()
                        self.assertEqual(
                            gh.call_args_list[-1].args[:3], ("api", "--method", "PATCH")
                        )
                        request = json.loads(gh.call_args_list[-1].kwargs["input"])
                        self.assertFalse(request["draft"])
                        self.assertEqual(request["prerelease"], "-" in value)

    def test_unowned_release_wrong_tag_and_corrupt_local_payload_never_upload(self):
        with self.payload() as (folder, value):
            server = self.server(folder, value)
            server["body"] = "somebody else owns this release"
            lookup = subprocess.CompletedProcess([], 0, json.dumps([[server]]), "")
            with mock.patch.object(
                publish_release.subprocess, "run", return_value=lookup
            ), mock.patch.object(publish_release, "gh") as gh:
                with self.assertRaisesRegex(ValueError, "not owned"):
                    publish_release.main()
                gh.assert_not_called()
            with mock.patch.dict(
                os.environ, {"GITHUB_REF": "refs/heads/main"}
            ), mock.patch.object(publish_release.subprocess, "run") as api:
                with self.assertRaisesRegex(ValueError, "exact version tag"):
                    publish_release.main()
                api.assert_not_called()
            next(p for p in folder.iterdir() if p.name != "SHA256SUMS").write_bytes(
                b"changed"
            )
            with mock.patch.object(publish_release.subprocess, "run") as api:
                with self.assertRaisesRegex(ValueError, "checksums"):
                    publish_release.main()
                api.assert_not_called()

    def test_published_retry_compares_every_byte_without_mutating_the_release(self):
        for corrupt in [False, True]:
            with self.subTest(corrupt=corrupt), self.payload() as (folder, value):
                server = self.server(folder, value, draft=False)
                lookup = subprocess.CompletedProcess([], 0, json.dumps([[server]]), "")

                def download(*args, **kwargs):
                    self.assertEqual(args[:2], ("release", "download"))
                    destination = Path(args[args.index("--dir") + 1])
                    for path in folder.iterdir():
                        (destination / path.name).write_bytes(path.read_bytes())
                    if corrupt:
                        (destination / "SHA256SUMS").write_bytes(b"changed remotely")

                with mock.patch.object(
                    publish_release.subprocess, "run", return_value=lookup
                ), mock.patch.object(publish_release, "gh", side_effect=download) as gh:
                    if corrupt:
                        with self.assertRaisesRegex(ValueError, "never overwrite"):
                            publish_release.main()
                    else:
                        publish_release.main()
                    self.assertEqual(gh.call_count, 1)


if __name__ == "__main__":
    unittest.main()
