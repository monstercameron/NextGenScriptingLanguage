#!/usr/bin/env python3
"""Compare Execute with bash (and dash where it applies) on the same jobs.

Each scenario pairs a bash command line with an Execute program that does the same
work, feeds both the same stdin, and reports the median wall time and, on POSIX,
the peak resident set size of each. The report ends with the size of each binary.
Bash runs with `--norc --noprofile`; nothing is cached between runs except what
the OS caches for both.

The script exits 0 after reporting. `--require-win` makes it exit 1 when Execute
loses any scenario, so a CI job can track the goal without blocking on it today.
Python 3.9+, standard library only.
"""
import argparse
import json
import os
import shutil
import statistics
import subprocess
import sys
import tempfile
import time
from dataclasses import dataclass, field
from pathlib import Path

PIPE_BYTES = 100_000_000
TEXT_BYTES = 1_000_000
LOOP_COUNT = 1000
SPAWN_COUNT = 100


@dataclass
class Scenario:
    key: str
    label: str
    bash: str
    program: str
    stdin: str = ""  # name of a generated input file, or empty for no stdin
    runs_divisor: int = 1
    dash: bool = False
    requires: list = field(default_factory=list)  # tools that must be on PATH


def text_input():
    line = b"the quick brown fox jumps over the lazy dog 0123456789\n"
    return line * (TEXT_BYTES // len(line))


def json_input():
    items = ",".join(f'{{"id":{n},"name":"item-{n}","tags":["a","b"]}}' for n in range(5000))
    return f'{{"a":{{"b":[{items}]}},"meta":{{"count":5000}}}}'.encode()


SCENARIOS = [
    Scenario("startup", "startup (no work)", ":", 'use value ""\n', dash=True),
    Scenario(
        "one_external",
        "one external command",
        "cat /dev/null",
        'run "cat" with arguments ["/dev/null"]\n',
        dash=True,
    ),
    Scenario(
        "pipeline_5",
        "5-stage pipeline",
        "printf 'a\\nb\\n' | cat | cat | cat | wc -l",
        'run "printf" with arguments ["a\\nb\\n"]\n'
        'pipe to "cat"\npipe to "cat"\npipe to "cat"\n'
        'pipe to "wc" with arguments ["-l"]\n',
        dash=True,
    ),
    Scenario(
        "builtin_loop",
        f"{LOOP_COUNT}-iteration builtin loop",
        f"for ((i=0;i<{LOOP_COUNT};i++)); do printf -v x '%s' 7; done",
        f"repeat {LOOP_COUNT} times\n  run number.to_text with value 7\n",
    ),
    Scenario(
        "spawn_loop",
        f"{SPAWN_COUNT}-spawn loop",
        f"for ((i=0;i<{SPAWN_COUNT};i++)); do /bin/true; done",
        f'repeat {SPAWN_COUNT} times\n  run "true"\n',
        runs_divisor=2,
    ),
    Scenario(
        "text_upper",
        "uppercase 1 MB (tr vs text.upper)",
        "tr '[:lower:]' '[:upper:]'",
        "read from stdin as text\nrun text.upper with text text\n",
        stdin="text",
        dash=True,
    ),
    Scenario(
        "json_get",
        "JSON pointer lookup (jq vs json.get)",
        "jq -c '.meta.count'",
        'read from stdin\nthen parse json as document\n'
        'run json.get with document document and pointer "/meta/count"\n',
        stdin="json",
        dash=True,
        requires=["jq"],
    ),
    Scenario(
        "pipe_100mb",
        "100 MB through a pipe",
        f"head -c {PIPE_BYTES} /dev/zero | cat",
        f'run "head" with arguments ["-c", "{PIPE_BYTES}", "/dev/zero"]\npipe to "cat"\n',
        runs_divisor=4,
        dash=True,
    ),
]

INPUTS = {"text": text_input, "json": json_input}


def run_once(command, stdin_path):
    """Return (elapsed_ms, returncode, stderr text)."""
    stdin = open(stdin_path, "rb") if stdin_path else subprocess.DEVNULL
    try:
        started = time.perf_counter()
        result = subprocess.run(command, stdin=stdin, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, check=False)
        elapsed = (time.perf_counter() - started) * 1000.0
    finally:
        if stdin_path:
            stdin.close()
    return elapsed, result.returncode, result.stderr.decode("utf-8", "replace").strip()


def peak_rss_kb(command, stdin_path, time_binary, repeats=3):
    """Peak RSS in KiB through GNU time, or None without it.

    os.wait4 is not used: on Linux a child keeps the high-water mark of the memory
    it was forked from, so every command would report the Python parent's size.
    GNU time forks from a small process, so its %M is the command's own peak.
    """
    if not time_binary:
        return None
    peak = None
    with tempfile.NamedTemporaryFile(prefix="rss-", delete=False) as output:
        path = output.name
    try:
        for _ in range(repeats):
            stdin = open(stdin_path, "rb") if stdin_path else subprocess.DEVNULL
            try:
                subprocess.run([time_binary, "-f", "%M", "-o", path, *command], stdin=stdin,
                               stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=False)
            finally:
                if stdin_path:
                    stdin.close()
            value = float(Path(path).read_text().strip().splitlines()[-1])
            peak = value if peak is None else max(peak, value)
    finally:
        os.unlink(path)
    return peak


def measure(command, stdin_path, runs, warmup, label, time_binary):
    for _ in range(warmup):
        run_once(command, stdin_path)
    samples = []
    for _ in range(runs):
        elapsed, returncode, message = run_once(command, stdin_path)
        if returncode != 0:
            raise RuntimeError(f"{label} failed with status {returncode}: {message}")
        samples.append(elapsed)
    return {
        "median_ms": statistics.median(samples),
        "min_ms": min(samples),
        "runs": runs,
        "max_rss_kb": peak_rss_kb(command, stdin_path, time_binary),
    }


def gnu_time():
    candidate = Path("/usr/bin/time")
    return str(candidate) if candidate.is_file() else None


def binary_size(path):
    if path is None:
        return None
    resolved = Path(path).resolve()
    return resolved.stat().st_size if resolved.is_file() else None


def format_ms(value):
    return "-" if value is None else f"{value:.2f}"


def format_kb(value):
    return "-" if value is None else f"{value:.0f}"


def render(results, sizes):
    lines = [
        "| scenario | bash ms | dash ms | execute ms | execute/bash | bash KiB | execute KiB |",
        "|---|---:|---:|---:|---:|---:|---:|",
    ]
    for row in results:
        bash, dash, execute = row["bash"], row.get("dash"), row["execute"]
        ratio = execute["median_ms"] / bash["median_ms"] if bash["median_ms"] else float("inf")
        lines.append(
            f"| {row['label']} | {format_ms(bash['median_ms'])} | "
            f"{format_ms(dash['median_ms'] if dash else None)} | {format_ms(execute['median_ms'])} | "
            f"{ratio:.2f}x | {format_kb(bash['max_rss_kb'])} | {format_kb(execute['max_rss_kb'])} |"
        )
    lines += ["", "| binary | bytes |", "|---|---:|"]
    for name, size in sizes.items():
        lines.append(f"| {name} | {'-' if size is None else f'{size:,}'} |")
    return "\n".join(lines)


def losses(results, sizes):
    lost = [row["key"] for row in results if row["execute"]["median_ms"] > row["bash"]["median_ms"]]
    lost += [
        f"{row['key']}_rss"
        for row in results
        if row["execute"]["max_rss_kb"] is not None
        and row["bash"]["max_rss_kb"] is not None
        and row["execute"]["max_rss_kb"] > row["bash"]["max_rss_kb"]
    ]
    if sizes.get("execute") and sizes.get("bash") and sizes["execute"] > sizes["bash"]:
        lost.append("binary_size")
    return lost


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("binary", type=Path, help="the execute binary")
    # Without --manifest-dir, Execute resolves its default directory the way an
    # installed binary does, which is the path that can use the bundled index.
    parser.add_argument("--manifest-dir", type=Path)
    parser.add_argument("--bash", default=shutil.which("bash"))
    parser.add_argument("--dash", default=shutil.which("dash"))
    parser.add_argument("--time", default=gnu_time(), help="GNU time, for peak RSS")
    parser.add_argument("--runs", type=int, default=20)
    parser.add_argument("--warmup", type=int, default=2)
    parser.add_argument("--only", action="append", help="run only these scenario keys")
    parser.add_argument("--json-out", type=Path)
    parser.add_argument("--require-win", action="store_true")
    args = parser.parse_args(argv)

    if not args.binary.is_file():
        print(f"error: {args.binary} is not a built executable", file=sys.stderr)
        return 1
    if not args.bash:
        print("error: bash was not found; pass --bash", file=sys.stderr)
        return 1

    results = []
    with tempfile.TemporaryDirectory(prefix="execute-vs-bash-") as directory:
        root = Path(directory)
        inputs = {}
        for name, make in INPUTS.items():
            inputs[name] = root / f"{name}.input"
            inputs[name].write_bytes(make())
        for scenario in SCENARIOS:
            if args.only and scenario.key not in args.only:
                continue
            missing = [tool for tool in scenario.requires if not shutil.which(tool)]
            if missing:
                print(f"skip {scenario.key}: {', '.join(missing)} not on PATH")
                continue
            program = root / f"{scenario.key}.program"
            program.write_text(scenario.program, encoding="utf-8")
            stdin_path = inputs[scenario.stdin] if scenario.stdin else None
            runs = max(3, args.runs // scenario.runs_divisor)
            row = {"key": scenario.key, "label": scenario.label}
            row["bash"] = measure(
                [args.bash, "--norc", "--noprofile", "-c", scenario.bash], stdin_path, runs, args.warmup, f"bash {scenario.key}", args.time
            )
            if scenario.dash and args.dash:
                row["dash"] = measure([args.dash, "-c", scenario.bash], stdin_path, runs, args.warmup, f"dash {scenario.key}", args.time)
            row["execute"] = measure(
                [str(args.binary), "run", str(program)]
                + (["--manifest-dir", str(args.manifest_dir)] if args.manifest_dir else []),
                stdin_path,
                runs,
                args.warmup,
                f"execute {scenario.key}",
                args.time,
            )
            results.append(row)
            print(f"{scenario.key}: bash {row['bash']['median_ms']:.2f} ms, execute {row['execute']['median_ms']:.2f} ms")

    sizes = {"execute": binary_size(args.binary), "bash": binary_size(args.bash), "dash": binary_size(args.dash)}
    table = render(results, sizes)
    print()
    print(table)
    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary:
        with open(summary, "a", encoding="utf-8") as handle:
            handle.write("## Execute vs bash\n\n" + table + "\n")
    lost = losses(results, sizes)
    if args.json_out:
        args.json_out.write_text(json.dumps({"results": results, "sizes": sizes, "losses": lost}, indent=2) + "\n", encoding="utf-8")
    print()
    print("execute loses: " + (", ".join(lost) if lost else "nothing"))
    return 1 if args.require_win and lost else 0


if __name__ == "__main__":
    sys.exit(main())
