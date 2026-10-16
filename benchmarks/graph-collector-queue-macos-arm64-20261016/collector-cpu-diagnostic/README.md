# Collector CPU counter diagnostic

This is a separate diagnostic for the same fixed collector graph used in the earlier collector-only wall-clock trial. It uses the same base commit `81bf9ba766a775cccf059007b19f5def9b9bfb03`, with an unmodified baseline and a candidate containing the shared collector-progress patch (`7d411b9b0fc3a8a37fbd589381720016916b6edabec43be06414e198cd39bca4`). The diagnostic is bound to its own harness, build receipts, source maps, preregistration, and saved binaries in this folder; it does not alter the original wall-clock evidence.

## Method and scope

Each attempt runs one finite GraphSession over the fixed NativeStream fixture: 4,096 records, 1,137,581 fixture bytes, 8,193 terminal events, one `/bin/cat` producer, exact payload/order/identity checks, natural EOF, and zero drops. Three warmup pairs precede twelve measured AB/BA pairs, with one graph invocation per attempt. All 30 attempts succeeded; there were no failures, unattempted attempts, retries, oracle mismatches, or drops. The interval for the separate wall-time metric remains GraphSession start through `run_to_completion`. `getrusage(RUSAGE_SELF)` and `getrusage(RUSAGE_CHILDREN)` snapshots bracket the operation; the resource counters and validation/report I/O are outside the wall-time interval.

The independent audit reports these user+system CPU counter results:

- Process-wide `RUSAGE_SELF` (covers the harness process and its threads): baseline median arm counter 287,241.5 µs; candidate 58,743.5 µs; median paired candidate/baseline ratio 0.198348; candidate lower in all 12 pairs (6 of 6 in each order).
- `RUSAGE_CHILDREN` for already-reaped children: baseline median arm counter 3,338 µs; candidate 2,789 µs; median paired candidate/baseline ratio 0.887452; candidate lower in 10 pairs and higher in 2.

These are counters for one finite workload on one host. They are not CPU utilization percentages, energy measurements, per-thread attribution, a portable efficiency estimate, sustained-capacity evidence, or a whole-application claim. The child counters cover children already reaped at the snapshot, and microsecond-resolution values can be small. The report includes component counters and all paired values for audit.

A post-run process snapshot records active desktop Codex and Chromium processes on the shared host. It was taken after the diagnostic, during a later product build; it is not an in-timer profile and does not identify exact overlap or attributable CPU for measured pairs. No pair was excluded or retried.

Do not combine this CPU-counter result with either wall-clock trial in the parent bundle. The collector-only wall-clock trial is a separate 48-pair run; the isolated `recv_any` trial has a different base commit, source composition, and harness. The measurements answer distinct questions and should remain separately reported.

## Included records

`evidence/paired-protocol.json` and `paired-preregistration-attempt-1.json` preserve the frozen schedule and exact inputs. The build receipts, compiler/configuration environment, complete build logs, and saved binaries pin both arms. The runner, build driver, test sources, audit implementation, source copies, full paired report, independent audit, and post-run host disclosure are retained here. `paired-run-attempt-1-raw-attempts.tar.xz` contains the 210 retained attempt artifacts (raw reports, fixture bytes, journals, stdout/stderr, and completed-call receipts); its adjacent receipt and member manifest record prior byte-for-byte and member verification. The publication verifier hashes the compressed archive without extracting it.
