"""Tests for tools/check_large_files.py. Run with: python tools/test_check_large_files.py"""

from __future__ import annotations

import contextlib
import io
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import check_large_files as gate  # noqa: E402

REPO = Path(__file__).resolve().parent.parent


class SizeAndMagicTests(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.root = Path(self._tmp.name)

    def tearDown(self):
        self._tmp.cleanup()

    def write(self, rel: str, data: bytes) -> str:
        path = self.root / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(data)
        return rel

    def run_check(self, *paths: str) -> tuple[int, str]:
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            code = gate.main(["--root", str(self.root), *paths])
        return code, out.getvalue()

    def test_two_megabyte_binary_under_benchmarks_fails(self):
        rel = self.write("benchmarks/run/test-binary", b"\x00" * (2 * 1024 * 1024))
        code, out = self.run_check(rel)
        self.assertEqual(code, 1)
        self.assertIn("benchmarks/run/test-binary", out)
        self.assertIn("over the", out)

    def test_ten_kilobyte_text_file_passes(self):
        rel = self.write("src/notes.rs", b"fn main() {}\n" * 800)
        code, out = self.run_check(rel)
        self.assertEqual(code, 0, out)

    def test_exactly_the_limit_passes(self):
        rel = self.write("data/limit.txt", b"a" * gate.MAX_BYTES)
        self.assertEqual(self.run_check(rel)[0], 0)

    def test_small_elf_executable_fails(self):
        rel = self.write("tools/helper", b"\x7fELF" + b"\x00" * 100)
        code, out = self.run_check(rel)
        self.assertEqual(code, 1)
        self.assertIn("ELF executable", out)

    def test_small_zip_archive_fails(self):
        rel = self.write("evidence/run.zip", b"PK\x03\x04" + b"\x00" * 100)
        code, out = self.run_check(rel)
        self.assertEqual(code, 1)
        self.assertIn("zip archive", out)

    def test_xz_archive_fails(self):
        rel = self.write("work/patch.xz", b"\xfd7zXZ\x00" + b"\x00" * 100)
        self.assertEqual(self.run_check(rel)[0], 1)

    def test_tar_magic_at_offset_257_fails(self):
        header = bytearray(512)
        header[257:262] = b"ustar"
        rel = self.write("evidence/run.tar", bytes(header))
        code, out = self.run_check(rel)
        self.assertEqual(code, 1)
        self.assertIn("tar archive", out)

    def test_allow_listed_blob_may_be_large_and_binary(self):
        rel = self.write(
            "src/bundled_manifests/bodies.bin", b"\x1f\x8b" + b"\x00" * (2 * 1024 * 1024)
        )
        self.assertEqual(self.run_check(rel)[0], 0)

    def test_allow_list_is_exact_for_other_bundled_files(self):
        rel = self.write("src/bundled_manifests/other.bin", b"\x00" * (2 * 1024 * 1024))
        self.assertEqual(self.run_check(rel)[0], 1)

    def test_missing_path_is_skipped(self):
        self.assertEqual(self.run_check("benchmarks/deleted-in-worktree.bin")[0], 0)

    def test_text_file_mentioning_elf_mid_content_passes(self):
        rel = self.write("docs/note.md", b"The header starts with \x7fELF in binaries only.\n")
        self.assertEqual(self.run_check(rel)[0], 0)


@unittest.skipUnless(shutil.which("git"), "git is required to read attributes")
class ArchiveAttributeTests(unittest.TestCase):
    def test_benchmarks_are_export_ignored_in_the_working_gitattributes(self):
        # Reads the attribute from the working tree .gitattributes, so it applies
        # to `git archive` once committed. Read-only: check-attr changes nothing.
        result = subprocess.run(
            ["git", "-C", str(REPO), "check-attr", "export-ignore", "--",
             "benchmarks/synthetic/test-binary"],
            capture_output=True,
            text=True,
            check=True,
        )
        self.assertEqual(result.stdout.strip(), "benchmarks/synthetic/test-binary: export-ignore: set")

    def test_source_paths_are_not_export_ignored(self):
        result = subprocess.run(
            ["git", "-C", str(REPO), "check-attr", "export-ignore", "--", "src/lib.rs"],
            capture_output=True,
            text=True,
            check=True,
        )
        self.assertTrue(result.stdout.strip().endswith("unspecified"), result.stdout)


if __name__ == "__main__":
    unittest.main()
