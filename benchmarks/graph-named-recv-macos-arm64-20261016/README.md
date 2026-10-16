# Named graph receive paired measurements on macOS arm64

This source-focused package preserves one completed local paired run on Darwin arm64. Both benchmark source checkouts were pinned to `c6f77a0cb199aedec564440a6a5b4b6170317831`; the baseline and candidate differ by the recorded transport patch while carrying the same frozen benchmark harness. The later product integration commit `81bf9ba` is not the source identity used by this measurement.

The preregistered run used 3 warmup pairs followed by 48 measured pairs, alternating baseline→candidate and candidate→baseline order with alternation restarted at the phase boundary. It retained all 102 successful invocations with no retries: 12 cases per invocation, 3 raw samples per case, 54,208 preloaded events per sample. Independent recomputation from raw attempt JSON yields 576 measured comparisons: 566 candidate improvements, 10 losses, and no ties. The per-case median-of-paired-ratios implies median reductions from 4.38% to 95.22%.

| Method | Sources | Inputs | Median candidate/baseline | Median reduction | Median paired delta (ns/event) | Improvements / losses / ties |
|---|---:|---:|---:|---:|---:|---:|
| `recv_timeout_zero` | 1 | 1 | 0.836832 | 16.32% | -17.347154 | 46 / 2 / 0 |
| `recv_timeout_zero` | 1 | 8 | 0.881152 | 11.88% | -17.696493 | 45 / 3 / 0 |
| `recv_timeout_zero` | 16 | 1 | 0.726157 | 27.38% | -53.742068 | 48 / 0 / 0 |
| `recv_timeout_zero` | 16 | 8 | 0.956203 | 4.38% | -6.183737 | 48 / 0 / 0 |
| `recv_timeout_zero` | 847 | 1 | 0.050563 | 94.94% | -3461.856414 | 48 / 0 / 0 |
| `recv_timeout_zero` | 847 | 8 | 0.341099 | 65.89% | -394.028557 | 48 / 0 / 0 |
| `recv` | 1 | 1 | 0.740115 | 25.99% | -24.149268 | 48 / 0 / 0 |
| `recv` | 1 | 8 | 0.764206 | 23.58% | -28.108157 | 47 / 1 / 0 |
| `recv` | 16 | 1 | 0.732649 | 26.74% | -48.455329 | 45 / 3 / 0 |
| `recv` | 16 | 8 | 0.935208 | 6.48% | -8.656471 | 47 / 1 / 0 |
| `recv` | 847 | 1 | 0.047785 | 95.22% | -3491.072591 | 48 / 0 / 0 |
| `recv` | 847 | 8 | 0.343542 | 65.65% | -379.955828 | 48 / 0 / 0 |

The ten measured losses are retained explicitly below. Positive delta means candidate p50 minus baseline p50 is greater than zero.

| Pair | Case | Baseline p50 (ns/event) | Candidate p50 (ns/event) | Delta (ns/event) | Candidate/baseline | Change |
|---:|---|---:|---:|---:|---:|---:|
| 1 | `recv_timeout_zero|sources=1,inputs=1` | 115.817776 | 122.937740 | 7.119964 | 1.061476 | 6.15% |
| 1 | `recv|sources=16,inputs=1` | 178.416931 | 189.420067 | 11.003136 | 1.061671 | 6.17% |
| 14 | `recv|sources=16,inputs=8` | 129.666451 | 129.847071 | 0.180619 | 1.001393 | 0.14% |
| 17 | `recv_timeout_zero|sources=1,inputs=8` | 126.223675 | 136.720281 | 10.496606 | 1.083159 | 8.32% |
| 17 | `recv|sources=16,inputs=1` | 170.593658 | 174.792761 | 4.199103 | 1.024615 | 2.46% |
| 19 | `recv|sources=1,inputs=8` | 117.375812 | 211.832958 | 94.457147 | 1.804741 | 80.47% |
| 19 | `recv|sources=16,inputs=1` | 177.178645 | 185.006512 | 7.827867 | 1.044181 | 4.42% |
| 28 | `recv_timeout_zero|sources=1,inputs=1` | 96.710818 | 101.618617 | 4.907800 | 1.050747 | 5.07% |
| 36 | `recv_timeout_zero|sources=1,inputs=8` | 139.592698 | 144.914035 | 5.321336 | 1.038120 | 3.81% |
| 38 | `recv_timeout_zero|sources=1,inputs=8` | 130.384353 | 138.712625 | 8.328273 | 1.063875 | 6.39% |

## Claim boundary

The timed region covers `NodeIo::recv` and `NodeIo::recv_timeout(Duration::ZERO)` receiving preloaded events from bounded queues. Setup, queue preload, reference-order construction, and result allocation happen before timing. The timeout case is therefore a no-wait API comparison. The Rust harness checks each observed event against its expected source, named input, sequence, revision, generation, and typed payload; it verifies the exact checksum and closed-queue EOF after the timer stops.

This is a narrow named-receive microbenchmark on a Darwin arm64 host. It is not an end-to-end `RuntimeGraphExecutor`, graph scheduling, desktop, or application performance result. For the 847-source/1-input case, the median paired candidate/baseline ratios are approximately 0.0478 for `recv` and 0.0506 for `recv_timeout_zero`; those figures must not be generalized to other graph workloads.

The recorded candidate patch is the exact benchmark arm patch and includes the ignored test harness appended to `src/graph/transport.rs`. The baseline patch adds the same harness without the candidate product refactor. Treat these paired patches and binaries as measurement provenance, not as the merge patch: product integration is tracked separately, and this result does not measure the later `81bf9ba` integration tree. Saved executable bytes are omitted from this source-only package; their exact hashes, Cargo artifact rows, and build receipts are included.

## Contents and verification

- `raw-artifacts.tar.gz` preserves the original paired report and every attempt's `started.json`, `finished.json`, raw JSON, stdout, and stderr. `raw-artifacts.manifest.json` records the SHA-256 and size of each archive file and verifies its relative path. `raw-artifacts.SHA256SUMS` pins the archive.
- `preregistration.json` is the pre-run protocol. `adversarial-audit.json` is the independent audit; it recomputed raw p50s, per-pair deltas/ratios, the summary, source identities, build receipts, and all ten losses. `result-summary.json` contains machine-readable case summaries and loss rows.
- `source/` carries both exact arm patches and transport files, the Rust harness, the Python driver and its offline tests, run plan, preparation receipt, source freeze, and build workflow. `source/source-manifest.json` records the original paths and hashes.
- `build/` carries each arm's binary receipt, Cargo JSONL, build/clean logs, and exact test listing. `build/build-manifest.json` records the saved binary SHA-256 values; executable blobs are deliberately omitted.
- `bundle.SHA256SUMS` verifies every other file in this staging package and omits only itself.

No Cargo, benchmark, API, or source-tree write was performed to prepare this package. It was assembled from retained evidence and independently recomputed raw data only.

Patch and build-log source artifacts are stored losslessly as `.xz` files. `compressed-source-artifacts.json` maps their original names and SHA-256 values to the stored files; decompression reproduces the exact reviewed bytes. Historical receipts retain the original source names and hashes.
