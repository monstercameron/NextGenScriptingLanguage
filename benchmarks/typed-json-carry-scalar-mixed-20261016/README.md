# Typed JSON carry: scalar mixed trial

This publication preserves one fixed public-runtime comparison of a proposed typed-JSON carry optimization against its baseline. The candidate is **not landed**, and this result does **not** support a robust speedup claim.

## Result

The workload was 1,000 literal `number.add` calls, measured as the median of three in-process `runtime::run` durations per invocation. The fixed schedule was three warmup pairs followed by 48 measured AB/BA pairs, with no retries. All 102 arm attempts succeeded, with no timeout and no missing raw report; all semantic oracles matched.

| Measure | Result |
| --- | ---: |
| Baseline arm median | 1,848,604 ns |
| Candidate arm median | 1,796,500 ns |
| Paired median delta (candidate − baseline) | −42,875 ns |
| Paired median ratio | 0.97568 |
| Measured pair outcomes | 27 wins, 21 losses, 0 ties |
| AB order | 10 wins / 14 losses; median ratio 1.0181 |
| BA order | 17 wins / 7 losses; median ratio 0.9452 |

The overall estimate is small and order-dependent. The candidate lost 14 of 24 AB pairs but won 17 of 24 BA pairs; the AB median delta was positive and the BA median delta negative. The independent audit concluded this narrow same-host measurement does not establish a robust general speedup. The candidate remains an unlanded experimental patch.

## Workload and boundaries

The fixed test is `literal_number_add_1000`: the source builds and runs 1,000 literal `number.add` operations using operands 1 and 2. Each invocation requires exact output byte `0x33` (`3`), zero child-process launches, zero cache hits, 256 retained successful status-0 records (two stdout bytes per record), and 744 dropped execution records and stages. The timer directly surrounds `runtime::run`; registry/source setup and result destruction are outside the timed interval. There is no claim about CLI startup, cold start, sustained load, other applets, general performance, or user productivity.

The trial ran on a shared Apple arm64 Darwin desktop. The before-run process snapshot is preserved at `provenance/host-observations-before-v3-attempt-1.json`; it explicitly notes background processes and that this was not an in-timer profile. Only `LC_ALL=C`, `TZ=UTC`, and the absolute report-path override were journaled; other parent environment variables were inherited. This is a finite, host-specific measurement.

A separate structured-CSV trial is not included in this scalar trial and must not be pooled with these results.

## Reproduction and provenance

`protocol/paired-preregistration-v3-attempt-2.json` records the frozen schedule and workload. `raw/paired-run-v3-attempt-1/` contains the full paired report plus every one of the 102 attempt directories, including the per-attempt raw report, stdout/stderr, and started/finished journals. `provenance/paired-run-v3-attempt-1-independent-audit.json` preserves the independent raw/oracle/provenance audit. `source/source-fingerprints-v3.json` carries the full 2,197-path per-arm source maps; `source/source-map.json` maps the source snapshots and artifact hashes in this bundle. Candidate and baseline snapshots include the changed source/test paths, Cargo manifest/lockfile, cargo configuration, and identical benchmark harness source.

The saved release test binaries for both arms and their sibling `execute-applet` helper copies are preserved under `artifacts/binaries/`; they are evidence artifacts and this package verifier does not execute them. Other regression test binaries are hash-only; their build receipts and hashes are retained in `provenance/build/other-regression-binaries-hash-only.json`. Build receipts record Cargo's `fresh:false` artifacts, source fingerprint identities, and tool/configuration provenance.

The pretrial strict Clippy first pass failed on a redundant `sample_index >= WARMUPS` check in the zero-warmup harness. That failure is retained alongside the source-only correction receipt and successful Clippy rerun. Final source also passed the recorded library regression run (309 passed, 20 ignored), typed-contract integration tests (9 passed), and baseline/candidate benchmark smoke checks. These pretrial checks are separate from the 102 paired timing attempts; the paired schedule itself had no retries.

The independent audit report is included, but no standalone independent-auditor script was present in the campaign evidence tree. `verify_bundle.py` verifies this bundle's inventory, hashes, and source-map-to-snapshot bindings; it does not re-run that audit.

Verify the publication with:

```sh
python3 verify_bundle.py
```
