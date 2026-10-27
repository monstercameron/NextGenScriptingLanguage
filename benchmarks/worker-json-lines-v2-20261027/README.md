# Worker JSON-lines v2 acceptance evidence

This report records the paired 1 MiB v1/v2 worker latency check on the exact
`origin/main` source revision `743fcf98f1ccc1724623e994a93c3a01f3910dab`.
The complete build and test output is in [`paired-release.log`](paired-release.log).

## Result

The ignored release benchmark passed with 15 measured pairs after three warmup
pairs. It verified that every response exactly matched the 1,048,576-byte input.
The v1 worker used the legacy integer-array protocol with an 8,388,608-byte
frame limit; v2 used Base64 with a 1,400,000-byte frame limit for both request
and response. Successful v2 invocations under that cap establish that both
observed frames stayed within 1.4 MB.

| Protocol | Median round trip | Measured samples (µs) |
| --- | ---: | --- |
| v1 | 152,942 µs | 152315, 152628, 152650, 152661, 152662, 152903, 152913, 152942, 153035, 153165, 153217, 153369, 153480, 153606, 153911 |
| v2 | 13,429 µs | 13292, 13371, 13375, 13388, 13402, 13403, 13419, 13429, 13440, 13482, 13541, 13561, 13602, 13616, 13976 |

Median speedup: **11.39×** (`152942 / 13429`), exceeding the 3× acceptance
threshold. Each pair measured one persistent-worker `invoke` per protocol;
three prior alternating calls warmed each worker. The test source asserts the
threshold and checks output equality for every sample.

## Reproduction

From a clean checkout of the source revision below, run:

```sh
CARGO_BUILD_JOBS=1 cargo test --locked --release --features full,test-fixtures \
  --test worker_backend \
  v2_worker_round_trip_is_at_least_three_times_faster_than_v1 \
  -- --ignored --nocapture
```

The benchmark used that command with Cargo at
`/Users/earlcameron/.cargo/bin/cargo` and Rust 1.99.1. No other Cargo build or
benchmark ran concurrently.

## Provenance

- Source commit: `743fcf98f1ccc1724623e994a93c3a01f3910dab`; working tree clean.
- Host: macOS 15.6, Darwin 25.6.0, arm64 (`aarch64-apple-darwin`).
- Rust: `rustc 1.99.1 (58cf8f9c3 2026-10-13)`; release profile.
- Features: `full,test-fixtures`; `CARGO_BUILD_JOBS=1`; locked dependencies.
- Test binary `target/release/deps/worker_backend-0becf0b9f57f5f8d`, 6,713,872 bytes, SHA-256 `2bcffba6969cc1dce00ee7872edbe468cb2b6b9d735dbc1139faf9aa6a4842eb`.
- Native fixture `target/release/native-fixture`, 453,712 bytes, SHA-256 `3e6461ba96af752835d54d6e5399d2d738e74a58e610a368d8032de5cd4cb839`.
- Relevant source SHA-256: `tests/worker_backend.rs` `d4234e37187b3f415330514bf30d0c5f7045903d56e4be38de4d951b0bbbe404`; `src/runtime/worker_backend.rs` `9f67d24b0608130f2b44f060c012021055f782e5c458195f066dbde6be4e41bc`; `tests/fixtures/native_fixture.rs` `f67942d4bc5142e219828b198cc97393118d9d0ab528bfb5a0d8062a4b6da3d0`.
- `base64` is already a direct dependency (`0.23`) locked to `0.23.1` in both the root and browser lockfiles.

The attached log is the filtered paired-benchmark invocation; it reports **1
passed, 0 failed, 24 filtered out**. Separately, the complete release
`worker_backend` integration target passed **24 tests, 0 failed, 1 ignored**.
