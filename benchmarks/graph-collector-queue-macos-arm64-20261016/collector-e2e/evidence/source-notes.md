# Finite graph transport end-to-end benchmark

This ignored Unix integration test compares the same finite runtime workload in the integrated 81bf baseline and its collector-only candidate. It exercises an actual process producer and graph consumers; it does not preload all source events into the graph or pace events with a timer.

## Source identity

- Baseline worktree: `work/graph-optimizations-integrated-81bf9ba`, base commit `81bf9ba766a775cccf059007b19f5def9b9bfb03`.
- Candidate worktree: `work/graph-collector-sleep-candidate`, based on the same 81bf tree with the separately scoped collector change. The paired runner pins its exact candidate source and build receipt at preparation time.
- Shared ignored harness: `tests/graph_transport_e2e_bench.rs`, SHA-256 `e3a8c29881fe904b6b68b9e4a97485704a919dcac03aff0be2cc1998456b0d1e` in both worktrees.
- New-file patch: `graph-transport-e2e-bench.patch`, SHA-256 `32884dd1d7e7d3256a89f3b317a396c6234e645b2a6c91e03b534f6b75801b5f`.
- Paired runner: `work/graph-transport-e2e-paired-runner/graph_transport_e2e_pair.py`, SHA-256 `8d39ec34c761f5a186ea45d8978c9a8a66e0cadf37149d3d4bb8dbcb9cfbd1b8`; evidence patch SHA-256 `ae872632e81beb5c5bf090c6cbe9cacfd8c6f472b345e8f381dd2703c6169335`.

The only benchmark source difference between the arms is the shared test file. The candidate's intended product change is recorded separately in its source/build receipts. Paired preparation also verifies matching rustc/Cargo versions and selected build environment (allowing only a target-directory value change) and pins the captured environment JSON, root build driver, and both arms' clean/build logs and test lists. The earlier emitter microbenchmark worktree is not used for this comparison.

## Workload and timed boundary

The test writes a deterministic JSONL fixture containing 4,096 typed records, each with an integer id and a 256-byte ASCII text field. The real finite `/bin/cat` `NativeStream` reads that fixture to natural EOF, decodes all JSON records, and emits each through `stdout`. Two custom pass-through consumers receive the producer output through separate bounded capacity-8 `Lossless` edges and emit the unchanged typed payloads to terminal `stdout` ports. There is no graph `take` stage: the expected success status now follows the complete finite input rather than a stage that could stop and reap the process early.

Fixture bytes, expected typed payloads, input and graph hashes, registry, parsed and validated plan, runtime options, per-sample executor, graph options, and the per-sample plan clone are prepared before the clock starts. The measured wall interval begins immediately before `GraphSession::start` and ends after `run_to_completion` returns. It includes graph admission and queue setup, task launch and joins, process launch and read, JSON decoding, fanout, consumer execution, and terminal collection. Exact payload, event identity, sequence/order, metadata, and checksum validation occurs after the timer stops.

Limits are explicit and sized for the full result: 3 graph nodes, 2 edges, queue-slot budget 64 (16 edge slots plus 48 terminal slots), 4,096-byte graph message limit, 1,024-byte maximum stream record, terminal capacity 16, 8,193 collected events, and an exact collected-byte budget for both full payload streams plus the producer's serialized tagged status payload, computed dynamically. The run fails unless it sees exactly 8,193 terminal outputs, both consumers receive and return all 4,096 records, the producer launches exactly one process and reports status zero, and the graph reports zero drops. Input and output ports, generation 17, revision 29, sequence ordering and fanout identity equality, `Unknown` sensitivity, absent remote metadata, complete typed payload equality, and the full output JSONL SHA-256 are checked.

The absolute report path, its sibling `.input.jsonl` fixture, and sibling `.journal.jsonl` status journal are claimed with `create_new` before the timed samples begin. The fixture is retained. The journal records preparation and appends then flushes one compact record after every completed warmup/sample; timed runtime failures and post-timer validation failures are recorded with their elapsed duration before the test returns an error. The final JSON report retains all raw durations in execution order, configuration and hashes, compact input identity receipts, full terminal event identity records, and exact verification results. Defaults are zero inner warmups and three raw samples. No statistics are aggregated in the test.

## Validation status

This revision was formatted directly with the repository's Rust 2024 `rustfmt`. No Cargo command, build, test run, or timing was performed by this worker. The root agent owns source/build receipt pinning and the paired build/timing gate. The test remains ignored unless explicitly selected by the paired runner.
