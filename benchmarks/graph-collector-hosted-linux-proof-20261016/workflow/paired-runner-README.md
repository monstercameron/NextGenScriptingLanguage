# Paired graph transport E2E runner

`graph_transport_e2e_pair.py` runs the frozen ignored integration test against two exact release test binaries. It does not build binaries. `prepare` pins both source trees, the shared Rust harness, receipts, matching Rust/Cargo versions and selected build environment (allowing only a target-directory difference), plus the root build driver and its environment/build logs; `run` rejects any later change to those inputs.

Cargo's artifact path is treated as build provenance: the runner checks it lexically under the recorded release `deps` directory and matches the exact `fresh: false` artifact row in that arm's pinned Cargo JSONL. It hashes and executes only the separately saved binary. Cargo may overwrite or remove the reusable artifact path during the next arm's clean/build without invalidating the already saved baseline binary.

## Frozen protocol

The sequence is 3 warmup pairs followed by 48 measured pairs, alternating AB/BA globally, so the first measured pair begins BA. Each test invocation has zero inner warmups, three raw `GraphSession` samples, a hard 120-second timeout, and no retry. Both sides of a pair are attempted; a failed pair stops subsequent execution, and every unrun slot is retained in the loss ledger. Candidate performance losses are reported separately from failed or unattempted pairs.

Every raw JSON report, sibling input fixture, sibling compact JSONL timing/status journal, stdout, stderr, start record, and finish record is kept uncompressed under the attempt directory. The runner validates the exact 4,096-record fixture, all 8,193 terminal event receipts, full payload checksums, metadata, input/output sequence ordering and uniqueness, sample journal, and exact configuration. It compares arms after validating each run, normalizing only scheduler-assigned global sequence numbers and attempt-specific absolute paths. The Rust harness checks every typed payload by value before emitting its full output checksum.

Before creating the first attempt, `run` requires at least 3 GiB free on the report filesystem. The current planning estimate is roughly 1.8–2.0 GB for 102 invocations, before small logs and metadata; the free-space gate provides headroom.

## Commands

After root-owned builds produce the two final receipts, prepare the preregistration:

```sh
python3 work/graph-transport-e2e-paired-runner/graph_transport_e2e_pair.py prepare \
  --candidate-root work/graph-collector-sleep-candidate \
  --baseline-receipt evidence/graph-transport-e2e/baseline-binary-receipt.json \
  --candidate-receipt evidence/graph-transport-e2e/candidate-binary-receipt.json \
  --harness-source evidence/graph-transport-e2e/graph_transport_e2e_bench.rs \
  --preregistration evidence/graph-transport-e2e/paired-preregistration.json
```

Only after the root agent opens the timing slot, run the preregistered sequence to a fresh report directory:

```sh
python3 work/graph-transport-e2e-paired-runner/graph_transport_e2e_pair.py run \
  --preregistration evidence/graph-transport-e2e/paired-preregistration.json \
  --report-dir evidence/graph-transport-e2e/paired-run
```

## Offline validation

```sh
python3 -m py_compile work/graph-transport-e2e-paired-runner/graph_transport_e2e_pair.py work/graph-transport-e2e-paired-runner/test_graph_transport_e2e_pair.py
python3 -m unittest -v test_graph_transport_e2e_pair.py
```

No Cargo, product build, end-to-end test invocation, or benchmark timing was run while preparing this runner.
