"""Tests for tools/applet_docs.py. Run with: python tools/test_applet_docs.py"""

from __future__ import annotations

import contextlib
import io
import json
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import applet_docs as gen  # noqa: E402

REPO = gen.ROOT


def entry(page: str, name: str) -> str:
    """Return the text of one applet section from a rendered page."""
    start = page.index(f"### `{name}`")
    end = page.find("\n### `", start + 1)
    return page[start:] if end == -1 else page[start:end]


def manifest(name: str, **overrides) -> dict:
    data = {
        "name": name,
        "description": f"Description of {name}.",
        "binary": "@execute-applet",
        "accepts_stdin": False,
        "parameters": {},
        "arguments": [],
        "contract": {
            "version": 1,
            "backend": "in_process",
            "lifecycle": {"kind": "oneshot"},
            "capabilities": ["filesystem.read"],
            "effects": ["filesystem.read"],
        },
    }
    data.update(overrides)
    return data


class CommittedPageTests(unittest.TestCase):
    def test_committed_page_matches_manifests(self):
        expected = gen.render_page(gen.load_manifests(REPO / "manifests"))
        actual = (REPO / gen.PAGE).read_bytes().decode("utf-8").replace("\r\n", "\n")
        self.assertEqual(
            actual,
            expected,
            f"{gen.PAGE.as_posix()} is stale; run python tools/applet_docs.py",
        )

    def test_every_core_manifest_has_one_entry(self):
        applets = gen.load_manifests(REPO / "manifests")
        names = [applet["name"] for applet in applets]
        self.assertEqual(len(names), len(set(names)))
        self.assertGreater(len(names), 0)
        page = gen.render_page(applets)
        for name in names:
            self.assertEqual(page.count(f"### `{name}`"), 1, name)

    def test_exec_which_entry_lists_capability_parameters_and_effects(self):
        page = gen.render_page(gen.load_manifests(REPO / "manifests"))
        section = entry(page, "exec.which")
        self.assertIn("- Capabilities: `filesystem.read`", section)
        self.assertIn("- Effects: `filesystem.read`", section)
        self.assertIn("| `name` | string | no |", section)
        self.assertIn("| `names` | list of string | no |", section)
        self.assertIn("| `path` | string | no |", section)
        self.assertIn('run exec.which with name "execute" and path "" as result', section)


class RenderTests(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.root = Path(self._tmp.name)
        self.manifests = self.root / "manifests"
        self.manifests.mkdir()

    def tearDown(self):
        self._tmp.cleanup()

    def write(self, filename: str, data: dict) -> None:
        (self.manifests / filename).write_text(json.dumps(data), encoding="utf-8")

    def render(self) -> str:
        return gen.render_page(gen.load_manifests(self.manifests))

    def test_editing_a_manifest_changes_its_entry(self):
        self.write("core.fs_demo.json", manifest("fs.demo"))
        before = self.render()
        self.assertIn("Description of fs.demo.", before)

        self.write("core.fs_demo.json", manifest("fs.demo", description="Now described differently."))
        after = self.render()
        self.assertIn("Now described differently.", after)
        self.assertNotIn("Description of fs.demo.", after)

    def test_required_enum_and_list_types_render(self):
        self.write(
            "core.text_demo.json",
            manifest(
                "text.demo",
                parameters={
                    "from": {"type": "path", "required": True, "description": "Source."},
                    "comment": {
                        "type": "string",
                        "enum": ["#", "//"],
                        "description": "Comment marker.",
                    },
                    "files": {"type": "list", "items": "string", "description": "Names."},
                },
            ),
        )
        section = entry(self.render(), "text.demo")
        self.assertIn("| `from` | path | yes | Source. |", section)
        self.assertIn("| `comment` | string, one of `#`, `//` | no | Comment marker. |", section)
        self.assertIn("| `files` | list of string | no | Names. |", section)

    def test_pipe_and_newline_in_description_are_escaped(self):
        self.write(
            "core.text_pipe.json",
            manifest(
                "text.pipe",
                parameters={"value": {"type": "string", "description": "a | b\nc"}},
            ),
        )
        section = entry(self.render(), "text.pipe")
        self.assertIn("| `value` | string | no | a \\| b c |", section)

    def test_applet_without_example_says_so(self):
        self.write("core.text_plain.json", manifest("text.plain"))
        section = entry(self.render(), "text.plain")
        self.assertIn("No example is declared in the manifest.", section)
        self.assertIn("This applet takes no parameters.", section)

    def test_only_core_manifests_are_read(self):
        self.write("core.text_plain.json", manifest("text.plain"))
        self.write("other.thing.json", manifest("other.thing"))
        page = self.render()
        self.assertIn("### `text.plain`", page)
        self.assertNotIn("other.thing", page)

    def test_check_mode_fails_on_stale_page_and_passes_after_write(self):
        self.write("core.text_plain.json", manifest("text.plain"))
        target = self.root / gen.PAGE
        target.parent.mkdir(parents=True)
        target.write_text("stale\n", encoding="utf-8")

        quiet = io.StringIO()
        with contextlib.redirect_stdout(quiet), contextlib.redirect_stderr(quiet):
            self.assertEqual(gen.main(["--root", str(self.root), "--check"]), 1)
            self.assertEqual(gen.main(["--root", str(self.root)]), 0)
            self.assertEqual(gen.main(["--root", str(self.root), "--check"]), 0)

        self.assertIn("### `text.plain`", target.read_text(encoding="utf-8"))

    def test_check_mode_fails_when_page_is_missing(self):
        self.write("core.text_plain.json", manifest("text.plain"))
        quiet = io.StringIO()
        with contextlib.redirect_stdout(quiet), contextlib.redirect_stderr(quiet):
            self.assertEqual(gen.main(["--root", str(self.root), "--check"]), 1)


if __name__ == "__main__":
    unittest.main()
