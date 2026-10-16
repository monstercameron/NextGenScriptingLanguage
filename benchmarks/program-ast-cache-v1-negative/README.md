# Program AST-cache v1 fixed-pair result (negative)

This source-only bundle preserves one completed Darwin arm64 local fixed-pair comparison. The candidate is an unmerged worktree patch; this run is a negative performance result and does not justify merging the cache change.

The schedule was 3 warmup pairs followed by 48 measured pairs, alternating AB/BA order, with 3 raw `runtime::run` samples per invocation. All 102 invocations completed and passed their exact output checks. Independent recomputation gives 21 candidate-faster pairs, 27 candidate-slower pairs, no ties, median paired candidate-minus-baseline **+54,917 ns**, and median per-pair candidate/baseline ratio **1.0220668117**. The full measured pair table and all 27 slower pairs are retained in `result-summary.json` and `independent-audit.json`.

## Timer boundary

The ignored Rust test parses the fixed caller, loads the checked-in module registry, and prepares the payload/runtime options before timing. Each sample then times the public `runtime::run` call. That timed call includes runtime validation, a module preload for that run, and the 128-iteration module-call workload; compilation and CLI startup are excluded. The harness checks the exact fixed 4 KiB input/output hashes, the caller source hash, module identity, workload shape, and zero native process launches. This is not a graph queue or `RuntimeGraphExecutor` benchmark and makes no end-to-end claim.

Metadata caveat: Both arms used the shared v1 harness, which emits `prepared_program_retention` as static descriptive text for candidate intent in both raw reports. That field is not a memory measurement and does not introspect baseline behavior, so it cannot support a measured memory-retention or runtime-retention claim. The timing data above are unchanged; the result remains negative and unmerged.

## Provenance boundary

The retained source fingerprints, paired-run driver and test, product-arm patches, harness/workload source, Cargo artifact JSONL, clean/build logs, binary receipts, and saved-binary SHA-256 identities are included. The executable blobs are omitted. The original completed build receipts did not record exact Cargo or rustc versions, so this bundle does not infer or retroactively attest a toolchain version. The two arms used the same reusable Cargo target path; the retained per-arm executables occupy distinct immutable evidence paths and match their recorded SHA-256 values. The original build logs, Cargo artifact records, and per-invocation before/after hashes are preserved.

## Bundle contents and verification

- `preregistration.json` is the exact pre-run protocol.
- `independent-audit.json` contains the independent raw recomputation, source checks, all pair results, and every loss; `independent-audit.sha256` binds that receipt.
- `source/` contains both exact arm product patches as XZ files, with uncompressed SHA-256 and member names in `source/patch-uncompressed-sha256.json`; it also contains arm source snapshots, benchmark harness, fixed workload/module fixtures, driver, offline driver tests, and the build helper where retained.
- `build/` contains the per-arm prepared/binary receipts and a build manifest. `raw-artifacts.tar.xz` losslessly preserves all attempt raw JSON, start/finish sidecars, stdout/stderr, paired report, Cargo JSONL, logs, clean logs, test listings, and validation logs; the external manifest maps each member name to its uncompressed SHA-256 and size. Executable binaries are omitted.
- `bundle.SHA256SUMS` covers every other file in this publication staging bundle. Run `python3 verify_bundle.py` from this directory to verify the checksum list, source files, decompressed product patches, and raw archive members without building or running the benchmark.

This package was assembled from retained evidence only. No Cargo, build, benchmark, timing, network API, or primary/source-tree mutation was performed.
