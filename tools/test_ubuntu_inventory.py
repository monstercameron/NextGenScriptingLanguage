#!/usr/bin/env python3
"""Regression checks for the Ubuntu script inventory tool (#415).

The tool writes one record per script (path, SHA-256, size, shebang, lexical
signals) with sorted, timestamp-free output. These checks run it over a small
synthetic tree written to a temporary directory, never over downloaded files.
"""

from __future__ import annotations

import hashlib
import json
import os
import sys
import tempfile
import unittest
from contextlib import redirect_stderr, redirect_stdout
from io import StringIO
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "tools"))
import ubuntu_inventory


MOTD_SCRIPT = """#!/bin/sh
# synthetic motd fragment
printf 'Updates: %s\\n' "$(apt list --upgradable 2>/dev/null | wc -l)"
"""

DISPATCHER_SCRIPT = """#!/bin/bash
set -e
trap 'echo done' EXIT
systemctl restart example.service
"""

HEREDOC_SCRIPT = """#!/bin/sh
cat <<EOF | dpkg -l
snapshot notes
EOF
"""

FIXTURES = {
    "update-motd.d/50-updates": MOTD_SCRIPT,
    "NetworkManager/dispatcher.d/10-restart": DISPATCHER_SCRIPT,
    "cron.daily/30-snap": HEREDOC_SCRIPT,
}

EXPECTED_SIGNALS = {
    "update-motd.d/50-updates": ["apt", "command_substitution", "pipeline"],
    "NetworkManager/dispatcher.d/10-restart": ["systemctl", "trap"],
    # "snapshot" must not count as the snap command.
    "cron.daily/30-snap": ["dpkg", "heredoc", "pipeline"],
}

EXPECTED_SHEBANG = {
    "update-motd.d/50-updates": "sh",
    "NetworkManager/dispatcher.d/10-restart": "bash",
    "cron.daily/30-snap": "sh",
}


def write_tree(root: Path) -> None:
    for relative, body in FIXTURES.items():
        path = root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(body.encode("utf-8"))


class UbuntuInventoryTest(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.root = Path(self._tmp.name) / "scripts"
        self.root.mkdir()
        write_tree(self.root)

    def tearDown(self) -> None:
        self._tmp.cleanup()

    def test_one_record_per_file_with_expected_hash_and_signals(self) -> None:
        inventory = ubuntu_inventory.build_inventory(str(self.root))
        self.assertEqual(inventory["schema"], ubuntu_inventory.SCHEMA_VERSION)
        self.assertEqual(inventory["count"], len(FIXTURES))
        records = {record["path"]: record for record in inventory["files"]}
        self.assertEqual(sorted(records), sorted(FIXTURES))
        for relative, body in FIXTURES.items():
            record = records[relative]
            data = body.encode("utf-8")
            self.assertEqual(record["sha256"], hashlib.sha256(data).hexdigest())
            self.assertEqual(record["size"], len(data))
            self.assertEqual(record["shebang"], EXPECTED_SHEBANG[relative])
            self.assertEqual(record["signals"], EXPECTED_SIGNALS[relative], relative)

    def test_files_are_sorted_by_path(self) -> None:
        inventory = ubuntu_inventory.build_inventory(str(self.root))
        paths = [record["path"] for record in inventory["files"]]
        self.assertEqual(paths, sorted(paths))

    def test_second_run_is_byte_identical(self) -> None:
        first = ubuntu_inventory.render(ubuntu_inventory.build_inventory(str(self.root)))
        second = ubuntu_inventory.render(ubuntu_inventory.build_inventory(str(self.root)))
        self.assertEqual(first, second)

    def test_cli_json_output_matches_second_run(self) -> None:
        out_a = Path(self._tmp.name) / "a.json"
        out_b = Path(self._tmp.name) / "b.json"
        for out in (out_a, out_b):
            stdout = StringIO()
            with redirect_stdout(stdout):
                code = ubuntu_inventory.main(["--root", str(self.root), "--json", str(out)])
            self.assertEqual(code, 0)
            self.assertEqual(stdout.getvalue(), "")
        self.assertEqual(out_a.read_bytes(), out_b.read_bytes())
        parsed = json.loads(out_a.read_text(encoding="utf-8"))
        self.assertEqual(parsed["count"], len(FIXTURES))

    def test_output_has_no_absolute_paths(self) -> None:
        text = ubuntu_inventory.render(ubuntu_inventory.build_inventory(str(self.root)))
        self.assertNotIn(str(self.root), text)

    def test_missing_root_is_an_error(self) -> None:
        missing = os.path.join(self._tmp.name, "absent")
        stderr = StringIO()
        with redirect_stderr(stderr):
            code = ubuntu_inventory.main(["--root", missing])
        self.assertEqual(code, 2)
        self.assertIn("not a directory", stderr.getvalue())

    def test_symlinks_are_skipped(self) -> None:
        target = Path(self._tmp.name) / "outside.sh"
        target.write_text("#!/bin/sh\napt update\n", encoding="utf-8")
        link = self.root / "linked.sh"
        try:
            os.symlink(target, link)
        except (OSError, NotImplementedError):
            self.skipTest("symlinks need privilege on this platform")
        inventory = ubuntu_inventory.build_inventory(str(self.root))
        self.assertNotIn("linked.sh", [record["path"] for record in inventory["files"]])

    def test_non_shell_shebang_is_reported(self) -> None:
        self.assertEqual(ubuntu_inventory.shebang_of("#!/usr/bin/env python3\nprint(1)\n"), "python3")
        self.assertIsNone(ubuntu_inventory.shebang_of("echo no shebang\n"))


if __name__ == "__main__":
    unittest.main()
