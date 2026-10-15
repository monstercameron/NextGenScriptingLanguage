#!/usr/bin/env python3
"""Paired saved-baseline/current-source Omarchy success-path harness.

This harness reuses the owned differential fixtures in tests/omarchy_rewrites.
Each side must first pass its existing Bash oracle and side-effect assertions;
then old/new Execute output, status and stderr must match exactly. Only the
selected Execute subprocess is timed. This wrapper never invokes Cargo.
"""
from __future__ import annotations

import argparse
import base64
from datetime import datetime, timezone
import hashlib
import importlib.util
import json
import math
import os
from pathlib import Path
import platform
import shutil
import statistics
import subprocess
import sys
import tempfile
import time
from typing import Any


ROOT = Path(__file__).resolve().parents[1]
BENCHMARKS = ROOT / "benchmarks"
BASELINE_REVISION = "8eec22985a786c1b67abe6dc4b88849b4a4e6f85"
BASELINE_EXECUTE_SHA256 = "5fc6302cd1795600c237ec718da41ffbd89a53a20e174588769188ffc4a5a502"
DEFAULT_BASELINE = Path(
    "/Users/earlcameron/Documents/Codex/2026-10-14/fi/work/contract-refinement-baseline"
)
DEFAULT_SAMPLES = 25
DEFAULT_WARMUPS = 6


SCENARIOS: list[dict[str, str]] = [
    {
        "id": "01-notification-time",
        "module": "test_01_notification_time.py",
        "test_class": "NotificationTimeTests",
        "fixture": "test_success",
        "assertions": "Exact successful output/status and recorded date/notification argv; owned HOME and Omarchy trees remain unchanged.",
    },
    {
        "id": "02-theme-current",
        "module": "test_02_theme_current.py",
        "test_class": "ThemeCurrentTests",
        "fixture": "ordinary oracle case",
        "assertions": "Exact Bash stdout/status/stderr and native sed argv on the host dialect; shell-looking file contents do not execute.",
    },
    {
        "id": "03-update-analyze-logs",
        "module": "test_03_update_logs.py",
        "test_class": "UpdateLogRewriteTests",
        "fixture": "success.log: update and success markers",
        "assertions": "Exact successful output/status/stderr and grep argv; source log and owned HOME remain unchanged.",
    },
    {
        "id": "04-power-present",
        "module": "test_04_power_present.py",
        "test_class": "PowerPresentTests",
        "fixture": "one online AC supply",
        "assertions": "Successful status/output and unchanged owned fake-sysfs tree compared with Bash.",
    },
    {
        "id": "05-theme-set-browser",
        "module": "test_05_browser_theme.py",
        "test_class": "BrowserThemeRewrite",
        "fixture": "valid RGB theme and successful policy utility",
        "assertions": "Exact Bash output/status/stderr, command argv and owned HOME files.",
    },
    {
        "id": "06-default-browser",
        "module": "test_06_default_browser.py",
        "test_class": "DefaultBrowserTests",
        "fixture": "known Firefox query",
        "assertions": "Exact successful output/status, XDG command trace, browser state, notifications and owned HOME.",
    },
    {
        "id": "07-workspace-layout-toggle",
        "module": "test_07_workspace_layout.py",
        "test_class": "WorkspaceLayoutTests",
        "fixture": "dwindle to scrolling success",
        "assertions": "Exact successful status/output, compositor argv and resulting Lua file/tree compared with Bash.",
    },
    {
        "id": "08-webapp-remove",
        "module": "test_08_webapp_remove.py",
        "test_class": "WebappRemoveTests",
        "fixture": "remove nested Space App launcher",
        "assertions": "Exact successful status/output, native command trace and complete owned HOME removal snapshot.",
    },
    {
        "id": "09-audio-output-volume",
        "module": "test_09_audio_volume.py",
        "test_class": "AudioVolumeTests",
        "fixture": "successful default raise from 50 to 55 percent",
        "assertions": "Exact successful status/output, pactl/OSD argv and runtime-file state compared with Bash.",
    },
    {
        "id": "10-theme-set-vscode",
        "module": "test_10_vscode_theme.py",
        "test_class": "VSCodeThemeRewrite",
        "fixture": "no installed editors",
        "assertions": "Exact successful no-op status/output, command order and owned HOME snapshot; JSONC edit divergences are outside this timing set.",
    },
]
_MODULES: dict[tuple[str, str], Any] = {}
_SETUP_CLASSES: set[tuple[str, str]] = set()
LEGACY_CATALOG_MANIFEST = {
    "name": "omarchy.benchmark.legacy-catalog",
    "binary": "unused-benchmark-entry",
    "arguments": [{"literal": "not-invoked"}],
}


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for block in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def percentile_nearest_rank(samples: list[int], percentile: float) -> int:
    if not samples:
        raise ValueError("percentile requires at least one sample")
    if not 0 < percentile <= 1:
        raise ValueError("percentile must be in (0, 1]")
    return sorted(samples)[math.ceil(percentile * len(samples)) - 1]


