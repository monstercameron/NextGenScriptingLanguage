"""Tests for tools/check_unsafe.py. Run with: python -m unittest tools/test_check_unsafe.py"""

from __future__ import annotations

import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import check_unsafe as gate  # noqa: E402


class DocumentedTests(unittest.TestCase):
    def check_lines(self, snippet: list[str], unsafe_line: int) -> bool:
        return gate.documented(snippet, unsafe_line - 1)

    def test_safety_comment_inside_assert_is_accepted(self):
        lines = [
            "        assert_eq!(",
            "            // SAFETY: restoring the owner lets TempDir clean up.",
            "            unsafe { libc::chown(c_path.as_ptr(), libc::geteuid(), 0) },",
            "            0",
            "        );",
        ]
        self.assertTrue(self.check_lines(lines, 3))

    def test_safety_comment_before_or_unsafe_is_accepted(self):
        lines = [
            "            if flags == -1",
            "                // SAFETY: F_SETFD reads no memory; a failure returns -1.",
            "                || unsafe { libc::fcntl(fd, libc::F_SETFD, flags) } == -1",
        ]
        self.assertTrue(self.check_lines(lines, 3))

    def test_safety_comment_inside_block_expression_is_accepted(self):
        lines = [
            "    let actual = {",
            "        // SAFETY: names is a writable buffer of `size` bytes.",
            "        unsafe { libc::flistxattr(fd, names.as_mut_ptr().cast(), len) }",
            "    };",
        ]
        self.assertTrue(self.check_lines(lines, 3))

    def test_comment_separated_by_code_is_rejected(self):
        lines = [
            "        // SAFETY: stale comment for the block above.",
            "        assert_eq!(",
            "            unsafe { libc::chown(c_path.as_ptr(), libc::geteuid(), 0) },",
            "            0",
            "        );",
        ]
        self.assertFalse(self.check_lines(lines, 3))

    def test_unsafe_impl_requires_safety_comment(self):
        path = gate.ROOT / "src" / "ipc" / "windows.rs"
        impls = [(line, ok) for line, kind, ok in gate.scan(path) if kind == "impl"]
        self.assertTrue(impls, "expected an unsafe impl in ipc/windows.rs")
        for _, ok in impls:
            self.assertTrue(ok)


class UnsafeGateTests(unittest.TestCase):
    def test_every_unsafe_block_in_the_tree_is_documented(self):
        self.assertEqual(gate.main([]), 0)


if __name__ == "__main__":
    unittest.main()
