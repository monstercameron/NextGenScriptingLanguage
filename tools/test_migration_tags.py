"""Tests for tools/migration_tags.py. Run with: python tools/test_migration_tags.py"""

from __future__ import annotations

import hashlib
import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(Path(__file__).resolve().parent))

import migration_tags as mt  # noqa: E402

FIXTURES = ROOT / "tests" / "fixtures" / "migration-tags"

GOLDEN = {
    "pam_lid_gate.sh": ["pam", "sudo", "writes:/etc"],
    "initramfs_luks.sh": ["boot", "sudo", "writes:/etc"],
    "plain_copy.sh": ["user-config:~/.config/waybar"],
}
GOLDEN_RISK = {
    "pam_lid_gate.sh": "boot-or-auth-sensitive",
    "initramfs_luks.sh": "boot-or-auth-sensitive",
    "plain_copy.sh": "user-config-only",
}


class FixtureGoldenTests(unittest.TestCase):
    def test_fixture_tags_match_golden(self):
        for name, expected in GOLDEN.items():
            with self.subTest(fixture=name):
                record = mt.review_record(FIXTURES / name)
                self.assertEqual(record["tags"], expected)

    def test_fixture_risk_floor_matches_golden(self):
        for name, expected in GOLDEN_RISK.items():
            with self.subTest(fixture=name):
                record = mt.review_record(FIXTURES / name)
                self.assertEqual(record["risk_floor"], expected)

    def test_record_is_keyed_by_file_sha256(self):
        path = FIXTURES / "pam_lid_gate.sh"
        record = mt.review_record(path)
        self.assertEqual(record["sha256"], hashlib.sha256(path.read_bytes()).hexdigest())
        self.assertEqual(record["name"], "pam_lid_gate.sh")

    def test_main_emits_json_keyed_by_sha(self):
        import contextlib
        import io
        import json

        buffer = io.StringIO()
        with contextlib.redirect_stdout(buffer):
            self.assertEqual(mt.main([str(FIXTURES)]), 0)
        records = json.loads(buffer.getvalue())
        self.assertEqual(len(records), len(GOLDEN))
        for key, record in records.items():
            self.assertEqual(key, record["sha256"])


class SnippetTests(unittest.TestCase):
    def test_comments_and_heredoc_bodies_add_no_tags(self):
        text = "# sudo pacman -Rns linux\ncat <<'EOF'\nsudo rm -rf /etc\nEOF\necho done\n"
        self.assertEqual(mt.scan_text(text), [])

    def test_package_remove_and_install(self):
        self.assertEqual(mt.scan_text("pacman -Rns foo\n"), ["package-remove"])
        self.assertEqual(mt.scan_text("pacman -Syu bar\n"), ["package-install"])
        self.assertEqual(mt.risk_floor(["package-remove"]), "package-change")

    def test_recursive_rm_systemd_network_and_reboot(self):
        tags = mt.scan_text("rm -rf ~/.cache/x\nsystemctl restart foo\ncurl -fsSL https://example.invalid\nreboot\n")
        self.assertEqual(
            tags,
            ["network", "reboot-required", "rm-recursive", "systemd-unit", "user-config:~/.cache"],
        )

    def test_write_targets_by_top_directory(self):
        self.assertEqual(mt.scan_text("echo x > /usr/share/foo\n"), ["writes:/usr"])
        self.assertEqual(mt.scan_text("cp /etc/hosts /tmp/hosts\n"), [])
        self.assertEqual(mt.scan_text("cp src /var/lib/foo\n"), ["writes:/var"])

    def test_sed_without_in_place_does_not_write(self):
        self.assertEqual(mt.scan_text("sed 's/a/b/' /etc/fstab\n"), [])

    def test_unresolved_variable_targets_are_ignored(self):
        self.assertEqual(mt.scan_text("rm \"$DIR/x\"\n"), [])

    def test_unknown_risk_is_none(self):
        self.assertEqual(mt.risk_floor([]), "none")


if __name__ == "__main__":
    unittest.main()
