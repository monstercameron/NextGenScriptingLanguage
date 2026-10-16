import json
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "tools"))
from pack_manifest_catalog import pack_directory


class ManifestCatalogPackerTests(unittest.TestCase):
    def test_output_is_deterministic_and_uses_sorted_direct_children(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "nested").mkdir()
            (root / "z.json").write_text(
                '{"name":"z","binary":"fixture","arguments":[]}', encoding="utf-8"
            )
            (root / "a.json").write_text(
                '{"name":"a","binary":"fixture","arguments":[]}', encoding="utf-8"
            )
            (root / "nested" / "ignored.json").write_text("{}", encoding="utf-8")
            output = root / "catalog.manifest-catalog"

            self.assertEqual(pack_directory(root, output), 2)
            first = output.read_bytes()
            self.assertEqual(pack_directory(root, output), 2)
            self.assertEqual(output.read_bytes(), first)
            catalog = json.loads(first)
            self.assertEqual(catalog["format"], "execute.manifest-catalog")
            self.assertEqual(catalog["version"], 1)
            self.assertEqual([entry["name"] for entry in catalog["manifests"]], ["a", "z"])

    def test_accepts_json_whitespace_but_rejects_non_json_whitespace(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "manifest.json"
            output = root / "catalog.manifest-catalog"
            source.write_text(
                '\t\r\n {"name":"sample","binary":"fixture","arguments":[]} \n',
                encoding="utf-8",
            )
            self.assertEqual(pack_directory(root, output), 1)

            source.write_text(
                '{"name":"sample","binary":"fixture",\u00a0"arguments":[]}',
                encoding="utf-8",
            )
            with self.assertRaises(ValueError):
                pack_directory(root, output)

    def test_rejects_nonfinite_json_constants(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "manifest.json"
            for token in ("NaN", "Infinity", "-Infinity"):
                with self.subTest(token=token):
                    source.write_text(
                        f'{{"name":"sample","binary":"fixture","metric":{token}}}',
                        encoding="utf-8",
                    )
                    with self.assertRaisesRegex(ValueError, token):
                        pack_directory(root, root / "catalog.manifest-catalog")

    def test_rejects_duplicate_object_keys_at_any_depth(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "manifest.json"
            for document in (
                '{"name":"first","name":"second","binary":"fixture","arguments":[]}',
                '{"name":"sample","binary":"fixture","parameters":{"item":{"type":"string"},"item":{"type":"string"}},"arguments":[]}',
            ):
                with self.subTest(document=document):
                    source.write_text(document, encoding="utf-8")
                    with self.assertRaisesRegex(ValueError, "duplicate JSON object key"):
                        pack_directory(root, root / "catalog.manifest-catalog")

    def test_rejects_symlink_manifest_origins_and_different_output_roots(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "source.json"
            source.write_text('{"name":"source","binary":"fixture"}', encoding="utf-8")
            try:
                (root / "alias.json").symlink_to(source)
            except (NotImplementedError, OSError) as error:
                self.skipTest(f"symlinks are unavailable: {error}")
            with self.assertRaisesRegex(ValueError, "symlink manifest origins"):
                pack_directory(root, root / "catalog.manifest-catalog")

            (root / "alias.json").unlink()
            with tempfile.TemporaryDirectory() as other:
                with self.assertRaisesRegex(ValueError, "source manifest directory"):
                    pack_directory(root, Path(other) / "catalog.manifest-catalog")


if __name__ == "__main__":
    unittest.main()
