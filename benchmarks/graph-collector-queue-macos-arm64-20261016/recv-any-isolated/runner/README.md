# recv_any paired end-to-end runner

This runner compares the shared collector-progress change against the same change plus the single-queue `recv_any` Condvar fast path. Both detached arms use base `a884f6561fd43f45e43b25a3cc2233be17ee3944` and the same frozen integration harness. The runner checks exact fixture bytes, typed record payload checksums, full input/output event ordering and identity, source status, sensitivity and remote metadata, source hashes, harness hashes, saved-binary hashes, and build receipts before it accepts a pair.

The timed operation is the harness's complete `GraphSession::start` to `run_to_completion` interval. It uses a real finite `/bin/cat` `NativeStream`, natural EOF, typed JSON decoding, bounded lossless fanout to two custom pass-through nodes that call `recv_any` on one input queue each, and terminal collection. All exact payload, ordering, and metadata checks run after the timed interval. The harness fixture and oracle are prepared outside that interval.

Do not start a build or run until root opens the build/timing slot. The source-build driver uses the existing campaign cache at `work/perf-target-candidate` as one shared `CARGO_TARGET_DIR` and runs `cargo clean --release --package execute` before each arm. This reuses dependency artifacts while cleaning the `execute` package's release outputs. It saves and hashes the baseline test executable and receipt before cleaning for the candidate, so the second clean cannot replace the first arm's measured binary. Build environment, Cargo JSONL, clean/build logs, and test listings are retained in `evidence/graph-recv-any-single-queue/`.

From the campaign directory, root can run the source-build driver after opening the slot:

```sh
python3 work/build_graph_recv_any_e2e_pair.py
```

After both receipts exist and have been reviewed, prepare a new preregistration path:

```sh
python3 work/graph-recv-any-e2e-paired-runner/graph_recv_any_e2e_pair.py prepare \
  --candidate-root "$PWD/work/graph-recv-any-collector-queue-a884" \
  --baseline-receipt "$PWD/evidence/graph-recv-any-single-queue/baseline-binary-receipt.json" \
  --candidate-receipt "$PWD/evidence/graph-recv-any-single-queue/candidate-binary-receipt.json" \
  --harness-source "$PWD/evidence/graph-recv-any-single-queue/graph_recv_any_e2e_bench.rs" \
  --preregistration "$PWD/evidence/graph-recv-any-single-queue/paired-preregistration.json"
```

Before starting, ensure the final report directory does not exist and the containing filesystem has at least 3 GiB free. The runner preregisters three warmup pairs and 48 measured pairs in alternating AB/BA order, runs three raw samples per arm invocation, caps each invocation at 120 seconds, never retries, and records every attempt, loss, and not-attempted slot. Give every run a fresh report directory:

```sh
python3 work/graph-recv-any-e2e-paired-runner/graph_recv_any_e2e_pair.py run \
  --preregistration "$PWD/evidence/graph-recv-any-single-queue/paired-preregistration.json" \
  --report-dir "$PWD/evidence/graph-recv-any-single-queue/paired-run-YYYYMMDDTHHMMSSZ"
```

The report path and all fixture, journal, stdout, stderr, failure, and loss evidence are retained without compression or retry.


This is the v2 source freeze. Rust sources were formatted with Rustfmt edition 2021 only; the queue assertion and harness imports are formatting-only changes. Active source/patch hashes are recorded in `evidence/graph-recv-any-single-queue/paired-runner-source-freeze.json`. The original source snapshot and paired-runner freeze remain under `*-v1` filenames.
