#!/usr/bin/env python3
"""Build deterministic native archives and Linux packages from release binaries."""
import argparse
import gzip
import hashlib
import io
import json
import os
import re
from pathlib import Path
import shutil
import struct
import subprocess
import sys
import tarfile
import tempfile
import time
import zipfile
from release import ROOT, TARGETS, package_version, version


def run(*args, **kwargs):
    return subprocess.run(list(map(str, args)), check=True, **kwargs)


def pe_imports(data):
    """Read PE32+ imports without depending on an installed SDK or objdump."""
    pe = struct.unpack_from("<I", data, 0x3C)[0]
    optional = pe + 24
    optional_size = struct.unpack_from("<H", data, pe + 20)[0]
    if optional_size < 240 or struct.unpack_from("<H", data, optional)[0] != 0x20B:
        raise ValueError("Windows release requires a PE32+ optional header")
    sections = []
    for i in range(struct.unpack_from("<H", data, pe + 6)[0]):
        offset = optional + optional_size + 40 * i
        virtual_size, address, size, raw = struct.unpack_from("<IIII", data, offset + 8)
        sections.append((address, max(size, virtual_size), raw, size))

    def location(rva):
        for address, extent, raw, size in sections:
            delta = rva - address
            if 0 <= delta < extent and delta < size and raw + delta < len(data):
                return raw + delta
        raise ValueError("invalid PE import RVA")

    directory_count = struct.unpack_from("<I", data, optional + 108)[0]
    if (
        directory_count > 13
        and struct.unpack_from("<I", data, optional + 112 + 13 * 8)[0]
    ):
        raise ValueError("delayed Windows imports require explicit packaging review")
    imports = []
    rva = (
        struct.unpack_from("<I", data, optional + 120)[0] if directory_count > 1 else 0
    )
    if not rva:
        return imports
    for index in range(4096):
        descriptor = struct.unpack_from("<IIIII", data, location(rva + index * 20))
        if not any(descriptor):
            return imports
        start = location(descriptor[3])
        end = data.find(b"\0", start, min(start + 4096, len(data)))
        if end < 0:
            raise ValueError("unterminated PE import name")
        imports.append(data[start:end].decode("ascii"))
    raise ValueError("too many PE import descriptors")


def check_binary(path, target):
    data = path.read_bytes()
    arm = target.startswith("aarch64")
    if "windows" in target:
        if data[:2] != b"MZ":
            raise ValueError("not a PE executable: " + str(path))
        offset = struct.unpack_from("<I", data, 0x3C)[0]
        if data[offset : offset + 4] != b"PE\0\0" or struct.unpack_from(
            "<H", data, offset + 4
        )[0] != (0xAA64 if arm else 0x8664):
            raise ValueError("PE architecture mismatch")
        for name in pe_imports(data):
            name = name.lower()
            if (
                name.startswith(("vcruntime", "msvcp", "concrt"))
                or name == "ucrtbased.dll"
                or re.fullmatch(r"msvcr[0-9]+d?\.dll", name)
            ):
                raise ValueError(
                    "Windows package depends on a non-system CRT DLL: " + name
                )
    elif "linux" in target:
        if data[:6] != b"\x7fELF\x02\x01" or struct.unpack_from("<H", data, 18)[0] != (
            183 if arm else 62
        ):
            raise ValueError("ELF architecture mismatch")
        offset = struct.unpack_from("<Q", data, 32)[0]
        size, count = struct.unpack_from("<HH", data, 54)
        if any(
            struct.unpack_from("<I", data, offset + i * size)[0] == 3
            for i in range(count)
        ):
            raise ValueError("Linux release must be static (no PT_INTERP)")
    else:
        if data[:4] != b"\xcf\xfa\xed\xfe" or struct.unpack_from("<I", data, 4)[0] != (
            0x100000C if arm else 0x1000007
        ):
            raise ValueError("Mach-O architecture mismatch")


