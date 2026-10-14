#!/usr/bin/env python3
"""Paired warm process-launch controls and descriptive offline model overhead."""
import argparse, hashlib, json, os, platform, statistics, subprocess, tempfile, time
from pathlib import Path

parser = argparse.ArgumentParser()
parser.add_argument("--project", type=Path, required=True)
parser.add_argument("--baseline", type=Path, required=True)
parser.add_argument("--report", type=Path, required=True)
parser.add_argument("--samples", type=int, default=80)
args = parser.parse_args()
project = args.project.resolve()
if not 1 <= args.samples <= 10000:
    parser.error("--samples must be between 1 and 10000")
baseline = args.baseline.resolve()
candidate = project / "target/release/execute"
helper = candidate.with_name("execute-model")


def digest(p):
    return hashlib.sha256(p.read_bytes()).hexdigest()


env = {k: v for k, v in os.environ.items() if k in ("PATH", "TMPDIR", "LANG", "LC_ALL")}
env["EXECUTE_MODELS_OFFLINE"] = "1"


def invoke(command, cwd, expected=None):
    start = time.perf_counter_ns()
    r = subprocess.run(
        [str(x) for x in command],
        cwd=cwd,
        env=env,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        timeout=10,
    )
    ns = time.perf_counter_ns() - start
    if r.returncode or r.stderr:
        raise RuntimeError(
            "control failed: status "
            + str(r.returncode)
            + " stderr "
            + r.stderr.decode(errors="replace")
        )
    if expected is not None and r.stdout != expected:
        raise RuntimeError("independent output oracle mismatch")
    return ns, r.stdout


def stats(ns):
    s = sorted(x / 1e6 for x in ns)
    return {
        "median_ms": statistics.median(s),
        "p95_ms": s[min(len(s) - 1, int(0.95 * len(s)))],
        "min_ms": s[0],
        "samples_ms": [x / 1e6 for x in ns],
    }


results = []
with tempfile.TemporaryDirectory(prefix="execute-model-measure-") as raw:
    cwd = Path(raw)
    catalog = cwd / "empty-manifests"
    catalog.mkdir()
    (catalog / "unused.json").write_text(
        json.dumps({"name": "unused", "binary": "true", "arguments": []})
    )
    compatible = cwd / "compatible-manifests"
    compatible.mkdir()
    for original in sorted((project / "manifests").glob("*.json")):
        document = json.loads(original.read_text())
        document.pop("uncertain_on_failure", None)
        (compatible / original.name).write_text(json.dumps(document))
    (cwd / "payload.json").write_text('{"n":7}\n')
    controls = [
        ("native-noop", 'run "true"\n', b""),
        ("native-printf", 'run "printf" with arguments ["ready\\n"]\n', b"ready\n"),
        (
            "native-pipe",
            'run "printf" with arguments ["abc\\n"]\npipe to "tr" with arguments ["a-z", "A-Z"]\n',
            b"ABC\n",
        ),
        (
            "native-json",
            'read "payload.json"\nthen parse json as data\ntake integer at "/n" from data as count\nrun "printf" with arguments ["%s", count]\n',
            b"7",
        ),
    ]
    for name, source, expected in controls:
        script = cwd / (name + ".program")
        script.write_text(source)
        for mode, manifests in [
            ("minimal-one-entry", catalog),
            ("full-compatible-catalog", compatible),
        ]:
            commands = {
                label: [binary, script, "--manifest-dir", manifests]
                for label, binary in [("baseline", baseline), ("candidate", candidate)]
            }
            samples = {label: [] for label in commands}
            for index in range(8 + args.samples):
                order = (
                    ("baseline", "candidate")
                    if index % 2 == 0
                    else ("candidate", "baseline")
                )
                for label in order:
                    elapsed, _ = invoke(commands[label], cwd, expected)
                    if index >= 8:
                        samples[label].append(elapsed)
            results.append(
                {
                    "case": name,
                    "manifest_mode": mode,
                    "source": source,
                    "oracle_hex": expected.hex(),
                    "baseline": stats(samples["baseline"]),
                    "candidate": stats(samples["candidate"]),
                }
            )
    fixture_path = project / "examples/models/fixtures/001/classify-evidence-ready.json"
    fixture = json.loads(fixture_path.read_text())
    request = fixture["expected_request"]
    payload = cwd / "model-state.json"
    payload.write_text(
        json.dumps(request["state"], ensure_ascii=False, separators=(",", ":"))
    )
    helper_args = [
        "classify",
        "--question",
        request["question"],
        "--labels",
        *request["labels"],
        "--minimum",
        str(request["minimum"]),
        "--fixture",
        str(fixture_path),
    ]
    script = cwd / "fixture.program"
    script.write_text(
        "read "
        + json.dumps(str(payload))
        + "\nthen jev.classify with question "
        + json.dumps(request["question"])
        + " and labels "
        + json.dumps(request["labels"])
        + " and minimum "
        + str(request["minimum"])
        + " and fixture "
        + json.dumps(str(fixture_path))
        + "\n"
    )
    # One deterministic result is independently checked against the authored provider response.
    expected_label = fixture["response"]["answers"]["decision"]["choice"]
    for name, command in [
        ("applet", [helper, *helper_args]),
        ("cli-alias", [candidate, "model", *helper_args]),
        ("program", [candidate, script, "--manifest-dir", project / "manifests"]),
    ]:
        ns = []
        for i in range(8 + args.samples):
            start = time.perf_counter_ns()
            with payload.open("rb") as incoming:
                r = subprocess.run(
                    [str(x) for x in command],
                    cwd=cwd,
                    env=env,
                    stdin=incoming,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.PIPE,
                    timeout=10,
                )
            elapsed = time.perf_counter_ns() - start
            if r.returncode or r.stderr:
                raise RuntimeError("fixture call failed")
            value = json.loads(r.stdout)
            if not (
                value["protocol"] == "execute.model/1"
                and value["provider"] == "jev"
                and value["kind"] == "classify"
                and value["transport"] == "fixture"
                and value["label"] == expected_label
                and value["accepted"] is True
            ):
                raise RuntimeError("model fixture oracle mismatch")
            if i >= 8:
                ns.append(elapsed)
        results.append(
            {"case": "fixture-" + name, "descriptive_only": True, "timing": stats(ns)}
        )
report = {
    "version": 1,
    "platform": platform.platform(),
    "timestamp_utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
    "baseline_sha256": digest(baseline),
    "candidate_sha256": digest(candidate),
    "helper_sha256": digest(helper),
    "harness_sha256": digest(Path(__file__)),
    "samples": args.samples,
    "warmups_per_variant": 8,
    "method": "Warm process-launch elapsed time including Python subprocess launch/wait. Alternating baseline/candidate per native sample. Exact independent stdout/status oracles. Same manifest catalog on both binaries. Full-compatible catalog copies current manifests and removes uncertain_on_failure because the baseline parser rejects that new field; these controls invoke no models. This isolates ordinary runtime overhead, not the cost of differing production catalogs. Fixture-only model measurements are descriptive, not paired or provider inference latency. No speedup claim.",
    "results": results,
}
args.report.write_text(json.dumps(report, indent=2) + "\n")
print(
    json.dumps(
        [
            {
                "case": r["case"],
                "manifest_mode": r.get("manifest_mode"),
                "baseline_ms": r.get("baseline", {}).get("median_ms"),
                "candidate_ms": r.get("candidate", r.get("timing", {})).get(
                    "median_ms"
                ),
            }
            for r in results
        ],
        indent=2,
    )
)
