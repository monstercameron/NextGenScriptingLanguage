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
import hashlib
import json
import os
import platform
import signal
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
GROUP_ORDER = {name: index for index, name in enumerate(("startup", "processes", "text", "files", "data"))}


@dataclass
class Scenario:
    key: str
    label: str
    bash: str
    program: str
    group: str = "processes"
    stdin: str = ""  # name of a generated input file, or empty for no stdin
    runs_divisor: int = 1
    dash: bool = False
    dash_command: str = ""
    requires: list = field(default_factory=list)  # tools that must be on PATH
    expected_stdout: bytes = b""
    execute_args: list = field(default_factory=list)
    reset_output_dir: bool = False
    expected_file_count: int = 0
    reset_checkpoint: bool = False


def text_input():
    line = b"the quick brown fox jumps over the lazy dog 0123456789\n"
    return line * (TEXT_BYTES // len(line))


def json_input():
    items = ",".join(f'{{"id":{n},"name":"item-{n}","tags":["a","b"]}}' for n in range(5000))
    return f'{{"a":{{"b":[{items}]}},"meta":{{"count":5000}}}}'.encode()


WRITE_FILE_NAMES = ", ".join(json.dumps(f"file-{n:03}.txt") for n in range(100))
WRITE_FILES_PROGRAM = (
    f"for each name in [{WRITE_FILE_NAMES}]\n"
    '  run text.concat with parts ["out/", name] as path\n'
    '  run fs.ensure_file with path path and content "written\\n" as receipt\n'
    'use value "100"\n'
)


SCENARIOS = [
    Scenario("startup", "startup (no work)", ":", 'use value ""\n', group="startup", dash=True),
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
        "printf 'a\\nb\\n' | cat | cat | cat | awk 'END { print NR }'",
        'run "printf" with arguments ["a\\nb\\n"]\n'
        'pipe to "cat"\npipe to "cat"\npipe to "cat"\n'
        'pipe to "awk" with arguments ["END { print NR }"]\n',
        group="processes",
        dash=True,
        expected_stdout=b"2\n",
    ),
    Scenario(
        "builtin_loop",
        f"{LOOP_COUNT}-iteration builtin loop",
        f'i=0; while [ "$i" -lt {LOOP_COUNT} ]; do x=7; i=$((i + 1)); done',
        f"repeat {LOOP_COUNT} times\n  run number.to_text with value 7 as value\n",
        group="processes",
        dash=True,
    ),
    Scenario(
        "spawn_loop",
        f"{SPAWN_COUNT}-spawn loop",
        f'i=0; while [ "$i" -lt {SPAWN_COUNT} ]; do /bin/true; i=$((i + 1)); done',
        f'repeat {SPAWN_COUNT} times\n  run "true"\n',
        runs_divisor=2,
        dash=True,
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
        expected_stdout=b"5000\n",
    ),
    Scenario(
        "pipe_100mb",
        "100 MB through a pipe",
        f"head -c {PIPE_BYTES} /dev/zero | wc -c | tr -d ' '",
        f'run "head" with arguments ["-c", "{PIPE_BYTES}", "/dev/zero"]\n'
        'pipe to "wc" with arguments ["-c"]\npipe to "tr" with arguments ["-d", " "]\n',
        group="processes",
        runs_divisor=4,
        dash=True,
        expected_stdout=f"{PIPE_BYTES}\n".encode(),
    ),
    Scenario(
        "read_10k_lines",
        "while read 10,000 lines",
        'first=1; printf "["; while IFS= read -r line; do if [ "$first" -eq 0 ]; then printf ","; fi; printf "\\"%s\\"" "$line"; first=0; done; printf "]"',
        "read from stdin as text\nrun text.lines with text text as lines\nthen encode lines as json\n",
        group="text",
        stdin="lines",
        dash=True,
        runs_divisor=2,
    ),
    Scenario(
        "grep_cut_sort_uniq",
        "grep/cut/sort/uniq chain",
        "grep -E '^(error|warn),' | cut -d, -f2 | sort | uniq -c | sed 's/^ *//'",
        'read from stdin\npipe to "grep" with arguments ["-E", "^(error|warn),"]\n'
        'pipe to "cut" with arguments ["-d,", "-f2"]\n'
        'pipe to "sort"\npipe to "uniq" with arguments ["-c"]\n'
        'pipe to "sed" with arguments ["s/^ *//"]\n',
        group="text",
        stdin="events",
        dash=True,
        expected_stdout=b"1 api\n3 worker\n",
    ),
    Scenario(
        "glob_1000",
        "glob count over 1,000 files and one matching directory",
        'count=0; for f in files/*.dat; do count=$((count + 1)); done; printf "%s" "$count"',
        'run fs.glob with pattern "files/*.dat" as paths\n'
        # `list.count` currently requires generic JSON, so normalize the
        # applet's list<text> result before counting it.
        "then encode paths as json\nthen parse json as parsed_paths\n"
        "run list.count with items parsed_paths as count\nthen encode count as json\n",
        group="files",
        dash=True,
        expected_stdout=b"1001",
    ),
    Scenario(
        "file_tests_1000",
        "file-test every match in a 1,000-file glob",
        'count=0; for f in files/*.dat; do if [ -f "$f" ]; then count=$((count + 1)); fi; done; printf "%s" "$count"',
        'run fs.glob with pattern "files/*.dat" as paths\n'
        'run json.parse with text "0" as count\n'
        'for each path in paths\n'
        '  run fs.test with path path and kind "file" as regular\n'
        '  if regular is true\n'
        '    run number.add with left count and right 1 as count\n'
        'then encode count as json\n',
        group="files",
        dash=True,
        runs_divisor=5,
        expected_stdout=b"1000",
    ),
    Scenario(
        "suffix_strip",
        "parameter suffix removal",
        'value="archive-2026.tar.gz"; printf "%s" "${value%.tar.gz}"',
        'run text.strip_suffix with text "archive-2026.tar.gz" and suffix ".tar.gz" as value\n'
        "use value value\n",
        group="text",
        dash=True,
        expected_stdout=b"archive-2026",
    ),
    Scenario(
        "here_string",
        "here-string scalar input",
        'read -r first second <<< "alpha beta"; printf "%s:%s\\n" "$first" "$second"',
        'read from stdin as value\n'
        'run text.split with text value and separator " " as parts\n'
        'run text.join with parts parts and separator ":" as joined\nuse value joined\n',
        group="text",
        stdin="here",
        expected_stdout=b"alpha:beta\n",
        dash=True,
        dash_command='printf "alpha beta\\n" | { IFS=" " read -r first second; printf "%s:%s\\n" "$first" "$second"; }',
    ),
    Scenario(
        "command_substitution_loop",
        "command substitution in a 1,000-iteration loop",
        'i=0; last=""; while [ "$i" -lt 1000 ]; do last=$(printf "%s" "$i"); i=$((i + 1)); done; printf "%s" "$last"',
        'use value "" as last\nrepeat 1000 times\n  run "printf" with arguments ["999"] as last\nuse value last\n',
        group="processes",
        dash=True,
        runs_divisor=2,
        expected_stdout=b"999",
    ),
    Scenario(
        "find_xargs",
        "find piped to xargs",
        "find files -type f -name '*.dat' -print | xargs -P 4 -n 1 basename | sort | head -n 10",
        'run "find" with arguments ["files", "-type", "f", "-name", "*.dat", "-print"]\n'
        'pipe to "xargs" with arguments ["-P", "4", "-n", "1", "basename"]\n'
        'pipe to "sort"\npipe to "head" with arguments ["-n", "10"]\n',
        group="files",
        dash=True,
        expected_stdout=b"item-0000.dat\nitem-0001.dat\nitem-0002.dat\nitem-0003.dat\nitem-0004.dat\nitem-0005.dat\nitem-0006.dat\nitem-0007.dat\nitem-0008.dat\nitem-0009.dat\n",
    ),
    Scenario(
        "capture_1mb",
        "capture 1 MB command output",
        'captured=$(cat capture.input); printf "%s" "${#captured}"',
        'run "cat" with arguments ["capture.input"] as captured\n'
        'run text.length with text captured and unit "bytes" as size\nthen encode size as json\n',
        group="processes",
        runs_divisor=4,
        dash=True,
        expected_stdout=b"1000000",
    ),
    Scenario(
        "csv_parse",
        "parse a 1,000-row CSV file",
        "jq -cjRn '[inputs | split(\",\")] | {headers: .[0], rows: .[1:]}'",
        "read from stdin as text\nrun csv.parse with text text as table\nthen encode table as json\n",
        group="data",
        stdin="csv",
        dash=True,
        requires=["jq"],
    ),
    Scenario(
        "environment",
        "environment variable selection",
        "env -i NAME=execute printenv NAME",
        'run "env" with arguments ["-i", "NAME=execute", "printenv", "NAME"]\n',
        group="data",
        dash=True,
        expected_stdout=b"execute\n",
    ),
    Scenario(
        "text_substitution",
        "string split and rejoin",
        'value="alpha,beta,gamma"; old_ifs=$IFS; IFS=,; set -- $value; IFS=$old_ifs; printf "%s|%s|%s" "$1" "$2" "$3"',
        'run text.split with text "alpha,beta,gamma" and separator "," as parts\n'
        'run text.join with parts parts and separator "|" as joined\nuse value joined\n',
        group="text",
        dash=True,
        expected_stdout=b"alpha|beta|gamma",
    ),
    Scenario(
        "checkpoint_resume",
        "checkpoint creation for a 100-step loop",
        'i=0; while [ "$i" -lt 100 ]; do i=$((i + 1)); done; printf "%s" "$i"',
        'repeat 100 times\n  run number.to_text with value 1 as completed\nuse value "100"\n',
        group="startup",
        dash=True,
        expected_stdout=b"100",
        execute_args=["--checkpoint", "checkpoint-data"],
        reset_checkpoint=True,
    ),
    Scenario(
        "write_small_files",
        "write 100 small files",
        'i=0; while [ "$i" -lt 100 ]; do name=$(printf "%03d" "$i"); printf "written\\n" > "out/file-$name.txt"; i=$((i + 1)); done; printf "100"',
        WRITE_FILES_PROGRAM,
        group="files",
        dash=True,
        expected_stdout=b"100",
        reset_output_dir=True,
        expected_file_count=100,
    ),
]

