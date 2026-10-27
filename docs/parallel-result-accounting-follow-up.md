# Parallel result accounting

## Current bounds

`parallel for each` charges captured stdout and stderr in raw bytes. Native
commands reserve from one shared group budget as bytes arrive. Other backends
retain a conservative per-worker cap and charge their completed buffers before
publishing a result. Restored element output counts against the budget before
new elements start.

For each-result records, schema version 3 stores valid UTF-8 bytes directly as
strings and arbitrary bytes as base64. This removes the duplicate UTF-8 text
field from version 2. Checkpoint sizing understands `u8_array`, `utf8`, and
valid base64 fields as raw bytes while retaining compatibility with older
records. The encoded record is bounded by 16 times the raw output limit, plus
512 bytes per item and 1 KiB. This covers the 10-byte redaction marker replacing
a one-byte secret, JSON escaping of unredacted text, base64 encoding for invalid
UTF-8, and bounded item metadata. Static metadata is checked before any element
starts.

Regression tests cover the reported multi-element native output, native and
in-process captures totaling 90% of the raw limit, binary output, stderr
redaction, and checkpoint sizing for old and new encodings. The RSS benchmark
is `python3 benchmarks/parallel_each_rss.py target/release/execute` and checks
10,000 elements with 1 KiB output against 64 MiB.

## Validation status

The rebased candidate was built with `CARGO_BUILD_JOBS=1 cargo build --release
--bin execute` and measured on macOS 26.6.2 arm64 using the benchmark command
above. Three runs exited successfully with peaks of 65,110,016, 65,421,312,
and 65,224,704 bytes against the 67,108,864-byte limit. The benchmark passed
on this Mac; Windows and Linux measurements remain unverified. A Windows
native-child regression is included in the explicit Windows CI test list.

Completed buffers from in-process, worker and remote backends are still
admitted after their effects finish; providing incremental shared admission
for those backends would require a budget-aware producer or bounded response
protocol. The final `RunResult.output` still materializes serialized pipeline
data when a program explicitly emits the full result record.
