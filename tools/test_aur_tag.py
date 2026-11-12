"""Tests for tools/aur.py tag pinning. Run with: python tools/test_aur_tag.py"""

from __future__ import annotations

import shutil
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import aur  # noqa: E402

REPO = Path(__file__).resolve().parent.parent


def read_pkgbuild_value(text: str, key: str) -> str:
    for line in text.splitlines():
        if line.startswith(key + "="):
            return line[len(key) + 1 :]
    raise AssertionError("no " + key + "= line")


class CheckedInPkgbuildTests(unittest.TestCase):
    def test_checked_in_tags_name_the_newest_git_tag(self):
        newest = aur.git_tags(REPO)[0]
        for name in aur.PACKAGES:
            text = (REPO / "packaging/aur" / name / "PKGBUILD").read_text()
            self.assertEqual(
                read_pkgbuild_value(text, "_tag"), newest, name + " names a missing tag"
            )


class RenderTests(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory(prefix="execute-aur-tag-")
        self.base = Path(self._tmp.name)
        self.root = self.base / "root"
        self.out = self.base / "out"
        self.newest = aur.git_tags(REPO)[0]
        self._make_fixture_root(self.newest)
        self.linux_x64 = self.base / "linux-x64.tar.gz"
        self.linux_arm64 = self.base / "linux-arm64.tar.gz"
        self.source = self.base / "source.tar.gz"
        for path in (self.linux_x64, self.linux_arm64, self.source):
            path.write_bytes(b"fixture " + path.name.encode())

    def tearDown(self):
        self._tmp.cleanup()

    def _make_fixture_root(self, version: str):
        self.root.mkdir()
        (self.root / "Cargo.toml").write_text(
            '[package]\nname = "execute"\nversion = "' + version + '"\n'
        )
        (self.root / "Cargo.lock").write_text(
            'name = "execute"\nversion = "' + version + '"\n'
        )
        for name in aur.PACKAGES:
            dest = self.root / "packaging/aur" / name
            dest.mkdir(parents=True)
            shutil.copyfile(REPO / "packaging/aur" / name / "PKGBUILD", dest / "PKGBUILD")

    def test_rendered_tag_is_newest_git_tag_with_real_checksums(self):
        aur.render(
            self.out,
            self.newest,
            linux_x64=self.linux_x64,
            linux_arm64=self.linux_arm64,
            source=self.source,
            root=self.root,
            tags=aur.git_tags(REPO),
        )
        for name in aur.PACKAGES:
            text = (self.out / name / "PKGBUILD").read_text()
            self.assertEqual(read_pkgbuild_value(text, "_tag"), self.newest)
        bin_text = (self.out / "execute-bin" / "PKGBUILD").read_text()
        self.assertNotIn("('SKIP')", bin_text)
        self.assertIn("sha256sums_x86_64=('", bin_text)
        src_text = (self.out / "execute" / "PKGBUILD").read_text()
        self.assertNotIn("sha256sums=('SKIP')", src_text)

    def test_render_refuses_a_version_without_a_git_tag(self):
        unreleased = "0.9.0-rc.99"
        shutil.rmtree(self.root)
        self._make_fixture_root(unreleased)
        with self.assertRaisesRegex(ValueError, "no git tag v" + unreleased):
            aur.render(
                self.out,
                unreleased,
                linux_x64=self.linux_x64,
                source=self.source,
                root=self.root,
                tags=aur.git_tags(REPO),
            )
        self.assertFalse((self.out / "execute-bin" / "PKGBUILD").exists())


if __name__ == "__main__":
    unittest.main()