def lines_input():
    return b"line\n" * 10_000


def events_input():
    return b"error,api\nwarn,worker\nerror,worker\ninfo,ignored\nwarn,worker\n"


def csv_input():
    return ("name,city\n" + "".join(f"name-{n},city-{n}\n" for n in range(1000))).encode()


INPUTS = {"text": text_input, "json": json_input, "lines": lines_input,
          "events": events_input, "csv": csv_input, "here": lambda: b"alpha beta\n",
          "capture": lambda: b"x" * 1_000_000}

def expected_output(scenario):
    if scenario.key == "text_upper":
        return text_input().upper()
    if scenario.key == "csv_parse":
        rows = [[f"name-{n}", f"city-{n}"] for n in range(1000)]
        value = {"headers": ["name", "city"], "rows": rows}
        return json.dumps(value, separators=(",", ":"), ensure_ascii=False).encode()
    if scenario.key == "read_10k_lines":
        return json.dumps(["line"] * 10_000, separators=(",", ":")).encode()
    return scenario.expected_stdout


def remaining_timeout(deadline):
    if deadline is None:
        return 30
    remaining = deadline - time.monotonic()
    if remaining <= 0:
        raise TimeoutError("bash comparison exceeded its end-to-end time budget")
    return min(30, remaining)


def run_once(command, stdin_path, cwd=None, capture=False, deadline=None):
    """Return elapsed time, status, stderr, and captured stdout when requested."""
    stdin = open(stdin_path, "rb") if stdin_path else subprocess.DEVNULL
    try:
        started = time.perf_counter()
        result = run_process(
            command, stdin=stdin, stdout=subprocess.PIPE if capture else subprocess.DEVNULL,
            stderr=subprocess.PIPE, cwd=cwd, timeout=remaining_timeout(deadline),
        )
        elapsed = (time.perf_counter() - started) * 1000.0
    finally:
        if stdin_path:
            stdin.close()
    return elapsed, result.returncode, result.stderr.decode("utf-8", "replace").strip(), result.stdout if capture else None


