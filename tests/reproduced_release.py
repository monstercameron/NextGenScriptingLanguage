#!/usr/bin/env python3
"""Safety and byte-comparison tests for shipped/rebuilt artifact verification."""
import io
import os
from pathlib import Path
import sys
import tarfile
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "tools"))
import compare_release_binaries as compare


def archive(path, missing=None, duplicate=None, link=None):
    with tarfile.open(path, "w:gz") as tar:
        for name in compare.BINARIES:
            if name == missing:
                continue
            info = tarfile.TarInfo(f"execute-1.2.3/bin/{name}")
            data = (name + "-bytes").encode()
            if name == link:
                info.type = tarfile.SYMTYPE
                info.linkname = "elsewhere"
                tar.addfile(info)
            else:
                info.size = len(data)
                tar.addfile(info, io.BytesIO(data))
            if name == duplicate:
                tar.addfile(info, io.BytesIO(data))


class CompareRelease(unittest.TestCase):
    def test_matches_shipped_bytes_and_rejects_each_rebuild_mismatch(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            package = root / "release.tar.gz"
            archive(package)
            shipped = root / "shipped"
            compare.extract_binaries(package, "1.2.3", shipped)
            builds = [root / "a", root / "b"]
            for build in builds:
                build.mkdir()
                for name in compare.BINARIES:
                    (build / name).write_bytes((shipped / name).read_bytes())
            compare.compare(shipped, builds)
            for build in builds:
                for name in compare.BINARIES:
                    binary = build / name
                    old = binary.read_bytes()
                    metadata = binary.stat()
                    binary.write_bytes(bytes([old[0] ^ 1]) + old[1:])
                    os.utime(binary, ns=(metadata.st_atime_ns, metadata.st_mtime_ns))
                    with self.assertRaisesRegex(ValueError, "differs from shipped"):
                        compare.compare(shipped, builds)
                    binary.write_bytes(old)
            (builds[1] / "execute").unlink()
            with self.assertRaisesRegex(ValueError, "differs from shipped"):
                compare.compare(shipped, builds)

    def test_rejects_missing_duplicate_and_link_members(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            for index, kwargs in enumerate([{"missing":"execute"}, {"duplicate":"execute"}, {"link":"execute"}]):
                package = root / f"bad-{index}.tar.gz"
                archive(package, **kwargs)
                with self.assertRaises(ValueError):
                    compare.extract_binaries(package, "1.2.3", root / f"out-{index}")

    def test_rejects_aliases_and_traversal_entries(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            for index, name in enumerate(["execute-1.2.3/bin/../bin/execute", "execute-1.2.3/bin//execute", "../escape"]):
                package = root / f"bad-{index}.tar.gz"
                with tarfile.open(package, "w:gz") as tar:
                    info = tarfile.TarInfo(name)
                    info.size = 1
                    tar.addfile(info, io.BytesIO(b"x"))
                with self.assertRaises(ValueError):
                    compare.extract_binaries(package, "1.2.3", root / f"out-{index}")

    def test_rejects_wrong_archive_version_and_unsafe_version(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            package = root / "release.tar.gz"
            archive(package)
            with self.assertRaises(ValueError):
                compare.extract_binaries(package, "1.2.4", root / "wrong")
            with self.assertRaises(ValueError):
                compare.extract_binaries(package, "../x", root / "unsafe")


if __name__ == "__main__":
    unittest.main()
