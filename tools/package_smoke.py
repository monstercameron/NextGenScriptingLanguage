#!/usr/bin/env python3
"""Run the installed tree from an unrelated cwd, without network or host effects."""
import argparse
import contextlib
import json
from pathlib import Path, PurePosixPath
import subprocess
import tempfile
import tarfile
import zipfile
from release import version


def smoke(executable, helper, manifests):
    executable, helper, manifests = [
        p.resolve() for p in [executable, helper, manifests]
    ]
    if (
        not (manifests / "printf.json").is_file()
        or not (manifests / "jev.classify.json").is_file()
    ):
        raise ValueError("installed applet catalog is incomplete")
    with tempfile.TemporaryDirectory(prefix="execute-install-test-") as tmp:
        folder = Path(tmp)

        def run(*args):
            result = subprocess.run(
                [str(executable), *map(str, args)],
                cwd=folder,
                capture_output=True,
                timeout=20,
            )
            if result.returncode:
                raise ValueError(result.stderr.decode(errors="replace"))
            return result.stdout

        if run("--version").decode().strip() != "execute " + version():
            raise ValueError("installed version differs from source")
        result = subprocess.run(
            [str(helper), "--help"],
            cwd=folder,
            capture_output=True,
            timeout=20,
            check=True,
        )
        if b"execute-model" not in result.stdout:
            raise ValueError("invalid installed model helper")
        if b"execute-model" not in run("model", "--help"):
            raise ValueError("bundled helper discovery failed")
        (folder / "input.json").write_text('{"count":42}')
        workflow = folder / "test.program"
        workflow.write_text(
            'read "input.json"\nthen parse json as value\ntake integer at "/count" from value as count\nthen encode count as json\nthen save to "answer.json"\n'
        )
        run(workflow)
        if json.loads((folder / "answer.json").read_text()) != 42:
            raise ValueError("installed workflow returned incorrect data")
        workflow.write_text('run printf with message "installed catalog"\n')
        run("check", workflow)  # automatic discovery, including /usr/share layout
        run("check", workflow, "--manifest-dir", manifests)
        workflow.write_text(
            "run " + json.dumps(str(helper)) + ' with arguments ["--help"]\n'
        )
        if b"execute-model" not in run(workflow):
            raise ValueError("installed native executable invocation failed")
    print("Installed tree smoke passed: " + str(executable))


@contextlib.contextmanager
def extracted_archive(path):
    # Never restore install trees from the Rust build cache. Each invocation gets
    # an owned empty directory, even if target/ contains a previous version.
    with tempfile.TemporaryDirectory(prefix="execute-archive-test-") as tmp:
        folder = Path(tmp)

        def safe(name):
            parts = PurePosixPath(name).parts
            if (
                not parts
                or name.startswith("/")
                or "\\" in name
                or ":" in name
                or ".." in parts
            ):
                raise ValueError("unsafe archive path")

        if path.suffix == ".zip":
            with zipfile.ZipFile(path) as archive:
                for entry in archive.infolist():
                    safe(entry.filename)
                    if entry.external_attr >> 16 & 0o170000 == 0o120000:
                        raise ValueError("archive symlink is not permitted")
                archive.extractall(folder)
        else:
            with tarfile.open(path) as archive:
                for entry in archive.getmembers():
                    safe(entry.name)
                    if not (entry.isfile() or entry.isdir()):
                        raise ValueError("archive special file is not permitted")
                archive.extractall(
                    folder,
                    **({"filter": "data"} if hasattr(tarfile, "data_filter") else {})
                )
        roots = list(folder.iterdir())
        if (
            len(roots) != 1
            or roots[0].name != "execute-" + version()
            or not roots[0].is_dir()
        ):
            raise ValueError("invalid archive root")
        yield roots[0]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    choice = parser.add_mutually_exclusive_group(required=True)
    choice.add_argument("--root", type=Path)
    choice.add_argument("--archive", type=Path)
    parser.add_argument("--system", action="store_true")
    args = parser.parse_args()

    def check(root):
        prefix = root / "usr" if args.system else root
        suffix = ".exe" if (prefix / "bin/execute.exe").is_file() else ""
        smoke(
            prefix / ("bin/execute" + suffix),
            prefix / ("bin/execute-model" + suffix),
            prefix / "share/execute/manifests" if args.system else prefix / "manifests",
        )

    if args.archive:
        if args.system:
            parser.error("--system is for an extracted package root")
        with extracted_archive(args.archive) as root:
            check(root)
    else:
        check(args.root)


if __name__ == "__main__":
    main()
