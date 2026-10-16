# Literal JSON argument paired runner

This source-only driver runs prebuilt release library-test binaries serially. It never builds and never retries. It is kept outside both source worktrees so the measurement driver does not change the compiled source fingerprints.

After the baseline and candidate builds have each been cleaned, built, copied to immutable evidence files, and recorded in Cargo JSON artifact receipts, create the preregistration before timing:

```sh
CAMPAIGN=/Users/earlcameron/Documents/Codex/2026-10-15/users-earlcameron-documents-codex-2026-10/work/execute-thesis-2026-10-15
EVIDENCE="$CAMPAIGN/evidence/resolved-json-arguments"
RUNNER="$CAMPAIGN/work/literal-json-paired-runner/resolved_json_arguments_pair.py"
python3 "$RUNNER" prepare \
  --baseline-receipt "$EVIDENCE/baseline-binary-receipt.json" \
  --candidate-receipt "$EVIDENCE/candidate-binary-receipt.json" \
  --preregistration "$EVIDENCE/preregistration.json"
```

Then run the preregistered fixed sequence into a new report directory:

```sh
python3 "$RUNNER" run \
  --preregistration "$EVIDENCE/preregistration.json" \
  --report-dir "$EVIDENCE/run-<fresh-id>"
```

The exact Rust test is `runtime::resolved_json_arguments_bench::repeated_number_add_runtime_samples`. Each invocation is one saved test binary called with `--exact`, `--ignored`, `--nocapture`, and `--test-threads=1`; the driver sets `EXECUTE_JSON_ARGUMENTS_BENCH_REPORT` to that attempt's fresh absolute `raw.json`, `EXECUTE_JSON_ARGUMENTS_BENCH_WARMUPS=0`, and `EXECUTE_JSON_ARGUMENTS_BENCH_SAMPLES=3`. The fixed sequence has 3 warmup pairs and 48 measured pairs. Global pair order alternates baseline/candidate then candidate/baseline, beginning baseline/candidate; therefore the measured sequence is balanced and begins candidate/baseline. It continues through all planned attempts after failures, with no retries. Per-invocation timeout is 120 seconds and total wall-clock budget is three hours.

Every raw report must identify the pinned workload, canonical input hash, expected output hash, release profile, zero inner warmups and exactly three samples. Each sample must report exact output `b"3"`, 1,000 completed calls, and zero child launches. The report preserves each raw JSON plus stdout/stderr and start/finish sidecars, including failures. The summary uses each invocation's median of its three `runtime::run` durations, then reports the median of 48 paired candidate-minus-baseline deltas. This is a descriptive fixed-workload microbenchmark, not a confidence interval or a general performance claim.

The source fingerprint is JSON shaped as `{"sha256":"<combined>","files":{"<relative path>":"<sha256>",...}}`. It hashes working-tree contents for paths listed by `git ls-files --cached --others --exclude-standard -z`, filtered to `src/`, `tests/`, `vendor/`, `.cargo/`, `manifests/`, the top-level Cargo/build/toolchain configuration files, and the fixed workload file. Its combined digest is SHA-256 over the sorted concatenation of `relative-path UTF-8`, NUL, lowercase file SHA-256 ASCII, and LF for each file. The paired source roots must have the same input path set and differ only in `src/runtime.rs` and the reviewed `tests/structured_tasks.rs` regression. Each receipt's `source_sha256` must equal that arm's `src/runtime.rs` hash. The harness and workload hashes are pinned in the runner and required identical across arms.

The runner validates the exact simple binary receipt schema, Cargo's release library-test artifact metadata (`fresh: false`, optimized test profile), the receipt and saved-binary hashes, and the live source fingerprints before preregistration, before timing, and after the full sequence. The saved binary must be an executable `baseline-test-binary` or `candidate-test-binary` beside its receipt and must differ from Cargo's reusable artifact path. Build/clean logs and the Cargo JSONL row are retained by the separate build workflow; this driver does not rebuild or claim to verify those external logs.

Offline contract tests only (no product binary or timing):

```sh
cd "$CAMPAIGN/work/literal-json-paired-runner"
PYTHONDONTWRITEBYTECODE=1 python3 -B -m unittest -v test_resolved_json_arguments_pair
```
