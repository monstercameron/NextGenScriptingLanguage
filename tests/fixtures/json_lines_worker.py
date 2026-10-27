#!/usr/bin/env python3
"""Owned protocol fixture for src/runtime/worker_backend.rs tests."""

import json
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
    response_id = request["id"]
    input_bytes = bytes(request["input"])
    output = list(input_bytes)
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

    response = {
        "id": response_id,
        "status": -17 if mode == "status" else (2 if mode == "secret_failure" else 0),
        "output": output,
    }
    if stderr is not None:
        response["stderr"] = stderr
    sys.stdout.buffer.write(json.dumps(response, separators=(",", ":")).encode() + b"\n")
    sys.stdout.buffer.flush()