def summarize(samples: list[int]) -> dict[str, int | float]:
    if not samples:
        raise ValueError("summary requires at least one sample")
    return {
        "sample_count": len(samples),
        "median_ns": statistics.median(samples),
        "p95_nearest_rank_ns": percentile_nearest_rank(samples, 0.95),
        "minimum_ns": min(samples),
        "maximum_ns": max(samples),
    }


def interleaved_order(round_index: int) -> tuple[str, str]:
    return ("baseline", "candidate") if round_index % 2 == 0 else ("candidate", "baseline")


def validate_report_path(path: Path, benchmark_directory: Path = BENCHMARKS) -> Path:
    directory = benchmark_directory.resolve()
    candidate = path if path.is_absolute() else Path.cwd() / path
    candidate = candidate.absolute()
    if candidate.parent.resolve() != directory:
        raise ValueError(f"report must be saved directly under {directory}")
    if not candidate.name.startswith("omarchy-success-") or not candidate.name.endswith(".json"):
        raise ValueError("report name must match omarchy-success-*.json")
    if candidate.exists() or candidate.is_symlink():
        raise FileExistsError(f"refusing to overwrite report: {candidate}")
    return candidate


def _verify_executable(path: Path, label: str) -> Path:
    resolved = path.expanduser().resolve(strict=True)
    if not resolved.is_file() or not os.access(resolved, os.X_OK):
        raise ValueError(f"{label} must be an executable regular file: {resolved}")
    return resolved


def verify_baseline(directory: Path) -> dict[str, Any]:
    root = directory.expanduser().resolve(strict=True)
    if not root.is_dir():
        raise ValueError(f"baseline directory is not a directory: {root}")
    receipt_path = root / "receipt.json"
    receipt = json.loads(receipt_path.read_text(encoding="utf-8"))
    if receipt.get("revision") != BASELINE_REVISION:
        raise ValueError("saved baseline receipt has an unexpected source revision")
    files = receipt.get("files")
    if not isinstance(files, dict):
        raise ValueError("saved baseline receipt has no file hash map")
    programs = {name: digest for name, digest in files.items() if name.endswith(".program")}
    if len(programs) != 10:
        raise ValueError("saved baseline receipt must pin all ten Omarchy programs")
    baseline_binary = _verify_executable(root / "execute", "saved baseline Execute binary")
    baseline_hash = sha256_file(baseline_binary)
    if baseline_hash != BASELINE_EXECUTE_SHA256:
        raise ValueError("saved baseline Execute binary does not match its preserved review hash")
    for name, expected in files.items():
        source = root / name
        if not source.is_file() or sha256_file(source) != expected:
            raise ValueError(f"saved baseline receipt artifact changed or is missing: {name}")
    return {
        "directory": str(root),
        "revision": receipt["revision"],
        "receipt_sha256": sha256_file(receipt_path),
        "execute": {"path": str(baseline_binary), "sha256": baseline_hash},
        "programs": {name: {"sha256": value} for name, value in sorted(programs.items())},
    }


