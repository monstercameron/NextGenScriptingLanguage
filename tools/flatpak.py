#!/usr/bin/env python3
"""Package prebuilt static executables and test the actual Flatpak sandbox."""
import os
from pathlib import Path
import shutil
import subprocess
import tarfile
from release import ROOT, version

APP = "io.github.monstercameron.Execute"


def run(*args, **kwargs):
    return subprocess.run(list(map(str, args)), check=True, **kwargs)


def main():
    value = version()
    arch = os.environ["FLATPAK_ARCH"]
    label = {"x86_64": "x64", "aarch64": "arm64"}[arch]
    branch = "beta" if "-" in value else "stable"
    work = ROOT / "target/flatpak"
    work.mkdir(parents=True, exist_ok=True)
    source = work / "source"
    source.mkdir()
    with tarfile.open(
        ROOT / ("dist/execute-" + value + "-linux-" + label + ".tar.gz")
    ) as archive:
        archive.extractall(source, filter="data")
    root = source / ("execute-" + value)
    build = work / "app"
    run(
        "flatpak",
        "build-init",
        "--arch=" + arch,
        build,
        APP,
        "org.freedesktop.Sdk",
        "org.freedesktop.Platform",
        "25.08",
    )
    shutil.copytree(root / "bin", build / "files/bin", dirs_exist_ok=True)
    share = build / "files/share/execute"
    shutil.copytree(root / "manifests", share / "manifests")
    for name in ["README.md", "LICENSE"]:
        shutil.copyfile(root / name, share / name)
    metadata = build / "files/share/metainfo"
    metadata.mkdir(parents=True)
    (metadata / (APP + ".metainfo.xml")).write_text(
        """<?xml version="1.0" encoding="UTF-8"?>
<component type="console-application">
 <id>"""
        + APP
        + '''</id><name>Execute</name>
 <summary>Readable native command workflows</summary>
 <metadata_license>CC0-1.0</metadata_license><project_license>MIT</project_license>
 <description><p>Run typed .program workflows with native tools, pipes, services and live state.</p><p>The Flatpak edition runs inside its sandbox. Use native packages for unrestricted host automation.</p></description>
 <url type="homepage">https://github.com/monstercameron/NextGenScriptingLanguage</url>
 <provides><binary>execute</binary></provides>
 <releases><release version="'''
        + value
        + '''" date="'''
        + subprocess.check_output(
            ["git", "show", "-s", "--format=%cs"], cwd=ROOT, text=True
        ).strip()
        + """"/></releases>
</component>
"""
    )
    run(
        "flatpak",
        "build-finish",
        "--command=execute",
        "--share=network",
        "--filesystem=home",
        build,
    )
    repo = work / "repo"
    run("flatpak", "build-export", "--arch=" + arch, repo, build, branch)
    bundle = ROOT / ("dist/execute-" + value + "-linux-" + label + ".flatpak")
    run(
        "flatpak",
        "build-bundle",
        "--arch=" + arch,
        "--runtime-repo=https://flathub.org/repo/flathub.flatpakrepo",
        repo,
        bundle,
        APP,
        branch,
    )
    run("flatpak", "install", "--user", "--noninteractive", "--bundle", bundle)
    output = subprocess.check_output(
        ["flatpak", "run", APP + "//" + branch, "--version"], text=True
    ).strip()
    if output != "execute " + value:
        raise ValueError("Flatpak executable version mismatch: " + output)
    result = subprocess.check_output(
        ["flatpak", "run", APP + "//" + branch, "model", "--help"]
    )
    if b"execute-model" not in result:
        raise ValueError("Flatpak bundled helper missing")
    fixture = work / "sandbox.program"
    data = work / "input.json"
    answer = work / "answer.json"
    data.write_text('{"n":42}')
    import json

    fixture.write_text(
        "read "
        + json.dumps(str(data))
        + '\nthen parse json as value\ntake integer at "/n" from value as n\nthen encode n as json\nthen save to '
        + json.dumps(str(answer))
        + "\n"
    )
    run("flatpak", "run", APP + "//" + branch, fixture)
    if json.loads(answer.read_text()) != 42:
        raise ValueError("Flatpak sandbox workflow failed")
    run("flatpak", "uninstall", "--user", "--noninteractive", APP + "//" + branch)
    print("Flatpak sandbox smoke passed: " + arch)


if __name__ == "__main__":
    main()
