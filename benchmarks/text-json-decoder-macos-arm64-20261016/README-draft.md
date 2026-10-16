# Public text JSON decoder candidate (source-only preparation)

This isolated candidate compares a one-file source change from `3be9fe7eac71c371d3ec5c5a5fadb3161a83ea6c`. In `OutputSchema::decode_json_value`, JSON `Value::String` now moves its already-owned `String` into the existing text/path validator instead of borrowing and cloning it. Other JSON types keep the same diagnostic, and the existing enum/path validation is still called. The change is intended to remove one string copy from JSON-encoded text/path decoding; this document makes no performance claim.

## Frozen source and harness

- Candidate worktree: `/Users/earlcameron/Documents/Codex/2026-10-15/users-earlcameron-documents-codex-2026-10/work/execute-thesis-2026-10-15/work/text-json-decoder-candidate-3be9fe`
- Baseline worktree: `/Users/earlcameron/Documents/Codex/2026-10-15/users-earlcameron-documents-codex-2026-10/work/execute-thesis-2026-10-15/work/text-json-decoder-baseline-3be9fe`
- Product patch: `text_json_decoder_product.patch`, SHA-256 `95f4d413f86f0905421500f182e372e55a3c30163dc3364810ca71386ca03f36`
- Public decoder harness: `tests/output_schema_text_json_decode.rs` in both worktrees, SHA-256 `2293bc1e04377fdf23b430655ec51bf41643723f9133c3cf181136edbc6c146d`
- Full 5,917-path source map: `source-fingerprints.json`, SHA-256 `3e414d06b9f1fa78f0dc70df25b958d439329636450fd67141d36bd7a140c57a`
- Build driver: `/Users/earlcameron/Documents/Codex/2026-10-15/users-earlcameron-documents-codex-2026-10/work/execute-thesis-2026-10-15/work/build_text_json_decoder_pair.py`, SHA-256 `2c25beda9297988a24a86af1dc411c588c29e9c2f0a3c55f8996d22db9c44c06`
- Paired runner: `/Users/earlcameron/Documents/Codex/2026-10-15/users-earlcameron-documents-codex-2026-10/work/execute-thesis-2026-10-15/work/text-json-decoder-paired-runner/text_json_decoder_pair.py`, SHA-256 `6dd826353dff66d318ae57b9cddaf7e08eeb529210f82f6f421af47992e893eb`
- Offline runner tests: `/Users/earlcameron/Documents/Codex/2026-10-15/users-earlcameron-documents-codex-2026-10/work/execute-thesis-2026-10-15/work/text-json-decoder-paired-runner/test_text_json_decoder_pair.py`, SHA-256 `27aa417b0a3a8933ebc680daae6de7a848ea0d893f50a5f047a17e5be875e9cf`

The harness calls the public `flow::contracts::OutputSchema::decode` API on one fixed 4,096-byte ASCII string encoded as JSON. It performs 10,000 decodes in each of three in-process samples. Every returned string is checked byte-for-byte and dropped during the timed loop. Schema and fixture construction, JSON-payload serialization, hashing, and report writing occur outside that loop. The paired schedule uses three warmup pairs, then 48 balanced AB/BA pairs; each invocation retains its three raw sample durations and semantic checks. The case is intentionally narrow and does not establish CLI startup or broad runtime improvement.

## Root-owned validation and measurement commands

The source scout did not build or execute product code. After native formatting, strict Clippy, correctness tests, and root-controlled clean release builds succeed, the runner can write a fresh preregistration and then start the fixed trial:

```sh
python3 /Users/earlcameron/Documents/Codex/2026-10-15/users-earlcameron-documents-codex-2026-10/work/execute-thesis-2026-10-15/work/build_text_json_decoder_pair.py
python3 /Users/earlcameron/Documents/Codex/2026-10-15/users-earlcameron-documents-codex-2026-10/work/execute-thesis-2026-10-15/work/text-json-decoder-paired-runner/text_json_decoder_pair.py prepare --preregistration /Users/earlcameron/Documents/Codex/2026-10-15/users-earlcameron-documents-codex-2026-10/work/execute-thesis-2026-10-15/evidence/text-json-decoder/preregistration-attempt-1.json
python3 /Users/earlcameron/Documents/Codex/2026-10-15/users-earlcameron-documents-codex-2026-10/work/execute-thesis-2026-10-15/work/text-json-decoder-paired-runner/text_json_decoder_pair.py run --preregistration /Users/earlcameron/Documents/Codex/2026-10-15/users-earlcameron-documents-codex-2026-10/work/execute-thesis-2026-10-15/evidence/text-json-decoder/preregistration-attempt-1.json --report-dir /Users/earlcameron/Documents/Codex/2026-10-15/users-earlcameron-documents-codex-2026-10/work/execute-thesis-2026-10-15/evidence/text-json-decoder/paired-run-attempt-1
```

The build driver uses the campaign's pinned toolchain, exact inherited environment allowlist, shared target, per-arm release cleanup, and immutable saved test binaries. The runner validates full source fingerprints, build JSONL artifact rows, receipts, and binary hashes before launching any invocation; it refuses existing preregistration/report paths and performs no retry.

The prior pre-format and pre-median runner snapshots are preserved in their adjacent draft directories with receipts. The current median summarizer retains half-nanosecond medians rather than truncating them.

## Attempt-1 smoke validation

The first baseline correctness-smoke invocation executed the ignored Rust harness test successfully (one test passed and three raw samples were retained), but the external report validator rejected the run because the runner’s expected fixture description differed from the harness’s identical-workload description. The runner now pins the exact harness string and has regression tests for both source agreement and rejection of the stale raw descriptor. This attempt did not start the paired timing trial; the candidate smoke was not attempted. No performance result is available from this attempt. The failure receipt and raw baseline output remain preserved under `evidence/text-json-decoder/correctness-smokes-attempt-1/`.
