#!/usr/bin/env python3
"""Compare two persistent parser/validator probes on a deterministic corpus.

The corpus includes every repository ``.program`` example plus generated
syntax/validation mutations. Probes receive one JSON line at a time, so neither
their stdin nor stdout can accumulate an unbounded pipeline. The harness only
reads source/catalog files, starts the two explicitly named probe executables,
and writes the explicitly requested JSON report.
"""

from __future__ import annotations

import argparse
import collections
import datetime as dt
import hashlib
import json
import os
import platform
import random
import select
import signal
import subprocess
import sys
import threading
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Iterator


ROOT = Path(__file__).resolve().parents[1]
MAX_GENERATED_CASES = 100_000
MAX_SOURCE_BYTES = 256 * 1024
MAX_REQUEST_BYTES = 2 * 1024 * 1024
MAX_RESPONSE_BYTES = 8 * 1024 * 1024
MAX_RETAINED_STDERR = 1024 * 1024
REQUEST_TIMEOUT_SECONDS = 5.0
EXIT_TIMEOUT_SECONDS = 5.0
MAX_RUN_SECONDS = 8 * 60 * 60

# There are currently no intentionally tolerated response differences. Keeping
# this explicit makes a future narrowly scoped exception visible in the report.
ALLOW_PATTERNS: tuple[str, ...] = ()


@dataclass(frozen=True)
class Case:
    case_id: str
    category: str
    mode: str | None
    source: str

    def record(self) -> dict[str, str | None]:
        return {
            "id": self.case_id,
            "category": self.category,
            "mode": self.mode,
            "source": self.source,
        }


def canonical_json(value: object) -> bytes:
    return json.dumps(
        value, ensure_ascii=False, sort_keys=True, separators=(",", ":")
    ).encode("utf-8")


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def fixed_cases() -> list[Case]:
    cases: list[Case] = []
    examples = ROOT / "examples"
    for path in sorted(examples.rglob("*.program")):
        source = path.read_text(encoding="utf-8")
        relative = path.relative_to(ROOT).as_posix()
        first_statement = next(
            (
                line.strip()
                for line in source.splitlines()
                if line.strip() and not line.lstrip().startswith("#")
            ),
            "",
        )
        category = (
            "ui_example"
            if first_statement.startswith("open a screen ")
            else "program_example"
        )
        cases.append(Case(relative, category, "auto", source))

    curated = [
        (
            "valid-pipeline-and-reference",
            "valid_template",
            "program",
            'run printf with message "Résumé 🐈" as greeting\n'
            'then tr with from "[:lower:]" and to "[:upper:]"\n',
        ),
        (
            "valid-named-list-item",
            "valid_template",
            "program",
            'run printf with message "-dash.txt" as operand\n'
            'then ls with flags ["-l"] and path operand\n',
        ),
        (
            "valid-trigger-interval",
            "service_header",
            "program",
            "every 2 seconds\n  run pwd\n",
        ),
        (
            "valid-trigger-cron",
            "service_header",
            "program",
            'on schedule "*/5 * * * *"\n  run pwd\n',
        ),
        (
            "valid-listener",
            "service_header",
            "program",
            "listen to stdin\n"
            "whenever a line arrives as record\n"
            '  then tr with from "a" and to "A"\n',
        ),
        (
            "raw-unicode-and-escapes",
            "unicode_escape_control",
            "program",
            r'run printf with message "raw café 🐈; escaped quote \"; slash \\; tab \t; newline \n"'
            "\n",
        ),
        (
            "raw-control-in-quoted-value",
            "unicode_escape_control",
            "program",
            'run printf with message "before\x01after"\n',
        ),
        (
            "bare-unicode-value",
            "unicode_escape_control",
            "program",
            "run printf with message café\n",
        ),
        (
            "manifest-unknown-parameter",
            "catalog_validation",
            "program",
            'run printf with surprise "value"\n',
        ),
        (
            "manifest-missing-required-parameter",
            "catalog_validation",
            "program",
            "run printf\n",
        ),
        (
            "manifest-wrong-scalar-type",
            "catalog_validation",
            "program",
            'run printf with message 17\n',
        ),
        (
            "manifest-wrong-list-item-type",
            "catalog_validation",
            "program",
            'run ls with flags ["-l", 17]\n',
        ),
        (
            "syntax-unterminated-string",
            "syntax_error",
            "program",
            'run printf with message "unterminated\n',
        ),
        (
            "syntax-unexpected-punctuation",
            "syntax_error",
            "program",
            "run printf with message [1,]\n",
        ),
        (
            "ui-minimal-screen",
            "ui_template",
            None,
            'open a screen titled "Boundary 🐈"\n'
            "place app on the left\n"
            "define a program named app\n"
            '  show a text panel named status with text "ready"\n',
        ),
        (
            "ui-invalid-action",
            "ui_error",
            "auto",
            'open a screen titled "Broken"\n'
            "place app on the left\n"
            "define a program named app\n"
            '  show a text panel named status with text "ready"\n'
            "  whenever the program opens\n"
            "    show the moon in status\n",
        ),
    ]
    cases.extend(Case(*item) for item in curated)
    for case in cases:
        if len(case.source.encode("utf-8")) > MAX_SOURCE_BYTES:
            raise ValueError(f"fixed case exceeds source limit: {case.case_id}")
    return cases


