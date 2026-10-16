# Typed-JSON carry V3 paired runner

This is the active V3 source-only paired runner. The V2 runner remains separately preserved in `work/typed-json-carry-paired-runner/`, along with the V2 product, source map, and build driver.

V3 is based on commit `e5fa5162b9bdca608f7f3c9d0d273db40a93c17b`. Its runtime and other production-file diffs are byte-identical to the V2 product. The only cumulative change from V2 is the Unix deep-cache test fixture creating its cache directory with `DirBuilderExt::mode(0o700)`, satisfying the runtime's existing current-user/private-directory precondition. No runtime or security-policy code changes.

Frozen source identities: product patch `99650ce4d9c2a2d4c8418e52bc3dbbda2f99a2a745e54c48332f9dc6a1958bb9`; formatted shared harness source `ba0336a0749d86e4b4f28231daea6fa36cb85a3020f93302bb0b94d83260f68b`; harness patch `3a227a33319519bbbcd64f29678c4d3879bba0121383f2cfc8afe9511c4d1141`; full 2,197-file source map `cd64998235087d2fb448cc8533b99c03c007204381de80b08d7046b622652e9e`; build driver `d6c09a51b2e21d736d2d46e470e7e3b6c42f9073e99e92ecd3077eecfd3c14d3`. The unique build evidence location is `evidence/typed-json-carry-bench/build-attempt-v3`; the existing shared Cargo dependency target is `work/perf-target-candidate`.

The fixed protocol is three warmup pairs followed by 48 measured AB/BA pairs (24 of each measured order), three in-process samples per invocation, 120 seconds per invocation, and no retries. Every raw report, stdout, stderr, exit status, semantic oracle, and provenance hash is retained. The runner requires the same complete catalog workload digest in both arms. The metric is limited to this public-runtime microbenchmark and makes no CLI-startup or general-speedup claim.

The runner never builds. `prepare` validates source maps, diffs, clean-release Cargo commands and receipts, immutable optimized test binaries, helper metadata and placement, logs, and test listings before writing a fresh preregistration. `run` revalidates those inputs and executes only `literal_number_add_1000`.

After root's local V3 source/build validation gate is complete, from the campaign root use new absolute destinations:

```sh
python3 -B work/typed-json-carry-v3-paired-runner/typed_json_carry_v3_pair.py prepare \
  --preregistration /absolute/new/location/typed-carry-v3-prereg.json
python3 -B work/typed-json-carry-v3-paired-runner/typed_json_carry_v3_pair.py run \
  --preregistration /absolute/new/location/typed-carry-v3-prereg.json \
  --report-dir /absolute/new/location/typed-carry-v3-report
```

This draft is source-only and must not be used until root completes local compile/test review and the fresh V3 build receipts exist.

Build-target note: the V3 build driver uses the campaign shared target directory, and runs `cargo clean --release -p execute` separately for each arm before building. This reuses the existing dependency cache without reusing the package artifacts under test. The benchmark arms remain source-pinned and their build receipts record the shared target path. Because the second arm can replace or remove first-arm Cargo outputs, the immutable per-arm test and helper copies in the evidence directory are authoritative. The runner binds each Cargo JSON artifact row and its original executable path lexically, but does not require that shared-target path to retain the earlier arm's bytes after the next clean/build.

Build provenance note: build subprocesses receive only `PATH`, `HOME`, `TMPDIR`, `LANG`, `LC_ALL`, `TZ`, `SDKROOT`, `DEVELOPER_DIR`, and `MACOSX_DEPLOYMENT_TARGET` from the host. Cargo home, rustup home, toolchain, shared target, and four build jobs are fixed. The exact resulting environment is recorded before either clean/build; unlisted host variables are not forwarded. SHA-256 fingerprints bind `config` and `config.toml` in `CARGO_HOME` and every `.cargo` directory from both source roots through filesystem root. Each arm records the fingerprint before and after its clean/build/listing phase, and the pair receipt records the full-pair before/after values. Configuration contents are not copied into evidence.
The shared harness was updated after strict native all-target Clippy identified an always-true `sample_index >= WARMUPS` branch with the fixed `WARMUPS = 0`. The loop now runs exactly the three measured samples and retains the same output and per-sample semantic checks. This changes benchmark-harness control flow only; it does not change the 1,000-call workload or product patch. The updated shared harness passed the pinned rustfmt check. Root completed the V3 build/test and correctness-smoke gate. The current offline paired-runner suite has 18 passing tests, including the real campaign-root/build-receipt path and full source-fingerprint validation, build-environment scrubbing, Cargo configuration drift checks, and acceptance of archived arm copies when later builds replace shared-target files. The fixed paired benchmark has not started; it still requires a fresh preregistration.
