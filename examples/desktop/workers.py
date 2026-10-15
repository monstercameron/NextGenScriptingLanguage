#!/usr/bin/env python3
"""Tiny persistent helpers for the renderer portability demonstration."""

from __future__ import annotations

import json
import sys
import time


def arguments_for(request: dict[str, object]) -> dict[str, object]:
    encoded = request["arguments"][-1]
    if not isinstance(encoded, str):
        raise ValueError("expected the applet JSON argument map")
    value = json.loads(encoded)
    if not isinstance(value, dict):
        raise ValueError("applet arguments must be an object")
    return value


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
    for line in sys.stdin.buffer:
        request_id: object = None
        try:
            request = json.loads(line)
            request_id = request["id"]
            output = json.dumps(evaluate(mode, arguments_for(request)), separators=(",", ":"))
            response = {"id": request_id, "status": 0, "output": list(output.encode("utf-8"))}
        except Exception as error:  # Return a bounded protocol error to this request.
            response = {
                "id": request_id,
                "status": 2,
                "output": list(b"{}"),
                "stderr": str(error)[:1024],
            }
        sys.stdout.buffer.write(json.dumps(response, separators=(",", ":")).encode("utf-8") + b"\n")
        sys.stdout.buffer.flush()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