def run_process(command, *, stdin, stdout, stderr, cwd, timeout):
    """Run a command tree with a hard timeout, including pipeline descendants."""
    process = subprocess.Popen(
        command, stdin=stdin, stdout=stdout, stderr=stderr, cwd=cwd,
        start_new_session=(os.name == "posix"),
    )
    try:
        out, err = process.communicate(timeout=timeout)
    except subprocess.TimeoutExpired:
        if os.name == "posix":
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
        else:
            process.kill()
        out, err = process.communicate()
        raise TimeoutError(f"command timed out after {timeout:.3f}s: {command!r}")
    return subprocess.CompletedProcess(command, process.returncode, out, err)


def reset_output_dir(cwd):
    output_dir = Path(cwd) / "out"
    shutil.rmtree(output_dir, ignore_errors=True)
    output_dir.mkdir()


def prepare_scenario(cwd, scenario):
    if scenario.reset_output_dir:
        reset_output_dir(cwd)
    if scenario.reset_checkpoint:
        shutil.rmtree(Path(cwd) / "checkpoint-data", ignore_errors=True)


def execute_command(binary, program, manifest_dir, scenario):
    command = [str(Path(binary).resolve()), "run", str(program)]
    if manifest_dir:
        command.extend(["--manifest-dir", str(Path(manifest_dir).resolve())])
    command.extend(scenario.execute_args)
    return command


