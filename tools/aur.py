#!/usr/bin/env python3
"""Render the AUR PKGBUILDs for one release: pin the version, and the checksums of the
release inputs when they are given. Without inputs the checksums stay SKIP, which is for
local inspection only; publishing requires the inputs. Python 3.9+."""
import argparse
import hashlib
import re
import shutil
import subprocess
from pathlib import Path

from package import explain_executables, resolve_depends, system_runner
from release import ROOT, version as release_version

PACKAGES = ["execute-bin", "execute"]
BINARY_ARCHES = {"x86_64": "linux-x64", "aarch64": "linux-arm64"}
PACKAGE_NAME = re.compile(r"[A-Za-z0-9@._+-]+")


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


def depends_line(packages):
    """The PKGBUILD depends array naming each package. Names must be plain package names,
    so nothing from a program's output can break out of the quoting."""
    for package in packages:
        if not PACKAGE_NAME.fullmatch(package):
            raise ValueError("not a package name: " + repr(package))
    return "depends=(" + " ".join("'" + package + "'" for package in packages) + ")"


def pin_depends(text, packages):
    """Set the depends array of a PKGBUILD template to the given packages."""
    return substitute(text, r"^depends=\(.*\)$", depends_line(packages))


def program_depends(programs, distro, execute=("execute",), runner=system_runner):
    """Packages that own every executable the programs invoke, from `explain --json`.
    An executable that no package owns, or that is ambiguous, fails here, so a package
    never ships with a silent dependency gap."""
    if distro != "arch":
        raise ValueError("PKGBUILD depends are only generated for arch, not " + distro)
    names = set()
    for program in programs:
        names.update(explain_executables(list(execute), program))
    depends, unresolved = resolve_depends(sorted(names), distro, runner)
    if unresolved:
        raise ValueError(
            "cannot resolve dependencies: "
            + "; ".join(name + " (" + reason + ")" for name, reason in unresolved)
        )
    return depends


def render_program(out, template, programs, distro="arch", execute=("execute",), runner=system_runner):
    """Write out/PKGBUILD from a template whose depends array is derived from the programs.
    Nothing is written when any executable cannot be resolved."""
    depends = program_depends(programs, distro, execute=execute, runner=runner)
    text = pin_depends(Path(template).read_text(), depends)
    out.mkdir(parents=True, exist_ok=True)
    (out / "PKGBUILD").write_text(text)
    return depends


def git_tags(repo=ROOT):
    """Return the release tags (without the leading v), newest first, as git sorts them."""
    result = subprocess.run(
        ["git", "-C", str(repo), "tag", "--list", "v*", "--sort=-v:refname"],
        capture_output=True,
        text=True,
        check=True,
    )
    return [tag[1:] for tag in result.stdout.split() if tag.startswith("v")]


def render(out, value, linux_x64=None, linux_arm64=None, source=None, root=ROOT, tags=None):
    """Write one directory per package under `out`, each with its PKGBUILD and the inputs
    it needs, named as the PKGBUILD names them, so makepkg finds them without downloading.
    The release must already exist as a git tag, so a PKGBUILD never names a missing tag."""
    if value != release_version(root):
        raise ValueError("version " + value + " is not the repository version")
    known = git_tags() if tags is None else tags
    if value not in known:
        raise ValueError("no git tag v" + value + " to render the AUR packages from")
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
    program_parser = sub.add_parser("program")
    program_parser.add_argument("--out", required=True, type=Path)
    program_parser.add_argument("--template", required=True, type=Path)
    program_parser.add_argument("--execute", default="execute")
    program_parser.add_argument("programs", nargs="+", type=Path)
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
    elif args.command == "program":
        depends = render_program(
            args.out, args.template, args.programs, execute=[args.execute]
        )
        print("Rendered " + str(args.out / "PKGBUILD") + " depends " + " ".join(depends))


if __name__ == "__main__":
    main()
