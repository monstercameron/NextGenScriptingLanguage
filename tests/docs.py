#!/usr/bin/env python3
"""Documentation link checks must reject broken targets without flagging examples."""
from pathlib import Path
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "tools"))
from check_docs import check_file


class DocumentationLinks(unittest.TestCase):
    def test_relative_encoded_paths_duplicate_headings_and_explicit_aliases(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "Guide notes.md").write_text(
                '# A guide!\n## Data\n## Data\n<a id="old-name"></a>\n'
            )
            page = root / "README.md"
            page.write_text(
                "[first](Guide%20notes.md#a-guide)\n[second](Guide%20notes.md#data-1)\n[legacy](Guide%20notes.md#old-name)\n"
            )
            self.assertEqual(check_file(page, root), [])

    def test_broken_file_and_fragment_have_source_line_diagnostics(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            page = root / "README.md"
            page.write_text("# Home\n[missing](gone.md)\n[typo](#hme)\n")
            errors = check_file(page, root)
            self.assertEqual(len(errors), 2)
            self.assertIn("README.md:2: gone.md: target does not exist", errors)
            self.assertIn("README.md:3: #hme: fragment does not exist", errors)

    def test_code_examples_and_remote_links_are_not_followed(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            page = root / "README.md"
            page.write_text(
                "```md\n[example](nonexistent.md)\n```\n~~~md\n[example](also-missing.md)\n~~~\n[remote](https://example.invalid/absent#no-network)\n"
            )
            self.assertEqual(check_file(page, root), [])

    def test_repository_escape_is_rejected_even_when_target_exists(self):
        with tempfile.TemporaryDirectory() as tmp:
            outer = Path(tmp)
            root = outer / "repo"
            root.mkdir()
            (outer / "outside.md").write_text("# Private\n")
            page = root / "README.md"
            page.write_text("[outside](../outside.md)\n")
            self.assertIn("local target escapes repository", check_file(page, root)[0])

    def test_html_fragment_and_directory_links_resolve(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "assets").mkdir()
            (root / "index.html").write_text('<main id="playground"></main>')
            page = root / "README.md"
            page.write_text("[site](index.html#playground)\n[assets](assets/)\n")
            self.assertEqual(check_file(page, root), [])


if __name__ == "__main__":
    unittest.main()
