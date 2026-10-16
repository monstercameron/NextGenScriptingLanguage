# Typed JSON carry: structured-output benchmark

This is a separate mechanistic trial for the typed JSON carry path. It uses the exact product patch from the earlier scalar typed-carry experiment and a new, fixed `csv.parse` public-runtime workload. Nothing here changes CLI defaults or product source.

The workload parses the same 4,063-byte, 64-row, four-column CSV 1,000 times through `runtime::run`, then JSON-encodes the last parsed table. The DSL rejects reusing an already-defined result name, so the program binds `table0000` through `table0999`; all 1,000 parsed tables remain live during the run. That memory cost is part of this workload and limits what the result can claim. The expected table, compact JSON bytes/hex/SHA-256, CSV bytes/SHA-256, program bytes/SHA-256, and exact per-call JSON stdout byte count (including the applet's line feed) are independently pinned in the harness and runner. The harness also validates every sample's final output, zero child processes/cache hits, 256 retained successful status-0 records, and exactly 744 dropped execution records/stages.

Source is frozen against base commit `e5fa5162b9bdca608f7f3c9d0d273db40a93c17b`. The baseline has no product delta; the candidate contains only the six-file product patch `typed_json_carry_product_v3.patch`. Both arms carry the identical ignored Rust harness. Full 2,197-file source maps, tracked/staged diff hashes, and the exact source roots are in `source-fingerprints.json`.

The builder is source-only preparation code until the root agent runs it. It captures the allowlisted build environment and Cargo configuration fingerprints, cleans the shared campaign target once per arm, runs only the release `--no-run` build for `typed_json_carry_structured`, and preserves each arm's test binary in an immutable evidence copy. It does not execute the test. After a reviewed successful build, the paired runner validates the receipts and source/binary pins, writes a fresh preregistration, then follows exactly three warmup pairs and 48 fixed alternating AB/BA measurement pairs. Each invocation takes three internal samples and has no retry; raw JSON, stdout/stderr, status, and failed outcomes are retained.

No structured timing has been run yet. The earlier scalar trial remains intact at `../typed-json-carry-bench/paired-run-v3-attempt-1`; it completed 102 invocations and 48 measurement pairs, with 27 wins and 21 losses and median candidate-minus-baseline delta `-42,875 ns`. Its mixed direction is retained as evidence and does not support a broad runtime-gain claim. The structured case is a separate follow-up, not a replacement or a rerun of that scalar trial.

After source review, the root can run:

```sh
python3 work/build_typed_json_carry_structured_pair.py
python3 -m unittest discover -s work/typed-json-carry-structured-paired-runner -p 'test_*.py' -v
python3 work/typed-json-carry-structured-paired-runner/typed_json_carry_structured_pair.py prepare \
  --preregistration "$PWD/evidence/typed-json-carry-structured/preregistration-try-1.json"
python3 work/typed-json-carry-structured-paired-runner/typed_json_carry_structured_pair.py run \
  --preregistration "$PWD/evidence/typed-json-carry-structured/preregistration-try-1.json" \
  --report-dir "$PWD/evidence/typed-json-carry-structured/paired-run-try-1"
```

The Python suite is an offline runner/builder contract test; it does not run Cargo or the Rust product. It currently skips only the post-build receipt check because this source preparation has no build artifacts yet.
