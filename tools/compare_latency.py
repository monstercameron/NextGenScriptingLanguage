#!/usr/bin/env python3
"""Compare two Execute binaries with interleaved latency samples.

This is a diagnostic companion to check_latency.py. It measures the same four
workloads, but alternates baseline/current processes on one runner and reports
paired deltas. Each side may receive a separate manifest directory. It never
enforces or changes the Linux latency budgets.
Python 3.9+, standard library only.
"""
import argparse
import hashlib
import json
import statistics
import sys
import tempfile
from pathlib import Path

from check_latency import HOT_LOOP, LABELS, ONE_LINE, TEN_STEPS, percentile, run_once


def run_checked(binary, arguments, runner, label):
    elapsed, rss, returncode, message = runner([str(binary), *arguments])
    if returncode != 0:
        raise RuntimeError(f"{label} failed with status {returncode}: {message}")
    return elapsed, rss


def paired_samples(
    baseline,
    candidate,
    arguments,
    runs,
    warmup,
    runner=run_once,
    baseline_arguments=None,
    candidate_arguments=None,
):
    """Return baseline/current samples and RSS values from balanced AB/BA pairs."""
    if runs < 1:
        raise ValueError("runs must be at least 1")
    if warmup < 0:
        raise ValueError("warmup must not be negative")

    samples = {"baseline": [], "candidate": []}
    rss = {"baseline": [], "candidate": []}
    arguments_by_side = {
        "baseline": arguments if baseline_arguments is None else baseline_arguments,
        "candidate": arguments if candidate_arguments is None else candidate_arguments,
    }

    def run_pair(index, measured):
        order = (
            ("baseline", "candidate")
            if index % 2 == 0
            else ("candidate", "baseline")
        )
        for name in order:
            binary = baseline if name == "baseline" else candidate
            elapsed, peak = run_checked(
                binary, arguments_by_side[name], runner, name
            )
            if measured:
                samples[name].append(elapsed)
                if peak is not None:
                    rss[name].append(peak)

    for index in range(warmup):
        run_pair(index, measured=False)
    for index in range(runs):
        run_pair(index, measured=True)
    return samples, rss


def summarize_pairs(samples, rss):
    baseline = samples["baseline"]
    candidate = samples["candidate"]
    if len(baseline) != len(candidate) or not baseline:
        raise ValueError("baseline and candidate must have equal nonempty samples")
    deltas = [(new - old) / old * 100.0 for old, new in zip(baseline, candidate) if old > 0]
    return {
        "baseline_median_ms": statistics.median(baseline),
        "baseline_p95_ms": percentile(baseline, 0.95),
        "candidate_median_ms": statistics.median(candidate),
        "candidate_p95_ms": percentile(candidate, 0.95),
        "paired_delta_pct_median": statistics.median(deltas) if deltas else None,
        "paired_delta_pct": deltas,
        "baseline_samples_ms": baseline,
        "candidate_samples_ms": candidate,
        "baseline_max_rss_kb": max(rss["baseline"]) if rss["baseline"] else None,
        "candidate_max_rss_kb": max(rss["candidate"]) if rss["candidate"] else None,
        "runs": len(baseline),
    }


def manifest_tree_metadata(directory):
    """Fingerprint direct JSON manifest files, matching directory loader scope."""
    directory = Path(directory)
    if not directory.is_dir():
        raise ValueError(f"manifest directory {directory} does not exist")
    files = sorted(
        (
            path
            for path in directory.iterdir()
            if path.suffix == ".json" and path.is_file()
        ),
        key=lambda path: path.name,
    )
    if not files:
        raise ValueError(f"manifest directory {directory} has no JSON files")

    tree_digest = hashlib.sha256()
    total_bytes = 0
    for path in files:
        content = path.read_bytes()
        total_bytes += len(content)
        tree_digest.update(path.name.encode("utf-8"))
        tree_digest.update(b"\0")
        tree_digest.update(hashlib.sha256(content).digest())
    return {
        "path": str(directory.resolve()),
        "json_files": len(files),
        "bytes": total_bytes,
        "sha256": tree_digest.hexdigest(),
    }