def peak_rss_kb(command, stdin_path, time_binary, repeats=3, cwd=None, before_each=None, deadline=None):
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
            if before_each:
                before_each()
            stdin = open(stdin_path, "rb") if stdin_path else subprocess.DEVNULL
            try:
                result = run_process([time_binary, "-f", "%M", "-o", path, *command], stdin=stdin,
                                     stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, cwd=cwd,
                                     timeout=remaining_timeout(deadline))
                if result.returncode != 0:
                    raise RuntimeError(f"RSS measurement failed with status {result.returncode}")
            finally:
                if stdin_path:
                    stdin.close()
            value = float(Path(path).read_text().strip().splitlines()[-1])
            peak = value if peak is None else max(peak, value)
    finally:
        os.unlink(path)
    return peak


def measure(command, stdin_path, runs, warmup, label, time_binary, cwd=None, before_each=None, deadline=None):
    for _ in range(warmup):
        if before_each:
            before_each()
        _, returncode, message, _ = run_once(command, stdin_path, cwd=cwd, deadline=deadline)
        if returncode != 0:
            raise RuntimeError(f"{label} warmup failed with status {returncode}: {message}")
    samples = []
    for _ in range(runs):
        if before_each:
            before_each()
        elapsed, returncode, message, _ = run_once(command, stdin_path, cwd=cwd, deadline=deadline)
        if returncode != 0:
            raise RuntimeError(f"{label} failed with status {returncode}: {message}")
        samples.append(elapsed)
    return {
        "median_ms": statistics.median(samples),
        "min_ms": min(samples),
        "runs": runs,
        "samples_ms": samples,
        "max_rss_kb": peak_rss_kb(
            command, stdin_path, time_binary, cwd=cwd, before_each=before_each, deadline=deadline
        ),
    }


