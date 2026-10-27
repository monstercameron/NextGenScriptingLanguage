#!/usr/bin/env python3
"""Owned protocol fixture for src/runtime/worker_backend.rs tests."""

import json
import base64
import os
import sys
import time


if len(sys.argv) < 4 or sys.argv[1] != "--worker":
    raise SystemExit("fixture expects --worker MODE MARKER [EFFECTS]")

mode = sys.argv[2]
marker = sys.argv[3]
effects = sys.argv[4] if len(sys.argv) > 4 else marker + ".effects"
with open(marker, "a", encoding="ascii") as record:
    record.write(f"{os.getpid()}\n")

sequence = 0
remembered = b""
for line in sys.stdin.buffer:
    request = json.loads(line)
    if request.get("type") == "handshake":
        response = {
            "type": "handshake",
            "protocol": "ngsl.json-lines-worker",
            "version": 2,
        }
        sys.stdout.buffer.write(json.dumps(response, separators=(",", ":")).encode() + b"\n")
        sys.stdout.buffer.flush()
        continue
    protocol_v2 = request.get("version") == 2
    response_id = request["id"]
    input_bytes = (
        base64.b64decode(request["input_base64"], validate=True)
        if protocol_v2
        else bytes(request["input"])
    )
    output = input_bytes if protocol_v2 else list(input_bytes)
    stderr = None

    if mode == "echo_arguments":
        output = list("|".join(request["arguments"]).encode("utf-8"))
    elif mode == "echo_both":
        output = list(input_bytes + b"|" + "|".join(request["arguments"]).encode("utf-8"))
    elif mode == "sequence":
        sequence += 1
        output = list(str(sequence).encode("ascii"))
    elif mode == "remember_first":
        if input_bytes:
            remembered = input_bytes
        output = list(remembered)
    elif mode == "remember_first_pause":
        if input_bytes:
            remembered = input_bytes
        with open(effects, "a", encoding="ascii") as record:
            record.write("received\n")
        time.sleep(0.3)
        output = list(remembered)
    elif mode in ("large_output", "large_output_effect"):
        if mode == "large_output_effect":
            with open(effects, "a", encoding="ascii") as record:
                record.write("effect\n")
        output = list(b"x" * 256)
    elif mode == "large_stderr":
        stderr = "e" * 128
    elif mode in ("secret_stderr", "secret_failure"):
        secret = os.environ["FLOW_SECRET"].encode("utf-8")
        output = list(secret)
        stderr = secret.decode("utf-8")
    elif mode == "status":
        output = [0, 255, 1]
        stderr = "é: diagnostic"
    elif mode == "typed_json":
        output = b'{"count":7,"label":"typed"}'
    elif mode == "echo_value":
        output = json.dumps(
            request.get("value", {}).get("payload", {}),
            separators=(",", ":"),
        ).encode("utf-8")
    elif mode == "wrong_id":
        response_id += 1
    elif mode == "eof":
        raise SystemExit(0)
    elif mode == "partial":
        sys.stdout.buffer.write(b'{"id":')
        sys.stdout.buffer.flush()
        raise SystemExit(0)
    elif mode == "invalid_json":
        sys.stdout.buffer.write(b"{invalid json}\n")
        sys.stdout.buffer.flush()
        raise SystemExit(0)
    elif mode == "slow":
        with open(effects, "a", encoding="ascii") as record:
            record.write("received\n")
        time.sleep(10)
    elif mode == "effect_then_eof":
        with open(effects, "a", encoding="ascii") as record:
            record.write("effect\n")
        raise SystemExit(0)

    status = -17 if mode == "status" else (2 if mode == "secret_failure" else 0)
    output_bytes = bytes(output) if isinstance(output, list) else output
    if protocol_v2:
        if mode == "duplicate_json":
            duplicate_response = (
                '{"version":2,"id":%d,"status":0,"value":'
                '{"count":1,"count":2,"label":"typed"}}\n'
            ) % response_id
            sys.stdout.buffer.write(duplicate_response.encode("ascii"))
            sys.stdout.buffer.flush()
            continue
        response = {
            "version": 2,
            "id": response_id,
            "status": status,
        }
        if request["json_output"]:
            response["value"] = json.loads(output_bytes)
        else:
            response["output_base64"] = base64.b64encode(output_bytes).decode("ascii")
    else:
        response = {
            "id": response_id,
            "status": status,
            "output": list(output),
        }
    if stderr is not None:
        response["stderr"] = stderr
    sys.stdout.buffer.write(json.dumps(response, separators=(",", ":")).encode() + b"\n")
    sys.stdout.buffer.flush()