def _current_programs() -> dict[str, Path]:
    programs = sorted((ROOT / "examples/omarchy-rewrites").glob("*.program"))
    if len(programs) != 10:
        raise ValueError("current Omarchy source must contain exactly ten pinned programs")
    by_name = {path.name: path for path in programs}
    return by_name


def _source_files(baseline: Path) -> list[Path]:
    paths = [ROOT / "Cargo.toml", ROOT / "Cargo.lock"]
    paths.extend((ROOT / "src").rglob("*.rs"))
    paths.extend((ROOT / "examples/omarchy-rewrites").glob("*.program"))
    paths.extend((ROOT / "manifests").glob("omarchy*.json"))
    paths.extend((ROOT / "tests/omarchy_rewrites").glob("test_*.py"))
    paths.extend((ROOT / "tests/fixtures/omarchy-rewrites").rglob("*"))
    paths.extend([ROOT / "benchmarks/omarchy_success.py"])
    paths.extend(baseline.glob("*.program"))
    paths.append(baseline / "receipt.json")
    return sorted(path for path in set(paths) if path.is_file())


def source_hashes(baseline: Path) -> dict[str, str]:
    results: dict[str, str] = {}
    for path in _source_files(baseline):
        try:
            name = path.relative_to(ROOT).as_posix()
        except ValueError:
            name = "saved-baseline/" + path.name
        results[name] = sha256_file(path)
    return results


def combined_source_hash(files: dict[str, str]) -> str:
    digest = hashlib.sha256()
    for name, content_hash in sorted(files.items()):
        digest.update(name.encode("utf-8"))
        digest.update(b"\0")
        digest.update(content_hash.encode("ascii"))
        digest.update(b"\n")
    return digest.hexdigest()


def make_legacy_catalog(root: Path) -> tuple[Path, str]:
    """Create the tiny pre-contract catalog needed by the preserved binary.

    The saved baseline programs use direct quoted commands, while the current
    acceptance fixtures pass a manifest directory to both binaries. Its new
    v1 manifests are intentionally not parseable by the older binary, so the
    baseline gets one inert legacy-shaped entry and the candidate gets the
    real current catalog.
    """
    manifest_dir = root / "manifests"
    manifest_dir.mkdir(parents=True, exist_ok=True)
    payload = (json.dumps(LEGACY_CATALOG_MANIFEST, sort_keys=True, separators=(",", ":")) + "\n").encode("utf-8")
    (manifest_dir / "legacy-catalog.json").write_bytes(payload)
    return manifest_dir, hashlib.sha256(payload).hexdigest()


def legacy_catalog_sha256() -> str:
    payload = (json.dumps(LEGACY_CATALOG_MANIFEST, sort_keys=True, separators=(",", ":")) + "\n").encode("utf-8")
    return hashlib.sha256(payload).hexdigest()


def _load_module(path: Path, identifier: str) -> Any:
    spec = importlib.util.spec_from_file_location(identifier, path)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"cannot import owned differential fixture: {path}")
    module = importlib.util.module_from_spec(spec)
    sys.modules[identifier] = module
    spec.loader.exec_module(module)
    return module


def _set_target_globals(module: Any, binary: Path, program: Path) -> None:
    module.PROGRAM = program
    if hasattr(module, "EXECUTE"):
        module.EXECUTE = binary
    if hasattr(module, "BINARY"):
        module.BINARY = binary


