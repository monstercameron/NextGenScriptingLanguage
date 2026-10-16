# Single-queue `recv_any` wait scout — source freeze v2

Base commit: `a884f6561fd43f45e43b25a3cc2233be17ee3944`
Product scout worktree: `/Users/earlcameron/Documents/Codex/2026-10-15/users-earlcameron-documents-codex-2026-10/work/execute-thesis-2026-10-15/work/graph-recv-any-single-queue-a884`

## Product scope

`NodeIo::recv_any` and `recv_any_timeout` poll empty queues at 2 ms intervals, including when exactly one active input name and one queue are present. The source patch adds a private non-consuming `BoundedQueue::wait_for_change` using the existing condition variable and applies it only to the single-active-queue path. Named `recv_timeout` already uses the queue condition variable for one queue. Multi-name and multi-edge paths remain unchanged. Existing queue scan, seed handling, fairness cursors, EOF checks, cancellation/deadline checks, zero-timeout behavior, and cancellation-before-poison precedence are preserved. Wait durations remain capped by the existing 2 ms poll interval and remaining deadline.

A focused unit test covers zero and positive empty timeouts, ordered receives through `recv_any_timeout` and named `recv_timeout`, EOF, cancellation, and expired graph deadlines. The newly added `recv_any_timeout(Duration::ZERO)` assertion was collapsed by Rustfmt in v2; no runtime logic changed.

## Paired end-to-end harness

The ignored integration harness runs a real finite `/bin/cat` `NativeStream` producer with natural EOF, typed JSON decoding, and two custom pass-through consumers calling `recv_any` on one bounded Lossless input queue each. It measures complete `GraphSession::start` through `run_to_completion` for 4,096 typed records with 256-byte text. Fixture/oracle setup is outside the timer; full payload, input/output metadata, sequence order, fixture checksum, terminal count, no-drop, and successful producer-status checks run after timing. The shared collector-progress patch is present in both comparison arms; only candidate adds the queue product change. The report is buffered and flushed outside the timed interval.

The v2 harness uses Rustfmt with `--edition 2021`; the import-list reordering is formatting-only. No runtime workload, timed code, or oracle behavior changed. Rustfmt `--check --edition 2021` passed on the intended transport and harness files. No Cargo command, Rust test execution, build, benchmark, or timing was run by this formatting pass.

## Versioned patch/source artifacts

The original frozen artifacts are preserved as `*-v1` files and remain byte-identical to their recorded hashes. The active v2 patch artifacts are `graph-recv-any-single-queue-product-v2.patch` and `graph-recv-any-e2e-benchmark-v2.patch`; the formatted source snapshot is `graph_recv_any_e2e_bench-v2.rs`.

- Product source patch v2 SHA-256: `9135c5ee1ea8b350e6e50acb76eef19d5603a044f6dbcf51271bde83088b0400`
- Benchmark harness patch v2 SHA-256: `36b09a09df6d23ab47ba1e5ed02bea1564a4d0c938325c072ff24f8824762002`
- Benchmark source v2 SHA-256: `c38f59198e056576b69481e10bfa0cf2ac95124095869550a0d863ceb2bb18c1`
- Product scout `src/graph/transport.rs` SHA-256: `b4b80660e6f5cf0bf4d05ea0e09559bdd17dc2159c14931a38d8cce8a8226101`
- Paired baseline transport SHA-256: `bfc03a7028b2871a4823ed0a130e3b735ca8dc535cac9c12cd39d876174dad9c`
- Paired candidate transport SHA-256: `ac546546ac93266b675ba0f37418961b3bc1672d6cd34c68974aeaf1f6c8ee45`

The Rust product and harness sources remain frozen for root-owned build validation. See `source-freeze.json`, `paired-source-bindings.json`, and `paired-runner-source-freeze.json` for current pins. The previous source and runner freezes are retained as `*-v1.json`.
