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
        # Operations that do not load manifests remain available on a detached binary.
        schema = subprocess.run([str(executable), "live", "schema"], cwd=directory,
                                capture_output=True, timeout=30)
        if schema.returncode != 0:
            raise ValueError(f"detached live schema failed: {schema.stderr!r}")
        program = directory / "live.program"
        program.write_text("run printf with message \"hi\"\n")
        live = subprocess.run([str(executable), "live", "serve", str(program), "--socket", str(directory / "live.sock")],
                              cwd=directory, capture_output=True, timeout=30)
        if live.returncode == 0 or b"no installed manifest directory found" not in (live.stdout + live.stderr):
            raise ValueError(f"detached live serve did not refuse missing manifests: {live.stdout!r} {live.stderr!r}")
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
