# Hosted `recv_any` paired measurement

This bundle preserves the hosted paired run **38851233022** comparing baseline `b78693294169c22892bb4f78539348a5755992ad` with candidate `c6f77a0cb199aedec564440a6a5b4b6170317831`. The result is limited to `NodeIo::recv_any` in an x86_64 Linux in-process microbenchmark with preloaded bounded queues. It does not support an end-to-end graph/runtime or general application speedup claim.

The performance and native-CI receipts are pinned to candidate `c6f77a0cb199aedec564440a6a5b4b6170317831`. That candidate was subsequently integrated into `main` as `0735e4bf2f10b4d366bd31cd0140acd574b80ee4`; this later integration SHA is recorded separately and is not the source identity used by those two hosted runs.

The preregistered design used 3 warmup pairs followed by 48 measured pairs, alternating baseline→candidate and candidate→baseline order. Each invocation covered 6 source/input shapes, with 54,208 events per case and 3 raw samples per case. All 102 invocations succeeded; all 612 raw cases and 1,836 samples were validated. Across the 48 paired repetitions and 6 cases there were 288 comparisons: 288 candidate wins, 0 ties, and 0 losses.

| Sources | Inputs | Median candidate/baseline p50 | Median improvement |
|---:|---:|---:|---:|
| 1 | 1 | 0.8059 | 19.41% |
| 1 | 8 | 0.7287 | 27.13% |
| 16 | 1 | 0.8014 | 19.86% |
| 16 | 8 | 0.6362 | 36.38% |
| 847 | 1 | 0.8485 | 15.15% |
| 847 | 8 | 0.6983 | 30.17% |

The timer covered known-count `recv_any` calls and appends to a preallocated result vector. Queue creation and preload, expected-order construction, event validation/checksum, and EOF were outside the timed region. The paired measurements reduce order bias, but do not eliminate shared-runner noise. Exploratory local measurements are not pooled into or used to qualify this hosted result.

## Evidence files

- `raw-artifacts.tar.gz` contains the downloaded hosted artifact tree and its original `SHA256SUMS` file. `raw-artifacts.manifest.json` records and verifies every member hash and safe relative path. `raw-artifacts.SHA256SUMS` verifies the archive itself; `bundle.SHA256SUMS` verifies every other bundle file and omits only itself.
- `preregistration.json` is the pre-dispatch plan.
- `adversarial-audit.json` is the independent raw-artifact audit; `adversarial-SHA256SUMS.txt` is its original checksum list with paths relative to the campaign's `evidence` directory.
- `source/graph_recv_any_benchmark.patch` and `source/graph-perf-benchmark-final.rs` preserve the exact pinned benchmark patch (SHA-256 `749f639c07957e20966981ca43ea824c078781d7f1a588264fa9252ff0162efe`) and harness source (SHA-256 `87eb3b72e27757d3f71977fa9d47d6adf5ff028504c3cb3da858656c350ce891`).
- `native-ci/native-success.json` records the five-job native CI success for exact candidate source `c6f77a0cb199aedec564440a6a5b4b6170317831` (run 38849295457). That run is separate from the hosted benchmark workflow.

The benchmark source pin and independent audit identify the sole product-source difference as `src/graph/transport.rs`; the benchmark harness is added only to run the ignored microbenchmark. This evidence concerns those pinned source revisions and does not automatically qualify later integration changes.
