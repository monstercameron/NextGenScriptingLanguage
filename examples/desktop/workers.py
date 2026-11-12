#!/usr/bin/env python3
"""Worker SDK for JSON-lines applet workers, and the renderer portability demonstration.

Protocol (frame version 1, one JSON object per line on stdin and stdout):

    request:  {"id": <int>, "arguments": [<str>, ...], "input": [<byte>, ...]}
    response: {"id": <int>, "status": <int>, "output": [<byte>, ...], "stderr": <str, optional>}

The host passes the applet argument map as a JSON object string in the last argument.
Status 0 means success and `output` holds the UTF-8 JSON result. Status 2 means the
request was rejected and `stderr` holds a bounded message. A worker answers every
request line, in order, and writes nothing else to stdout.

To write a worker, build a handler that takes the decoded argument map and returns a
JSON-serializable value, then call `serve(handler, sys.stdin.buffer, sys.stdout.buffer)`.
Raise `ValueError` (or `WorkerRequestError`) to reject a request with a message.
"""

from __future__ import annotations

import json
import sys
import time
from collections.abc import Callable
from typing import BinaryIO

STATUS_OK = 0
STATUS_REJECTED = 2
MAX_STDERR_BYTES = 1024

Handler = Callable[[dict[str, object]], object]


class WorkerRequestError(ValueError):
    """The request is well framed but cannot be served. Sent as status 2 with the message."""


class WorkerProtocolError(Exception):
    """A request line is not a valid frame. Sent as status 2 with a null id."""


def decode_arguments(request: dict[str, object]) -> dict[str, object]:
    arguments = request.get("arguments")
    if not isinstance(arguments, list) or not arguments:
        raise WorkerRequestError("request must carry at least one argument")
    encoded = arguments[-1]
    if not isinstance(encoded, str):
        raise WorkerRequestError("expected the applet JSON argument map")
    try:
        value = json.loads(encoded)
    except ValueError as error:
        raise WorkerRequestError(f"applet arguments are not JSON: {error}") from None
    if not isinstance(value, dict):
        raise WorkerRequestError("applet arguments must be an object")
    return value


def answer(line: bytes, handler: Handler) -> dict[str, object]:
    """Return the response object for one request line. Never raises."""
    request_id: object = None
    try:
        request = json.loads(line)
        if not isinstance(request, dict) or "id" not in request:
            raise WorkerProtocolError("request frame must be an object with an id")
        request_id = request["id"]
        output = json.dumps(handler(decode_arguments(request)), separators=(",", ":"))
        return {"id": request_id, "status": STATUS_OK, "output": list(output.encode("utf-8"))}
    except Exception as error:  # Reject this request only; the worker keeps serving.
        message = str(error).encode("utf-8")[:MAX_STDERR_BYTES].decode("utf-8", "ignore")
        return {
            "id": request_id,
            "status": STATUS_REJECTED,
            "output": list(b"{}"),
            "stderr": message,
        }


def serve(handler: Handler, reader: BinaryIO, writer: BinaryIO) -> int:
    for line in reader:
        response = answer(line, handler)
        writer.write(json.dumps(response, separators=(",", ":")).encode("utf-8") + b"\n")
        writer.flush()
    return 0


def evaluate(mode: str, arguments: dict[str, object]) -> object:
    if mode == "clock_local":
        if not isinstance(arguments.get("pulse"), int):
            raise ValueError("pulse must be an integer")
        return int(time.time())
    if mode == "clock_format":
        seconds = arguments.get("seconds")
        if not isinstance(seconds, int):
            raise ValueError("seconds must be an integer")
        day_seconds = seconds % 86_400
        hours, remainder = divmod(day_seconds, 3_600)
        minutes, second = divmod(remainder, 60)
        return {"text": f"{hours:02d}:{minutes:02d}:{second:02d} UTC"}
    raise ValueError(f"unsupported worker mode {mode!r}")


def main() -> int:
    if len(sys.argv) != 3 or sys.argv[1] != "--worker":
        print("expected --worker MODE", file=sys.stderr)
        return 2
    mode = sys.argv[2]
    return serve(lambda arguments: evaluate(mode, arguments), sys.stdin.buffer, sys.stdout.buffer)


if __name__ == "__main__":
    raise SystemExit(main())
