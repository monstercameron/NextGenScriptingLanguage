#!/usr/bin/env python3
"""Native-process protocol peer for Rust lifecycle tests; never a QML oracle."""

import json
import sys
import time


def main() -> int:
    arguments = sys.argv[1:]
    if arguments[:1] == ["--path"]:
        while True:
            time.sleep(1)
    if (
        len(arguments) != 7
        or arguments[:2] != ["ipc", "--path"]
        or not arguments[2]
        or arguments[3] != "call"
        or arguments[5] != "request"
    ):
        return 2
    request = json.loads(arguments[6])
    body = request["body"]
    if body["kind"] == "negotiate":
        reply = {
            "kind": "negotiated",
            "renderer": "native-process-mock-only",
            "supported": body["supported"],
            "max_frame_bytes": body["max_frame_bytes"],
            "max_widgets": body["max_widgets"],
            "max_queued_events": body["max_queued_events"],
        }
    elif body["kind"] == "poll_events":
        cursor = body["after_cursor"]
        reply = {
            "kind": "event_batch",
            "events": [],
            "next_cursor": cursor,
            "oldest_cursor": cursor + 1,
            "overflow": False,
        }
    else:
        reply = {"kind": "applied"}
    response = {
        "protocol": request["protocol"],
        "version": 1,
        "request_id": request["request_id"],
        "session_id": request["session_id"],
        "generation": request["generation"],
        "revision": request["revision"],
        "body": reply,
    }
    sys.stdout.write(json.dumps(response, separators=(",", ":")))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
