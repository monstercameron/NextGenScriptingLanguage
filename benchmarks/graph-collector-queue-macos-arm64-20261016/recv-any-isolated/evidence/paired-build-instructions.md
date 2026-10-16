# Paired recv_any E2E source and build handoff — v2

## Source composition

Use the two detached worktrees recorded in `paired-source-bindings.json`:

- Baseline: base `a884f6561fd43f45e43b25a3cc2233be17ee3944`, plus the exact shared collector-progress patch and the identical edition-2021-formatted `graph_recv_any_e2e_bench.rs` harness.
- Candidate: the same composition, plus only `graph-recv-any-single-queue-product-v2.patch`.

The shared collector patch is `collector-progress.patch` (SHA-256 `7d411b9b0fc3a8a37fbd589381720016916b6edabec43be06414e198cd39bca4`). The v2 queue product patch is `graph-recv-any-single-queue-product-v2.patch` (SHA-256 `9135c5ee1ea8b350e6e50acb76eef19d5603a044f6dbcf51271bde83088b0400`). The v2 shared benchmark patch is `graph-recv-any-e2e-benchmark-v2.patch` (SHA-256 `36b09a09df6d23ab47ba1e5ed02bea1564a4d0c938325c072ff24f8824762002`); both worktrees contain identical harness SHA-256 `c38f59198e056576b69481e10bfa0cf2ac95124095869550a0d863ceb2bb18c1`. The candidate-minus-baseline transport diff is in `paired-candidate-minus-baseline-transport.diff`.

The v1 product patch, harness patch, source snapshot, and source/runner manifests remain preserved as `*-v1` artifacts. The v2 change only applies Rustfmt with edition 2021 to the intended candidate transport unit assertion and shared harness imports; timed runtime code and benchmark workload/oracles are unchanged.

No Cargo command, Rust test, build, benchmark, or timing was run in this formatting pass. Rustfmt `--check --edition 2021` passed on all intended Rust files.

## Root-owned build/run sequence

Root owns the build/timing slot. Use `work/build_graph_recv_any_e2e_pair.py`, which captures compiler/Cargo versions and selected environment before the first clean, builds the baseline, saves and hashes its immutable test binary, then cleans/builds the candidate using the existing campaign cache `work/perf-target-candidate`. It runs `cargo clean --release --package execute` between arms. The saved baseline binary and receipt are outside Cargo’s target directory and survive the clean. The release integration-test target is `graph_recv_any_e2e_bench`; the exact ignored test is `bounded_native_stream_recv_any_has_exact_end_to_end_samples`.

Each invocation uses zero inner warmups and three raw samples, with a fresh absolute report path:

```sh
EXECUTE_GRAPH_RECV_ANY_E2E_WARMUPS=0 \
EXECUTE_GRAPH_RECV_ANY_E2E_SAMPLES=3 \
EXECUTE_GRAPH_RECV_ANY_E2E_REPORT=/absolute/fresh/path.json \
/path/to/saved/test-binary --exact bounded_native_stream_recv_any_has_exact_end_to_end_samples \
  --ignored --nocapture --test-threads=1
```

The fixed paired schedule is three warmup pairs followed by 48 measured pairs in alternating AB/BA order. Each attempt has a 120-second cap and no retries. Preserve raw reports, fixtures, journals, stdout, stderr, failures, and losses. Confirm the configured 3 GiB free-space gate before execution.