PROGRAM_TEMPLATES = [
    'run printf with message "hello"\n',
    'run printf with message "Résumé 🐈" as value\n'
    'then tr with from "[:lower:]" and to "[:upper:]"\n',
    'run ls with flags ["-la"] and path "."\n',
    'run printf with message "hash # inside quotes" # trailing comment\n',
    'run printf with message "value" as output\n'
    'then grep with pattern "value" and flags ["-F"]\n',
]

UI_TEMPLATE = (
    'open a screen titled "Mutation screen"\n'
    "place app on the left\n"
    "define a program named app\n"
    '  show a text panel named status with text "ready"\n'
)

ERROR_SOURCES = [
    "then\n",
    "pipe to\n",
    "run printf with message [\n",
    'run printf with message "bad\\q"\n',
    "whenever a line arrives as record\n",
    'on schedule "not cron"\n  run pwd\n',
    "every 0 seconds\n  run pwd\n",
    "every 1 fortnight\n  run pwd\n",
    "run printf with message \"x\"\n  then pwd\n",
]


def depth_list(depth: int) -> str:
    return "[" * depth + '"x"' + "]" * depth


def generated_cases(seed: int, count: int) -> Iterator[Case]:
    rng = random.Random(seed)
    categories = (
        "whitespace",
        "indentation",
        "unicode_escape_control",
        "nested_lists",
        "syntax_error",
        "service_header",
        "catalog_validation",
        "ui_mutation",
        "unicode_word_tokens",
        "depth_boundary",
    )

    for index in range(count):
        category = categories[index % len(categories)]
        source: str
        mode = "program"

        if category == "whitespace":
            source = rng.choice(PROGRAM_TEMPLATES)
            source = source.replace(" with ", rng.choice([" with ", "\twith\t", "   with  "]))
            source = source.replace(" and ", rng.choice([" and ", "\tand ", " and\t"]))
            if rng.randrange(2):
                source = source.replace("\n", "\r\n")
            if rng.randrange(2):
                source = "# deterministic prefix comment\n\n" + source
        elif category == "indentation":
            source = rng.choice(
                [
                    f"every {rng.choice([0, 1, 2, 5])} seconds\n"
                    f"{' ' * rng.choice([0, 1, 2, 4, 8])}run pwd\n",
                    UI_TEMPLATE.replace(
                        "  show a text panel", f"{' ' * rng.choice([1, 2, 3, 4])}show a text panel"
                    ),
                ]
            )
            mode = "auto"
        elif category == "unicode_escape_control":
            payload = rng.choice(
                [
                    "raw naïve 雪 🐈",
                    r'quote \" and slash \\',
                    r'escaped tab \t and line feed \n',
                    r'escaped nul \u0000',
                    "raw\x01control",
                    "combining e\u0301 and full-width 界",
                ]
            )
            source = f'run printf with message "{payload}"\n'
        elif category == "nested_lists":
            depth = rng.choice([1, 2, 3, 8, 32, 63, 64, 65])
            source = f"run ls with flags {depth_list(depth)}\n"
        elif category == "syntax_error":
            source = rng.choice(ERROR_SOURCES)
            if rng.randrange(2):
                source = "# error case\n" + source
        elif category == "service_header":
            source = rng.choice(
                [
                    f"every {rng.choice([1, 2, 0, 18446744073709551615])} "
                    f"{rng.choice(['millisecond', 'seconds', 'hours', 'fortnight'])}\n  run pwd\n",
                    f'on schedule "{rng.choice(["*/5 * * * *", "60 * * * *", "@hourly", ""]) }"\n'
                    "  run pwd\n",
                    "listen to stdin\n"
                    f"whenever a line arrives as {rng.choice(['record', 'true', 'not-valid!'])}\n"
                    '  then tr with from "a" and to "A"\n',
                    'listen to printf with message "x"\n'
                    "whenever a line arrives as record\n"
                    '  then tr with from "a" and to "A"\n',
                ]
            )
        elif category == "catalog_validation":
            source = rng.choice(
                [
                    'run printf with typo "x"\n',
                    "run printf with message []\n",
                    'run ls with flags ["-l", ["-a"]]\n',
                    'run printf with message "one" and message "two"\n',
                    'run tr with from "a" and to "A"\n',
                    'run definitely-unknown with value "x"\n',
                    'run printf with message "x" and cache\n',
                ]
            )
        elif category == "ui_mutation":
            mode = "auto"
            source = rng.choice(
                [
                    UI_TEMPLATE,
                    UI_TEMPLATE.replace("place app on the left", "place app on the right"),
                    UI_TEMPLATE.replace("  show a text panel", "   show a text panel"),
                    UI_TEMPLATE.replace('"ready"', '"raw café 🐈 # text"'),
                    UI_TEMPLATE + "  whenever the program opens\n    show the result in status\n",
                    UI_TEMPLATE.replace("define a program named app", "define a program named true"),
                ]
            )
        elif category == "unicode_word_tokens":
            source = rng.choice(
                [
                    "run café\n",
                    "run a" + "x" * rng.choice([255, 4096, 8192]) + " with value foo\n",
                    'run printf with message café\n',
                    "run _leading_underscore\n",
                    "run printf with message e\u0301\n",
                ]
            )
        elif category == "depth_boundary":
            depth = 64 if index % 20 < 10 else 65
            source = f"run ls with flags {depth_list(depth)}\n"
        else:
            raise AssertionError(category)

        if len(source.encode("utf-8")) > MAX_SOURCE_BYTES:
            raise ValueError(f"generated source exceeded the cap at case {index}")
        yield Case(f"generated-{index:06d}", category, mode, source)


