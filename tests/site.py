#!/usr/bin/env python3
"""Keep the Pages showcase linked to real source and benchmark evidence."""
import hashlib
from html.parser import HTMLParser
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from urllib.parse import unquote, urlsplit

ROOT = Path(__file__).resolve().parents[1]
DOCS = ROOT / "docs"


class Page(HTMLParser):
    def __init__(self):
        super().__init__()
        self.targets, self.ids, self.code = [], set(), ""
        self.in_hero = False

    def handle_starttag(self, tag, attrs):
        attrs = dict(attrs)
        if "id" in attrs:
            self.ids.add(attrs["id"])
        if tag == "code" and attrs.get("id") == "hero-source":
            self.in_hero = True
        for attribute in ["href", "src"]:
            if attribute in attrs:
                self.targets.append(attrs[attribute])
        if tag == "img" and not attrs.get("alt"):
            raise ValueError("Images require alt text")

    def handle_endtag(self, tag):
        if tag == "code":
            self.in_hero = False

    def handle_data(self, data):
        if self.in_hero:
            self.code += data


class SiteContracts(unittest.TestCase):
    def test_local_links_assets_and_fragments_resolve(self):
        page = Page()
        page.feed((DOCS / "index.html").read_text())
        for target in page.targets:
            parsed = urlsplit(target)
            if parsed.scheme or parsed.netloc:
                continue
            if parsed.path:
                self.assertTrue((DOCS / unquote(parsed.path)).is_file(), target)
            elif parsed.fragment:
                self.assertIn(parsed.fragment, page.ids)
        self.assertTrue((DOCS / ".nojekyll").is_file())

    def test_curated_downloads_are_exact_canonical_source_and_validate(self):
        entries = json.loads((DOCS / "assets/data/examples.json").read_text())
        self.assertGreaterEqual(len(entries), 9)
        binary = Path(
            os.environ.get("EXECUTE_TEST_BINARY", ROOT / "target/release/execute")
        ).resolve()
        for entry in entries:
            with self.subTest(file=entry["file"]):
                source = (ROOT / entry["file"]).read_bytes()
                self.assertEqual(source, (DOCS / entry["download"]).read_bytes())
                self.assertEqual(source.decode(), entry["source"])
                self.assertEqual(hashlib.sha256(source).hexdigest(), entry["sha256"])
                result = subprocess.run(
                    [
                        str(binary),
                        "check",
                        str(ROOT / entry["file"]),
                        "--manifest-dir",
                        str(ROOT / "manifests"),
                    ],
                    capture_output=True,
                    timeout=5,
                )
                self.assertEqual(result.returncode, 0, result.stderr)

    def test_hero_is_valid_and_runs_with_the_advertised_output(self):
        page = Page()
        page.feed((DOCS / "index.html").read_text())
        binary = Path(
            os.environ.get("EXECUTE_TEST_BINARY", ROOT / "target/release/execute")
        ).resolve()
        with tempfile.TemporaryDirectory() as tmp:
            folder = Path(tmp)
            (folder / "count-lines.program").write_text(page.code)
            (folder / "input.txt").write_text("hello\nworld\n")
            result = subprocess.run(
                [
                    str(binary),
                    "count-lines.program",
                    "--manifest-dir",
                    str(ROOT / "manifests"),
                ],
                cwd=folder,
                capture_output=True,
                timeout=5,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(result.stderr, b"")
            self.assertEqual(result.stdout.strip(), b"2")

    def test_charts_use_identical_raw_reports_and_capture_sources_match(self):
        for path in (DOCS / "assets/data").glob("process-inputs-*.json"):
            self.assertEqual(
                path.read_bytes(), (ROOT / "benchmarks" / path.name).read_bytes()
            )
            report = json.loads(path.read_text())
            self.assertEqual(report["status"], "passed")
            for variants in report["results"].values():
                for variant in variants.values():
                    self.assertEqual(
                        len(variant["raw_us"]), report["samples_per_variant"]
                    )
        captures = json.loads((DOCS / "assets/captures/provenance.json").read_text())
        self.assertEqual(len(captures), 2)
        for capture in captures:
            self.assertEqual(
                hashlib.sha256((ROOT / capture["program"]).read_bytes()).hexdigest(),
                capture["program_sha256"],
            )
            self.assertTrue(
                (DOCS / "assets/captures" / capture["image"])
                .read_bytes()
                .startswith(b"\x89PNG\r\n\x1a\n")
            )
            self.assertIn("fixture", capture["method"])


if __name__ == "__main__":
    unittest.main()
