#!/usr/bin/env python3
"""Same-binary explicit directory-vs-packed CLI comparison (not default startup)."""
from __future__ import annotations

import argparse
import base64
import hashlib
import importlib.util
import json
import os
import platform
import statistics
import subprocess
import sys
import time
from pathlib import Path
from typing import Any, TextIO

CAMPAIGN = Path(__file__).resolve().parent.parent
SOURCE_DEFAULT = CAMPAIGN / "work" / "catalog-cli-worktree"
EVIDENCE_DEFAULT = CAMPAIGN / "evidence" / "catalog-cli-validation"
COUNT, WARMUPS, PAIRS, TIMEOUT = 75, 5, 48, 30
CASES = ("check_json", "applets_list")


def file_hash(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            h.update(block)
    return h.hexdigest()


def tree_hash(paths: list[Path], root: Path) -> str:
    h = hashlib.sha256()
    for path in sorted(paths, key=lambda p: p.relative_to(root).as_posix()):
        name, data = path.relative_to(root).as_posix().encode(), path.read_bytes()
        h.update(len(name).to_bytes(8, "big") + name + len(data).to_bytes(8, "big") + data)
    return h.hexdigest()


def source_hash(root: Path) -> str:
    # Include all root-level build/toolchain files plus the inputs and vendored
    # code that can affect the executable or the packed representation.
    paths = {p for p in root.iterdir() if p.is_file() or p.is_symlink()}
    for name in ("src", "tests", "manifests", "tools", "vendor", ".cargo",
                 ".github", ".githooks", "build"):
        base = root / name
        if base.is_symlink():
            raise ValueError(f"source hash directory is symlinked: {base}")
        if base.is_dir():
            paths.update(p for p in base.rglob("*") if p.is_file() or p.is_symlink())
    paths = sorted(paths)
    if any(not p.is_file() or p.is_symlink() for p in paths):
        raise ValueError("source hash inputs are missing or symlinked")
    return tree_hash(paths, root)


def fixture_hash(root: Path) -> str:
    paths = sorted(p for p in root.rglob("*") if p.is_file())
    if any(p.is_symlink() for p in paths):
        raise ValueError("fixture contains a symlink")
    return tree_hash(paths, root)


def journal_record(row: dict[str, Any]) -> dict[str, Any]:
    saved = dict(row)
    stdout, stderr = saved.pop("stdout", None), saved.pop("stderr", None)
    if stdout is not None:
        if not isinstance(stdout, bytes) or not isinstance(stderr, bytes):
            raise TypeError("journal stdout and stderr must be bytes")
        saved["stdout_base64"] = base64.b64encode(stdout).decode("ascii")
        saved["stderr_base64"] = base64.b64encode(stderr).decode("ascii")
    return saved


def write_journal_record(journal: TextIO, row: dict[str, Any]) -> None:
    journal.write(json.dumps(journal_record(row), ensure_ascii=False, separators=(",", ":")) + "\n")
    journal.flush()


def write_preregistration(path: Path, record: dict[str, Any]) -> str:
    data = json.dumps(record, indent=2, ensure_ascii=False) + "\n"
    with path.open("x", encoding="utf-8", newline="\n") as stream:
        stream.write(data)
        stream.flush()
        os.fsync(stream.fileno())
    return file_hash(path)


def write_report_snapshot(path: Path, record: dict[str, Any], *, initial: bool = False) -> None:
    data = json.dumps(record, indent=2, ensure_ascii=False) + "\n"
    if initial:
        with path.open("x", encoding="utf-8", newline="\n") as stream:
            stream.write(data)
            stream.flush()
            os.fsync(stream.fileno())
        return
    temporary = path.with_name(path.name + ".tmp")
    with temporary.open("x", encoding="utf-8", newline="\n") as stream:
        stream.write(data)
        stream.flush()
        os.fsync(stream.fileno())
    os.replace(temporary, path)


def parsed_output(case: str, status: int | None, stdout: bytes, workflow: Path, names: list[str]) -> Any:
    if status != 0:
        raise ValueError(f"exit status {status}")
    value = json.loads(stdout.decode("utf-8"))
    if not isinstance(value, dict):
        raise ValueError("JSON output is not an object")
    if case == "check_json":
        if (value.get("protocol"), value.get("validation")) != ("execute.check/1", "static"):
            raise ValueError("check JSON protocol/result is invalid")
        if value.get("ok") is not True or value.get("diagnostics") != []:
            raise ValueError("fixed valid workflow did not produce a clean check result")
        if value.get("path") != workflow.name or value.get("path_bytes") != list(workflow.name.encode()):
            raise ValueError("check JSON workflow path changed")
    else:
        applets = value.get("applets")
        listed = [x.get("name") if isinstance(x, dict) else None for x in applets] if isinstance(applets, list) else []
        if (value.get("protocol"), value.get("validation")) != ("execute.applets/1", "manifest_static"):
            raise ValueError("applets list protocol is invalid")
        if listed != names or len(listed) != COUNT:
            raise ValueError("applets list does not match the 75-manifest fixture")
    return value


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=EVIDENCE_DEFAULT / "execute-binary")
    parser.add_argument("--build-receipt", type=Path, default=EVIDENCE_DEFAULT / "build-receipt.json")
    parser.add_argument("--source-root", type=Path, default=SOURCE_DEFAULT)
    parser.add_argument("--output-dir", type=Path, required=True)
    args = parser.parse_args()
    root, binary, receipt = args.source_root.resolve(), args.binary.resolve(), args.build_receipt.resolve()
    manifests = (root / "manifests").resolve()
    output = args.output_dir.resolve()
    output.mkdir(parents=True, exist_ok=False)
    fixture, journal_path = output / "fixture", output / "raw-invocations.jsonl"
    prereg_path = output / "preregistration.json"
    report_path = output / "paired-report.json"
    drifts: list[dict[str, Any]] = []
    samples: dict[str, list[dict[str, Any]]] = {case: [] for case in CASES}
    journal = journal_path.open("x", encoding="utf-8", newline="\n")
    raw_record_count = 0
    report: dict[str, Any] = {
        "schema": "execute.catalog-cli-paired-benchmark/1",
        "estimand": "same-binary explicit directory-vs-packed representation comparison; not default-startup speedup",
        "plan": {"manifest_count": COUNT, "warmup_pairs_per_case": WARMUPS,
                 "measured_pairs_per_case": PAIRS, "case_order": "AB/BA alternation, 24 each per case",
                 "A": "--manifest-dir", "B": "--manifest-catalog",
                 "cases": ["check --json fixed printf workflow", "applets list"],
                 "timer": "perf_counter_ns around subprocess.run only; setup, parsing, comparison, hashes and journal writes are untimed"},
        "paths": {"binary": str(binary), "build_receipt": str(receipt), "source_root": str(root),
                  "manifest_source": str(manifests), "output_dir": str(output), "raw_journal": str(journal_path),
                  "preregistration": str(prereg_path)},
        "host": {"platform": platform.platform(), "machine": platform.machine(), "python": sys.version},
        "integrity": {}, "oracle_drift": drifts, "samples": samples,
    }
    def log(row: dict[str, Any]) -> None:
        nonlocal raw_record_count
        write_journal_record(journal, row)
        raw_record_count += 1

    log({"kind": "run_start", "schema": report["schema"], "plan": report["plan"],
         "paths": report["paths"], "host": report["host"]})

    try:
        revision = subprocess.run(["git", "-C", str(root), "rev-parse", "HEAD"], check=True,
                                  stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True).stdout.strip()
        src_before, bin_before, receipt_before = source_hash(root), file_hash(binary), file_hash(receipt)
        build = json.loads(receipt.read_text(encoding="utf-8"))["execute"]
        if build.get("sha256") != bin_before or Path(build["saved_binary"]).resolve() != binary:
            raise ValueError("saved execute binary differs from build receipt")
        if Path(build["artifact"]["manifest_path"]).resolve() != root / "Cargo.toml":
            raise ValueError("source root differs from execute build receipt")
        report["source_commit"] = revision
        report["integrity"].update({"source_hash_before": src_before, "binary_sha256_before": bin_before,
                                    "build_receipt_sha256_before": receipt_before,
                                    "build_receipt_binary_sha256": build["sha256"],
                                    "source_hash_scope": "root-level files and src/tests/manifests/tools/vendor/.cargo/.github/.githooks/build trees"})

        source_files = sorted(manifests.glob("*.json"), key=lambda p: p.name)
        if len(source_files) != COUNT or any(p.is_symlink() or not p.is_file() for p in source_files):
            raise ValueError(f"expected exactly {COUNT} regular source manifest JSON files")
        manifest_dir = fixture / "manifests"
        manifest_dir.mkdir(parents=True)
        for source in source_files:
            (manifest_dir / source.name).write_bytes(source.read_bytes())
        workflow = fixture / "fixed-printf-check.program"
        workflow.write_bytes(b'run printf with message "catalog CLI benchmark"\n')
        catalog = manifest_dir / "packed.manifest-catalog"
        packer_path = root / "tools" / "pack_manifest_catalog.py"
        spec = importlib.util.spec_from_file_location("manifest_packer", packer_path)
        if spec is None or spec.loader is None:
            raise ValueError("cannot load repository catalog packer")
        packer = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(packer)
        if packer.pack_directory(manifest_dir, catalog) != COUNT:
            raise ValueError("packer did not include all 75 manifests")
        source_objects = [json.loads(p.read_text(encoding="utf-8")) for p in source_files]
        envelope = json.loads(catalog.read_text(encoding="utf-8"))
        if (envelope.get("format"), envelope.get("version"), envelope.get("manifests")) != (
            "execute.manifest-catalog", 1, source_objects
        ):
            raise ValueError("packed catalog differs from sorted source manifest objects")
        names = sorted(item["name"] for item in source_objects)
        if len(names) != COUNT or len(set(names)) != COUNT:
            raise ValueError("source canonical names are not 75 unique entries")
        fixture_before = fixture_hash(fixture)
        report.update({"fixture_hash_before": fixture_before, "manifest_count": COUNT,
                       "fixture_filenames": [p.name for p in source_files],
                       "packed_catalog_sha256": file_hash(catalog), "measurement_started": False})
        prereg = {
            "schema": "execute.catalog-cli-paired-benchmark-preregistration/1",
            "status": "fixed before preflight and timing",
            "estimand": report["estimand"],
            "plan": report["plan"],
            "source_commit": revision,
            "paths": report["paths"],
            "identity": {
                "source_hash": src_before,
                "source_hash_scope": report["integrity"]["source_hash_scope"],
                "binary_sha256": bin_before,
                "build_receipt_sha256": receipt_before,
                "build_receipt_binary_sha256": build["sha256"],
                "fixture_hash": fixture_before,
                "packed_catalog_sha256": report["packed_catalog_sha256"],
                "manifest_specs_sha256": hashlib.sha256(json.dumps(
                    source_objects, sort_keys=True, separators=(",", ":"), ensure_ascii=False
                ).encode()).hexdigest(),
            },
        }
        report["preregistration_sha256"] = write_preregistration(prereg_path, prereg)
        report["status"] = "preregistered"
        write_report_snapshot(report_path, report, initial=True)
        log({"kind": "preregistration", "path": str(prereg_path),
             "sha256": report["preregistration_sha256"], "identity": prereg["identity"]})

        def invoke(case: str, side: str, phase: str, pair: int | None, timed: bool) -> dict[str, Any]:
            selector = ["--manifest-dir", str(manifest_dir)] if side == "directory" else ["--manifest-catalog", str(catalog)]
            argv = ([str(binary), "check", workflow.name, "--json", *selector] if case == "check_json"
                    else [str(binary), "applets", "list", *selector])
            env = os.environ.copy(); env["LC_ALL"] = "C"; env["LANG"] = "C"
            start = time.perf_counter_ns() if timed else None
            execution_error = None
            try:
                result = subprocess.run(argv, cwd=fixture, env=env, stdout=subprocess.PIPE,
                                        stderr=subprocess.PIPE, timeout=TIMEOUT, check=False)
                status, stdout, stderr = result.returncode, result.stdout, result.stderr
                timeout_hit = False
            except subprocess.TimeoutExpired as error:
                status, stdout, stderr, timeout_hit = None, error.stdout or b"", error.stderr or b"", True
                if isinstance(stdout, str): stdout = stdout.encode("utf-8", "replace")
                if isinstance(stderr, str): stderr = stderr.encode("utf-8", "replace")
            except OSError as error:
                status, stdout, stderr, timeout_hit = None, b"", str(error).encode(), False
                execution_error = str(error)
            elapsed = time.perf_counter_ns() - start if start is not None else None
            parsed, parse_error = None, None
            try:
                parsed = parsed_output(case, status, stdout, workflow, names)
            except Exception as error:
                parse_error = str(error)
            row = {"kind": "invocation", "phase": phase, "case": case, "pair": pair, "side": side,
                   "argv": argv, "duration_ns": elapsed, "returncode": status, "timed_out": timeout_hit,
                   "output_valid": parse_error is None, "parse_error": parse_error,
                   "execution_error": execution_error,
                   "parsed_sha256": hashlib.sha256(json.dumps(parsed, sort_keys=True, separators=(",", ":"),
                                                           ensure_ascii=False).encode()).hexdigest() if parsed is not None else None,
                   "stdout": stdout, "stderr": stderr, "_parsed": parsed}
            log({k: v for k, v in row.items() if not k.startswith("_")})
            return row

        def compare(case: str, phase: str, pair: int | None, left: dict[str, Any], right: dict[str, Any],
                    oracle: dict[str, Any]) -> dict[str, Any]:
            order = [left["side"], right["side"]]
            errors = []
            for row in (left, right):
                if row["parse_error"]: errors.append(f"{row['side']}: {row['parse_error']}")
                if (row["returncode"], row["stdout"], row["stderr"], row["_parsed"]) != (
                    oracle["returncode"], oracle["stdout"], oracle["stderr"], oracle["_parsed"]
                ): errors.append(f"{row['side']}: differs from preflight directory oracle")
            same = (left["returncode"], left["stdout"], left["stderr"], left["_parsed"]) == (
                right["returncode"], right["stdout"], right["stderr"], right["_parsed"])
            if not same: errors.append("directory and packed exact outputs differ")
            if errors: drifts.append({"phase": phase, "case": case, "pair": pair, "errors": errors})
            record = {"kind": "pair", "phase": phase, "case": case, "pair": pair, "order": order,
                      "exact_equal": same,
                      "directory_duration_ns": next((x["duration_ns"] for x in (left, right) if x["side"] == "directory"), None),
                      "packed_duration_ns": next((x["duration_ns"] for x in (left, right) if x["side"] == "packed"), None)}
            log(record)
            return record

        oracles: dict[str, dict[str, Any]] = {}
        preflight_ok = True
        for case in CASES:
            directory = invoke(case, "directory", "oracle", None, False)
            oracles[case] = directory
            packed = invoke(case, "packed", "oracle", None, False)
            row = compare(case, "oracle", None, directory, packed, directory)
            preflight_ok = (preflight_ok and row["exact_equal"]
                            and not directory["parse_error"] and not packed["parse_error"])
        if preflight_ok:
            report["measurement_started"] = True
            report["status"] = "running"
            write_report_snapshot(report_path, report)
            log({"kind": "measurement_start", "warmup_pairs_per_case": WARMUPS,
                 "measured_pairs_per_case": PAIRS})
            for phase, count in (("warmup", WARMUPS), ("measure", PAIRS)):
                for index in range(count):
                    order = ["directory", "packed"] if index % 2 == 0 else ["packed", "directory"]
                    case_order = CASES if index % 2 == 0 else tuple(reversed(CASES))
                    for case in case_order:
                        first = invoke(case, order[0], phase, index + 1, phase == "measure")
                        second = invoke(case, order[1], phase, index + 1, phase == "measure")
                        pair = compare(case, phase, index + 1, first, second, oracles[case])
                        if phase == "measure": samples[case].append(pair)
        else:
            report["measurement_skipped"] = "pre-timing exact-output oracle failed"

        summary = {}
        for case, rows in samples.items():
            valid = [r for r in rows if r["directory_duration_ns"] and r["packed_duration_ns"]]
            ratios = [r["packed_duration_ns"] / r["directory_duration_ns"] for r in valid]
            summary[case] = {"pairs": len(rows), "packed_wins": sum(r["packed_duration_ns"] < r["directory_duration_ns"] for r in valid),
                             "packed_losses": sum(r["packed_duration_ns"] > r["directory_duration_ns"] for r in valid),
                             "ties": sum(r["packed_duration_ns"] == r["directory_duration_ns"] for r in valid),
                             "directory_median_ns": statistics.median(r["directory_duration_ns"] for r in valid) if valid else None,
                             "packed_median_ns": statistics.median(r["packed_duration_ns"] for r in valid) if valid else None,
                             "median_packed_over_directory_ratio": statistics.median(ratios) if ratios else None}
        report["summary"] = summary
    except Exception as error:
        drifts.append({"phase": "setup_or_run", "errors": [f"{type(error).__name__}: {error}"]})
        report["error"] = f"{type(error).__name__}: {error}"

    try:
        journal.flush()
        journal.close()
    except Exception as error:
        drifts.append({"phase": "journal_close", "errors": [str(error)]})

    integrity = report["integrity"]
    for key, path, hasher in (("source_hash_after", root, source_hash),
                              ("binary_sha256_after", binary, file_hash),
                              ("build_receipt_sha256_after", receipt, file_hash),
                              ("fixture_hash_after", fixture, fixture_hash),
                              ("preregistration_sha256_after", prereg_path, file_hash)):
        try:
            integrity[key] = hasher(path.resolve(strict=True))
        except Exception as error:
            integrity[key + "_error"] = str(error)
    integrity["stable"] = (
        integrity.get("source_hash_after") == integrity.get("source_hash_before")
        and integrity.get("binary_sha256_after") == integrity.get("binary_sha256_before")
        and integrity.get("build_receipt_sha256_after") == integrity.get("build_receipt_sha256_before")
        and integrity.get("fixture_hash_after") == report.get("fixture_hash_before")
        and integrity.get("preregistration_sha256_after") == report.get("preregistration_sha256")
    )
    if not integrity["stable"]:
        drifts.append({"phase": "integrity", "errors": ["source, binary, build receipt, or fixture changed"]})
    report["status"] = "passed" if not drifts else "failed"
    report["raw_record_count"] = raw_record_count
    write_report_snapshot(report_path, report)
    print(report_path)
    return 0 if report["status"] == "passed" else 1


if __name__ == "__main__":
    raise SystemExit(main())
