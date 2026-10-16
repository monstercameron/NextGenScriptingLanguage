# Mixed RuntimeGraphExecutor lifecycle receipts

This package preserves two reports from the successful manual Linux CI run and the earlier failed attempt. The successful run was built from commit [`58b42f6`](https://github.com/monstercameron/NextGenScriptingLanguage/commit/58b42f65d8c4de171baf8f3049ab70cf37c51af4) in [workflow run 38833931575](https://github.com/monstercameron/NextGenScriptingLanguage/actions/runs/38833931575). Its artifact SHA-256 was checked against GitHub (`95f47dbb21beb08352e69cb57008467921c239e307599362053d9a8068437ffb`); both report source manifests were verified against 87 source-file hashes. The reports and parent verification are included here.

The 32-actor smoke and explicit 1,000-actor run both passed their report validators with `measurement_recorded_no_claim`. The 1,000-actor layout was 847 timers, 64 finite native applets, 16 distinct registry-owned workers, 32 in-process applets, 32 pure in-process scheduled `.program` services, 8 loopback listeners, and one timer collector. All expected actors reached readiness, and each timer delivered during the active window.

The 32-actor smoke used 22 timers, 3 finite native applets, 1 registry-owned worker, 2 in-process applets, 2 scheduled services, 1 loopback listener, and one timer collector. Readiness took 31,109 µs; its 1,000 ms active window was observed for 1,007,093 µs, with all 22 timers active and 2,200 timer events consumed. Cancellation and join took 9,737 µs, and worker reap took 5 µs. Rust-process RSS was 7,376,896 bytes at baseline, 9,752,576 at sampled peak, and 9,433,088 after cleanup; thread count was 2, 33, and 2 respectively.

The 1,000-actor readiness warm-up took 344,459 µs. Its requested 1,000 ms active window was observed for 1,042,569 µs; 85,282 timer events were consumed during that window and 107,582 total including warm-up. Event-counter checkpoints were 22,284 at the readiness barrier, 22,300 before the active window, 107,582 at its end, and 107,582 total, with zero events after the window. Cancellation and actor join took 2,412 µs; registry worker reap took 45 µs. The 16 registry workers were alive before final registry drop and reaped afterward.

For the 1,000-actor run, Rust-process RSS was 10,391,552 bytes at baseline, 44,191,744 bytes at the sampled peak, and 26,275,840 bytes after join and reap. Thread count was 2 at baseline, 913 at sampled peak, and 2 after cleanup. These are measurements of the Rust probe process only. They exclude child-process RSS, concurrent process-tree peaks, loopback-server resources, and machine-wide usage; sampled RSS may miss short peaks. The service fixtures were pure in-process handlers. The timer fan-in used capacity-one lossless edges, but the run does not claim those queues were saturated; a separate cancellation regression covers a full queue.

The earlier [run 38827031141](https://github.com/monstercameron/NextGenScriptingLanguage/actions/runs/38827031141) is retained too. Its 32-actor report passed, but its 1,000-actor attempt failed the exact timer-source coverage check: 846 of 847 expected timer sources emitted. The raw failure JSON remains in this package. Keep it as failure evidence. Runtime behavior and instrumentation changed before the later run, so these receipts do not support a throughput comparison.

These results establish one bounded Linux lifecycle execution, not a general capacity guarantee, 1,000 independent capabilities, or process-tree ceiling. They do not certify actual Arch/Omarchy or desktop behavior, WAN endpoints, power, disk exhaustion, or machine-wide resource limits.

## Files

- `graph-mixed-lifecycle-38833931575-32.json` and `graph-mixed-lifecycle-38833931575-1000.json`: byte-identical copies of successful reports.
- `graph-mixed-lifecycle-38827031141-32.json`: prior 32-actor report from the run with the failed 1,000-actor attempt.
- `graph-mixed-lifecycle-38827031141-1000-failure.json`: retained exact failure receipt.
- `receipt-manifest.json` and `parent-verification.json`: CI artifact manifest and independent receipt/source verification.
- `summary.json`: compact machine-readable summary and local SHA-256 inventory.