# Shell completions, the man page, the editor grammar and the docs. A system install
# (deb, rpm, AUR) puts each file where its tooling looks; an archive keeps the same files
# under the relative names below. Source paths are relative to the repository root.
COMPLETIONS = [
    ("completions/execute.bash", "share/bash-completion/completions/execute"),
    ("completions/_execute", "share/zsh/site-functions/_execute"),
    ("completions/execute.fish", "share/fish/vendor_completions.d/execute.fish"),
]
MAN_PAGE = "docs/man/execute.1"
GRAMMAR = [
    "editors/vscode/syntaxes/execute.tmLanguage.json",
    "editors/vscode/language-configuration.json",
]
DOCS = ["docs/language.md", "docs/grammar.md", "docs/windows.md", "docs/releases.md"]


def extra_files(system):
    """(source, destination) pairs for the extras, relative to the package prefix."""
    if system:
        completions = COMPLETIONS
        man = [(MAN_PAGE, "share/man/man1/execute.1")]

        def under(source):
            return "share/execute/" + source

    else:
        completions = [(source, source) for source, _ in COMPLETIONS]
        man = [(MAN_PAGE, "man/execute.1")]

        def under(source):
            return source

    pairs = list(completions) + man + [(s, under(s)) for s in GRAMMAR + DOCS]
    missing = [source for source, _ in pairs if not (ROOT / source).is_file()]
    if missing:
        raise ValueError("missing package extras: " + ", ".join(missing))
    return pairs


def populate(root, binaries, target, system=False):
    prefix = root / "usr" if system else root
    bindir = prefix / "bin"
    share = prefix / "share/execute" if system else root
    bindir.mkdir(parents=True)
    share.mkdir(parents=True, exist_ok=True)
    suffix = ".exe" if "windows" in target else ""
    for name in ["execute", "execute-model", "execute-applet"]:
        source = binaries / (name + suffix)
        check_binary(source, target)
        dest = bindir / source.name
        shutil.copyfile(source, dest)
        dest.chmod(0o755)
        if name != "execute":
            # Record the shipped helper hash beside it (issue #515): the
            # interpreter refuses a helper whose content differs from the
            # `<helper>.sha256` sidecar packaged here.
            digest = hashlib.sha256(dest.read_bytes()).hexdigest()
            (bindir / (name + ".sha256")).write_text(
                f"{digest}  {dest.name}\n", encoding="utf-8"
            )
    shutil.copytree(ROOT / "manifests", share / "manifests")
    for name in ["README.md", "LICENSE"]:
        shutil.copyfile(ROOT / name, share / name)
    for source, relative in extra_files(system):
        destination = prefix / relative
        destination.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(ROOT / source, destination)


def archive(root, destination, epoch):
    if destination.suffix == ".zip":
        date = time.gmtime(max(315532800, epoch))[:6]
        with zipfile.ZipFile(
            destination, "w", compression=zipfile.ZIP_DEFLATED, compresslevel=9
        ) as out:
            for p in sorted(root.rglob("*")):
                if p.is_file():
                    info = zipfile.ZipInfo(p.relative_to(root.parent).as_posix(), date)
                    info.external_attr = (p.stat().st_mode & 0xFFFF) << 16
                    info.create_system = 3
                    info.compress_type = zipfile.ZIP_DEFLATED
                    out.writestr(info, p.read_bytes())
    else:
        with destination.open("wb") as raw, gzip.GzipFile(
            filename="", mode="wb", fileobj=raw, mtime=epoch
        ) as zipped, tarfile.open(fileobj=zipped, mode="w") as out:
            for p in [root] + sorted(root.rglob("*")):
                info = out.gettarinfo(str(p), p.relative_to(root.parent).as_posix())
                info.uid = info.gid = 0
                info.uname = info.gname = "root"
                info.mtime = epoch
                info.mode = 0o755 if p.is_dir() or p.parent.name == "bin" else 0o644
                out.addfile(info, io.BytesIO(p.read_bytes()) if p.is_file() else None)