def _prepare_fixture(scenario: dict[str, str], side: str, binary: Path, program: Path, evidence: Path) -> tuple[Any, Any]:
    key = (scenario["id"], side)
    module = _MODULES.get(key)
    if module is None:
        module_path = ROOT / "tests/omarchy_rewrites" / scenario["module"]
        identifier = f"omarchy_success_{scenario['id'].replace('-', '_')}_{side}"
        module = _load_module(module_path, identifier)
        _MODULES[key] = module
    _set_target_globals(module, binary, program)
    test_class = getattr(module, scenario["test_class"])
    if scenario["id"] == "05-theme-set-browser":
        test_class.binary = binary
    if scenario["id"] == "10-theme-set-vscode":
        test_class.binary = binary
        module.EVIDENCE = evidence / "10"
    if key not in _SETUP_CLASSES:
        if hasattr(test_class, "setUpClass"):
            test_class.setUpClass()
        _SETUP_CLASSES.add(key)
    return module, test_class


def _run_scenario_body(scenario_id: str, module: Any, test_class: Any) -> None:
    instance = test_class("runTest")
    needs_setup = scenario_id in {
        "01-notification-time", "04-power-present", "06-default-browser",
        "07-workspace-layout-toggle", "09-audio-output-volume",
    }
    setup_started = False
    try:
        if needs_setup:
            setup_started = True
            instance.setUp()
        if scenario_id == "01-notification-time":
            instance.compare(notification_stdout="recorded\n")
        elif scenario_id == "02-theme-current":
            instance.run_case(test_class.cases[0])
        elif scenario_id == "03-update-analyze-logs":
            instance.differential("success.log", [module.UPDATE, module.SUCCESS])
        elif scenario_id == "04-power-present":
            instance.supply()
            instance.compare(0)
        elif scenario_id == "05-theme-set-browser":
            pair = instance.invoke_pair("250,249,246\n", {"policy_stdout": "policy written\n"})
            instance.assert_parity(pair, "faf9f6")
        elif scenario_id == "06-default-browser":
            instance.compare(args=("firefox",))
        elif scenario_id == "07-workspace-layout-toggle":
            instance.compare()
        elif scenario_id == "08-webapp-remove":
            instance.case(
                arguments=("Space", "App"),
                expected_name="Space App",
                expected_removed=instance.removals(
                    "Space App", "space-app", "nested/Space App.desktop"
                ),
                expected_launcher="nested/Space App.desktop",
            )
        elif scenario_id == "09-audio-output-volume":
            instance.compare()
        elif scenario_id == "10-theme-set-vscode":
            pair = instance.pair("absent")
            instance.assert_pair(pair)
        else:
            raise ValueError(f"unknown benchmark scenario {scenario_id}")
    finally:
        if setup_started:
            tear_down = getattr(instance, "tearDown", None)
            if tear_down:
                tear_down()
            instance.doCleanups()


