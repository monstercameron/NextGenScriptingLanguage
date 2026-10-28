#!/usr/bin/env python3
"""Fail when CPU-bound Execute instruction counts regress over a stored report.

Both inputs are JSON reports produced by tools/bash_compare.py. The baseline
report must come from the same OS family, machine architecture, manifest
selection, and instruction backend as the candidate. A baseline is
intentionally required: this tool never invents one from a current run. In CI
the pair is two focused runs on the same runner from two distinct builds --
the baseline built at the base revision and the candidate at the head
revision (pull_request base/head SHAs; push before/sha with a HEAD~1
fallback for a zero before; dispatch inputs or HEAD~1 versus HEAD), both
measured back-to-back with the same head-revision measurement tool so an
old base without newer flags stays supported and tool hashes stay
comparable -- so a regression verdict never depends on cross-runner numbers;
run with --fail-closed (and --require-provenance) so missing gated data
fails instead of passing silently. Same-runner is procedural (one job
measures both binaries); runner identity is not derived from hashes:
runner_sha256/tool_sha256 attest the measurement-tool version and
valgrind_version attests the cachegrind provider, and mismatches in either
are rejected as incomparable rather than gated.

Only CPU-bound scenarios are gated: rows where the Execute arm is dominated by
interpreter work rather than child processes or filesystem traffic, so the
instruction count is a stable signal for the optimization backlog (#305).
Every other scenario's instruction counts are reported for stability but never
fail the gate. Without --fail-closed, scenarios without instruction data in
either report (no working counter was available or counting was forbidden) are
listed as skipped, not failed; with --fail-closed, a gated row without data in
either report fails the gate as incomplete rather than passing silently.

Instruction counts are backend-specific: perf `instructions:u` follows forked
children while cachegrind `I refs` counts the leader process only, so reports
whose backends differ — including a missing backend against a recorded one,
since unknown units are never assumed comparable — are rejected as
incomparable. Use --canary to
prove the gate wiring is live: it derives a synthetic baseline from a real
candidate report with gated counts deflated by --canary-percent and checks
that the gate fails on it. The canary never stands in for the base-versus-head
comparison; only the two-build pair can report a real regression.
"""
import argparse
import copy
import json
import math
import sys
from pathlib import Path


MAX_REGRESSION = 0.02

# Execute arms dominated by interpreter work: in-process applets and string,
# JSON, CSV, or line handling with no spawned children. Child-heavy rows
# (spawn_loop, pipelines, find/xargs, file writes, checkpoint journaling) mix
# child execution into the counter and are reported but not gated.
CPU_BOUND_SCENARIOS = frozenset({
    "builtin_loop",
    "text_upper",
    "json_get",
    "csv_parse",
    "read_10k_lines",
    "suffix_strip",
    "here_string",
    "text_substitution",
})


def report_instructions(report, label):
    if not isinstance(report, dict):
        raise ValueError(f"{label} report must be a JSON object")
    metadata = report.get("metadata")
    rows = report.get("results")
    if not isinstance(metadata, dict) or not isinstance(rows, list):
        raise ValueError(f"{label} report must contain metadata and results")
    required_metadata = ("source_revision", "platform", "machine", "manifest_dir")
    missing_metadata = [field for field in required_metadata if field not in metadata]
    if missing_metadata:
        raise ValueError(f"{label} report metadata is missing: " + ", ".join(missing_metadata))
    if any(not isinstance(metadata[field], str) or not metadata[field] for field in required_metadata[:3]):
        raise ValueError(f"{label} report must record source revision, platform, and machine")
    instructions = {}
    for row in rows:
        key = row.get("key") if isinstance(row, dict) else None
        if not isinstance(key, str) or key in instructions:
            raise ValueError(f"{label} report has a missing or duplicate scenario key")
        try:
            value = row["execute"]["median_instructions"]
        except (KeyError, TypeError) as error:
            raise ValueError(f"{label} report has no Execute instruction count for {key!r}") from error
        if value is None:
            continue
        try:
            number = float(value)
        except (TypeError, ValueError) as error:
            raise ValueError(f"{label} report has an invalid instruction count for {key!r}") from error
        if not math.isfinite(number) or number <= 0:
            raise ValueError(f"{label} report has a nonpositive or non-finite instruction count for {key!r}")
        instructions[key] = number
    if not rows:
        raise ValueError(f"{label} report contains no scenario results")
    return metadata, instructions


