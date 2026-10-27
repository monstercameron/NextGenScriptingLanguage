#!/usr/bin/env python3
"""Check that a release executable has no build-root path or ambient manifest fallback."""
import argparse
import json
from pathlib import Path
import shutil
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]


def verify(binary, build_root=ROOT):
    data = binary.read_bytes()
    # Check both normal and escaped Windows spellings in common encodings. Rust may
    # store the constant separately from the literal "manifests" path component.
    root = str(build_root.resolve())
    for text in {root, root.replace("\\", "/"), root.replace("\\", "\\\\")}:
        for encoding in ("utf-8", "utf-16-le"):
            if text.encode(encoding) in data:
                raise ValueError("release executable contains the build root")
    with tempfile.TemporaryDirectory() as temp:
        directory = Path(temp)
        bin_dir = directory / "bin"
        bin_dir.mkdir()
        executable = bin_dir / binary.name
        shutil.copy2(binary, executable)
        result = subprocess.run([str(executable), "applets", "list"], cwd=directory,
                                capture_output=True, timeout=30)
        if result.returncode == 0 or b"no installed manifest directory found" not in result.stderr:
            raise ValueError(f"detached executable did not refuse missing manifests: {result.stderr!r}")
        direct = directory / "direct.program"
        direct.write_text("run " + json.dumps(str(executable)) + " with arguments [\"--version\"]\n")
        direct_run = subprocess.run([str(executable), "run", str(direct)], cwd=directory,
                                    capture_output=True, timeout=30)
        if direct_run.returncode != 0 or not direct_run.stdout.startswith(b"execute "):
            raise ValueError(f"detached direct command failed: {direct_run.stderr!r}")
        # Operations that do not load manifests remain available on a detached binary.
        schema = subprocess.run([str(executable), "live", "schema"], cwd=directory,
                                capture_output=True, timeout=30)
        if schema.returncode != 0:
            raise ValueError(f"detached live schema failed: {schema.stderr!r}")
        program = directory / "live.program"
        program.write_text("run printf with message \"hi\"\n")
        live = subprocess.run([str(executable), "live", "serve", str(program), "--socket", str(directory / "live.sock")],
                              cwd=directory, capture_output=True, timeout=30)
        if live.returncode == 0 or b"unknown applet" not in (live.stdout + live.stderr):
            raise ValueError(f"detached live serve did not refuse missing manifests: {live.stdout!r} {live.stderr!r}")
        # Regular files planted where default directories belong are not legacy
        # explicit manifests and must not be loaded by workflow or live serve.
        for planted in (directory / "manifests", directory / "share/execute/manifests"):
            planted.parent.mkdir(parents=True, exist_ok=True)
            planted.write_text(json.dumps({"name":"printf", "binary":str(executable), "arguments":[]}))
            named = subprocess.run([str(executable), "run", str(program)], cwd=directory,
                                   capture_output=True, timeout=5)
            if named.returncode == 0 or b"unknown applet" not in (named.stdout + named.stderr):
                raise ValueError(f"workflow loaded a planted default file: {named.stdout!r} {named.stderr!r}")
            try:
                blocked = subprocess.run([str(executable), "live", "serve", str(program), "--socket", str(directory / "planted.sock")],
                                         cwd=directory, capture_output=True, timeout=5)
            except subprocess.TimeoutExpired as error:
                raise ValueError("live serve loaded a planted default manifest file") from error
            if blocked.returncode == 0 or b"unknown applet" not in (blocked.stdout + blocked.stderr):
                raise ValueError(f"live serve did not reject planted default file: {blocked.stdout!r} {blocked.stderr!r}")
            planted.unlink()
        # A default catalog with a changed bundled entry and an extra file must
        # not bypass the guarded loader used by live serve.
        default = directory / "manifests"
        shutil.copytree(ROOT / "manifests", default)
        awk = default / "awk.json"
        original = awk.read_text()
        changed = original.replace('"binary": "awk"', '"binary": "sed"')
        if changed == original:
            raise ValueError("awk fixture no longer has the expected binary")
        awk.write_text(changed)
        (default / "extra.json").write_text(json.dumps({"name":"fixture.extra", "binary":"printf", "arguments":[]}))
        program.write_text("run cat\n")
        try:
            rejected = subprocess.run([str(executable), "live", "serve", str(program), "--socket", str(directory / "guard.sock")],
                                      cwd=directory, capture_output=True, timeout=5)
        except subprocess.TimeoutExpired as error:
            raise ValueError("live serve accepted a changed bundled catalog") from error
        if rejected.returncode == 0 or b"differs from the bundled" not in (rejected.stdout + rejected.stderr):
            raise ValueError(f"live serve did not reject the changed catalog: {rejected.stdout!r} {rejected.stderr!r}")
        manifests = directory / "custom"
        manifests.mkdir()
        (manifests / "sample.json").write_text(json.dumps({"name":"sample", "binary":"printf", "arguments":[]}))
        explicit = subprocess.run([str(executable), "applets", "inspect", "sample", "--manifest-dir", str(manifests)],
                                  cwd=directory, capture_output=True, timeout=30)
        if explicit.returncode != 0:
            raise ValueError(f"explicit manifests failed: {explicit.stderr!r}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    args = parser.parse_args()
    verify(args.binary.resolve())
    print("Release executable has no build-root fallback; detached and explicit discovery verified")


if __name__ == "__main__":
    main()