def all_cases(seed: int, count: int) -> Iterator[Case]:
    yield from fixed_cases()
    yield from generated_cases(seed, count)


def corpus_hash(seed: int, count: int) -> tuple[str, int, dict[str, int]]:
    digest = hashlib.sha256()
    total = 0
    category_counts: collections.Counter[str] = collections.Counter()
    for case in all_cases(seed, count):
        digest.update(canonical_json(case.record()))
        digest.update(b"\n")
        category_counts[case.category] += 1
        total += 1
    return digest.hexdigest(), total, dict(sorted(category_counts.items()))


def reportable_catalog(path: Path) -> tuple[list[Path], dict[str, str]]:
    if path.is_dir():
        files = sorted(p for p in path.iterdir() if p.suffix == ".json")
    else:
        files = [path]
    if not files:
        raise ValueError(f"manifest catalog has no JSON files: {path}")
    return files, {p.name: sha256_file(p) for p in files}


def write_report(path: Path, report: dict[str, object]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(f".{path.name}.{os.getpid()}.tmp")
    encoded = json.dumps(report, ensure_ascii=False, indent=2).encode("utf-8") + b"\n"
    with temporary.open("wb") as stream:
        stream.write(encoded)
        stream.flush()
        os.fsync(stream.fileno())
    os.replace(temporary, path)


def process_environment() -> dict[str, str]:
    # Probes do not use PATH or user configuration, but keep a small predictable
    # environment for dynamic loader/runtime conventions.
    return {
        "PATH": "/usr/bin:/bin:/usr/sbin:/sbin",
        "LC_ALL": "C",
        "LANG": "C",
        "TZ": "UTC",
    }


class ProbeClient:
    def __init__(self, binary: Path, manifest_dir: Path):
        self.binary = binary
        self.manifest_dir = manifest_dir
        self.process: subprocess.Popen[bytes] | None = None
        self.stdout_pending = bytearray()
        self.stderr_lock = threading.Lock()
        self.stderr_retained = bytearray()
        self.stderr_total = 0
        self.stderr_thread: threading.Thread | None = None

    def __enter__(self) -> "ProbeClient":
        self.process = subprocess.Popen(
            [str(self.binary), "--manifest-dir", str(self.manifest_dir)],
            cwd=ROOT,
            env=process_environment(),
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            bufsize=0,
            start_new_session=True,
        )
        self.stderr_thread = threading.Thread(target=self._drain_stderr, daemon=True)
        self.stderr_thread.start()
        return self

    def _drain_stderr(self) -> None:
        assert self.process is not None and self.process.stderr is not None
        while True:
            try:
                chunk = os.read(self.process.stderr.fileno(), 16 * 1024)
            except OSError:
                return
            if not chunk:
                return
            with self.stderr_lock:
                self.stderr_total += len(chunk)
                remaining = MAX_RETAINED_STDERR - len(self.stderr_retained)
                if remaining > 0:
                    self.stderr_retained.extend(chunk[:remaining])

    def stderr_text(self) -> str:
        with self.stderr_lock:
            text = bytes(self.stderr_retained).decode("utf-8", errors="replace")
            if self.stderr_total > len(self.stderr_retained):
                text += f"\n[stderr truncated; {self.stderr_total} bytes total]"
            return text

    def request(
        self, request: dict[str, str], run_deadline: float | None = None
    ) -> tuple[bytes, dict[str, object]]:
        if self.process is None or self.process.stdin is None or self.process.stdout is None:
            raise RuntimeError("probe process is not running")
        if self.process.poll() is not None:
            raise RuntimeError(
                f"probe exited early with status {self.process.returncode}: {self.stderr_text()}"
            )
        if b"\n" in self.stdout_pending:
            raise RuntimeError("probe emitted more than one response for the prior request")

        payload = canonical_json(request) + b"\n"
        if len(payload) > MAX_REQUEST_BYTES:
            raise ValueError("encoded request exceeded the harness request cap")
        request_deadline = time.monotonic() + REQUEST_TIMEOUT_SECONDS
        if run_deadline is not None:
            request_deadline = min(request_deadline, run_deadline)
        fd_in = self.process.stdin.fileno()
        os.set_blocking(fd_in, False)
        offset = 0
        while offset < len(payload):
            while True:
                try:
                    written = os.write(fd_in, payload[offset:])
                    if written <= 0:
                        raise RuntimeError("probe closed its request stream")
                    offset += written
                    break
                except BlockingIOError:
                    remaining = request_deadline - time.monotonic()
                    if remaining <= 0:
                        raise TimeoutError(
                            f"probe stopped reading requests; stderr: {self.stderr_text()}"
                        )
                    _, writable, _ = select.select([], [fd_in], [], remaining)
                    if not writable:
                        raise TimeoutError(
                            f"probe stopped reading requests; stderr: {self.stderr_text()}"
                        )

        deadline = request_deadline
        fd_out = self.process.stdout.fileno()
        while b"\n" not in self.stdout_pending:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError(
                    f"probe timed out after {REQUEST_TIMEOUT_SECONDS}s; stderr: {self.stderr_text()}"
                )
            readable, _, _ = select.select([fd_out], [], [], remaining)
            if not readable:
                continue
            chunk = os.read(fd_out, min(64 * 1024, MAX_RESPONSE_BYTES + 1 - len(self.stdout_pending)))
            if not chunk:
                status = self.process.poll()
                raise RuntimeError(
                    f"probe closed stdout before replying (status={status}): {self.stderr_text()}"
                )
            self.stdout_pending.extend(chunk)
            if len(self.stdout_pending) > MAX_RESPONSE_BYTES and b"\n" not in self.stdout_pending:
                raise RuntimeError("probe response exceeded the harness response cap")

        newline = self.stdout_pending.index(b"\n")
        if newline > MAX_RESPONSE_BYTES:
            raise RuntimeError("probe response exceeded the harness response cap")
        raw = bytes(self.stdout_pending[:newline])
        del self.stdout_pending[: newline + 1]
        try:
            response = json.loads(raw)
        except (UnicodeDecodeError, json.JSONDecodeError) as error:
            raise RuntimeError(f"probe emitted invalid JSON response {raw[:200]!r}: {error}") from error
        if not isinstance(response, dict) or not isinstance(response.get("type"), str):
            raise RuntimeError(f"probe response lacks its typed envelope: {raw[:200]!r}")
        return raw, response

    def finish(self) -> None:
        if self.process is None:
            return
        if self.process.stdin is not None:
            self.process.stdin.close()
            self.process.stdin = None
        try:
            status = self.process.wait(timeout=EXIT_TIMEOUT_SECONDS)
        except subprocess.TimeoutExpired as error:
            self.terminate()
            raise TimeoutError(f"probe did not exit after stdin EOF: {self.stderr_text()}") from error
        if self.stderr_thread is not None:
            self.stderr_thread.join(timeout=2)
        if self.stdout_pending:
            raise RuntimeError(f"probe emitted an unmatched response suffix: {bytes(self.stdout_pending[:200])!r}")
        assert self.process.stdout is not None
        extra = os.read(self.process.stdout.fileno(), MAX_RESPONSE_BYTES + 1)
        if extra:
            raise RuntimeError(f"probe emitted output without a request: {extra[:200]!r}")
        if status != 0:
            raise RuntimeError(f"probe exited with status {status}: {self.stderr_text()}")

    def terminate(self) -> None:
        if self.process is None or self.process.poll() is not None:
            return
        try:
            os.killpg(self.process.pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        try:
            self.process.wait(timeout=2)
        except subprocess.TimeoutExpired:
            try:
                os.killpg(self.process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            self.process.wait()

    def __exit__(self, exc_type: object, exc: object, traceback: object) -> None:
        try:
            if exc_type is None:
                self.finish()
            else:
                self.terminate()
        finally:
            if self.process is not None:
                for stream in (self.process.stdin, self.process.stdout, self.process.stderr):
                    if stream is not None:
                        stream.close()


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--baseline-probe", required=True, type=Path)
    parser.add_argument("--candidate-probe", required=True, type=Path)
    parser.add_argument("--manifest-dir", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument(
        "--cases",
        type=int,
        default=2_000,
        help=f"number of deterministic generated mutations in addition to checked-in examples (0..{MAX_GENERATED_CASES})",
    )
    parser.add_argument("--seed", type=int, default=1495)
    args = parser.parse_args()
    if not 0 <= args.cases <= MAX_GENERATED_CASES:
        parser.error(f"--cases must be between 0 and {MAX_GENERATED_CASES}")
    return args


def normalized_response(raw: bytes) -> bytes:
    # No path normalization is currently needed: probe responses intentionally
    # omit executable/catalog paths. Do not trim errors or reorder AST payloads.
    return raw


def main() -> int:
    args = parse_args()
    baseline = args.baseline_probe.resolve(strict=True)
    candidate = args.candidate_probe.resolve(strict=True)
    manifest_dir = args.manifest_dir.resolve(strict=True)
    output = args.output.resolve(strict=False)
    if not baseline.is_file() or not candidate.is_file():
        raise ValueError("baseline and candidate probes must be executable files")
    if not manifest_dir.exists():
        raise ValueError(f"manifest catalog does not exist: {manifest_dir}")
    manifest_files, manifest_hashes = reportable_catalog(manifest_dir)
    protected = {
        baseline,
        candidate,
        Path(__file__).resolve(),
        ROOT / "examples" / "parse_probe.rs",
        manifest_dir,
        *(path.resolve() for path in manifest_files),
    }
    source_trees = [ROOT / "src", ROOT / "examples", ROOT / "tests"]
    under_source_tree = False
    for source_tree in source_trees:
        try:
            output.relative_to(source_tree.resolve())
            under_source_tree = True
            break
        except ValueError:
            pass
    under_manifest_tree = False
    if manifest_dir.is_dir():
        try:
            output.relative_to(manifest_dir)
            under_manifest_tree = True
        except ValueError:
            pass
    if output in protected or under_source_tree or under_manifest_tree:
        raise ValueError("--output must not replace either probe, the harness, or a manifest")

    seed_hash = hashlib.sha256(canonical_json({"seed": args.seed})).hexdigest()
    corpus_digest, total_cases, category_counts = corpus_hash(args.seed, args.cases)
    report: dict[str, object] = {
        "schema_version": 1,
        "status": "running",
        "started_at_utc": dt.datetime.now(dt.timezone.utc).isoformat(),
        "platform": platform.platform(),
        "seed": args.seed,
        "seed_sha256": seed_hash,
        "generated_cases_requested": args.cases,
        "total_cases": total_cases,
        "case_counts_by_category": category_counts,
        "corpus_sha256": corpus_digest,
        "harness_sha256": sha256_file(Path(__file__)),
        "allow_patterns": list(ALLOW_PATTERNS),
        "normalization": "none; exact response line bytes are compared",
        "manifest_dir": str(manifest_dir),
        "manifest_sha256": manifest_hashes,
        "probes": {
            "baseline": {"path": str(baseline), "sha256": sha256_file(baseline)},
            "candidate": {"path": str(candidate), "sha256": sha256_file(candidate)},
        },
        "response_type_counts": {"baseline": {}, "candidate": {}},
        "validation_counts": {"baseline": {}, "candidate": {}},
        "completed_cases": 0,
        "elapsed_seconds": 0.0,
        "first_mismatch": None,
        "error": None,
    }
    type_counts = {"baseline": collections.Counter(), "candidate": collections.Counter()}
    validation_counts = {"baseline": collections.Counter(), "candidate": collections.Counter()}
    started = time.monotonic()

    def snapshot(status: str) -> None:
        report["status"] = status
        report["elapsed_seconds"] = round(time.monotonic() - started, 6)
        report["response_type_counts"] = {
            side: dict(sorted(counter.items())) for side, counter in type_counts.items()
        }
        report["validation_counts"] = {
            side: dict(sorted(counter.items())) for side, counter in validation_counts.items()
        }
        write_report(output, report)

    try:
        with ProbeClient(baseline, manifest_dir) as baseline_client, ProbeClient(
            candidate, manifest_dir
        ) as candidate_client:
            for index, case in enumerate(all_cases(args.seed, args.cases)):
                if time.monotonic() - started >= MAX_RUN_SECONDS:
                    raise TimeoutError(f"hard harness runtime limit is {MAX_RUN_SECONDS}s")
                request: dict[str, str] = {"source": case.source}
                if case.mode is not None:
                    request["mode"] = case.mode
                run_deadline = started + MAX_RUN_SECONDS
                baseline_raw, baseline_response = baseline_client.request(request, run_deadline)
                candidate_raw, candidate_response = candidate_client.request(request, run_deadline)
                type_counts["baseline"][baseline_response["type"]] += 1
                type_counts["candidate"][candidate_response["type"]] += 1
                for side, response in (
                    ("baseline", baseline_response),
                    ("candidate", candidate_response),
                ):
                    validation = response.get("validation")
                    if isinstance(validation, dict):
                        state = "valid" if validation.get("ok") is True else "invalid"
                        validation_counts[side][state] += 1
                    elif str(response.get("type", "")).endswith("parse_error"):
                        validation_counts[side]["parse_error"] += 1
                    else:
                        validation_counts[side][str(response.get("type"))] += 1

                report["completed_cases"] = index + 1
                if normalized_response(baseline_raw) != normalized_response(candidate_raw):
                    report["first_mismatch"] = {
                        "index": index,
                        "case": case.record(),
                        "baseline": baseline_response,
                        "candidate": candidate_response,
                        "baseline_raw": baseline_raw.decode("utf-8", errors="replace"),
                        "candidate_raw": candidate_raw.decode("utf-8", errors="replace"),
                    }
                    snapshot("mismatch")
                    print(
                        f"mismatch at case {index} ({case.case_id}, {case.category}); "
                        f"report: {output}",
                        file=sys.stderr,
                        flush=True,
                    )
                    return 1
                if (index + 1) % 250 == 0:
                    snapshot("running")

        if report["completed_cases"] != total_cases:
            raise RuntimeError(
                f"corpus count changed between hashing and execution: "
                f"expected {total_cases}, got {report['completed_cases']}"
            )
        snapshot("equal")
        print(
            f"equal responses for {total_cases} cases; corpus sha256 {corpus_digest}; "
            f"report: {output}",
            flush=True,
        )
        return 0
    except BaseException as error:
        report["error"] = f"{type(error).__name__}: {error}"
        try:
            snapshot("error")
        except BaseException as report_error:
            print(f"could not save report: {report_error}", file=sys.stderr, flush=True)
        print(f"differential run failed: {error}", file=sys.stderr, flush=True)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