def run_fixture_once(scenario: dict[str, str], side: str, binary: Path, program: Path) -> dict[str, Any]:
    if side not in ("baseline", "candidate"):
        raise ValueError(f"invalid benchmark side: {side}")
    binary = _verify_executable(binary, side + " Execute binary")
    program = program.resolve(strict=True)
    previous_environment = {
        name: os.environ.get(name)
        for name in ("EXECUTE_TEST_BINARY", "OMARCHY_REWRITE_EVIDENCE_DIR", "PYTHONDONTWRITEBYTECODE")
    }
    with tempfile.TemporaryDirectory(prefix="omarchy-success-evidence-") as evidence_dir:
        evidence = Path(evidence_dir)
        os.environ["EXECUTE_TEST_BINARY"] = str(binary)
        os.environ["OMARCHY_REWRITE_EVIDENCE_DIR"] = str(evidence)
        os.environ["PYTHONDONTWRITEBYTECODE"] = "1"
        try:
            module, test_class = _prepare_fixture(scenario, side, binary, program, evidence)
            original_module_root = getattr(module, "ROOT", None)
            legacy_catalog_sha256: str | None = None
            if side == "baseline" and scenario["id"] in {
                "05-theme-set-browser", "10-theme-set-vscode"
            }:
                legacy_root = evidence / "legacy-baseline"
                _, legacy_catalog_sha256 = make_legacy_catalog(legacy_root)
                module.ROOT = legacy_root
            original_run = subprocess.run
            captures: list[dict[str, Any]] = []

            def measured_run(command: Any, *args: Any, **kwargs: Any) -> Any:
                if isinstance(command, (list, tuple)) and len(command) >= 2:
                    try:
                        invoked_binary = Path(os.fspath(command[0])).resolve()
                        invoked_program = Path(os.fspath(command[1])).resolve()
                    except (TypeError, OSError):
                        invoked_binary = None
                        invoked_program = None
                    if invoked_binary == binary and invoked_program == program:
                        start = time.perf_counter_ns()
                        completed = original_run(command, *args, **kwargs)
                        elapsed = time.perf_counter_ns() - start
                        captures.append(
                            {
                                "elapsed_ns": elapsed,
                                "status": completed.returncode,
                                "stdout": completed.stdout if isinstance(completed.stdout, bytes) else str(completed.stdout).encode(),
                                "stderr": completed.stderr if isinstance(completed.stderr, bytes) else str(completed.stderr).encode(),
                            }
                        )
                        return completed
                return original_run(command, *args, **kwargs)

            try:
                subprocess.run = measured_run  # type: ignore[assignment]
                _run_scenario_body(scenario["id"], module, test_class)
            finally:
                subprocess.run = original_run  # type: ignore[assignment]
                if original_module_root is not None:
                    module.ROOT = original_module_root
            if len(captures) != 1:
                raise AssertionError(
                    f"{scenario['id']} {side} fixture executed {len(captures)} measured calls; expected exactly one"
                )
            capture = captures[0]
            return {
                "elapsed_ns": capture["elapsed_ns"],
                "status": capture["status"],
                "stdout_b64": base64.b64encode(capture["stdout"]).decode("ascii"),
                "stderr_b64": base64.b64encode(capture["stderr"]).decode("ascii"),
                "stdout_sha256": hashlib.sha256(capture["stdout"]).hexdigest(),
                "stderr_sha256": hashlib.sha256(capture["stderr"]).hexdigest(),
                "baseline_compatibility_manifest_sha256": legacy_catalog_sha256,
                "differential_assertions": "passed",
            }
        finally:
            for name, value in previous_environment.items():
                if value is None:
                    os.environ.pop(name, None)
                else:
                    os.environ[name] = value


def assert_success_parity(baseline: dict[str, Any], candidate: dict[str, Any], scenario_id: str) -> None:
    for side, observation in (("baseline", baseline), ("candidate", candidate)):
        if observation["status"] != 0:
            raise AssertionError(f"{scenario_id} {side} did not exit successfully: {observation['status']}")
    for key in ("status", "stdout_b64", "stderr_b64"):
        if baseline[key] != candidate[key]:
            raise AssertionError(f"{scenario_id} baseline and candidate differ in {key}")


def _environment() -> dict[str, Any]:
    return {
        "os": platform.system(),
        "os_release": platform.release(),
        "platform": platform.platform(),
        "architecture": platform.machine(),
        "processor": platform.processor() or None,
        "logical_cpu_count": os.cpu_count(),
        "python": sys.version.splitlines()[0],
        "bash": shutil.which("bash"),
        "locale": {name: os.environ.get(name) for name in ("LANG", "LC_ALL", "TZ")},
        "path_sha256": hashlib.sha256(os.environ.get("PATH", "").encode()).hexdigest(),
    }


