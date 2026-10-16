# Payload-size counter: local and hosted paired evidence

This bundle preserves two local macOS ARM64 measurements for the payload-size counter change. The baseline is commit `69a9e74abcdf09efd8ecb83e4b7f1a8d5f30488a`; both arms are based on that revision. In the frozen `src/graph/transport.rs`, the baseline file SHA-256 is `36e59989fc8abe3afeeab0922792cfb4fff1110ef234b6e8946c9930c54dc056` and the candidate file SHA-256 is `5975e969cf68e8b5d7cc5c11548d0b2234477f397697da81760c499d3ccb3927`. The candidate patch and harness sources are preserved under `source/`; source freeze and build receipts record the binary identities.

Each local comparison used 3 warmup pairs, 48 measured pairs, and 3 raw samples per case. All 102 invocations succeeded for each measurement. The tables use the independent audit's ratio of candidate to baseline median-of-pair-medians. Loss counts are retained from all 48 pair comparisons, with positive candidate-minus-baseline deltas treated as losses.

## Direct serialized-size counter

This is the isolated `payload_size` counter over four fixed JSON value shapes, with 4,096 calls per sample and exact serialized-byte/hash oracles.

| Case | Candidate/baseline ratio | Relative change | Wins/losses |
|---|---:|---:|---:|
| `escape_heavy_unicode_control_text` | 0.5324 | -46.8% | 48 / 0 |
| `nested_record` | 0.4088 | -59.1% | 48 / 0 |
| `scalar_integer` | 0.2051 | -79.5% | 48 / 0 |
| `text_4k` | 0.8015 | -19.8% | 48 / 0 |

The direct microbenchmark had 192 wins and no losses. It is a narrow serializer-size result, not a whole-runtime speed claim.

## Full bounded emit path

This separate measurement uses four preallocated synthetic `NodeIo::emit` queue cases. All four case medians favor the candidate, while the per-pair losses remain visible below.

| Case | Candidate/baseline ratio | Relative change | Wins/losses |
|---|---:|---:|---:|
| `fanout_4_text` | 0.9011 | -9.9% | 29 / 19 |
| `fanout_4_typed_record` | 0.9686 | -3.1% | 35 / 13 |
| `fanout_8_integer` | 0.9358 | -6.4% | 31 / 17 |
| `single_sink_integer` | 0.8932 | -10.7% | 42 / 6 |

This run retained 55 losses across 192 case-pair comparisons. It measures the fixed preallocated emit setups only; it does not measure graph scheduling, external transport, producer contention, application workloads, allocation counts, or peak memory. Direct-size and full-emit results are separate and are not pooled.

## Hosted Linux full-emit replication

The separately preregistered hosted run `38854235381` completed on Linux x86_64 against the same `69a9e74abcdf09efd8ecb83e4b7f1a8d5f30488a` source base. It used 3 warmup pairs and 48 measured pairs; all 102 invocations succeeded. The independent static audit found the frozen inputs, binary hashes, all 48 pair oracles, all attempt artifacts, and summary calculations internally consistent. It did not rerun the binaries.

The table distinguishes the median of the 48 candidate/baseline paired ratios from the ratio of candidate and baseline arm medians. The former is the preregistered paired-ratio summary; the latter is reported separately because they differ.

| Case | Median of paired ratios | Ratio of arm medians | Faster/slower pairs |
|---|---:|---:|---:|
| `single_sink_integer` | 0.9521 | 0.9697 | 37 / 11 |
| `fanout_8_integer` | 0.9835 | 0.9920 | 30 / 18 |
| `fanout_4_text` | 0.9333 | 0.9383 | 47 / 1 |
| `fanout_4_typed_record` | 0.9450 | 0.9444 | 44 / 4 |

All four arm-median ratios favor the candidate, but the run retains 34 slower pairs across 192 comparisons. `fanout_8_integer` is especially weak: its arm-median ratio is 0.9920 (about 0.8% lower) and 18 of 48 pairs are slower. These results cover the fixed synthetic, preallocated, lossless `NodeIo::emit` cases only. The audit does not establish CPU-model representativeness, end-to-end graph or application performance, or production throughput. This is not an isolated direct-serialization-size measurement; direct-size remains local macOS evidence only.

- `raw-measurements.tar.gz` contains both complete local paired-run directories, per-attempt raw JSON, start/finish records, stdout/stderr, reports, and the original run logs.
- `hosted-linux-38854235381/artifact/` contains the complete downloaded hosted artifact; all original files are recoverable. `.log` files are stored as lossless `.xz` streams, with original names and both original/stored hashes in `hosted-linux-38854235381/artifact.manifest.json`.
- `hosted-linux-38854235381/audit/payload-size-hosted-audit-38854235381.json` is the independent static audit (SHA-256 `d37787de6eea7ceb7045fae9e1e8104d887145cbb14a00df1f2a0282b4b7932f`).
- `compressed-files.manifest.json` maps every bundled patch and log to its original filename, decoded SHA-256, and stored XZ SHA-256. Existing dispatch-pinned `.xz` patches are retained byte-for-byte.
- `raw-measurements.manifest.json` records every archive member's hash and safe relative path; `raw-measurements.SHA256SUMS` hashes the archive.
- `preregistrations/` includes the bound direct-size and full-emit plans, the explicitly superseded never-run plan, and the separate hosted dispatch plan.
- `audits/local-independent-audit.json` is the independent audit of the local raw reports and source/oracle identities.
- `identities/` contains the local frozen source and binary receipts, selected build logs, and test lists; local executable binaries are represented by their SHA-256 receipts. The hosted run's binaries and build receipts are preserved in its downloaded artifact.
- `source/` preserves both full source patches, the authoritative product and harness patches, exact Rust harness sources, runners, and hosted-profile support files, including the shared emit patches and the separate payload-size patches pinned by the hosted dispatch plan.
- `verification.json` records report-to-preregistration and audit bindings, source and binary identities, hosted dispatch pins, and full archive-member checks. The hosted result's report and generated preregistration are bound by the remote audit; the local dispatch registration is also included, but the hosted report itself does not hash those local registration bytes.
- `bundle.SHA256SUMS` verifies every bundle file except itself.