def evaluate(baseline, candidate, max_regression=MAX_REGRESSION, *,
              fail_closed=False, require_provenance=False):
    if not 0 <= max_regression < 1:
        raise ValueError("max regression must be between 0 and 1")
    baseline_meta, baseline_instructions = report_instructions(baseline, "baseline")
    candidate_meta, candidate_instructions = report_instructions(candidate, "candidate")
    if require_provenance:
        for field in ("execute_sha256", "runner_sha256"):
            value = baseline_meta.get(field)
            if not isinstance(value, str) or not value:
                raise ValueError(f"baseline lacks verified provenance: {field} is missing")
    for field in ("runner_sha256", "tool_sha256", "instruction_repeats"):
        before = baseline_meta.get(field)
        after = candidate_meta.get(field)
        if before is not None and after is not None and before != after:
            raise ValueError(
                f"baseline/candidate {field} differ: {before!r} != {after!r}"
            )
    before_backend = baseline_meta.get("instructions_backend")
    after_backend = candidate_meta.get("instructions_backend")
    before_valgrind = baseline_meta.get("valgrind_version")
    after_valgrind = candidate_meta.get("valgrind_version")
    if before_backend == "cachegrind" or after_backend == "cachegrind":
        if before_valgrind != after_valgrind:
            raise ValueError(
                f"baseline/candidate valgrind_version differ: "
                f"{before_valgrind!r} != {after_valgrind!r} "
                f"(cachegrind I refs are provider-specific; different valgrind "
                f"versions, or a pre-tool baseline without a recorded version, "
                f"are incomparable, never assumed equal)"
            )
    if before_backend != after_backend:
        raise ValueError(
            f"baseline/candidate instructions_backend differ: "
            f"{before_backend!r} != {after_backend!r} "
            f"(perf instructions:u and cachegrind I refs are different units; "
            f"a missing backend is unknown units, never assumed comparable)"
        )
    if str(baseline_meta.get("platform", "")).split("-", 1)[0] != str(
        candidate_meta.get("platform", "")
    ).split("-", 1)[0]:
        raise ValueError(
            f"baseline/candidate platform differ: "
            f"{baseline_meta.get('platform')!r} != {candidate_meta.get('platform')!r}"
        )
    for field in ("machine", "manifest_dir"):
        if baseline_meta.get(field) != candidate_meta.get(field):
            raise ValueError(
                f"baseline/candidate {field} differ: "
                f"{baseline_meta.get(field)!r} != {candidate_meta.get(field)!r}"
            )
    baseline_revision = baseline_meta.get("source_revision")
    candidate_revision = candidate_meta.get("source_revision")
    if not baseline_revision or not candidate_revision or baseline_revision == candidate_revision:
        raise ValueError("baseline and candidate must name distinct source revisions")

    baseline_keys = {row["key"] for row in baseline["results"] if isinstance(row, dict)}
    candidate_keys = {row["key"] for row in candidate["results"] if isinstance(row, dict)}
    missing = sorted(candidate_keys - baseline_keys)
    removed = sorted(baseline_keys - candidate_keys)
    if missing or removed:
        parts = []
        if missing:
            parts.append("scenarios without a stored baseline: " + ", ".join(missing))
        if removed:
            parts.append("stored baseline scenarios missing from candidate: " + ", ".join(removed))
        raise ValueError("; ".join(parts))

    failures = []
    comparisons = []
    skipped = []
    for key in sorted(baseline_keys):
        before = baseline_instructions.get(key)
        after = candidate_instructions.get(key)
        if before is None or after is None:
            skipped.append(key)
            if fail_closed and key in CPU_BOUND_SCENARIOS:
                failures.append(key)
            continue
        regression = after / before - 1.0
        gated = key in CPU_BOUND_SCENARIOS
        comparisons.append((key, before, after, regression, gated))
        if gated and regression > max_regression + 1e-12:
            failures.append(key)
    return comparisons, failures, skipped


