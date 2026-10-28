#!/usr/bin/env python3
"""Prove the latency gate rejects a startup regression.

Two layers. The synthetic layer checks the gate math with a script binary: a
500 ms floor plus a 100 ms delay must fail a 575 ms budget (a 20% regression
against 15% headroom). The real-binary layer builds the `failpoints`
test-only binary and proves the `EXECUTE_LATENCY_TEST_DELAY_MS` hook delays
that binary while the release binary ignores the variable, then runs the real
`tools/check_latency.py` gate against the delayed binary and requires it to
fail. The hook is compiled in only with the `failpoints` feature, which no
default, full, or release build enables.
"""
import json
import os
import shutil
import statistics
import subprocess
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]

DELAY_MS = "500"
# The failpoints debug floor is a few milliseconds; 100 ms is far above its
# noise but far below the injected delay, so only the hook can trip it.
CANARY_VERSION_BUDGET_MS = 100
GENERIOUS_BUDGET_MS = 60000


def run_gate(binary, budget, environment, runs="2"):
    return subprocess.run(
        [
            sys.executable,
            str(ROOT / "tools/check_latency.py"),
            str(binary),
            str(budget),
            "--runs",
            runs,
            "--warmup",
            "0",
        ],
        cwd=ROOT,
        env=environment,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        check=False,
    )


def synthetic_gate_math():
    with tempfile.TemporaryDirectory(prefix="execute-latency-regression-") as directory:
        root = Path(directory)
        binary = root / "synthetic-execute"
        binary.write_text(
            "#!/usr/bin/env python3\n"
            "import os, sys, time\n"
            "delay = 0.5\n"
            "if sys.argv[1:] == ['--version']:\n"
            "    delay += float(os.environ.get('EXECUTE_LATENCY_TEST_DELAY_MS', '0')) / 1000\n"
            "time.sleep(delay)\n",
            encoding="utf-8",
        )
        binary.chmod(0o755)

        # Baseline is 500 ms. The extra 100 ms is a 20% startup regression,
        # while the budget grants 15% headroom (575 ms).
        budget = root / "budget.json"
        budget.write_text(
            json.dumps(
                {
                    "version_median_ms": 575,
                    "one_line_median_ms": 1000,
                    "ten_step_median_ms": 1000,
                    "hot_loop_median_ms": 1000,
                }
            ),
            encoding="utf-8",
        )
        environment = dict(os.environ, EXECUTE_LATENCY_TEST_DELAY_MS="100")
        result = run_gate(binary, budget, environment, runs="1")
        if result.returncode == 0:
            raise AssertionError("latency gate accepted a synthetic 20% startup regression")
        if "process floor (--version) median" not in result.stdout or "exceeds the 575 ms budget" not in result.stdout:
            raise AssertionError(f"latency gate failed for an unexpected reason:\n{result.stdout}")
    print("synthetic 20% startup regression rejected by latency budget")


def find_cargo():
    cargo = shutil.which("cargo")
    if cargo:
        return cargo
    fallback = Path.home() / ".cargo" / "bin" / "cargo"
    if fallback.is_file():
        return str(fallback)
    raise AssertionError(
        "cargo is required to build the failpoints canary binary but was not found on PATH"
    )


def build_failpoints_binary():
    cargo = find_cargo()
    result = subprocess.run(
        [cargo, "build", "--locked", "--features", "failpoints", "--bin", "execute"],
        cwd=ROOT,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        check=False,
    )
    if result.returncode != 0:
        raise AssertionError(f"failpoints canary build failed:\n{result.stdout}")
    binary = ROOT / "target" / "debug" / "execute"
    if not binary.is_file():
        raise AssertionError("failpoints canary build produced no target/debug/execute")
    return binary


def generous_budget(path):
    path.write_text(
        json.dumps(
            {
                "version_median_ms": GENERIOUS_BUDGET_MS,
                "one_line_median_ms": GENERIOUS_BUDGET_MS,
                "ten_step_median_ms": GENERIOUS_BUDGET_MS,
                "hot_loop_median_ms": GENERIOUS_BUDGET_MS,
            }
        ),
        encoding="utf-8",
    )


def real_binary_canary():
    binary = build_failpoints_binary()
    clean_environment = dict(os.environ)
    clean_environment.pop("EXECUTE_LATENCY_TEST_DELAY_MS", None)
    with tempfile.TemporaryDirectory(prefix="execute-latency-canary-") as directory:
        root = Path(directory)
        budget = root / "budget.json"
        generous_budget(budget)
        control = run_gate(binary, budget, clean_environment)
        if control.returncode != 0:
            raise AssertionError(
                "latency gate failed without any injected delay:\n" f"{control.stdout}"
            )

        budget.write_text(
            json.dumps(
                {
                    "version_median_ms": CANARY_VERSION_BUDGET_MS,
                    "one_line_median_ms": GENERIOUS_BUDGET_MS,
                    "ten_step_median_ms": GENERIOUS_BUDGET_MS,
                    "hot_loop_median_ms": GENERIOUS_BUDGET_MS,
                }
            ),
            encoding="utf-8",
        )
        delayed_environment = dict(clean_environment, EXECUTE_LATENCY_TEST_DELAY_MS=DELAY_MS)
        result = run_gate(binary, budget, delayed_environment)
        if result.returncode == 0:
            raise AssertionError(
                "latency gate accepted the real binary with a "
                f"{DELAY_MS} ms test-only startup delay"
            )
        expected = f"exceeds the {CANARY_VERSION_BUDGET_MS} ms budget"
        if "process floor (--version) median" not in result.stdout or expected not in result.stdout:
            raise AssertionError(
                "latency gate failed for an unexpected reason:\n" f"{result.stdout}"
            )
    print(
        "real failpoints binary with EXECUTE_LATENCY_TEST_DELAY_MS="
        f"{DELAY_MS} rejected by latency gate"
    )


def release_binary_ignores_hook():
    release = ROOT / "target" / "release" / "execute"
    if not release.is_file():
        print("release binary is not built; skipping release-inertness check")
        return
    samples = []
    environment = dict(os.environ, EXECUTE_LATENCY_TEST_DELAY_MS=DELAY_MS)
    for _ in range(3):
        started = time.perf_counter()
        result = subprocess.run(
            [str(release), "--version"],
            env=environment,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            check=False,
        )
        if result.returncode != 0:
            raise AssertionError("release --version failed with the test variable set")
        samples.append((time.perf_counter() - started) * 1000.0)
    median = statistics.median(samples)
    if median >= CANARY_VERSION_BUDGET_MS:
        raise AssertionError(
            "release binary honors EXECUTE_LATENCY_TEST_DELAY_MS "
            f"(median {median:.0f} ms with the variable set)"
        )
    print(f"release binary ignores EXECUTE_LATENCY_TEST_DELAY_MS (median {median:.0f} ms)")


def main():
    synthetic_gate_math()
    real_binary_canary()
    release_binary_ignores_hook()


if __name__ == "__main__":
    main()
