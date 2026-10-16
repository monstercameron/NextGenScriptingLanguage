# Public JSON text decoder optimization proof (in progress)

This directory is a compact publication skeleton. The paired trial has not yet been copied, audited, or summarized; no performance result is reported here. The package does **not** contain the full repository or 5,917 source files. It contains the complete path-to-hash source maps, the two `src/contracts.rs` snapshots, the identical integration harnesses, the one-file product patch, the build and runner sources, and the source-bound build receipts and saved benchmark binaries.

## Candidate and benchmark scope

The frozen base is Git commit `3be9fe7eac71c371d3ec5c5a5fadb3161a83ea6c`. The sole product delta moves a decoded `serde_json::Value::String` into existing text/path validation instead of cloning its contents. The benchmark calls public `flow::contracts::OutputSchema::decode` for a fixed 4,096-byte ASCII string encoded once as JSON. Each sample runs 10,000 decodes, checking each returned string byte-for-byte and dropping it in the loop; each invocation contains three measured samples. This measures JSON parsing, ownership of the decoded string, equality checking, and per-result drop. It is a narrow decoder case, not CLI startup or general runtime performance.

The trial preregisters three warmup pairs followed by 48 balanced AB/BA measured pairs, with no retry. All raw invocation records and per-sample timings are intended to be retained. A result and loss/win counts will be added only after root-controlled smoke checks, the fixed trial, and independent raw audit complete.

## Source replay requirement

The source maps bind every tracked and non-ignored untracked path plus its content hash for each arm. Those maps are included here; the files themselves are not. To replay the full source fingerprints, obtain a checkout at the pinned base commit, apply the packaged product patch and harness patch in the baseline/candidate arrangement, then recompute the map using the packaged build driver's Git path enumeration and symlink hashing rules. The current checkout path recorded in receipts is machine-specific and is not required for self-contained verification of the copies and raw reports.

## Pending completion

The next package update should add the preregistration, complete raw paired report and per-attempt records, independent audit script/receipt, result tables cross-checked against raw pairs, and a final exact-membership SHA-256 manifest. The verifier must check all packaged bytes, both source-map digests and their one-path diff, build JSONL artifact-row freshness and receipt agreement, immutable saved-binary hashes, every raw oracle, all 102 attempt slots, AB/BA balance, all wins/losses/ties, and any summary-table values. Failed source-only formatting and runner drafts are preserved in `drafts/`; successful build/test logs are in `validation/`.