def run_benchmark(args: argparse.Namespace) -> dict[str, Any]:
    if not args.final_release_build_confirmed:
        raise ValueError("pass --final-release-build-confirmed only after the final release build is ready")
    if not args.measurement_window_idle:
        raise ValueError("pass --measurement-window-idle only after builds and tests have stopped")
    if args.samples <= 0 or args.warmups <= 0:
        raise ValueError("samples and warmups must both be positive")
    report_path = validate_report_path(args.report)
    baseline = verify_baseline(args.baseline_dir)
    baseline_binary = Path(baseline["execute"]["path"])
    candidate_binary = _verify_executable(args.candidate_binary, "final candidate Execute binary")
    baseline_root = Path(baseline["directory"])
    candidate_programs = _current_programs()
    baseline_programs = {path.name: path for path in baseline_root.glob("*.program")}
    if set(candidate_programs) != set(baseline_programs):
        raise ValueError("baseline and candidate program inventories do not match")

    initial_hashes = {
        "baseline_execute": sha256_file(baseline_binary),
        "candidate_execute": sha256_file(candidate_binary),
        "sources": source_hashes(baseline_root),
    }
    source_digest = combined_source_hash(initial_hashes["sources"])
    correct_outputs: dict[str, Any] = {}
    for scenario in SCENARIOS:
        old = run_fixture_once(scenario, "baseline", baseline_binary, baseline_programs[scenario["id"] + ".program"])
        new = run_fixture_once(scenario, "candidate", candidate_binary, candidate_programs[scenario["id"] + ".program"])
        assert_success_parity(old, new, scenario["id"])
        correct_outputs[scenario["id"]] = {
            "baseline": {key: old[key] for key in ("status", "stdout_b64", "stderr_b64", "stdout_sha256", "stderr_sha256")},
            "candidate": {key: new[key] for key in ("status", "stdout_b64", "stderr_b64", "stdout_sha256", "stderr_sha256")},
            "bash_oracle_and_effect_assertions": "passed independently for both sides",
            "baseline_compatibility_manifest_sha256": old["baseline_compatibility_manifest_sha256"],
        }

    measurements: dict[str, Any] = {}
    for scenario_index, scenario in enumerate(SCENARIOS):
        program_paths = {
            "baseline": baseline_programs[scenario["id"] + ".program"],
            "candidate": candidate_programs[scenario["id"] + ".program"],
        }
        executables = {"baseline": baseline_binary, "candidate": candidate_binary}
        side_samples: dict[str, list[int]] = {"baseline": [], "candidate": []}
        raw_order: list[dict[str, Any]] = []
        # Every warmup and measured round includes both sides, with the first
        # side alternating; the fixture runs its Bash oracle before the timed
        # Execute call and performs correctness/effects checks on every sample.
        for warmup_index in range(args.warmups):
            order = interleaved_order(warmup_index + scenario_index)
            paired: dict[str, dict[str, Any]] = {}
            for side in order:
                paired[side] = run_fixture_once(scenario, side, executables[side], program_paths[side])
            assert_success_parity(paired["baseline"], paired["candidate"], scenario["id"])
        for sample_index in range(args.samples):
            order = interleaved_order(sample_index + args.warmups + scenario_index)
            paired = {}
            for side in order:
                paired[side] = run_fixture_once(scenario, side, executables[side], program_paths[side])
                side_samples[side].append(paired[side]["elapsed_ns"])
                raw_order.append(
                    {
                        "sample_index": sample_index,
                        "side": side,
                        "elapsed_ns": paired[side]["elapsed_ns"],
                        "status": paired[side]["status"],
                        "stdout_sha256": paired[side]["stdout_sha256"],
                        "stderr_sha256": paired[side]["stderr_sha256"],
                    }
                )
            assert_success_parity(paired["baseline"], paired["candidate"], scenario["id"])
        measurements[scenario["id"]] = {
            "fixture": scenario["fixture"],
            "oracle_assertions": scenario["assertions"],
            "timed_boundary": "one Execute subprocess only; fixture setup and Bash oracle are excluded",
            "warmups_per_side": args.warmups,
            "samples_per_side": args.samples,
            "summary_ns": {side: summarize(values) for side, values in side_samples.items()},
            "raw_interleaved_samples": raw_order,
            "correctness": correct_outputs[scenario["id"]],
        }

    final_hashes = {
        "baseline_execute": sha256_file(baseline_binary),
        "candidate_execute": sha256_file(candidate_binary),
        "sources": source_hashes(baseline_root),
    }
    if final_hashes != initial_hashes:
        raise RuntimeError("baseline, candidate or source files changed during measurement; no report was written")
    return {
        "schema": "execute.omarchy-success-pairs/1",
        "measured_at_utc": datetime.now(timezone.utc).isoformat(),
        "measurement_policy": {
            "final_release_build_confirmed": True,
            "measurement_window_idle_confirmed": True,
            "cargo_invoked": False,
            "correctness_and_effect_assertions_precede_timing": True,
            "fixture_oracle": "Each selected existing differential fixture independently compares its implementation with pinned Bash and asserts output/status plus recorded effects.",
            "pairing": "Baseline and candidate are both sampled in each round; first side alternates by round and workload.",
            "timed_boundary": "Only the selected Execute subprocess wall time; fixture construction, Bash oracle and effect assertions are excluded.",
            "statistics": "median and nearest-rank p95; every measured nanosecond sample retained",
            "claim_boundary": "Only these ten successful owned fixtures; intentional behavior changes and correctness improvements are excluded, and no universal speed claim follows.",
            "legacy_baseline_catalog": "For the two fixture adapters that always pass --manifest-dir, the old direct-call program receives one inert legacy-shaped manifest; the candidate receives the current manifest directory.",
        },
        "source": {
            "git_revision": _git_revision(),
            "working_tree_status": "dirty" if _git_status() else "clean",
            "combined_source_sha256": source_digest,
            "files_sha256": initial_hashes["sources"],
        },
        "baseline": baseline,
        "baseline_compatibility_manifest_sha256": legacy_catalog_sha256(),
        "candidate": {
            "execute": {"path": str(candidate_binary), "sha256": initial_hashes["candidate_execute"]},
            "programs": {
                path.name: {"path": str(path), "sha256": sha256_file(path)}
                for path in sorted(candidate_programs.values())
            },
        },
        "environment": _environment(),
        "sample_counts": {"warmups_per_side_per_scenario": args.warmups, "measured_per_side_per_scenario": args.samples},
        "scenarios": measurements,
    }


