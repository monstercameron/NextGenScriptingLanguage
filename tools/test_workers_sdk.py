#!/usr/bin/env python3
"""Checks for the worker SDK in examples/desktop/workers.py. Standard library only.

Run with: python tools/test_workers_sdk.py
"""

from __future__ import annotations

import importlib.util
import io
import json
import subprocess
import sys
import unittest
from pathlib import Path

WORKERS = Path(__file__).resolve().parent.parent / "examples" / "desktop" / "workers.py"

_spec = importlib.util.spec_from_file_location("desktop_workers", WORKERS)
assert _spec is not None and _spec.loader is not None
workers = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(workers)


def request_line(request_id: object, arguments: object) -> bytes:
    frame = {"id": request_id, "arguments": arguments, "input": []}
    return json.dumps(frame).encode("utf-8") + b"\n"


def decoded_output(response: dict[str, object]) -> object:
    return json.loads(bytes(response["output"]).decode("utf-8"))  # type: ignore[arg-type]


def echo_handler(arguments: dict[str, object]) -> object:
    if "fail" in arguments:
        raise ValueError(str(arguments["fail"]))
    return {"echo": arguments}


class AnswerTests(unittest.TestCase):
    def test_success_is_typed_status_zero_with_json_output(self) -> None:
        line = request_line(7, ["{\"x\": 1}"])
        response = workers.answer(line, echo_handler)
        self.assertEqual(response["id"], 7)
        self.assertEqual(response["status"], workers.STATUS_OK)
        self.assertEqual(decoded_output(response), {"echo": {"x": 1}})

    def test_bad_arguments_are_typed_status_two_with_message(self) -> None:
        for arguments in ([], ["not json"], ["[1, 2]"], [5]):
            with self.subTest(arguments=arguments):
                response = workers.answer(request_line(9, arguments), echo_handler)
                self.assertEqual(response["id"], 9)
                self.assertEqual(response["status"], workers.STATUS_REJECTED)
                self.assertEqual(decoded_output(response), {})
                self.assertIsInstance(response["stderr"], str)
                self.assertTrue(response["stderr"])

    def test_handler_rejection_keeps_the_request_id(self) -> None:
        response = workers.answer(request_line(3, ["{\"fail\": \"no pulse\"}"]), echo_handler)
        self.assertEqual(response["id"], 3)
        self.assertEqual(response["status"], workers.STATUS_REJECTED)
        self.assertEqual(response["stderr"], "no pulse")

    def test_malformed_line_answers_with_null_id(self) -> None:
        for line in (b"not json\n", b"[1, 2]\n", b"{\"arguments\": []}\n"):
            with self.subTest(line=line):
                response = workers.answer(line, echo_handler)
                self.assertIsNone(response["id"])
                self.assertEqual(response["status"], workers.STATUS_REJECTED)

    def test_stderr_is_bounded_to_the_protocol_limit(self) -> None:
        def loud(arguments: dict[str, object]) -> object:
            raise ValueError("é" * 5000)

        response = workers.answer(request_line(1, ["{}"]), loud)
        self.assertLessEqual(len(response["stderr"].encode("utf-8")), workers.MAX_STDERR_BYTES)  # type: ignore[union-attr]


class ServeTests(unittest.TestCase):
    def test_in_process_pipe_answers_each_line_in_order(self) -> None:
        reader = io.BytesIO(
            request_line(1, ["{\"x\": 1}"])
            + b"garbage\n"
            + request_line(2, ["{\"fail\": \"bad\"}"])
        )
        writer = io.BytesIO()
        self.assertEqual(workers.serve(echo_handler, reader, writer), 0)
        responses = [json.loads(line) for line in writer.getvalue().splitlines()]
        self.assertEqual([r["id"] for r in responses], [1, None, 2])
        self.assertEqual([r["status"] for r in responses], [0, 2, 2])


class FixtureWorkerTests(unittest.TestCase):
    """Drive the demo worker as a real child process, the way the host runs it."""

    def run_worker(self, mode: str, lines: bytes) -> list[dict[str, object]]:
        completed = subprocess.run(
            [sys.executable, str(WORKERS), "--worker", mode],
            input=lines,
            capture_output=True,
            timeout=60,
            check=False,
        )
        self.assertEqual(completed.returncode, 0, completed.stderr)
        return [json.loads(line) for line in completed.stdout.splitlines()]

    def test_clock_format_answers_typed_success_and_typed_rejection(self) -> None:
        responses = self.run_worker(
            "clock_format",
            request_line(11, ["{\"seconds\": 3661}"])
            + request_line(12, ["{\"seconds\": \"soon\"}"]),
        )
        self.assertEqual(responses[0]["id"], 11)
        self.assertEqual(responses[0]["status"], workers.STATUS_OK)
        self.assertEqual(decoded_output(responses[0]), {"text": "01:01:01 UTC"})
        self.assertEqual(responses[1]["id"], 12)
        self.assertEqual(responses[1]["status"], workers.STATUS_REJECTED)
        self.assertIn("seconds must be an integer", responses[1]["stderr"])

    def test_clock_local_returns_an_integer_for_a_valid_pulse(self) -> None:
        responses = self.run_worker("clock_local", request_line(4, ["{\"pulse\": 1}"]))
        self.assertEqual(responses[0]["status"], workers.STATUS_OK)
        self.assertIsInstance(decoded_output(responses[0]), int)

    def test_unknown_mode_is_rejected_per_request(self) -> None:
        responses = self.run_worker("no_such_mode", request_line(5, ["{}"]))
        self.assertEqual(responses[0]["id"], 5)
        self.assertEqual(responses[0]["status"], workers.STATUS_REJECTED)
        self.assertIn("unsupported worker mode", responses[0]["stderr"])


if __name__ == "__main__":
    unittest.main()
