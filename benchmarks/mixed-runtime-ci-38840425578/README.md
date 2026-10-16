# Mixed runtime: one bounded 300-second observation

This bundle records one preregistered manual run of the mixed `RuntimeGraphExecutor` probe: [workflow run 38840425578](https://github.com/monstercameron/NextGenScriptingLanguage/actions/runs/38840425578). It used commit [`71b9f0954236cfaa14076fa644d91b9b6283aa81`](https://github.com/monstercameron/NextGenScriptingLanguage/commit/71b9f0954236cfaa14076fa644d91b9b6283aa81) on Ubuntu 24.04 with `run_1000=true`, `window_ms=300000`, and `replications=1`. The workflow first built the exact-checkout release probe and passed its 32-actor, 1-second smoke; it then ran the one requested 1,000-actor trial. The job and artifact upload succeeded. No retry was made.

The successful 1,000-actor report records 847 timer actors, 64 finite native applets, 16 registry-owned workers, 32 in-process applets, 32 scheduled program services, 8 loopback listeners, and one timer collector. All 847 expected timer IDs progressed at least once during the full active window; this does not establish continuous per-second progress. The requested 300,000 ms window was observed for 300,042,614 µs. It consumed 25,410,650 timer events during the active window and zero after it. Every declared backend category produced output, and the eight loopback listeners completed sixteen requests. The worker processes were alive before registry drop and all 16 were reaped afterward; cancel/join and watchdog cleanup receipts passed.

The sampled resources belong to the Rust probe process only: RSS was 10,285,056 bytes at baseline, 42,680,320 bytes at sampled peak, and 25,174,016 bytes after join/reap; thread count was 2, 912, and 2. These figures do not measure child-process or process-tree peaks, concurrent process-tree size, machine-wide resources, or brief RSS peaks. The scheduled service fixture is pure in-process encoding. Capacity-one lossless timer fan-in was configured, but this run does not assert queue saturation; the probe’s own scope notes a separate full-queue cancellation regression.

Both raw reports retain `claim: none` and `status: measurement_recorded_no_claim`. This is one bounded observation of one fixed graph on one CI runner. It does not establish sustained production availability, endurance, churn, broad capacity, 1,000 independent capabilities, or cross-revision throughput. The production Rust source differs from reference `58b42f65d8c4de171baf8f3049ab70cf37c51af4` only in `src/main.rs`; the probe and wrapper were updated for this trial. No cross-revision performance comparison is made.

The parent verification checked both full report summaries and all 174 report-to-source hash comparisons against the candidate source set. The 87-file source manifest, pre-registered plan, local validator replay, and parent verification are included here. The original CI ZIP and full job log are intentionally not duplicated in this compact bundle; their exact SHA-256 values, GitHub links, and retained local paths are listed in `external-evidence.json`.

## Bundle files

- `graph-mixed-lifecycle-38840425578-32.json` and `graph-mixed-lifecycle-38840425578-1000-trial-1.json`: byte-preserved reports.
- `summary.json`: concise machine-readable result and limitations.
- `preregistered-plan.json`: exact pre-dispatch source/workflow hashes and planned inputs.
- `source-hashes-71b9f09.json`: full 87-file fingerprint and production-source comparison.
- `verification.json` and `parent-verification.json`: report/source checks and independent parent receipt.
- `run-metadata.json` and `artifact-metadata.json`: GitHub run and artifact records.
- `external-evidence.json`: links and hashes for the original ZIP and full job log retained outside this bundle.
- `SHA256SUMS`: bundle integrity manifest, excluding itself.