def synthesize_canary_baseline(candidate, percent):
    """Derive a synthetic baseline whose gated counts are lower by percent.

    Only gated CPU-bound rows with data are deflated; every other row is left
    identical so a correct gate fails exactly on the gated rows. Returns the
    baseline report and the gated keys expected to fail.
    """
    if not percent > 0:
        raise ValueError("canary percent must be positive")
    if not isinstance(candidate, dict):
        raise ValueError("canary needs a candidate report object")
    baseline = copy.deepcopy(candidate)
    metadata = baseline.get("metadata")
    results = baseline.get("results")
    if not isinstance(metadata, dict) or not isinstance(results, list):
        raise ValueError("canary needs a candidate report with metadata and results")
    revision = metadata.get("source_revision")
    if not revision:
        raise ValueError("canary needs a candidate report with a source revision")
    metadata["source_revision"] = f"{revision}-canary-baseline"
    expected = []
    for row in results:
        if not isinstance(row, dict):
            raise ValueError("canary needs a candidate report with scenario results")
        key = row.get("key")
        execute = row.get("execute")
        if not isinstance(execute, dict) or key not in CPU_BOUND_SCENARIOS:
            continue
        median = execute.get("median_instructions")
        if median is None:
            continue
        baseline_value = float(median) / (1.0 + percent / 100.0)
        if not math.isfinite(baseline_value) or baseline_value <= 0:
            raise ValueError(f"canary cannot scale the instruction count for {key!r}")
        execute["median_instructions"] = baseline_value
        samples = execute.get("instruction_samples")
        if isinstance(samples, list):
            execute["instruction_samples"] = [
                sample / (1.0 + percent / 100.0) for sample in samples
            ]
        expected.append(key)
    return baseline, sorted(expected)


