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
from package import extra_files
from release import version


def smoke(executable, model_helper, applet_helper, manifests):
    executable, model_helper, applet_helper, manifests = [
        p.resolve() for p in [executable, model_helper, applet_helper, manifests]
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
        helper_version = (
            subprocess.check_output(
                [str(model_helper), "--version"], cwd=folder, timeout=20
            )
            .decode()
            .strip()
        )
        if helper_version != "execute-model " + version():
            raise ValueError("installed helper version differs from source")
        if run("model", "--version").decode().strip() != helper_version:
            raise ValueError("delegated helper version differs")
        result = subprocess.run(
            [str(model_helper), "--help"],
            cwd=folder,
            capture_output=True,
            timeout=20,
            check=True,
        )
        if b"execute-model" not in result.stdout:
            raise ValueError("invalid installed model helper")
        if b"execute-model" not in run("model", "--help"):
            raise ValueError("bundled helper discovery failed")
        applet_version = subprocess.check_output(
            [str(applet_helper), "--version"], cwd=folder, timeout=20
        ).decode().strip()
        if applet_version != "execute-applet " + version():
            raise ValueError("installed applet helper version differs from source")
        applet_help = subprocess.run(
            [str(applet_helper), "--help"],
            cwd=folder,
            capture_output=True,
            timeout=20,
            check=True,
        )
        if b"Persistent workers use --worker" not in applet_help.stdout:
            raise ValueError("invalid installed applet helper")
        applet_result = subprocess.run(
            [str(applet_helper), "path.basename", "--args", '{"path":"/tmp/a file.txt"}'],
            cwd=folder,
            input=b"",
            capture_output=True,
            timeout=20,
            check=True,
        )
        if applet_result.stdout != b'"a file.txt"\n':
            raise ValueError("installed applet helper returned incorrect data")
        helper_manifests = folder / "helper-manifests"
        helper_manifests.mkdir()
        (helper_manifests / "helper.json").write_text(
            json.dumps(
                {
                    "name": "packaged.helper",
                    "binary": "@execute-applet",
                    "parameters": {
                        "path": {"type": "path", "required": True}
                    },
                    "arguments": [
                        {"literal": "path.basename"},
                        {"literal": "--args"},
                        {"json_arguments": True},
                    ],
                    "contract": {
                        "version": 1,
                        "backend": "command",
                        "output": {"type": "text", "encoding": "json"},
                    },
                }
            )
        )
        workflow = folder / "applet-helper.program"
        workflow.write_text(
            'run packaged.helper with path "/tmp/a file.txt" as name\n'
            "then encode name as json\n"
        )
        result = run(workflow, "--manifest-dir", helper_manifests)
        if result != b'"a file.txt"':
            raise ValueError("execute did not resolve its adjacent applet helper")
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
            "run " + json.dumps(str(model_helper)) + ' with arguments ["--help"]\n'
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
            prefix / ("bin/execute-applet" + suffix),
            prefix / "share/execute/manifests" if args.system else prefix / "manifests",
        )
        if args.system:
            missing = [
                relative
                for _, relative in extra_files(True)
                if not (prefix / relative).is_file()
            ]
            if missing:
                raise SystemExit("system package lacks " + ", ".join(missing))

    if args.archive:
        if args.system:
            parser.error("--system is for an extracted package root")
        with extracted_archive(args.archive) as root:
            check(root)
    else:
        check(args.root)


if __name__ == "__main__":
    main()
