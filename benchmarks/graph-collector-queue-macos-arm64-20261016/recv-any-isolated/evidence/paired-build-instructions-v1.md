# Paired recv_any E2E source and build handoff

## Source composition

Use the two prepared detached worktrees recorded in `paired-source-bindings.json`:

- Baseline: base `a884f6561fd43f45e43b25a3cc2233be17ee3944`, plus the exact shared collector-progress patch and the identical buffered `graph_recv_any_e2e_bench.rs` harness.
- Candidate: the same composition, plus only `graph-recv-any-single-queue-product.patch`.

The shared collector patch is `collector-progress.patch` (SHA-256 `7d411b9b0fc3a8a37fbd589381720016916b6edabec43be06414e198cd39bca4`). The queue product patch remains byte-identical to its frozen source version (SHA-256 `3fdb7b11062ebca674b314e046bab9f71ce1656712c231c92a0b18a280c2fa12`). The shared benchmark patch SHA-256 is `f426bc6535aad17b1a830f4b9a452bafd0fdd311e870100369b48d361c25c38c`; both worktrees contain harness SHA-256 `4294e731012d4f3d28d3deea61e40fcb92a5a4b77ea32c3fc8bdae37f12095e8`. The separate candidate-minus-baseline `transport.rs` diff is in `paired-candidate-minus-baseline-transport.diff`.

No existing frozen benchmark arms were changed. Both fresh worktrees have no `target/` directory. No Cargo command, Rust test, build, benchmark, or timing was run while preparing them.

## Root-owned build/run sequence

Wait until root releases the current timing/build slot. Use the dedicated driver at `work/build_graph_recv_any_e2e_pair.py`. It records compiler/Cargo versions and the selected environment before the first clean, then builds the baseline, saves and hashes its immutable test binary, and cleans/builds the candidate using the same `CARGO_TARGET_DIR`, explicitly set to the existing campaign cache `work/perf-target-candidate`. `cargo clean --release --package execute` runs between arms; dependency artifacts in the shared target are reused, while the execute package's release outputs are cleaned. The baseline saved binary and receipt are outside the shared Cargo target and survive that clean. Keep source worktrees stable throughout. The driver captures the exact Cargo JSONL artifact row and source SHA in each receipt. The release integration-test target is `graph_recv_any_e2e_bench`, and its exact ignored test is `bounded_native_stream_recv_any_has_exact_end_to_end_samples`.

Each invocation should use zero inner warmups and three raw samples, with a fresh absolute report path and the harness-specific variables:

```sh
EXECUTE_GRAPH_RECV_ANY_E2E_WARMUPS=0 \
EXECUTE_GRAPH_RECV_ANY_E2E_SAMPLES=3 \
EXECUTE_GRAPH_RECV_ANY_E2E_REPORT=/absolute/fresh/path.json \
/path/to/saved/test-binary --exact bounded_native_stream_recv_any_has_exact_end_to_end_samples \
  --ignored --nocapture --test-threads=1
```

Use the fixed paired schedule already specified for the graph E2E study: three warmup pairs followed by 48 measured pairs in alternating AB/BA order, each attempt capped at 120 seconds, no retries, and preserve all raw report, fixture, journal, stdout, stderr, failure, and loss records. Both arms use the same full exact payload and metadata oracle. The report writer buffers the large final identity document and explicitly flushes after serialization; that work is outside the measured `GraphSession::start` through `run_to_completion` interval.

The filesystem had approximately 2.8 GiB free immediately after preparing these two source-only worktrees. The earlier E2E runner required at least 3 GiB before a 102-invocation run; recheck and meet the root runner’s configured free-space gate after the current session, without compressing or deleting raw evidence to force a start.
