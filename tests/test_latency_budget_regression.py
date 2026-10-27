#!/usr/bin/env python3
"""Prove the latency gate rejects a synthetic 20% startup regression."""
import json
import os
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


def main():
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
        result = subprocess.run(
            [
                sys.executable,
                str(ROOT / "tools/check_latency.py"),
                str(binary),
                str(budget),
                "--runs",
                "1",
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
        if result.returncode == 0:
            raise AssertionError("latency gate accepted a synthetic 20% startup regression")
        if "process floor (--version) median" not in result.stdout or "exceeds the 575 ms budget" not in result.stdout:
            raise AssertionError(f"latency gate failed for an unexpected reason:\n{result.stdout}")
    print("synthetic 20% startup regression rejected by latency budget")


if __name__ == "__main__":
    main()