def gnu_time():
    candidate = Path("/usr/bin/time")
    if not candidate.is_file():
        return None
    result = subprocess.run([str(candidate), "--version"], capture_output=True, text=True, check=False)
    return str(candidate) if result.returncode == 0 and "GNU time" in result.stdout else None


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
        "| group | scenario | bash ms | dash ms | execute ms | execute/bash | bash KiB | execute KiB |",
        "|---|---|---:|---:|---:|---:|---:|---:|",
    ]
    for row in results:
        bash, dash, execute = row["bash"], row.get("dash"), row["execute"]
        ratio = execute["median_ms"] / bash["median_ms"] if bash["median_ms"] else float("inf")
        lines.append(
            f"| {row['group']} | {row['label']} | {format_ms(bash['median_ms'])} | "
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
    parser.add_argument("--validate-only", action="store_true",
                        help="run all output and filesystem-effect oracles without collecting timings")
    parser.add_argument("--max-runtime-seconds", type=float, default=285,
                        help="hard end-to-end wall-clock cap, at most 300 seconds (default: 285)")
    parser.add_argument("--require-win", action="store_true")
    args = parser.parse_args(argv)

    if not args.binary.is_file():
        print(f"error: {args.binary} is not a built executable", file=sys.stderr)
        return 1
    if not args.bash:
        print("error: bash was not found; pass --bash", file=sys.stderr)
        return 1
    if args.max_runtime_seconds <= 0 or args.max_runtime_seconds > 300:
        parser.error("--max-runtime-seconds must be greater than zero and at most 300")
    benchmark_started = time.monotonic()
    deadline = benchmark_started + args.max_runtime_seconds

    results = []
    with tempfile.TemporaryDirectory(prefix="execute-vs-bash-") as directory:
        root = Path(directory)
        inputs = {}
        for name, make in INPUTS.items():
            inputs[name] = root / f"{name}.input"
            inputs[name].write_bytes(make())
        files = root / "files"
        files.mkdir()
        for n in range(1000):
            (files / f"item-{n:04}.dat").write_text(f"record-{n}\n", encoding="utf-8")
        # This matching directory makes the file-test scenario exercise its
        # predicate rather than seeing only regular-file glob results.
        (files / "directory-guard.dat").mkdir()
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
            command_pairs = [
                ("bash", [args.bash, "--norc", "--noprofile", "-c", scenario.bash]),
            ]
            if scenario.dash and args.dash:
                command_pairs.append(("dash", [args.dash, "-c", scenario.dash_command or scenario.bash]))
            command_pairs.append(("execute", execute_command(args.binary, program, args.manifest_dir, scenario)))
            oracle = expected_output(scenario)
            for runtime, command in command_pairs:
                prepare_scenario(root, scenario)
                _, returncode, message, output = run_once(
                    command, stdin_path, cwd=root, capture=True, deadline=deadline
                )
                if returncode:
                    raise RuntimeError(f"{runtime} {scenario.key} oracle preflight failed with status {returncode}: {message}; command={command!r}; stdin={stdin_path!r}; cwd={root!r}")
                if output != oracle:
                    digest = hashlib.sha256(output).hexdigest()
                    expected_digest = hashlib.sha256(oracle).hexdigest()
                    raise RuntimeError(
                        f"{runtime} {scenario.key} output mismatch: expected {len(oracle)} bytes "
                        f"sha256={expected_digest}, got {len(output)} bytes sha256={digest}"
                    )
                if scenario.expected_file_count:
                    output_files = sorted(path.name for path in (root / "out").iterdir())
                    expected_names = [f"file-{n:03}.txt" for n in range(scenario.expected_file_count)]
                    written = sorted((root / "out").glob("file-*.txt"))
                    lockfiles = sorted((root / "out").glob(".*.execute-applet.lock"))
                    expected_locks = (
                        [f".file-{n:03}.txt.execute-applet.lock" for n in range(scenario.expected_file_count)]
                        if runtime == "execute" else []
                    )
                    if output_files != sorted(expected_names + expected_locks) or [path.name for path in written] != expected_names or any(
                        path.read_bytes() != b"written\n" for path in written
                    ) or [path.name for path in lockfiles] != expected_locks or any(
                        path.stat().st_size for path in lockfiles
                    ):
                        raise RuntimeError(f"{runtime} {scenario.key} output-file oracle failed")
            if args.validate_only:
                print(f"{scenario.key}: output oracle passed")
                continue
            runs = max(3, args.runs // scenario.runs_divisor)
            row = {"key": scenario.key, "label": scenario.label, "group": scenario.group}
            reset_before_run = (
                (lambda: prepare_scenario(root, scenario))
                if scenario.reset_output_dir or scenario.reset_checkpoint else None
            )
            row["bash"] = measure(
                [args.bash, "--norc", "--noprofile", "-c", scenario.bash], stdin_path, runs, args.warmup,
                f"bash {scenario.key}", args.time, cwd=root, before_each=reset_before_run, deadline=deadline
            )
            if scenario.dash and args.dash:
                row["dash"] = measure(
                    [args.dash, "-c", scenario.dash_command or scenario.bash], stdin_path, runs, args.warmup,
                    f"dash {scenario.key}", args.time, cwd=root, before_each=reset_before_run, deadline=deadline
                )
            row["execute"] = measure(
                execute_command(args.binary, program, args.manifest_dir, scenario),
                stdin_path,
                runs,
                args.warmup,
                f"execute {scenario.key}",
                args.time,
                cwd=root,
                before_each=reset_before_run,
                deadline=deadline,
            )
            results.append(row)
            print(f"{scenario.key}: bash {row['bash']['median_ms']:.2f} ms, execute {row['execute']['median_ms']:.2f} ms")

    if args.validate_only:
        print("All selected output and file oracles passed.")
        return 0

    results.sort(key=lambda row: GROUP_ORDER[row["group"]])
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
        manifest_location = None
        if args.manifest_dir:
            try:
                manifest_location = str(args.manifest_dir.resolve().relative_to(Path.cwd().resolve()))
            except ValueError:
                manifest_location = "external"
        source_revision = subprocess.run(
            ["git", "rev-parse", "HEAD"], cwd=Path.cwd(), capture_output=True,
            text=True, check=False,
        )
        source_revision = source_revision.stdout.strip() if source_revision.returncode == 0 else None
        metadata = {
            "source_revision": source_revision,
            "platform": platform.platform(),
            "machine": platform.machine(),
            "python": platform.python_version(),
            "execute_sha256": hashlib.sha256(args.binary.read_bytes()).hexdigest(),
            "runner_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
            "manifest_dir": manifest_location,
            "time_binary": args.time,
            "max_runtime_seconds": args.max_runtime_seconds,
            "elapsed_seconds": round(time.monotonic() - benchmark_started, 3),
        }
        args.json_out.write_text(json.dumps({"metadata": metadata, "results": results, "sizes": sizes, "losses": lost}, indent=2) + "\n", encoding="utf-8")
    print()
    print("execute loses: " + (", ".join(lost) if lost else "nothing"))
    return 1 if args.require_win and lost else 0


if __name__ == "__main__":
    sys.exit(main())