def linux_packages(work, binaries, target, dest, value, epoch):
    _, debarch, rpmarch = TARGETS[target]
    pkg = work / "deb"
    populate(pkg, binaries, target, system=True)
    control = pkg / "DEBIAN"
    control.mkdir()
    pv = package_version(value)
    (control / "control").write_text(
        "Package: execute\nVersion: "
        + pv
        + "\nArchitecture: "
        + debarch
        + "\nMaintainer: Execute maintainers <noreply@github.com>\nSection: devel\nPriority: optional\nHomepage: https://github.com/monstercameron/NextGenScriptingLanguage\nDescription: Readable native command workflows\n Typed .program glue, direct pipes, services, cache and live state.\n"
    )
    env = dict(os.environ, SOURCE_DATE_EPOCH=str(epoch))
    run(
        "dpkg-deb",
        "--root-owner-group",
        "--build",
        pkg,
        dest / ("execute_" + value + "_" + debarch + ".deb"),
        env=env,
    )
    rpmroot = work / "rpm"
    for name in ["BUILD", "BUILDROOT", "RPMS", "SOURCES", "SPECS", "SRPMS"]:
        (rpmroot / name).mkdir(parents=True)
    payload = work / "payload"
    populate(payload, binaries, target, system=True)
    # Files outside /usr/share/execute, which the spec lists one by one.
    outside_share_execute = "\n".join(
        "/usr/" + relative
        for _, relative in extra_files(True)
        if not relative.startswith("share/execute/")
    )
    # No shell interpolation of arbitrary input: all paths are controlled build directories.
    spec = rpmroot / "SPECS/execute.spec"
    spec.write_text(
        """Name: execute
Version: %s
Release: 1
Summary: Readable native command workflows
License: MIT
URL: https://github.com/monstercameron/NextGenScriptingLanguage
AutoReqProv: no
%%global debug_package %%{nil}
%%global __os_install_post %%{nil}
%%description
Typed .program glue, direct pipes, services, cache and live state.
%%install
mkdir -p %%{buildroot}/usr
cp -a "%s/usr/." %%{buildroot}/usr/
%%files
/usr/bin/execute
/usr/bin/execute-model
/usr/bin/execute-applet
/usr/share/execute
%s
"""
        % (pv, str(payload), outside_share_execute)
    )
    run(
        "rpmbuild",
        "-bb",
        "--target",
        rpmarch,
        "--define",
        "_topdir " + str(rpmroot),
        "--define",
        "use_source_date_epoch_as_buildtime 1",
        "--define",
        "clamp_mtime_to_source_date_epoch 1",
        spec,
        env=env,
    )
    packages = list((rpmroot / "RPMS").rglob("*.rpm"))
    if len(packages) != 1:
        raise ValueError("expected exactly one RPM")
    shutil.copyfile(packages[0], dest / ("execute-" + value + "-1." + rpmarch + ".rpm"))


DISTROS = ("arch", "debian")
DPKG_LINE = re.compile(r"^([^:\s]+)(?::[^:\s]+)?: (/\S+)$")


def explain_executables(command, program):
    """Executable names a program invokes, from `explain --json` (summary.executables).

    `command` is the argv prefix that runs execute, for example ["execute"]."""
    out = subprocess.run(
        list(map(str, command)) + ["explain", str(program), "--json"],
        check=True,
        capture_output=True,
        text=True,
    ).stdout
    return executables_from_explain(out)


def executables_from_explain(text):
    names = json.loads(text).get("summary", {}).get("executables")
    if not isinstance(names, list) or not all(isinstance(name, str) for name in names):
        raise ValueError("explain output has no summary.executables list")
    return names


def system_runner(argv):
    """Stdout of a package-database query. Exit 1 means no match, which is an empty answer;
    any other failure (missing database, missing tool) is an error, not a silent gap."""
    done = subprocess.run(argv, capture_output=True, text=True)
    if done.returncode == 1:
        return ""
    if done.returncode != 0:
        raise RuntimeError(" ".join(argv) + " failed: " + done.stderr.strip())
    return done.stdout


