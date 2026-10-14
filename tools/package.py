#!/usr/bin/env python3
"""Build deterministic native archives and Linux packages from release binaries."""
import argparse
import gzip
import io
import os
from pathlib import Path
import shutil
import struct
import subprocess
import tarfile
import tempfile
import time
import zipfile
from release import ROOT, TARGETS, package_version, version


def run(*args, **kwargs):
    return subprocess.run(list(map(str, args)), check=True, **kwargs)


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


def populate(root, binaries, target, system=False):
    prefix = root / "usr" if system else root
    bindir = prefix / "bin"
    share = prefix / "share/execute" if system else root
    bindir.mkdir(parents=True)
    share.mkdir(parents=True, exist_ok=True)
    suffix = ".exe" if "windows" in target else ""
    for name in ["execute", "execute-model"]:
        source = binaries / (name + suffix)
        check_binary(source, target)
        dest = bindir / source.name
        shutil.copyfile(source, dest)
        dest.chmod(0o755)
    shutil.copytree(ROOT / "manifests", share / "manifests")
    for name in ["README.md", "LICENSE"]:
        shutil.copyfile(ROOT / name, share / name)
    docs = share / "docs"
    docs.mkdir()
    for name in ["windows.md", "releases.md", "syntax.md"]:
        source = ROOT / "docs" / name
        if source.is_file():
            shutil.copyfile(source, docs / name)


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
        dest / ("execute_" + pv + "_" + debarch + ".deb"),
        env=env,
    )
    rpmroot = work / "rpm"
    for name in ["BUILD", "BUILDROOT", "RPMS", "SOURCES", "SPECS", "SRPMS"]:
        (rpmroot / name).mkdir(parents=True)
    payload = work / "payload"
    populate(payload, binaries, target, system=True)
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
/usr/share/execute
"""
        % (pv, str(payload))
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
    shutil.copyfile(packages[0], dest / ("execute-" + pv + "-1." + rpmarch + ".rpm"))


def main():
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
