# Line framing delimiter search: source review

Status: product candidate and paired-measurement package prepared in isolated detached worktrees at `a884f6561fd43f45e43b25a3cc2233be17ee3944`. No Cargo build, Rust test execution, benchmark execution, or timing was run by this work. Independent source review found no semantic blocker; performance remains unmeasured.

## Candidate

`src/runtime/streaming.rs::LineSink::push` scans each read chunk with `bytes.split_inclusive(|byte| *byte == b'\n')`. Replace only the delimiter search with `memchr::memchr_iter(b'\n', bytes)`, building the same inclusive borrowed slices from the monotonically increasing byte offsets. After the last match, process the remaining unterminated suffix exactly as the current iterator does. `memchr 2.8.3` is already present in `Cargo.lock` transitively through `serde_json`; the product change needs only a direct `memchr = "2.8.3"` dependency and the execute-package dependency-list entry in the lockfile. It should add no crate version or vendored source.

## Semantics that must remain unchanged

- Complete lines with empty `pending` continue to borrow the read buffer for the synchronous callback. If that callback, cancellation check, or deadline check fails, retain the complete record in `pending` before returning the error.
- When `pending` already contains a partial record, append each inclusive segment and use `emit_pending`; callback/check failures must retain the full pending bytes and record count.
- Keep the maximum-record-size check before extending or emitting, including the current saturating subtraction behavior.
- Return immediately on the same record-limit boundary and ignore the remainder of that read chunk after the limit is reached.
- Preserve binary data, CRLF bytes, empty lines, arbitrary chunk boundaries, and final unterminated EOF delivery. `finish`, callback order, cancellation/deadline checks, and stream process handling remain untouched.
- Match `split_inclusive` at the boundaries: empty input creates no segment; a newline at the final byte creates no extra empty tail. Check an unterminated suffix against the remaining limit before appending it, so an oversized suffix leaves the earlier pending bytes unchanged.

## Existing oracles and paired workload

The `streaming` unit tests exercise binary/split records, maximum length, record limits, unterminated EOF, cancellation, callback failure, and native-stage final-emission behavior. The `framing_evidence::borrowed_record_framing_matches_legacy_failures_and_bytes` differential test compares production `LineSink` with a legacy splitter over more than five thousand chunk/limit/callback/cancel/deadline combinations. Keep these oracles intact.

The fresh build plan must also save a release `--lib --no-run` test binary for each arm so the focused LineSink differential test can be run directly before timing. Validate its Cargo artifact row as a fresh optimized library-test artifact and preserve its immutable copy and receipt alongside the benchmark integration-test binary.

Use the exact existing `tests/native_stream_status_bench.rs` harness bytes (SHA-256 `7171a2cb59c6749d656ed8eb5202109c17810e22d5e41c4256daeeec926256ad`) for the public `runtime::run_stream` workload: 131,072 deterministic JSONL records, one/four/eight `/bin/cat` stages, exact output digest/count/bytes/status/launch-count checks, fixture/program setup outside the timer, and callback hashing/counting inside. Do not add a private microbenchmark or alter the workload. Baseline and candidate must use byte-identical harness files and separately saved fresh release test binaries.

## Expected comparison risks

The public workload is newline-dense JSONL with moderate record size and can show whether separator search affects the measured end-to-end path; it may also be dominated by process launch/readiness and callback hashing. Treat it as an attribution check, not proof of general stream speedups. Retain raw per-case durations and the existing exact correctness/process oracles; do not weaken them or claim improvement from a source-only estimate.

The candidate delta is frozen in `line-framing-memchr.patch` (SHA-256 `eeb7c5512fd78c0264d6d96a85b6959c8f1302ac481ab1e98047e3fb52145c94`). It adds only a direct `memchr = "2.8.3"` dependency/lock reference and changes delimiter iteration in `LineSink::push`; memchr 2.8.3 was already present transitively, so no package version or vendored source changes. The unchanged public stream harness SHA is `7171a2cb59c6749d656ed8eb5202109c17810e22d5e41c4256daeeec926256ad`.

The candidate build plan saves the baseline benchmark binary, candidate benchmark binary, and a candidate `--lib --no-run` binary. The paired runner checks Cargo JSONL artifact rows as fresh optimized release tests, copies each executable outside the shared target, and validates source fingerprints and exact bytes. Before any timing schedule it runs the existing candidate-only LineSink differential test by exact filter; a failed preflight is retained and skips timing. The benchmark protocol is fixed at three paired warmups, 48 balanced AB/BA measured pairs, three samples per case, stages 1/4/8, 131,072 JSONL records, 120 seconds per invocation, and no retries. Root retains build and timing ownership.

The offline Python contract suite is `source/test_line_framing_pair.py`; the final recorded run passed 8 checks for source/path pins, current plan/input hashes, the prepared build-driver worktrees and artifact target specs, balanced schedule, exact report oracles, Cargo artifact validation, saved-copy hashes, and the differential-test “exactly one test ran” gate. No Rust/Cargo command or benchmark binary was invoked by this preparation.

Independent review: `/root/native_observer_review` read the scoped source delta and found no semantic blocker. The reviewer did not run Cargo, tests, or timing. Its review confirms the retained >5,000-case `framing_evidence::borrowed_record_framing_matches_legacy_failures_and_bytes` oracle covers split behavior and pending/error semantics.
