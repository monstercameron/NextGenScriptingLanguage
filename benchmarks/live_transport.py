#!/usr/bin/env python3
"""Serial live read RPC pairs using identical cwd, environment and exact reply bytes."""
import argparse
from datetime import datetime, timezone
import hashlib
import json
from pathlib import Path
import select
import platform
import statistics
import subprocess
import tempfile
import time

PROJECT = Path(__file__).resolve().parents[1]


def digest(value):
    return hashlib.sha256(value).hexdigest()


def main():
    import socket

    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--baseline", type=Path, required=True)
    parser.add_argument("--candidate", type=Path, required=True)
    parser.add_argument("--report", type=Path, required=True)
    parser.add_argument("--samples", type=int, default=20)
    args = parser.parse_args()
    if not 10 <= args.samples <= 100:
        parser.error("samples must be between 10 and 100")
    binaries = {
        name: getattr(args, name).resolve() for name in ["baseline", "candidate"]
    }
    records = {}
    with tempfile.TemporaryDirectory(prefix="execute-live-rpc-") as temporary:
        root = Path(temporary)
        program = root / "job.program"
        source = "then encode 42 as json\n"
        program.write_text(source)
        address = root / "s"
        environment = {
            "PATH": "/usr/bin:/bin",
            "LC_ALL": "C",
            "LANG": "C",
            "HOME": temporary,
            "TMPDIR": temporary,
        }
        for count in [0, 16]:
            expected = {f"p{i}": "x" * 262144 for i in range(count)}
            raw = {name: [] for name in binaries}
            hashes = set()
            orders = []
            for round_index in range(3):
                order = (
                    list(binaries) if round_index % 2 == 0 else list(reversed(binaries))
                )
                orders.append(order)
                for name in order:
                    child = subprocess.Popen(
                        [
                            str(binaries[name]),
                            "live",
                            "serve",
                            str(program),
                            "--socket",
                            str(address),
                            "--paused",
                            "--manifest-dir",
                            str(PROJECT / "manifests"),
                        ],
                        cwd=root,
                        env=environment,
                        stdin=subprocess.DEVNULL,
                        stdout=subprocess.PIPE,
                        stderr=subprocess.PIPE,
                    )

                    def call(request):
                        encoded = (
                            json.dumps(request, separators=(",", ":")).encode() + b"\n"
                        )
                        start = time.perf_counter_ns()
                        with socket.socket(socket.AF_UNIX) as client:
                            client.settimeout(5)
                            client.connect(str(address))
                            client.sendall(encoded)
                            with client.makefile("rb") as reader:
                                reply = reader.readline(16 * 1024 * 1024 + 1)
                        elapsed = time.perf_counter_ns() - start
                        response = json.loads(reply)
                        if not response["ok"]:
                            raise RuntimeError(response)
                        return response, elapsed, digest(reply)

                    try:
                        if not select.select([child.stdout], [], [], 5)[0]:
                            raise RuntimeError("server startup deadline")
                        if json.loads(child.stdout.readline()).get("event") != "ready":
                            raise RuntimeError("server not ready")
                        for key, value in expected.items():
                            call(
                                {"op": "create", "path": "/user/" + key, "value": value}
                            )
                        batches = []
                        for sample in range(args.samples + 3):
                            response, elapsed, fingerprint = call(
                                {"op": "read", "path": "/user"}
                            )
                            if response["result"] != expected:
                                raise RuntimeError("projected state mismatch")
                            hashes.add(fingerprint)
                            if sample >= 3:
                                batches.append(elapsed)
                        raw[name].append(batches)
                        final, _, _ = call(
                            {"op": "read", "path": "/runtime/completed_native_launches"}
                        )
                        if final["result"] != 0:
                            raise RuntimeError("unexpected native execution")
                        call({"op": "control", "action": "stop"})
                        if child.wait(timeout=5) != 0:
                            raise RuntimeError("server exit failure")
                        tail, error = child.communicate(timeout=2)
                        if (
                            error
                            or json.loads(tail).get("event") != "stopped"
                            or address.exists()
                        ):
                            raise RuntimeError("unclean server shutdown")
                    finally:
                        if child.poll() is None:
                            try:
                                call({"op": "control", "action": "stop"})
                                child.wait(timeout=3)
                            except Exception:
                                child.kill()
                                child.wait(timeout=2)
                        child.stdout.close()
                        child.stderr.close()
            if len(hashes) != 1:
                raise RuntimeError("full reply bytes differ between rounds/variants")
            medians = {
                name: [statistics.median(batch) for batch in rounds]
                for name, rounds in raw.items()
            }
            summary = {
                name: statistics.median(rounds) for name, rounds in medians.items()
            }
            records[str(count * 262144)] = {
                "raw_round_ns": raw,
                "round_p50_ns": medians,
                "median_of_round_p50_ns": summary,
                "candidate_change_percent": (
                    summary["candidate"] / summary["baseline"] - 1
                )
                * 100,
                "full_reply_sha256": hashes.pop(),
                "expected_result_sha256": digest(
                    json.dumps(expected, sort_keys=True, separators=(",", ":")).encode()
                ),
                "round_order": orders,
            }
    report = {
        "recorded_at_utc": datetime.now(timezone.utc).isoformat(),
        "binary_sha256": {
            name: digest(path.read_bytes()) for name, path in binaries.items()
        },
        "harness_sha256": digest(Path(__file__).read_bytes()),
        "platform": platform.platform(),
        "samples_per_round": args.samples,
        "warmups_per_round": 3,
        "request_sha256": digest(b'{"op":"read","path":"/user"}\n'),
        "source": source,
        "source_sha256": digest(source.encode()),
        "method": "Three alternating serial paired warm live server rounds; same source/cwd/explicit nonsecret environment/catalog/input; configured measured reads after3warmups. Socket connect,write,full reply read and close timed; server setup/state loading,Python JSON decoding and exact full reply/state hashes excluded. No native executions/providers. All owned server exits and socket cleanup verified. Background activity uncontrolled; not CLI startup,network throughput or cold/RSS measurement.",
        "results": records,
    }
    args.report.write_text(json.dumps(report, indent=2) + "\n")
    print(
        json.dumps(
            {key: value["candidate_change_percent"] for key, value in records.items()},
            indent=2,
        )
    )


if __name__ == "__main__":
    main()
