# Frozen named-receive paired runner

This scratch runner drives the already-frozen Rust benchmark in both source arms. It does not build code and does not edit either checkout. Keep this driver separate from the published `recv_any` driver.

The benchmark is pinned to base commit `c6f77a0cb199aedec564440a6a5b4b6170317831`, the source freeze at `evidence/graph-named-recv/source-freeze.json`, and harness suffix SHA-256 `1faae3e5100c515bd752b55b569f5cd5bec04a304c0d53fbc18ee78fd1648521`. The source freeze binds the baseline and candidate transport source hashes, and the runner checks each exact source hash and the common harness hash before it accepts build receipts.

The frozen benchmark emits 12 ordered cases: `recv` and zero-duration `recv_timeout`, each across `(sources, inputs)` shapes `(1,1)`, `(1,8)`, `(16,1)`, `(16,8)`, `(847,1)`, and `(847,8)`. Every case contains three raw samples of 54,208 preloaded events, exact checksum and queue-order oracles, and a p50 derived from those samples. The driver checks every case and raw sample, then compares a canonical metadata signature across baseline and candidate for each pair.

The run schedule is fixed: three warmup pairs, then 48 measured pairs, alternating AB/BA order with the sequence restarted for measurement. Each binary invocation has a 120 second ceiling; the whole sequence is capped at 10,800 seconds. If the total budget expires, the report enumerates every unrun slot. The runner never retries. It keeps each attempt's start and finish records, raw JSON when present, stdout, stderr, timeout/return status, and validation error. Failed attempts remain in the report, and measured summaries include all valid pairs, per-case paired deltas, candidate losses, improvements, and ties.

The pinned build workflow uses one Cargo target directory sequentially: it runs `cargo clean --release -p execute` before each arm, builds baseline and then candidate, and copies each freshly compiled library-test executable into a distinct immutable evidence binary before the next clean. Cargo's `fresh: false` means rustc compiled that artifact; the runner verifies the exact receipt against the Cargo JSONL row, the per-arm clean/build logs, the pinned build script, the saved binary's full SHA-256, and the recorded test list. It accepts the intentionally shared target directory while requiring distinct saved binary paths and checking their hashes and source identities before and after the run. The run command consumes the existing `baseline-binary-receipt.json` and `candidate-binary-receipt.json` plus the exact frozen patch files. The report directory must be new.

Run the offline validation suite with:

```sh
python3 -m unittest -v test_graph_recv_named_pair.py
```

No Cargo build or benchmark invocation was run while preparing this runner.