def run_canary(candidate, percent):
    """Prove the gate catches a synthetic regression in a real report.

    Returns an (exit_code, detail) pair: 0 when the gate fails the synthetic
    regression as it should, 1 when the gate misses it or the report is
    unusable, and 2 when the report has no gated instruction data (or no
    usable revision), which is inconclusive rather than acceptance.
    """
    if not percent / 100.0 > MAX_REGRESSION:
        raise ValueError(
            f"canary percent must clear the +{MAX_REGRESSION * 100:g}% gate threshold"
        )
    try:
        baseline, expected = synthesize_canary_baseline(candidate, percent)
    except ValueError as error:
        return 2, f"canary incomplete: {error}"
    if not expected:
        return 2, (
            "canary incomplete: the candidate report has no gated CPU-bound "
            "instruction data (counters unavailable or forbidden); this is not acceptance"
        )
    try:
        _, failures, skipped = evaluate(
            baseline, candidate, MAX_REGRESSION, fail_closed=True
        )
    except ValueError as error:
        return 2, f"canary incomplete: {error}"
    missing = sorted(set(expected) - set(failures))
    if missing:
        return 1, (
            f"canary FAILED: the +{MAX_REGRESSION * 100:g}% gate missed a synthetic "
            f"+{percent:g}% regression in: " + ", ".join(missing)
        )
    caught = sorted(set(expected) & set(failures))
    # Failures outside the synthetic set that are not explained by missing
    # data mean the gate is gating rows it should not (a gate bug), so they
    # fail the canary rather than passing silently.
    extra = sorted(set(failures) - set(expected) - set(skipped))
    detail = (
        f"canary: the +{MAX_REGRESSION * 100:g}% gate correctly failed a synthetic "
        f"+{percent:g}% regression in: " + ", ".join(caught)
    )
    if skipped:
        detail += " (no instruction data in either report: " + ", ".join(skipped) + ")"
    if extra:
        detail += " (unexpected non-gated failures: " + ", ".join(extra) + ")"
    return (1, detail) if extra else (0, detail)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("baseline", type=Path, nargs="?",
                        help="retained baseline bash_compare JSON report")
    parser.add_argument("candidate", type=Path, nargs="?",
                        help="current bash_compare JSON report")
    parser.add_argument(
        "--canary", type=Path, default=None, metavar="CANDIDATE",
        help="prove the gate is live: derive a synthetic baseline from CANDIDATE with "
             "gated counts deflated by --canary-percent and check the gate fails on it; "
             "exit 0 when the gate catches it, 1 when the gate misses it, "
             "2 when inconclusive (no gated data)",
    )
    parser.add_argument(
        "--canary-percent", type=float, default=3.0,
        help="synthetic regression size for --canary; must clear the gate threshold "
             "(default: 3)",
    )
    parser.add_argument(
        "--max-regression-percent",
        type=float,
        default=MAX_REGRESSION * 100,
        help="maximum allowed instruction regression on CPU-bound rows (default: 2)",
    )
    parser.add_argument(
        "--fail-closed",
        action="store_true",
        help="fail when a gated CPU-bound row lacks instruction data in either report",
    )
    parser.add_argument(
        "--require-provenance",
        action="store_true",
        help="require the baseline to record execute_sha256 and runner_sha256 "
             "(the measurement-tool hash; tool/valgrind mismatches are always "
             "compared, provenance or not)",
    )
    args = parser.parse_args(argv)
    if args.canary is not None:
        if not args.canary.is_file():
            print(
                "error: canary incomplete: the candidate report does not exist; "
                "a missing report is not acceptance",
                file=sys.stderr,
            )
            return 2
        try:
            candidate = json.loads(args.canary.read_text(encoding="utf-8"))
        except (json.JSONDecodeError, ValueError) as error:
            print(f"error: canary incomplete: {error}", file=sys.stderr)
            return 2
        try:
            code, detail = run_canary(candidate, args.canary_percent)
        except ValueError as error:
            print(f"error: canary incomplete: {error}", file=sys.stderr)
            return 2
        print(detail, file=sys.stderr if code else sys.stdout)
        return code
    if args.baseline is None or args.candidate is None:
        print(
            "error: gate incomplete: baseline and candidate reports are both required "
            "(or use --canary CANDIDATE to prove the gate catches a synthetic regression)",
            file=sys.stderr,
        )
        return 2
    if not args.baseline.is_file() or not args.candidate.is_file():
        print(
            "error: gate incomplete: baseline and candidate reports must both exist; "
            "no verified baseline is checked in, so a missing baseline is not acceptance",
            file=sys.stderr,
        )
        return 2
    try:
        baseline = json.loads(args.baseline.read_text(encoding="utf-8"))
        candidate = json.loads(args.candidate.read_text(encoding="utf-8"))
        comparisons, failures, skipped = evaluate(
            baseline,
            candidate,
            args.max_regression_percent / 100,
            fail_closed=args.fail_closed,
            require_provenance=args.require_provenance,
        )
    except (json.JSONDecodeError, ValueError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 2
    for key, before, after, regression, gated in comparisons:
        tag = "gated" if gated else "reported"
        print(f"{key}: Execute instructions {before:,.0f} -> {after:,.0f} ({regression:+.1%}) [{tag}]")
    if skipped:
        print("skipped (no instruction data in one or both reports): " + ", ".join(skipped))
    if failures:
        incomplete = sorted(set(skipped) & set(CPU_BOUND_SCENARIOS)) if args.fail_closed else []
        if incomplete:
            print(
                "error: gate incomplete: gated CPU-bound rows lack instruction data in: "
                + ", ".join(incomplete)
                + " (no working counter was available or counting was forbidden; this is not acceptance)",
                file=sys.stderr,
            )
        regressed = [key for key in failures if key not in set(skipped)]
        if regressed:
            print(
                f"error: CPU-bound instruction count regressed by more than {args.max_regression_percent:g}% in: "
                + ", ".join(regressed),
                file=sys.stderr,
            )
        return 1
    print(f"all gated CPU-bound instruction counts remain within {args.max_regression_percent:g}% of the stored baseline")
    return 0


if __name__ == "__main__":
    sys.exit(main())
