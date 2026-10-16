# Mixed RuntimeGraphExecutor: bounded 3 × 10-second receipts

This publication bundle preserves the complete report set and evidence for [workflow run 38835685785](https://github.com/monstercameron/NextGenScriptingLanguage/actions/runs/38835685785). The workflow commit was [`e4d9176ee40bc98e0b16325f7d48db0b76b0befa`](https://github.com/monstercameron/NextGenScriptingLanguage/commit/e4d9176ee40bc98e0b16325f7d48db0b76b0befa); runtime and probe source were verified against [`58b42f65d8c4de171baf8f3049ab70cf37c51af4`](https://github.com/monstercameron/NextGenScriptingLanguage/commit/58b42f65d8c4de171baf8f3049ab70cf37c51af4). The workflow completed its 32-actor, 1-second correctness smoke and all three explicitly requested 1,000-actor, 10-second trials. It did not rerun selected trials until green.

All four reports have `measurement_recorded_no_claim` status. The three 1,000-actor reports each reached readiness for all 847 timers, observed all 847 timers progress during the active window, recorded zero timer events after that window, and reported registry worker cleanup after final registry drop. Each run used the same declared layout: 847 timers, 64 finite native applets, 16 registry-owned workers, 32 in-process applets, 32 pure in-process scheduled `.program` services, 8 loopback remote listeners, and one timer collector.

| Report | Readiness (µs) | Active window (µs) | Timer events during window | Timer sources | Cancel/join (µs) | Worker reap (µs) | Rust-process RSS baseline / sampled peak / after cleanup (bytes) | Threads baseline / sampled peak / after cleanup |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | --- | --- |
| `graph-mixed-lifecycle-38835685785-32.json` | 29270 | 1008236 | 2,205 | 22/22 | 2121 | 4 | 7,417,856 / 9,920,512 / 9,539,584 | 2 / 33 / 2 |
| `graph-mixed-lifecycle-38835685785-1000-trial-1.json` | 202184 | 10034641 | 847,314 | 847/847 | 783 | 25 | 10,362,880 / 45,264,896 / 27,398,144 | 2 / 935 / 2 |
| `graph-mixed-lifecycle-38835685785-1000-trial-2.json` | 272425 | 10034576 | 847,426 | 847/847 | 674 | 24 | 10,436,608 / 43,819,008 / 27,172,864 | 2 / 917 / 2 |
| `graph-mixed-lifecycle-38835685785-1000-trial-3.json` | 234065 | 10033734 | 847,438 | 847/847 | 1140 | 26 | 10,432,512 / 44,879,872 / 26,542,080 | 2 / 916 / 2 |

The artifact ZIP SHA-256 is `6a147c66806c53f245f8b9308ffd9549949e7f889540b511ecd9ea6eb0f33e00` and matches the digest recorded by GitHub. The four raw reports share an identical 87-file source manifest; the independent verification record says all 348 report/path comparisons matched the runtime-source commit. The common test binary SHA-256 is `cd58323e368fa4632fde5d6198044ed157b43a156e1e4bba686252bd564387ab`. The raw reports, source-hash manifest, receipt manifest, and parent verification are retained here. The original artifact ZIP and full job log remain in the campaign archive; the linked workflow and receipt identify their GitHub artifact and hashes. `SHA256SUMS` covers every package file except itself.

## Limits

This is one bounded Linux CI execution with three sequential 10-second replications. It is not evidence of sustained operation, churn or repeated graph replacement, broad capacity, or 1,000 independent capabilities. RSS and thread readings cover only the Rust probe process; they do not measure child-process or process-tree peaks, loopback service resources, or machine-wide usage, and sampling may miss short peaks. The services were pure in-process handlers and the remote listeners were loopback fixtures. Capacity-one lossless timer edges were configured, but saturation is not claimed. Do not combine event totals into a throughput comparison or read the measurements as desktop, Omarchy, WAN, power, disk-exhaustion, or process-tree certification.

## Files

- `graph-mixed-lifecycle-38835685785-32.json` and the three `graph-mixed-lifecycle-38835685785-1000-trial-*.json` files: byte-preserved raw reports.
- `receipt-manifest.json`: run artifact and report inventory.
- `parent-verification.json`: independent source, artifact, and report verification.
- `source-hashes-commit58.json`: shared 87-file runtime source manifest.
- `summary.json`: compact machine-readable run, layout, measurements, and limits.
- `SHA256SUMS`: package integrity manifest (self-excluded).
