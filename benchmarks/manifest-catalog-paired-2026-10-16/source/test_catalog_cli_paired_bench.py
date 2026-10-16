import base64
import importlib.util
import json
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).with_name("catalog_cli_paired_bench.py")
SPEC = importlib.util.spec_from_file_location("catalog_cli_paired_bench", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
BENCH = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(BENCH)


class JournalTests(unittest.TestCase):
    def test_flush_retains_raw_bytes_and_failed_exit_status(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "raw.jsonl"
            with path.open("x", encoding="utf-8", newline="\n") as journal:
                BENCH.write_journal_record(journal, {
                    "kind": "invocation",
                    "returncode": 17,
                    "parse_error": "exit status 17",
                    "execution_error": None,
                    "stdout": b"partial\x00out",
                    "stderr": b"failed\xff",
                })
                # The line is already available while the writer remains open.
                record = json.loads(path.read_text(encoding="utf-8"))
                self.assertEqual(record["returncode"], 17)
                self.assertEqual(record["parse_error"], "exit status 17")
                self.assertEqual(base64.b64decode(record["stdout_base64"]), b"partial\x00out")
                self.assertEqual(base64.b64decode(record["stderr_base64"]), b"failed\xff")
            with self.assertRaises(FileExistsError):
                path.open("x", encoding="utf-8")


if __name__ == "__main__":
    unittest.main()
