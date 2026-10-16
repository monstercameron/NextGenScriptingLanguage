# Graph `NodeIo::emit` paired measurements

This bundle contains two separate 48-pair measurements of the same `NodeIo::emit` candidate: a local macOS ARM64 run and an independent hosted Linux x86_64 replication. Both compare baseline commit `b78693294169c22892bb4f78539348a5755992ad` and pin the only changed product source file, `src/graph/transport.rs`, at baseline SHA-256 `db5430bd3a8cd25ed5e20bac4395b56814ec1ad6e2189d4e89d2f152c180c17f` and candidate SHA-256 `7cc3497231fd60d7c31ed7c98390281df4be0913398e7ccee1e6ffe75f4c48d3`. The identical benchmark harness SHA-256 is `098db9038e8e8cc684eb6533d3fb60aaa33184f3e16cc9e9a028ac2c860a48f3`.

Each platform used 3 warmup pairs followed by 48 measured pairs; all 102 invocations per platform succeeded. There were 4 fixed cases, 3 timed samples per case per invocation, and no ties. The table preserves platform-specific losses and median paired candidate-minus-baseline deltas (positive values are slowdowns); the two hosts are not pooled.

| Case | macOS wins/losses | macOS median delta (ns) | Linux wins/losses | Linux median delta (ns) |
|---|---:|---:|---:|---:|
| `fanout_4_text` | 29 / 19 | -4,563 | 41 / 7 | -6,136.5 |
| `fanout_4_typed_record` | 42 / 6 | -8,479 | 48 / 0 | -12,253 |
| `fanout_8_integer` | 21 / 27 | +2,520.5 | 37 / 11 | -12,494 |
| `single_sink_integer` | 48 / 0 | -28,438 | 47 / 1 | -25,006.5 |

The local run therefore retained 52 losses across 192 case-pair comparisons; Linux retained 19 across 192. In particular, the macOS `fanout_8_integer` case had a positive median paired delta. These results describe four bounded, preallocated synthetic `NodeIo::emit` setups. They do not measure full graph execution, producer contention, scheduling, adapters, or application workloads, and they do not establish an end-to-end speedup.

## Included evidence

- `raw-measurements.tar.gz` preserves the complete local paired run and complete downloaded Linux artifact, including raw samples, start/finish records, stdout/stderr, reports, audits, and hosted build binaries.
- `raw-measurements.manifest.json` records each archive member hash, byte count, and path-safety result; `raw-measurements.SHA256SUMS` hashes the archive.
- `preregistrations/` contains the local plan, hosted dispatch plan, and the hosted artifact's bound plan. `audits/` preserves both independent audits and the Linux audit checksum list.
- `identities/` records frozen source/harness fingerprints and exact local and hosted baseline/candidate binary hashes. Local executables are represented by their receipts; the hosted archive retains its original binaries.
- `source/` contains the frozen local product/harness patches and the exact hosted baseline/candidate patch material and runners.
- `verification.json` records the static report-to-plan and audit bindings, dispatch-pinned source hashes, and a full archive-member hash/path-safety check.
- `bundle.SHA256SUMS` verifies all bundle files except itself.

The local Mac timing used the attempt-2 binaries recorded in the receipts, after the harness-only Clippy correction; the earlier build attempt was not used for the paired result. This run is the local result and Linux is a separate replication. No cross-platform aggregate is claimed.
