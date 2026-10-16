# Resolved JSON arguments paired runtime result

This publication preserves two independent, one-run fixed-workload measurements of the literal-argument runtime change. The Darwin arm64 and hosted Linux x86_64 results are shown separately; they are not pooled, averaged, or treated as a combined estimate.

| Platform / run | Candidate faster | Candidate slower | Ties | Median paired delta (candidate − baseline) | Median per-pair candidate/baseline ratio |
| --- | ---: | ---: | ---: | ---: | ---: |
| Darwin arm64 local | 47 / 48 | 1 / 48 | 0 | −180,395.5 ns | 0.8239204161 |
| Linux x86_64 hosted run 38862234332 | 48 / 48 | 0 / 48 | 0 | −165,905.5 ns | 0.8265255155 |

These are descriptive paired results for one fixed workload on two different systems. Neither establishes end-to-end, general, or cross-platform performance. All losses are retained: the Darwin slower pair is identified in `result-summary.json` and `independent-audit.json`; the hosted Linux audit retains all 48 measured pairs and confirms 48 wins, 0 losses, 0 ties.

## Timed work and source boundary

Both harnesses exercise public `runtime::run` with 1,000 in-process `core.number_add` calls. Workload parsing, registry loading, options construction, test-process startup, and report serialization are outside the measured sample; runtime validation/execution is inside. This is not a preloaded graph queue test and does not represent end-to-end application latency.

Both measured arm pairs were reconstructed from source base `b824aeaa7bccaa4d3d75d3fc6a363a99138287d4`. The hosted run used workflow/source as checked in at dispatch commit `278541e7d9a88bc55058ca133b3031675dfb84d0`; exact files and both compressed dispatch patches are preserved under `hosted-linux/source/checked-in-at-dispatch/`, alongside the applied plaintext patches and hashes. The product was later integrated at `a884f6561fd43f45e43b25a3cc2233be17ee3944`; that commit is provenance only. The benchmark results do not attest performance of that integrated tree.

## Darwin run details

The local schedule was 3 warmup pairs followed by 48 measured pairs, alternating AB/BA, with 3 raw samples per invocation; all 102 invocations succeeded. The median paired ratio is 0.8239204161427683. The one slower pair was measured pair 47, +571,750 ns. `raw-artifacts.tar.xz`, its member manifest/checksum, `independent-audit.json`, and `result-summary.json` remain the original Darwin evidence and retain their original bytes.

The local audit did not retain exact Cargo/rustc versions, so this bundle makes no toolchain version claim for the Darwin run. The two local arm binaries were distinct and their SHA-256 values are preserved in receipts, while executable blobs are omitted.

## Hosted Linux run details

The hosted Linux schedule was also 3 warmup pairs followed by 48 measured pairs with three raw samples per invocation. It ran on Ubuntu 24.04 x86_64 with kernel `6.17.0-1022-azure`; the CPU model was not recorded. All 102 invocations passed the raw oracle (`b"3"`, 1,000 completed in-process calls, zero runtime-reported child launches), and no retries were observed. The median ratio was 0.8265255154756348; ratio of arm medians was 0.8279160252186648. Cargo 1.99.1 and rustc 1.99.1 are recorded by the hosted build receipts.

`hosted-linux/evidence/` preserves the exact request and preregistration, all 102 raw attempt folders and sidecars, paired report, run status, build/source-freeze receipts, logs, and independent audit. `hosted-linux/source/source-manifest.json` and the nested `SHA256SUMS` verify dispatch source and complete hosted file membership. Executable bytes are excluded from the source-only publication; `hosted-linux/excluded-executables.json` records their sizes and SHA-256 identities from the original receipts/audit, and the original hosted CI artifact/local download retains the bytes.

## Verification

Run `python3 verify_bundle.py` from this directory. It checks the outer bundle file list, local raw archive members, source files and patches, both audit receipts, hosted subtree membership/source hashes, all 102 hosted raw attempts, exact 48-pair schedule, raw correctness, and recomputed hosted statistics. The `publication-index.json` in the parent staging directory keeps the Darwin and Linux results as separate platform rows.

This bundle was assembled from retained evidence. No Cargo/build/benchmark/timing action or primary source-tree mutation was performed during integration.
