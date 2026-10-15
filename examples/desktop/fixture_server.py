#!/usr/bin/env python3
"""Loopback-only execute.applet/1 fixture for the desktop demo."""

from __future__ import annotations

import json
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any


class FixtureServer(ThreadingHTTPServer):
    daemon_threads = True

    def __init__(self, address: tuple[str, int]):
        super().__init__(address, Handler)
        self.sync_count = 0


class Handler(BaseHTTPRequestHandler):
    server: FixtureServer
    protocol_version = "HTTP/1.1"

    def do_POST(self) -> None:
        if self.path != "/invoke":
            self.send_error(404)
            return
        try:
            length = int(self.headers.get("Content-Length", "0"))
            if not 0 <= length <= 1_048_576:
                self.send_error(413)
                return
            request = json.loads(self.rfile.read(length))
            operation = request["operation"]
            arguments = request.get("arguments", {})
            if operation == "desktop.clock_remote":
                if not isinstance(arguments.get("pulse"), int):
                    self.send_error(400)
                    return
                value: Any = int(time.time())
            elif operation == "desktop.fixture_sync":
                click = arguments.get("click")
                if click != {}:
                    self.send_error(400)
                    return
                self.server.sync_count += 1
                value = {"synced": True, "message": "local fixture accepted sync"}
            else:
                self.send_error(404)
                return
            response = {
                "protocol_version": 1,
                "request_id": request["request_id"],
                "status": 0,
                "outputs": {"value": {"encoding": "json", "data": value}},
            }
            encoded = json.dumps(response, separators=(",", ":")).encode("utf-8")
            self.send_response(200)
            self.send_header("Content-Type", "application/vnd.execute.applet+json;version=1")
            self.send_header("Content-Length", str(len(encoded)))
            self.end_headers()
            self.wfile.write(encoded)
        except (KeyError, TypeError, ValueError, json.JSONDecodeError):
            self.send_error(400)

    def log_message(self, format: str, *args: object) -> None:
        return


def main() -> None:
    server = FixtureServer(("127.0.0.1", 0))
    print(server.server_address[1], flush=True)
    try:
        server.serve_forever(poll_interval=0.1)
    finally:
        server.server_close()


if __name__ == "__main__":
    main()
