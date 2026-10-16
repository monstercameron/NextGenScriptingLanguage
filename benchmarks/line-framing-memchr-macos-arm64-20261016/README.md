# Line framing optimization: audited macOS arm64 trial

This bundle records one isolated Rust line-framing change and its complete paired measurement. The measured candidate is exactly base commit `a884f6561fd43f45e43b25a3cc2233be17ee3944` plus product patch SHA-256 `eeb7c5512fd78c0264d6d96a85b6959c8f1302ac481ab1e98047e3fb52145c94`. The patch replaces closure-based newline splitting in `LineSink::push` with `memchr::memchr_iter(b'\n', bytes)`. The direct dependency is the already locked `memchr` 2.8.3; there is no package-version or vendor change. Differential framing coverage checks byte output, record boundaries, limit behavior, callback/error retention, and EOF edge cases.

## Trial result

The candidate improved the median on all three pipeline sizes. Across the 48 paired comparisons per size, the candidate was faster in 38/48 one-stage pairs, 40/48 four-stage pairs, and 37/48 eight-stage pairs. There were 29 slower stage-pair outcomes overall; every one is listed below and in `evidence/paired-slower-stage-outcomes.csv`. These are paired performance regressions, not failed invocations: the trial had 0 timeout/error/lost invocations.

### Exact stage and order summaries

The first row for each stage is the aggregate of all 48 measured pairs. `AB` means baseline then candidate; `BA` means candidate then baseline. Centers/deltas/ratios use each invocation's median of its three raw samples. The raw-sample medians include all 144 measured raw durations per arm and stage. Ratios below 1.0 favor the candidate.

| Stages | Order | Pairs | Baseline raw median (ns) | Candidate raw median (ns) | Baseline paired-median center (ns) | Candidate paired-median center (ns) | Median paired Δ (ns) | Median paired ratio | Ratio of arm medians | Faster / slower / ties |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 1 | all | 48 | 23429166.5 | 21686083.0 | 23272291.5 | 21579312.5 | -1598688.0 | 0.9294940528125881 | 0.9272534464429513 | 38 / 10 / 0 |
| 1 | AB | 24 | — | — | 23434479.0 | 21425500.0 | -1906958.5 | 0.9173689812221004 | 0.9142725127364684 | 19 / 5 / 0 |
| 1 | BA | 24 | — | — | 22917417.0 | 21973604.0 | -1377875.0 | 0.9410128729811154 | 0.9588167811407368 | 19 / 5 / 0 |
| 4 | all | 48 | 23658270.5 | 21922750.0 | 23534333.5 | 22017771.0 | -1727354.0 | 0.9275503481570909 | 0.9355595729957681 | 40 / 8 / 0 |
| 4 | AB | 24 | — | — | 23507875.0 | 21652312.5 | -1820770.5 | 0.9223935405409365 | 0.9210663447887144 | 21 / 3 / 0 |
| 4 | BA | 24 | — | — | 23629875.0 | 22226188.0 | -1263667.0 | 0.9452455578589465 | 0.9405969350239898 | 19 / 5 / 0 |
| 8 | all | 48 | 25294041.5 | 23730041.5 | 25105333.0 | 23902187.0 | -1878270.5 | 0.9266100640370625 | 0.9520760788156046 | 37 / 11 / 0 |
| 8 | AB | 24 | — | — | 24803854.0 | 23152854.0 | -2121187.0 | 0.9139065056782825 | 0.9334377633411324 | 20 / 4 / 0 |
| 8 | BA | 24 | — | — | 26164374.5 | 24378375.5 | -1320771.5 | 0.9444838412721112 | 0.9317392815945208 | 17 / 7 / 0 |

### All 29 slower paired stage outcomes

Each row is one measured pair-stage comparison where the candidate median exceeded the baseline median. Pair indices are the runner's zero-based indices; the first three indices are warmup pairs, so measured pairs begin at index 3. No observation was filtered.