def candidate_paths(name):
    """Root-relative paths where a bare or absolute executable name is installed, or None
    when the name cannot name a system executable (relative or empty)."""
    if name.startswith("/"):
        return [name.lstrip("/")]
    if not name or "/" in name or name in (".", ".."):
        return None
    return ["usr/bin/" + name, "usr/sbin/" + name]


def owners(distro, relative, runner):
    """Packages that own one root-relative file, from `pacman -F` or `dpkg -S`."""
    if distro == "arch":
        packages = set()
        package = None
        for line in runner(["pacman", "-F", relative]).splitlines():
            if not line.strip():
                continue
            if line[0].isspace():
                if package and line.strip() == relative:
                    packages.add(package)
            else:
                # Header line: "repo/name version"; the package name follows the repo.
                package = line.split()[0].split("/")[-1]
        return packages
    absolute = "/" + relative
    packages = set()
    for line in runner(["dpkg", "-S", absolute]).splitlines():
        match = DPKG_LINE.match(line.strip())
        if match and match.group(2) == absolute:
            packages.add(match.group(1))
    return packages


def resolve_depends(executables, distro, runner):
    """(depends, unresolved): the packages that own the executables, and each executable
    that no package owns, is ambiguous, or cannot name a system executable."""
    depends = set()
    unresolved = []
    for name in executables:
        relatives = candidate_paths(name)
        if relatives is None:
            unresolved.append((name, "not an installed system executable name"))
            continue
        found = set()
        for relative in relatives:
            found |= owners(distro, relative, runner)
        if not found:
            unresolved.append((name, "no " + distro + " package owns it"))
        elif len(found) > 1:
            unresolved.append((name, "ambiguous: " + ", ".join(sorted(found))))
        else:
            depends |= found
    return sorted(depends), unresolved


def deps_main(argv):
    parser = argparse.ArgumentParser(
        prog="package.py deps",
        description="Print the distro packages that own the executables the programs invoke. "
        "Queries local file databases only (pacman -F, dpkg -S); it never syncs them.",
    )
    parser.add_argument("programs", nargs="+", type=Path)
    parser.add_argument("--distro", required=True, choices=DISTROS)
    parser.add_argument("--execute", default="execute", help="execute binary to run explain with")
    args = parser.parse_args(argv)
    names = set()
    for program in args.programs:
        names.update(explain_executables([args.execute], program))
    depends, unresolved = resolve_depends(sorted(names), args.distro, system_runner)
    for package in depends:
        print(package)
    for name, reason in unresolved:
        print("unresolved: " + name + " (" + reason + ")", file=sys.stderr)
    return 1 if unresolved else 0


def main():
    if sys.argv[1:2] == ["deps"]:
        sys.exit(deps_main(sys.argv[2:]))
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", required=True, choices=TARGETS)
    parser.add_argument("--binaries", required=True, type=Path)
    parser.add_argument("--output", type=Path, default=ROOT / "dist")
    parser.add_argument("--linux-packages", action="store_true")
    args = parser.parse_args()
    value = version()
    epoch = int(
        os.environ.get("SOURCE_DATE_EPOCH")
        or subprocess.check_output(
            ["git", "show", "-s", "--format=%ct"], cwd=ROOT, text=True
        ).strip()
    )
    dest = args.output.resolve()
    dest.mkdir(parents=True, exist_ok=True)
    binaries = args.binaries.resolve()
    with tempfile.TemporaryDirectory(prefix="execute-package-") as temp:
        work = Path(temp)
        root = work / ("execute-" + value)
        populate(root, binaries, args.target)
        label = TARGETS[args.target][0]
        name = (
            "execute-"
            + value
            + "-"
            + label
            + (".zip" if "windows" in label else ".tar.gz")
        )
        archive(root, dest / name, epoch)
        if args.linux_packages:
            if "linux" not in args.target:
                parser.error("Linux packages require a Linux target")
            linux_packages(work, binaries, args.target, dest, value, epoch)
    print("Packaged " + label + " " + value)


if __name__ == "__main__":
    main()
