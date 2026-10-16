# Public text JSON decoder optimization: compact evidence bundle

The committed candidate moves an already decoded `serde_json::Value::String` into existing text/path output validation instead of cloning the string. The committed patch is byte-identical to the measured patch (`95f4d413f86f0905421500f182e372e55a3c30163dc3364810ca71386ca03f36`) and contains only `src/contracts.rs`. It preserves the existing text/path validators and diagnostics.

## Result and scope

On the pinned ARM64 macOS host, the preregistered public `flow::contracts::OutputSchema::decode` case ran 10,000 decodes per sample of one fixed 4,096-byte ASCII string encoded as JSON, with three samples per invocation. After three warmup pairs, all 48 balanced measured pairs completed with identical valid outputs: **42 wins, 6 losses, 0 ties**. The median paired candidate-minus-baseline difference was **−864,729 ns**. The median of per-pair candidate/baseline ratios was **0.901801164140** (about 9.82% lower); the ratio of the two arms' median invocation times was **0.895047252485** (baseline 9,000,479 ns; candidate 8,055,854 ns). The paired median difference is the primary result; both ratio definitions are reported to make aggregation explicit. All six losses and every measured pair are retained in [`results/tables.md`](results/tables.md) and the raw report. No pair was filtered.

This is a narrow decoder microbenchmark. It includes parsing the JSON string, owning and checking each returned string, and dropping each result inside the timed loop. It does **not** establish CLI startup, whole-runtime latency, broad workload gains, RSS effects, or end-to-end performance.

## Validation and retained failure history

Both clean release benchmark builds used the same pinned harness and source-map protocol; each Cargo artifact receipt records the exact release test artifact with `fresh: false`, and the saved binaries are immutable and hashed. Native formatting and strict Clippy, the WASM formatting/Clippy check, and 313 candidate library tests passed (20 ignored, 0 failed). Both attempt-2 correctness smokes passed with three samples and exact per-sample oracles. The local integration commit is `690c5aa11d05976caa43447be2c7a75bafd34b67`, whose committed patch is byte-identical to the measured patch. See `validation/main-integration-validation-receipt.json` and the commit hook log; remote CI status is not asserted by this bundle.

The first smoke attempt is retained as a failed validation record: its baseline harness test passed and produced three raw samples, but the external runner rejected the report because its expected fixture description differed from the harness wording. The candidate smoke was not attempted, and no timing trial began in that attempt. A source-only fix pinned the exact harness string and added regression tests. The subsequent two-arm smoke and paired trial are separate retained attempts; the first failure was not counted as a measurement or erased.

An independent raw audit passed. The original auditor script and report are included byte-for-byte under `reviews/independent-raw-audit-attempt-1/`. The portable verifier in this bundle independently checks the package hash manifest, both complete 5,917-path source maps and their single-path difference, build artifact/receipt/binary identity, all 102 raw invocations and 306 semantic sample oracles, the complete AB/BA schedule, every pair, and the published loss and summary tables.

## Verify and replay

Run `python3 verify_package.py` from this directory. It uses only the Python standard library and does not run or build the product. `SHA256SUMS.json` lists every packaged file except itself and the verifier checks exact membership as well as each listed hash.

The package includes full path-to-hash maps for both arms, source snapshots for the changed file, identical harness snapshots, product/harness patches, receipts, logs, saved binaries, smoke records, raw per-invocation directories, and the independent audit. It does not bundle the full repository. Recomputing complete source maps requires a checkout of base commit `3be9fe7eac71c371d3ec5c5a5fadb3161a83ea6c` with the included patches arranged as described in the preregistration. Rebuilding requires the original campaign structure and recorded pinned toolchain environment; the package's portable verification path does not invoke Cargo.