| Pair index | Stages | Order | Baseline median (ns) | Candidate median (ns) | Δ (ns) | Candidate / baseline |
| --- | --- | --- | --- | --- | --- | --- |
| 6 | 1 | AB | 24291208 | 26156417 | 1865209 | 1.07678535377903 |
| 8 | 1 | AB | 21910166 | 27114792 | 5204626 | 1.237543887161786 |
| 16 | 1 | AB | 22021375 | 22340917 | 319542 | 1.0145105380567745 |
| 21 | 1 | BA | 22067917 | 27571917 | 5504000 | 1.249411849790807 |
| 23 | 1 | BA | 23512209 | 30459125 | 6946916 | 1.2954599459370235 |
| 33 | 1 | BA | 21990250 | 25440666 | 3450416 | 1.156906629074249 |
| 36 | 1 | AB | 24738750 | 32527875 | 7789125 | 1.3148552372290434 |
| 40 | 1 | AB | 23516500 | 23846666 | 330166 | 1.0140397593179258 |
| 41 | 1 | BA | 22829250 | 24081667 | 1252417 | 1.0548601903260073 |
| 47 | 1 | BA | 23625708 | 24232042 | 606334 | 1.0256641621068032 |
| 15 | 4 | BA | 22866167 | 23544792 | 678625 | 1.0296781266401143 |
| 23 | 4 | BA | 23198958 | 24297041 | 1098083 | 1.0473332897106844 |
| 27 | 4 | BA | 23732208 | 24171334 | 439126 | 1.0185033773511507 |
| 30 | 4 | AB | 23541125 | 24597292 | 1056167 | 1.0448647632600396 |
| 39 | 4 | BA | 28362625 | 31443917 | 3081292 | 1.1086391686242016 |
| 40 | 4 | AB | 25283334 | 29647250 | 4363916 | 1.1726004964376928 |
| 41 | 4 | BA | 23113958 | 49794750 | 26680792 | 2.1543151545053427 |
| 46 | 4 | AB | 23222834 | 24353125 | 1130291 | 1.0486715359546557 |
| 7 | 8 | BA | 24546791 | 26045125 | 1498334 | 1.0610399135267823 |
| 8 | 8 | AB | 24448542 | 25525000 | 1076458 | 1.0440295376305058 |
| 19 | 8 | BA | 26824208 | 27773625 | 949417 | 1.0353940366105123 |
| 21 | 8 | BA | 24023958 | 26523375 | 2499417 | 1.1040385185488586 |
| 23 | 8 | BA | 25706959 | 28561666 | 2854707 | 1.1110480240000382 |
| 33 | 8 | BA | 24093125 | 28569959 | 4476834 | 1.1858137539235778 |
| 36 | 8 | AB | 24099708 | 26169417 | 2069709 | 1.0858810820446456 |
| 40 | 8 | AB | 24266666 | 31898083 | 7631417 | 1.3144814784198209 |
| 41 | 8 | BA | 26462709 | 34337167 | 7874458 | 1.2975680985646632 |
| 46 | 8 | AB | 26020000 | 37185000 | 11165000 | 1.4290930053804765 |
| 49 | 8 | BA | 29596875 | 30353459 | 756584 | 1.025562969063457 |

## Workload and protocol

- Public API: `runtime::run_stream` with a deterministic 131,072-record JSONL fixture, 5,000,698 bytes, SHA-256 `2ee93eabea3e8bdd50e7461df8242e78cf9eb9d57f84849154b4411433b698ce`.
- Each invocation ran one, four, or eight `/bin/cat` pipeline stages. All samples required exactly the expected process launches, record/callback counts, successful statuses, output byte count, and output checksum.
- Timing included the public stream call, native process launch/pipeline supervision, and synchronous record callback. Fixture creation, program parsing, registry/options setup, and input/output hash setup were outside the timed section.
- Schedule: 3 paired warmup rounds followed by 48 balanced AB/BA pairs (24 of each order). Within an invocation, stage counts rotated `[1,4,8]`, `[4,8,1]`, `[8,1,4]`; each case had three raw samples. All 102 invocations and all 918 samples are retained.
- Host: macOS-26.6.2-arm64-arm-64bit; the timed schedule was accepted by an independent 85-check raw audit. The audit recomputed all per-stage and per-order summaries from the raw reports.
- `/bin/cat` was pinned as `/bin/cat` with SHA-256 `2b6e8f3f90c42a9d8d1e5471efb4173ad3499fc8ca4039b39327b394616cf39e`.

The publication includes the complete invocation reports, stdout/stderr captures, run journal/results, build receipts and logs, immutable saved test binaries, candidate differential-test binary, the final preregistration, source fingerprints, full baseline/candidate source snapshots, benchmark harness, and the exact patch. `python3 verify_bundle.py` checks the bundle checksums, pinned source snapshots, build artifacts, raw result oracles, the 29 slower outcomes, and the independent audit receipt without executing Rust or benchmark binaries.

## Integration follow-up (separate from the measured comparison)

The exact measured native patch was later integrated on main in commit `da81d266d9f423ad979b8bc45d8139f2a0c272e2` (`perf(runtime): scan stream record boundaries with memchr`). The integration adds the locked `memchr` dependency entry to the browser lockfile as well as the product changes; the combined integration patch is retained at `evidence/integration-followup/combined-integration-product.patch` (SHA-256 `b25eb3fe4a2dfa39b685f2314603e0f986b9c9643471c606af3a28a3769597c1`). Its receipt confirms the native portion is byte-identically the measured patch.

The integrated tree passed formatting, native all-target Clippy with warnings denied, WASM Clippy with warnings denied, and 381 tests (313 library tests with 20 ignored plus 68 integration tests). Logs and the source receipt are under `evidence/integration-followup/`. This follow-up establishes that the integrated source passed those checks; it does not change or extend the benchmark claim above, which remains bound to the frozen baseline-plus-patch candidate.

## Scope and exclusions

These numbers apply to this exact patch on the pinned macOS arm64 host and workload. They do not establish the performance of later integration commits or unrelated runtime changes. The separate native-child observer/status-gating experiment is excluded from this bundle and remains unmerged; none of its measurements are attributed to this patch.

The copied build receipts and preregistration retain original host paths as provenance. All files needed for offline source, checksum, and raw-result verification are present within this bundle. To rerun builds or timings, use a compatible macOS arm64 Rust environment and the pinned stage executable; no rerun is required for bundle verification.
