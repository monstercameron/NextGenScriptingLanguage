#!/usr/bin/env python3
"""Render the AUR PKGBUILDs for one release: pin the version, and the checksums of the
release inputs when they are given. Without inputs the checksums stay SKIP, which is for
local inspection only; publishing requires the inputs. Python 3.9+."""
import argparse
import hashlib
import re
import shutil
from pathlib import Path

from release import ROOT, version as release_version

PACKAGES = ["execute-bin", "execute"]
BINARY_ARCHES = {"x86_64": "linux-x64", "aarch64": "linux-arm64"}


def sha256(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def substitute(text, pattern, replacement):
    text, count = re.subn(pattern, lambda _: replacement, text, flags=re.MULTILINE)
    if count != 1:
        raise ValueError("expected exactly one match for " + pattern)
    return text


def pin(text, value):
    """Set the release tag and the pkgver derived from it. Arch's pkgver takes no hyphen."""
    text = substitute(text, r"^_tag=.*$", "_tag=" + value)
    return substitute(text, r"^pkgver=.*$", "pkgver=${_tag//-/_}")


def pin_checksum(text, array, digest):
    return substitute(text, r"^" + array + r"=\(.*\)$", array + "=('" + digest + "')")


def render(out, value, linux_x64=None, linux_arm64=None, source=None, root=ROOT):
    """Write one directory per package under `out`, each with its PKGBUILD and the inputs
    it needs, named as the PKGBUILD names them, so makepkg finds them without downloading."""
    if value != release_version(root):
        raise ValueError("version " + value + " is not the repository version")
    binaries = {"x86_64": linux_x64, "aarch64": linux_arm64}
    for name in PACKAGES:
        target = out / name
        target.mkdir(parents=True, exist_ok=True)
        text = pin(
            (root / "packaging/aur" / name / "PKGBUILD").read_text(), value
        )
        if name == "execute-bin":
            for arch, label in BINARY_ARCHES.items():
                archive = binaries[arch]
                if archive is None:
                    continue
                staged = "execute-" + value + "-" + label + ".tar.gz"
                shutil.copyfile(archive, target / staged)
                text = pin_checksum(text, "sha256sums_" + arch, sha256(target / staged))
        else:
            if source is not None:
                staged = "NextGenScriptingLanguage-" + value + ".tar.gz"
                shutil.copyfile(source, target / staged)
                text = pin_checksum(text, "sha256sums", sha256(target / staged))
        (target / "PKGBUILD").write_text(text)
    return out


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)
    render_parser = sub.add_parser("render")
    render_parser.add_argument("--out", required=True, type=Path)
    render_parser.add_argument("--linux-x64", type=Path)
    render_parser.add_argument("--linux-arm64", type=Path)
    render_parser.add_argument("--source", type=Path)
    args = parser.parse_args()
    if args.command == "render":
        render(
            args.out,
            release_version(),
            linux_x64=args.linux_x64,
            linux_arm64=args.linux_arm64,
            source=args.source,
        )
        print("Rendered " + ", ".join(PACKAGES) + " in " + str(args.out))


if __name__ == "__main__":
    main()
