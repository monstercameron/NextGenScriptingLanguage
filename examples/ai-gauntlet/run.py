#!/usr/bin/env python3
"""Check and run every gauntlet example with local fixtures and exact oracles.

No live mode exists in this runner. EXECUTE_MODELS_OFFLINE blocks fixtureless
requests inside the helper before credentials or network access.
"""
from __future__ import annotations

import argparse
import json
import os
from pathlib import Path, PurePosixPath
import shutil
import subprocess
import tempfile

HERE = Path(__file__).resolve().parent
PROJECT = HERE.parents[1]
TIMEOUT = 15


class ContractError(ValueError):
    """A local example or its result violates the authored contract."""


def relative(raw):
    if not isinstance(raw, str) or not raw or "\\" in raw:
        raise ContractError(f"not a relative POSIX path: {raw!r}")
    path = PurePosixPath(raw)
    if path.is_absolute() or ".." in path.parts or str(path) != raw:
        raise ContractError(f"path escapes its fixture workspace: {raw!r}")
    return Path(*path.parts)


def catalog():
    document = json.loads((HERE / "catalog.json").read_text(encoding="utf-8"))
    workflows = document["workflows"]
    if document["version"] != 1 or len(workflows) != 24:
        raise ContractError("expected version 1 and exactly 24 examples")
    if [item["id"] for item in workflows] != list(range(1, 25)):
        raise ContractError("example IDs must be exactly 1..24")
    if {item["file"] for item in workflows} != {path.name for path in HERE.glob("*.program")}:
        raise ContractError("catalog must cover every program exactly once")
    for item in workflows:
        source = (HERE / relative(item["file"])).read_text(encoding="utf-8")
        if 'accept live as boolean with default false' not in source:
            raise ContractError(f'{item["file"]}: missing offline default')
        if len(item["cases"]) < 4 or not item["advantage"] or not item["bash_would_need"]:
            raise ContractError(f'{item["file"]}: missing cases or comparison')
        prefix = f'fixtures/{item["id"]:03}/'
        tree = HERE / prefix
        for path in tree.rglob("*"):
            if path.is_symlink() or (not path.is_file() and not path.is_dir()):
                raise ContractError(f"fixture must be a regular local file: {path}")
        for case in item["cases"]:
            for name, value in case["inputs"].items():
                if name == "confirmed":
                    if value not in {"true", "false"}:
                        raise ContractError("invalid confirmation input")
                elif name in {"fixture", "result_fixture", "source_path"}:
                    relative(value)
                    if not value.startswith(prefix):
                        raise ContractError("input must reference this example's fixture bundle")
                else:
                    raise ContractError(f"unsupported offline input: {name}")
            for path in case["outputs"]:
                relative(path)
                if not path.startswith("out/"):
                    raise ContractError("expected output must stay inside out/")
    return workflows


def offline_environment():
    environment = dict(os.environ)
    credentials = {"openai", "openai_api_key", "jev", "jev_api_key", "typesafe_api_key"}
    for name in list(environment):
        if name.lower() in credentials or name.upper().startswith("EXECUTE_MODEL_KEY_"):
            del environment[name]
    environment["EXECUTE_MODELS_OFFLINE"] = "1"
    return environment


def invoke(arguments, directory):
    try:
        result = subprocess.run(arguments, cwd=directory, env=offline_environment(),
                                stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                                stderr=subprocess.PIPE, timeout=TIMEOUT, check=False)
    except subprocess.TimeoutExpired as error:
        raise ContractError(f"example exceeded {TIMEOUT}s: {arguments}") from error
    if result.returncode:
        raise ContractError(f"execute failed ({result.returncode}): {result.stderr.decode('utf-8', 'replace')}")
    return result


def run(binary, manifests, workflows):
    results = []
    for item in workflows:
        source = HERE / item["file"]
        invoke([str(binary), "check", str(source), "--manifest-dir", str(manifests)], HERE)
        cases = []
        if item["unix_only"] and os.name != "posix":
            print(f'SKIP {item["file"]}: Unix-only literal pwd; static check passed')
            results.append(dict(file=item["file"], checked=True, skipped="requires Unix", cases=[]))
            continue
        ordinal = f'{item["id"]:03}'
        for case in item["cases"]:
            with tempfile.TemporaryDirectory(prefix=f"ai-gauntlet-{ordinal}-") as temporary:
                workspace = Path(temporary)
                shutil.copytree(HERE / "fixtures" / ordinal, workspace / "fixtures" / ordinal)
                (workspace / "out").mkdir()
                before = {path.relative_to(workspace): path.read_bytes()
                          for path in (workspace / "fixtures").rglob("*") if path.is_file()}
                arguments = [str(binary), str(source), "--manifest-dir", str(manifests)]
                for name, value in case["inputs"].items():
                    arguments += ["--input", f"{name}={value}"]
                actual = invoke(arguments, workspace)
                if actual.stdout != case["expected_stdout"].encode("utf-8"):
                    raise ContractError(f'{item["file"]}/{case["name"]}: stdout mismatch: {actual.stdout!r}')
                for path, expected in case["outputs"].items():
                    target = workspace / relative(path)
                    if not target.is_file() or target.read_bytes() != expected.encode("utf-8"):
                        observed = target.read_bytes() if target.is_file() else b"<missing>"
                        raise ContractError(f'{item["file"]}/{case["name"]}/{path}: {observed!r} != {expected!r}')
                if (workspace / "MODEL_TEXT_EXECUTED").exists():
                    raise ContractError("model shell syntax was executed")
                if any((workspace / path).read_bytes() != data for path, data in before.items()):
                    raise ContractError("an input fixture changed")
                cases.append(dict(name=case["name"], passed=True))
        print(f'PASS {item["file"]}: {len(cases)} cases')
        results.append(dict(file=item["file"], checked=True, cases=cases))
    return dict(version=1, programs_checked=len(results),
                programs_run=sum(bool(item["cases"]) for item in results),
                cases_passed=sum(len(item["cases"]) for item in results),
                network_policy="EXECUTE_MODELS_OFFLINE=1; sanitized credentials; fixture-only inputs",
                results=results)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--manifest-dir", type=Path, default=PROJECT / "manifests")
    parser.add_argument("--report", type=Path)
    arguments = parser.parse_args()
    report = run(arguments.binary.resolve(strict=True), arguments.manifest_dir.resolve(strict=True), catalog())
    if arguments.report:
        arguments.report.parent.mkdir(parents=True, exist_ok=True)
        arguments.report.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
    print(f'{report["programs_checked"]} checked; {report["programs_run"]} run; {report["cases_passed"]} cases passed')


if __name__ == "__main__":
    try:
        main()
    except (ContractError, OSError, KeyError, ValueError) as error:
        raise SystemExit(f"gauntlet: {error}") from error
