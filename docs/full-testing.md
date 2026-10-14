# Full test audit — Execute 0.6.1

The audit found and fixed two CLI panics, then verified the rebuilt runtime with
unit/integration tests, seeded mutation trials, real terminal interaction,
source-coverage instrumentation, and repeated native-process lifecycle probes.
This is broad measured coverage, not a claim that every possible execution works.

[Machine-readable receipt](full-validation-v0.6.1.json) ·
[Source coverage](coverage-v0.6.1-macos-arm64.json) ·
[Raw logs](validation-v0.6.1)

## Results

All final runs below passed on **macOS arm64**.

| Area | Evidence |
| --- | --- |
| Rust regressions | 237 tests in the optimized release suite; the serial instrumented suite also passes |
| Opt-in probes | All 14 run separately in release mode, including isolated SIGCHLD preservation and allocation/performance probes |
| Practical programs | 150 utilities, 146 alternate inputs, one invalid-input rejection, and nine retained failure/recovery cases |
| Live control and terminal UI | 31 scenarios, including real PTY interaction and terminal restoration |
| Concurrent state edits | 1,000 mixed CRUD/patch requests across eight clients, with exact state checks |
| Seeded robustness trials | 4,096 program mutations across finite/stream/UI parsers; 1,024 argv strings; 1,024 manifests; 4,096 cron mutations |
| Persistent streaming | 8 MiB and decimal 1 GB inputs; exact byte counts and SHA-256; two native launches per pipeline |
| Lifecycle stress | 1,000 cycles × seven cases = 7,000 probes; all expected exits and direct native-child cleanup checks pass |
| Harness integrity | Eleven Python contract tests across utility, Unix benchmark, and replacement-proof harnesses |
| Static/build checks | Formatting, native Clippy, Linux-target Clippy, and release compilation pass |

Counts overlap: the Rust integration suite invokes the utility and live scenarios.
They must not be added together as independent tests. The 10,240 seeded trials
are finite, deterministic mutation samples; they are not an exhaustive fuzzing proof.

## Defects fixed

**Non-UTF-8 command-line arguments.** `std::env::args()` previously panicked and
returned status 101. Argument decoding now returns status 1 with a useful diagnostic.
Percent-encoded local file URIs still preserve Unix filename bytes where the
filesystem supports them; the regression also checks graceful rejection on APFS,
which disallows the tested invalid byte sequence.

**Closed stdout.** CLI help/version/explain, headless UI output, and live JSON
responses previously used infallible printing macros that panic on a broken pipe.
They now propagate ordinary I/O errors. Regressions close the read end before
starting the command, making the failure deterministic rather than timing-dependent.

## Coverage-directed checks

The initial coverage report exposed missing live producer branches. New tests
fill the actual 128-record inbox, verify its exact FIFO contents while paused,
drain all 300 source records through EOF, and check producer reaping. Additional
tests cover producer exit 7 followed by source repair and invalid UTF-8 listener
input followed by recovery on a valid record.

Socket tests also cover malformed types, invalid JSON/UTF-8, excessive nesting,
partial idle requests, shutdown with stalled clients, and preservation of a new
file replacing the socket path. None of those rejected requests may poison the
server or silently mutate its revision.

The production executable's source mappings report **89.35% line coverage**,
**86.96% function coverage**, and **86.98% region coverage** across eighteen source
files. Inline `cfg(test)` code and dependencies are excluded from those numbers.
A separate all-source total, including inline test/benchmark code, is retained in
the JSON report. Coverage uses matching Rust LLVM tools and profiles from both
test processes and the real CLI children they execute.

Coverage is not 100%. Remaining gaps include defensive I/O/OS error paths and
less common UI/supervisor branches. Line execution alone does not prove branch,
interleaving, crash, or security correctness.

## Timing-sensitive test observation

One initial parallel coverage run failed because the timeout fixture's PID file
was absent after the command deadline. The run overlapped compilation, and the
native timeout includes startup. The same test passed in an isolated instrumented
run and in the complete serial instrumented rerun. No runtime deadline was relaxed
and the original [failure log](validation-v0.6.1/initial-coverage-failure.log) is
retained beside the [isolated rerun](validation-v0.6.1/isolated-timeout-rerun.log).
This remains a recorded test-scheduling sensitivity rather than a discarded result.

The October 14 campaign candidate corrects the test oracle: completed native
spawn traces provide child identities for cleanup checks, without requiring
Python to initialize a PID file before the deadline. A timeout before a spawn
trace does not provide a child identity to assert against. Recovery compares
the observed partial effect before/after repair, including a valid absent or
partial marker, while retaining explicit uncertain-retry refusal. Runtime
deadlines are unchanged. This is a test correction, not a new coverage result;
the historical instrumented failure and rerun remain preserved.

## Performance observations

[Fresh live-control measurements](../benchmarks/live-control-v0.6.1-macos-arm64.json)
use small payloads, warm caches, and a paused server:

| Operation | p50 ms | p95 ms |
| --- | ---: | ---: |
| Local socket read | 0.073 | 0.129 |
| Fresh CLI read, including startup | 3.318 | 4.291 |
| Widget edit through applied headless render | 0.205 | 0.211 |

The [stress results](../benchmarks/replacement-proofs-v0.6.1-macos-arm64.json)
record roughly 2.7 MiB command high-water usage at both tested bulk sizes. That
is not summed simultaneous process-tree RSS. Bulk timing includes tracing and
the incremental Python hashing sink, and overlapped other verification work;
it is not a clean throughput comparison or evidence of a performance regression.
No physical terminal display-latency or cold-cache guarantee is claimed.

## Reproduce

With current stable Rust, Python 3, `jq`, and standard Unix tools:

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --release -- --test-threads=1
EXECUTE_PROOF_BINARY=target/release/execute python3 tests/replacement_proofs.py
python3 tests/utility_catalog.py
python3 tests/unix_bench.py
python3 tests/live_control.py target/release/execute
python3 benchmarks/replacement_proofs.py --binary target/release/execute \
  --lifecycle-repetitions 1000 --report /tmp/execute-stress.json
python3 benchmarks/live_control.py --binary target/release/execute \
  --report /tmp/execute-live-latency.json
```

The fourteen opt-in tests need the report-path environment variables named by
those test sources. Run them with `cargo test --locked --release -- --ignored
--test-threads=1`. Coverage additionally needs `llvm-tools-preview`,
`RUSTFLAGS="-C instrument-coverage"`, and an absolute `LLVM_PROFILE_FILE` containing
`%m-%p.profraw`; use a separate target directory and merge profiles before export.

## Limits of this audit

Native Linux execution, Omarchy desktop/Quickshell integration, real authenticated
network services, and the 10,000-cycle challenge are unmeasured here. Linux-target
Clippy establishes compilation/type checking, not Linux runtime behavior.

Tests use owned temporary files and processes. GitHub UI tests use controlled
CLI fixtures; CPU checks also exercise actual read-only `ps`/`jq` output. No
privileged applets, package installation, OS configuration, Git commits, or pushes
are part of these runtime tests. SIGKILL and deliberately escaped process groups
remain outside the current containment contract.