def main(argv=None, runner=run_once):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--baseline", required=True, type=Path)
    parser.add_argument("--candidate", required=True, type=Path)
    parser.add_argument("--baseline-label", default="baseline")
    parser.add_argument("--candidate-label", default="candidate")
    parser.add_argument("--manifest-dir", type=Path, default=Path("manifests"))
    parser.add_argument("--baseline-manifest-dir", type=Path)
    parser.add_argument("--candidate-manifest-dir", type=Path)
    parser.add_argument("--runs", type=int, default=20)
    parser.add_argument("--warmup", type=int, default=2)
    parser.add_argument("--only", action="append", choices=[key for key, _ in LABELS])
    parser.add_argument("--json-out", type=Path)
    args = parser.parse_args(argv)

    for name, binary in (("baseline", args.baseline), ("candidate", args.candidate)):
        if not binary.is_file():
            print(f"error: {name} binary {binary} does not exist", file=sys.stderr)
            return 1
    if args.runs < 1 or args.warmup < 0:
        print("error: --runs must be positive and --warmup cannot be negative", file=sys.stderr)
        return 1

    baseline_manifest_dir = args.baseline_manifest_dir or args.manifest_dir
    candidate_manifest_dir = args.candidate_manifest_dir or args.manifest_dir
    try:
        baseline_manifest_metadata = manifest_tree_metadata(baseline_manifest_dir)
        candidate_manifest_metadata = manifest_tree_metadata(candidate_manifest_dir)
    except ValueError as error:
        print(f"error: {error}", file=sys.stderr)
        return 1

    selected = set(args.only or [key for key, _ in LABELS])
    results = {}
    with tempfile.TemporaryDirectory(prefix="execute-paired-latency-") as directory:
        programs = {
            "one_line": (Path(directory) / "one_line.program", ONE_LINE),
            "ten_step": (Path(directory) / "ten_step.program", TEN_STEPS),
            "hot_loop": (Path(directory) / "hot_loop.program", HOT_LOOP),
        }
        for path, source in programs.values():
            path.write_text(source, encoding="utf-8")
        plans = {"version": (["--version"], ["--version"])}
        for key, (path, _) in programs.items():
            prefix = ["run", str(path), "--manifest-dir"]
            plans[key] = (
                [*prefix, str(baseline_manifest_dir)],
                [*prefix, str(candidate_manifest_dir)],
            )
        for key, label in LABELS:
            if key not in selected:
                continue
            runs = args.runs if key != "hot_loop" else max(3, args.runs // 4)
            baseline_arguments, candidate_arguments = plans[key]
            samples, rss = paired_samples(
                args.baseline,
                args.candidate,
                [],
                runs,
                args.warmup,
                runner=runner,
                baseline_arguments=baseline_arguments,
                candidate_arguments=candidate_arguments,
            )
            results[key] = summarize_pairs(samples, rss)
            result = results[key]
            delta = result["paired_delta_pct_median"]
            delta_text = "unavailable" if delta is None else f"{delta:+.1f}%"
            print(
                f"{label}: {args.baseline_label} median "
                f"{result['baseline_median_ms']:.1f} ms (p95 {result['baseline_p95_ms']:.1f}); "
                f"{args.candidate_label} median {result['candidate_median_ms']:.1f} ms "
                f"(p95 {result['candidate_p95_ms']:.1f}); median paired delta {delta_text} "
                f"over {result['runs']} pairs"
            )
    print("Diagnostic comparison only; existing latency budgets are unchanged and not enforced here.")

    if args.json_out:
        args.json_out.parent.mkdir(parents=True, exist_ok=True)
        args.json_out.write_text(
            json.dumps(
                {
                    "baseline": {"label": args.baseline_label, "binary": str(args.baseline)},
                    "candidate": {"label": args.candidate_label, "binary": str(args.candidate)},
                    "manifest_dir": (
                        str(baseline_manifest_dir.resolve())
                        if baseline_manifest_dir.resolve() == candidate_manifest_dir.resolve()
                        else None
                    ),
                    "baseline_manifest_dir": baseline_manifest_metadata,
                    "candidate_manifest_dir": candidate_manifest_metadata,
                    "runs": args.runs,
                    "warmup_pairs": args.warmup,
                    "results": results,
                },
                indent=2,
                sort_keys=True,
            )
            + "\n",
            encoding="utf-8",
        )
    return 0


if __name__ == "__main__":
    sys.exit(main())