def _git_revision() -> str | None:
    git = shutil.which("git")
    if not git:
        return None
    result = subprocess.run([git, "-C", str(ROOT), "rev-parse", "HEAD"], capture_output=True, text=True, check=False)
    return result.stdout.strip() if result.returncode == 0 else None


def _git_status() -> str | None:
    git = shutil.which("git")
    if not git:
        return None
    result = subprocess.run([git, "-C", str(ROOT), "status", "--porcelain"], capture_output=True, text=True, check=False)
    return result.stdout.strip() if result.returncode == 0 else None


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--baseline-dir", type=Path, default=DEFAULT_BASELINE, help="preserved baseline directory with receipt.json, execute and ten programs")
    parser.add_argument("--candidate-binary", type=Path, required=True, help="final release candidate Execute binary")
    parser.add_argument("--report", type=Path, required=True, help="fresh benchmarks/omarchy-success-<label>.json output path")
    parser.add_argument("--samples", type=int, default=DEFAULT_SAMPLES)
    parser.add_argument("--warmups", type=int, default=DEFAULT_WARMUPS)
    parser.add_argument("--final-release-build-confirmed", action="store_true", help="root-task release gate: confirm this exact candidate build is ready")
    parser.add_argument("--measurement-window-idle", action="store_true", help="confirm builds, tests and other benchmarks have stopped")
    args = parser.parse_args()
    try:
        report = run_benchmark(args)
        destination = validate_report_path(args.report)
        payload = (json.dumps(report, indent=2, sort_keys=True) + "\n").encode("utf-8")
        descriptor = os.open(destination, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        with os.fdopen(descriptor, "wb") as output:
            output.write(payload)
            output.flush()
            os.fsync(output.fileno())
        print(f"Wrote {destination}")
        return 0
    except (AssertionError, FileExistsError, OSError, RuntimeError, ValueError, json.JSONDecodeError) as error:
        print(f"Omarchy success benchmark failed: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
