# Program-module AST-cache paired runner

This runner compares only the prepared finite `.program` AST cache against its
baseline. It invokes saved Rust integration-test binaries directly and never
invokes Cargo. Each binary invocation runs the same checked-in benchmark once:
one module call in each of 128 loop iterations, with a 4 KiB text field. The
runner fixes three warmup pairs followed by 48 measured pairs. Order alternates
AB/BA; measured pairs start BA. It performs no retries, continues after failed
attempts, retains all raw files and counts candidate losses from signed paired
median differences.

The Rust harness requires zero launched processes and checks every successful
run's final output against the exact expected module record. Its AST-shape
assertion checks one `examples.records.report` invocation inside the fixed
128-count repeat. `RunResult` retains only the final caller output, so there is
no per-iteration output vector to compare; the harness records that limit and
the runner checks the exact final output byte length and SHA-256 for each arm.
The cache retains one validated finite child `Program` per `ModuleKey` in the
execution session and shares that AST across calls; the report records that
bounded per-session memory tradeoff.

## Build receipts

The root build script writes a compact raw receipt after each successful
build. Keep those receipts, Cargo JSON output, stderr build log, release-clean
log, saved test binary, and source checkout together. The raw receipt only
records a stable hash for `src/runtime/program_backend.rs`; `prepare-receipt`
also fingerprints the full current build/runtime input tree and verifies that
the JSONL stream contains exactly the matching compiler artifact plus one
successful final `build-finished` record. It refuses a source tree, harness,
binary, artifact stream, or build driver that differs from the recorded
inputs.

The existing build script did not save `rustc -Vv`, `cargo --version`, or the
complete inherited build environment. The prepared receipt labels those
versions as unavailable and records the script's explicit toolchain/build
overrides plus a bounded compile-environment snapshot taken during
preparation. That snapshot is not evidence of the complete environment at
build time. Likewise, a Cargo artifact record and source fingerprint do not
cryptographically prove every byte consumed by rustc; the raw receipt and all
build logs remain attached as reviewable source evidence.

Prepare one enriched receipt for each arm after the builds have finished:

```sh
python3 evidence/program-ast-cache/paired-runner/program_module_cache_pair.py prepare-receipt \
  --side baseline \
  --raw-receipt evidence/program-ast-cache/baseline-binary-receipt.json \
  --build-jsonl evidence/program-ast-cache/baseline-build-attempt-1.jsonl \
  --build-log evidence/program-ast-cache/baseline-build-attempt-1.log \
  --release-clean-log evidence/program-ast-cache/baseline-release-clean-attempt-1.log \
  --source-root work/program-ast-cache-baseline-0735 \
  --harness-source work/program-ast-cache-baseline-0735/tests/program_module_cache_bench.rs \
  --build-driver work/build_program_ast_pair.py \
  --output evidence/program-ast-cache/baseline-prepared-receipt.json

python3 evidence/program-ast-cache/paired-runner/program_module_cache_pair.py prepare-receipt \
  --side candidate \
  --raw-receipt evidence/program-ast-cache/candidate-binary-receipt.json \
  --build-jsonl evidence/program-ast-cache/candidate-build-attempt-1.jsonl \
  --build-log evidence/program-ast-cache/candidate-build-attempt-1.log \
  --release-clean-log evidence/program-ast-cache/candidate-release-clean-attempt-1.log \
  --source-root work/program-ast-cache-worktree \
  --harness-source work/program-ast-cache-worktree/tests/program_module_cache_bench.rs \
  --build-driver work/build_program_ast_pair.py \
  --output evidence/program-ast-cache/candidate-prepared-receipt.json
```

The pinned Cargo command in the build driver is a locked release build for the
benchmark test, `program_modules` test, and library test with JSON output. The
runner verifies target `program_module_cache_bench`, release test profile,
`opt_level=3`, `debug_assertions=false`, and `fresh=false`. It hashes the
immutable saved binary against the raw receipt. The Cargo artifact executable
path and exact JSON event are retained, but that reusable target path is not
rehash-checked: the build script uses the same target directory sequentially,
so the candidate build may overwrite the earlier baseline artifact after the
baseline binary has been copied and hashed. The candidate tracked diff must
have the expected AST-cache patch hash, and
the baseline must have no tracked changes. The shared benchmark harness stays
untracked in both checkouts and is pinned to the SHA-256 below.

The source fingerprint hashes sorted contents of tracked files under `src/`,
`tests/`, `vendor/`, `.cargo/`, and `examples/modules/`, plus `Cargo.toml`,
`Cargo.lock`, toolchain/build configuration files, and the explicitly included
benchmark test file. Its digest is SHA-256 over each relative UTF-8 path, NUL,
lowercase file SHA-256, and newline.

Preregister only after both prepared receipts validate:

```sh
python3 evidence/program-ast-cache/paired-runner/program_module_cache_pair.py preregister \
  --baseline-receipt evidence/program-ast-cache/baseline-prepared-receipt.json \
  --candidate-receipt evidence/program-ast-cache/candidate-prepared-receipt.json \
  --harness-source work/program-ast-cache-worktree/tests/program_module_cache_bench.rs \
  --preregistration evidence/program-ast-cache/paired-run-preregistration.json

python3 evidence/program-ast-cache/paired-runner/program_module_cache_pair.py run \
  --preregistration evidence/program-ast-cache/paired-run-preregistration.json \
  --report-dir evidence/program-ast-cache/paired-run-attempt-1
```

`preregister` refuses to overwrite its output. `run` records a one-use marker
for that preregistration and refuses to overwrite its report directory. A
timeout, nonzero process result, failed Rust oracle, absent/invalid raw report,
or source/binary change is retained as a failed attempt; later scheduled
attempts still run, but the final report is marked `completed_with_failures`
and returns nonzero. A partial report after interruption remains incomplete
evidence and must not be replaced or represented as a completed run.
