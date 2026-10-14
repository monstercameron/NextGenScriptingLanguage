#!/usr/bin/env python3
"""Run the installed tree from an unrelated cwd, without network or host effects."""
import argparse
import json
from pathlib import Path
import subprocess
import tempfile
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


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, required=True)
    parser.add_argument("--system", action="store_true")
    args = parser.parse_args()
    prefix = args.root / "usr" if args.system else args.root
    suffix = ".exe" if (prefix / "bin/execute.exe").is_file() else ""
    smoke(
        prefix / ("bin/execute" + suffix),
        prefix / ("bin/execute-model" + suffix),
        prefix / "share/execute/manifests" if args.system else prefix / "manifests",
    )


if __name__ == "__main__":
    main()
