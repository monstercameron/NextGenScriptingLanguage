#!/usr/bin/env python3
"""Check that Linux RSS samples come from GNU time, separate from wall timing."""
import shutil
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "tools"))

from check_latency import gnu_time, run_once  # noqa: E402


class LatencyTimingTests(unittest.TestCase):
    def test_gnu_time_rss_invocation_is_not_included_in_wall_time(self):
        with tempfile.TemporaryDirectory(prefix="execute-fake-time-") as temporary:
            wrapper = Path(temporary) / "fake-time"
            wrapper.write_text(
                "#!/usr/bin/env python3\n"
                "import subprocess, sys, time\n"
                "time.sleep(0.7)\n"
                "result = subprocess.run(sys.argv[5:], check=False)\n"
                "with open(sys.argv[4], 'w', encoding='ascii') as output: output.write('2048\\n')\n"
                "sys.exit(result.returncode)\n",
                encoding="utf-8",
            )
            wrapper.chmod(0o755)
            command = [sys.executable, "-c", "import time; time.sleep(0.1)"]
            with patch("check_latency.gnu_time", return_value=str(wrapper)):
                elapsed, rss, returncode, message = run_once(command)

        self.assertEqual(returncode, 0, message)
        self.assertEqual(rss, 2048)
        self.assertLess(elapsed, 500, "the 700 ms GNU-time RSS run must not affect wall time")


@unittest.skipUnless(sys.platform.startswith("linux"), "GNU time RSS validation is Linux-specific")
class LatencyRssTests(unittest.TestCase):
    def test_run_once_matches_gnu_time_and_distinguishes_workloads(self):
        time_binary = gnu_time()
        if not time_binary:
            self.fail("GNU time is required for the Linux RSS gate")

        commands = [
            [sys.executable, "-c", "pass"],
            [sys.executable, "-c", "import time; memory = bytearray(64 * 1024 * 1024); time.sleep(0.05)"],
        ]
        samples = []
        for command in commands:
            _, rss, returncode, message = run_once(command)
            self.assertEqual(returncode, 0, message)
            self.assertIsNotNone(rss, "run_once must report GNU time RSS on Linux")

            with tempfile.NamedTemporaryFile(prefix="execute-rss-reference-") as output:
                reference = subprocess.run(
                    [time_binary, "-f", "%M", "-o", output.name, *command],
                    stdout=subprocess.DEVNULL,
                    stderr=subprocess.PIPE,
                    check=False,
                )
                self.assertEqual(reference.returncode, 0, reference.stderr.decode("utf-8", "replace"))
                output.seek(0)
                reference_rss = float(output.read().decode("ascii").strip().splitlines()[-1])

            self.assertLessEqual(abs(rss - reference_rss) / reference_rss, 0.05)
            samples.append(rss)

        self.assertNotEqual(samples[0], samples[1], "child RSS must distinguish small and memory-heavy workloads")


if __name__ == "__main__":
    unittest.main()
