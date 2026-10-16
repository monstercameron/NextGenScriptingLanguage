# Benchmark results index — October 16, 2026

These reports cover separate, source-pinned workloads and timing boundaries. Read each report for its platform, paired samples, and retained losses. The results are not combined into an overall speedup or readiness claim.

| Measurement | Result | Scope and status |
| --- | --- | --- |
| [`recv_any`, hosted Linux](graph-recv-any-ci-38851233022/README.md) | 288/288 paired comparisons favored the candidate. | Preloaded bounded-queue `NodeIo::recv_any` on x86_64 Linux; this does not measure whole-graph or end-to-end execution. |
| [`emit`, local macOS and hosted Linux](graph-emit-paired-macos-linux-20261016/README.md) | macOS: three case ratios favored the candidate; `fanout_8_integer` regressed about 2.0% (median paired ratio 1.02012; 21 wins/27 losses). Linux: all four case ratios were below 1.0. Overall, 52/192 macOS and 19/192 Linux comparisons lost. | Separate ARM64 macOS and x86_64 Linux `NodeIo::emit` runs. Platform results remain separate. |
| [Named receive operations, local macOS](graph-named-recv-macos-arm64-20261016/README.md) | 566 improvements and 10 losses across 576 paired comparisons. | Preloaded bounded-queue `recv` and zero-duration `recv_timeout`; in-process operation only. |
| [Manifest catalog selection](manifest-catalog-paired-2026-10-16/README.md) | Library selection: 48 wins/0 losses. Explicit CLI selectors: `check --json` 47/1 and `applets list` 46/2. | Opt-in catalog selection; default directory discovery is unchanged. The library and CLI comparisons are distinct. |
| [Serialized payload size and full emit](payload-size-paired-macos-linux-20261016/README.md) | The local direct-size counter had 192 wins/0 losses. Full-emit runs retained 55/192 macOS losses and 34/192 hosted Linux losses. | Direct-size measurement is local macOS only. Full emit is a separate workload on each platform; this is not a hosted direct-size comparison.  |
| [Resolved JSON arguments](resolved-json-arguments-positive/README.md) | Local macOS: 47 wins/1 loss; hosted Linux: 48 wins/0 losses. | Separate fixed 1,000-call public-runtime workloads: median paired ratios 0.8239 on macOS and 0.8265 on Linux. Both raw audits passed; platform results remain separate. |
| [Collector and single-input queue, local macOS](graph-collector-queue-macos-arm64-20261016/README.md) | Collector: 48/0, median paired wall ratio 0.0531. Queue after collector: 48/0, ratio 0.4331. Separate CPU diagnostic: process CPU lower in 12/12 pairs; reaped-child CPU lower in 10/12. | Two distinct fixed finite GraphSession workloads and one CPU-counter diagnostic; all raw audits passed. Ratios are separate and cannot be combined. |
| [Typed JSON carry, scalar workload](typed-json-carry-scalar-mixed-20261016/README.md) | Mixed result: 27 wins/21 losses; median paired ratio 0.9757. AB order regressed (1.0181); BA improved (0.9452). | The order-sensitive 1,000-call scalar result does not independently justify landing. The candidate remains unmerged. |
| [Program AST cache v1](program-ast-cache-v1-negative/README.md) | Negative result: 21 wins/27 losses; median paired ratio was 1.0221. | Candidate remains unmerged. This result does not support the proposed cache change. |

Pair counts, ratios, and deltas use each report's stated denominator. Do not sum values across rows: platforms, source revisions, and workload boundaries differ.

## Earlier benchmark records (preserved)

### Five-hour performance and security session

The [session summary](session-2026-10-14-summary.json) indexes 17 retained reports
from the fixed October 14–15 session. Runtime commit `41b08da` passed Linux,
macOS, Windows x64/ARM64 and Pages CI. These source changes are separate from
published release artifacts.

| Workload | Observed per-pass result |
| --- | --- |
| Fresh CLI checks with 1,000 literal arguments | Median 6.3–6.7% lower across three repeats. |
| In-process catalog decode / warm catalog load | Medians 50.9% / 12.5% lower. Ordinary CLI controls roughly flat. |
| Fresh CLI with a 1,000-input interface | Medians about 27–33% lower; interfaces up to 64 inputs stay within 1%. |
| Trace-heavy WASM in an actual Chromium worker | Operation medians about 21–32% lower; small cases quantized or flat. |
| Real 4 MiB table edits, publication pass | Edit-to-applied medians 7.3–7.6% lower. Includes socket/scheduling observation. |
| Actual native `cat` stream controls | Within 2%; no broad throughput or task-latency gain claimed. |

Each row uses its own baseline and workload. Do not combine percentages or
interpret private-helper gains as whole-application speedups. Timings are local
macOS arm64 with warm filesystem caches; native Linux/Windows performance,
true cold startup and peak memory remain unmeasured by this session.

#### Applet backend boundaries (harness; no result recorded)

[`applet_boundaries.py`](applet_boundaries.py) invokes an already-built Rust
benchmark test binary; it never runs Cargo or compiles while measuring. Wrapper
unit tests run through `make applet-boundaries-harness-check` and do not measure.
After the final release build is declared ready, build the ignored release test
target separately, then pass that executable and its matching `execute-applet`
helper to the wrapper with a fresh report name. It requires both
`--final-release-build-confirmed` and `--measurement-window-idle` gates. The
same parsed `.program`, manifest parameters, `execute-applet` operation and
typed record output run through the one-shot command, `in_process` and
`json_lines_worker` backends. Before any timing, the harness checks exact
encoded output, the status binding, typed fields and the separate absence
envelope path. It reuses a Registry so worker persistence is exercised, then
interleaves backend samples after warmups. Per-call launch counts come from the
runtime result. Worker frame encode/decode costs have their own samples and are
not added to the end-to-end invocation timings.

Each report retains raw nanosecond samples, nearest-rank p95 and median,
successful-output hashes, helper/test-binary/source hashes, request/response
frame sizes, measured worker/command launch counts and host/build environment.
It requires a previously built release test binary and `execute-applet`, and a
fresh, explicitly named `applet-boundaries-*.json` destination. Do not run it
concurrently with builds or tests. The helper-backend case isolates the same
operation boundary; it excludes interpreter process startup by measuring
`runtime::run` in one process. It cannot establish a universal interpreter or
Bash speed advantage.

Exact successful Omarchy rewrite timings use a separate paired harness:

```sh
python3 benchmarks/omarchy_success.py \
  --baseline-dir /path/to/preserved/contract-refinement-baseline \
  --candidate-binary target/release/execute \
  --report benchmarks/omarchy-success-<host>.json \
  --samples 25 --warmups 6 \
  --final-release-build-confirmed --measurement-window-idle
```

The harness verifies the saved baseline receipt and hashes, then runs one owned
successful fixture per Omarchy program for both baseline and candidate. Each
side must pass the existing pinned-Bash assertions for exact output/status and
recorded effects; the two Execute results must also match before any timing.
The baseline programs use direct quoted calls. Where an existing fixture
adapter always supplies a manifest directory, the saved binary receives one
inert legacy-shaped manifest instead of the candidate's new v1 catalog; its
exact bytes are hashed into the report.
Fixture creation and the Bash oracle are outside the timed boundary. The harness
alternates baseline-first and candidate-first rounds and retains per-side raw
samples, hashes, environment and summaries. New error, absence, transaction or
side-effect correctness paths are separate evidence; intentionally corrected
behavior is excluded from performance parity. This measures only the ten named
happy paths on that host, not all Omarchy scripts or universal interpreter
speed. The harness does not invoke Cargo. Run it after the final release build
is ready and with builds, tests and other benchmarks stopped. No Omarchy
performance result is recorded here.

The session also added bounded `repeat`, `for each` and `while` loops and fixed
cache filesystem and live socket availability defects. Security costs remain
visible: cache checks add roughly 1–2% to the measured listener acknowledgement,
and the final socket deadline fix makes healthy 4 MiB state replies about 17%
slower (roughly 6.6 to 7.7 ms). The actual CLI now ends a slowly drained request
write around two seconds; the previous commit was still writing at 3.5 seconds.
This does not establish a sandbox or general filesystem deadlines.

Final local verification passed 460 Rust tests, 61 live-control scenarios,
34 WASM contracts and 22 editor tests, plus Windows compile checks and seven
lifecycle smoke probes. Raw pairs, exact output oracles, source/binary hashes,
rejected experiments and scope limits remain in the indexed reports and local
session evidence. No universal Bash advantage or production completeness is claimed.

#### Paired source revisions

[`source_pair.py`](source_pair.py) compares a preserved Execute build and its
catalog with an already-built candidate. It covers fresh CLI checks with
different literal argument counts, native byte pipelines, a finite typed graph,
a 1,000-manifest discovery catalog, and an existing isolated Omarchy log utility.
The generated discovery catalog measures metadata matching; it does not prove
interoperability among 1,000 independently implemented applets.

```sh
python3 benchmarks/source_pair.py \
  --baseline-binary /path/to/baseline/execute \
  --baseline-manifest-dir /path/to/baseline/manifests \
  --baseline-revision BASELINE_COMMIT \
  --candidate-binary target/release/execute \
  --candidate-manifest-dir manifests \
  --candidate-revision CANDIDATE_COMMIT \
  --report benchmarks/source-pair-HOST-REVISION.json \
  --samples 30 --warmups 5 \
  --candidate-build-confirmed --measurement-window-idle
```

Build and run correctness tests before entering the measurement window. The
harness never builds either executable and refuses to overwrite a report.
Independent byte, status and effect oracles precede timing and check every
warmup and sample. The graph oracle checks its protocol, typed terminal ports,
identity, sensitivity metadata and ordered sequence numbers; paired parity
compares payloads by port because valid actor scheduling can change the global
event order. Each invocation still retains its raw output size and hash.

Reports retain alternating raw samples, medians and nearest-rank p95, excluded
warmups, source/catalog/executable hashes, inputs and environment. Measurements
include fresh interpreter startup and the named workload. They compare two
Execute revisions on the recorded host; cold-cache behavior, process-tree RSS,
other operating systems and Bash/Nushell parity need separate measurements.
`tests/source_pair_bench.py` checks the harness without collecting performance
data.

##### October 15 source comparison

The [summary](source-pair-macos-arm64-1eabbfa-summary.json) and
[compressed raw report](source-pair-macos-arm64-1eabbfa.json.xz) compare baseline
`0c57e22` with runtime candidate `1eabbfa` using harness `91fbaae`. This is one
paired run on macOS arm64 with 30 measured samples per side per workload and
five excluded warmups. Every invocation passed independent output, status,
stderr and effect checks; the recorded artifacts stayed unchanged.

| Workload | Baseline median (ms) | Candidate median (ms) | Change |
| --- | ---: | ---: | ---: |
| Fresh direct `true` | 9.999 | 9.950 | -0.5% |
| Literal argv, 8 values | 10.741 | 10.549 | -1.8% |
| Literal argv, 64 values | 10.021 | 10.268 | +2.5% |
| Literal argv, 256 values | 10.655 | 10.873 | +2.0% |
| Literal argv, 1,024 values | 11.064 | 10.693 | -3.4% |
| Native `cat` to `tr` pipe | 25.255 | 24.858 | -1.6% |
| Two in-process graph nodes | 11.689 | 11.505 | -1.6% |
| Existing Omarchy log success fixture | 13.671 | 13.000 | -4.9% |
| Match 1,000 metadata manifests, top 100 | 1,055.144 | 118.672 | -88.8% |

The discovery result supports the top-K storage optimization on this metadata
workload. Ranking still compares the source/target cross product; this does not
prove 1,000 concurrent applet instances or independently implemented providers.
Other task medians are mixed, and some tails worsen: native-pipe p95 rises from
29.860 to 38.633 ms, and 1,024-argument p95 rises from 12.918 to 14.733 ms.
These measurements do not establish a broad runtime speedup or a production
Bash/Omarchy replacement.

The raw JSON contains repeated fixture snapshots and is retained losslessly as
XZ instead of adding a 102 MB JSON file to Git. The summary records compressed
and uncompressed hashes; decompression was checked against the original bytes.
Use `xz -dc benchmarks/source-pair-macos-arm64-1eabbfa.json.xz` to inspect it.

#### October 15 matched public reference workflows

The [summary](matched-language-macos-arm64-987ef77-summary.json) and [lossless raw report](matched-language-macos-arm64-987ef77.json.xz) record Execute `987ef77` (rc.5), macOS Bash 3.2.57 and Nushell 0.116.0 on local macOS arm64. Three checked-in public reference workflows each have two fixture cases. These are hand-authored evaluator references; the separate AI-generation pilot has its own results.

Each language has 30 measured samples and five excluded warmups per case, interleaved through all six language-order permutations. All 540 measured invocations, 90 warmups and 18 preflights passed exact stdout, empty stderr, zero status, output-file and allowed workspace-effect oracles. Recorded source, catalog, executable and tool fingerprints matched before and after. The summary independently recomputes medians and nearest-rank p95 from the raw samples.

| Fixture case | Execute median / p95 (ms) | Bash median / p95 (ms) | Nu median / p95 (ms) |
| --- | ---: | ---: | ---: |
| filesystem_inventory/names_with_spaces_and_hidden_files | 25.828 / 40.521 | 17.011 / 19.183 | 30.239 / 37.541 |
| filesystem_inventory/second_directory_and_different_names | 25.764 / 36.802 | 17.042 / 32.462 | 28.943 / 39.316 |
| layered_config_merge/nested_object_and_array_replacement | 18.727 / 21.986 | 9.555 / 11.499 | 21.515 / 26.463 |
| layered_config_merge/empty_layers_are_an_empty_object | 18.728 / 23.217 | 9.600 / 15.356 | 20.472 / 29.791 |
| typed_event_normalizer/mixed_optional_actor_fields | 18.673 / 21.189 | 9.561 / 11.973 | 20.700 / 25.127 |
| typed_event_normalizer/different_event_content | 18.440 / 21.439 | 9.447 / 11.387 | 19.807 / 23.903 |

Bash has lower medians in all six cases. Execute has lower medians than Nu in all six; its first filesystem-case p95 is higher than Nu. The workloads have matched behavior but different command plans: Execute uses internal `save` and an explicit `cat` stage for `jq`, while shell references use `tee` or file arguments. `save` syncs a temporary file and atomically renames it; `tee` does not request those operations. The byte/effect oracles do not establish equal write durability, and these results cannot isolate interpreter overhead. Timings include fresh interpreter startup, command execution, stream capture and exit. Fixture setup and correctness checks are outside the timed interval.

This is one local run with warm working filesystems and an idle measurement window. It measures neither CPU nor peak RSS, interpreter-only overhead, cold startup, failure contracts, other operating systems, held-out tasks, nor programmer tokens/cost. Execute-only graph and catalog workloads have no matched shell arm and remain explicitly excluded in the raw report. No broad Bash replacement or AI superiority claim follows.

An earlier attempt stopped before timing because the harness reused the first task’s reference path for later tasks. Commit `987ef77` selects each task’s own script and adds a regression covering all six cases; the failed preflight is not counted as a measurement.

The raw JSON is retained losslessly as XZ with both hashes in the summary. Inspect it with `xz -dc benchmarks/matched-language-macos-arm64-987ef77.json.xz`.

### Native stream framing

The [stream-framing report](performance-2026-10-14-stream-framing.json) compares
`3a07fe5` with borrowing complete newline-terminated records from the native
read buffer. Fragmented records still use the pending buffer. Failed callbacks
retain the complete failed record, and size checks, record quotas, EOF bytes,
backpressure, cancellation and deadlines keep their previous behavior.

Three alternating release rounds measured private framing medians about 53%
lower for complete 8-byte records and 4% lower for 1–8 KiB records. Tiny
3-byte fragments measured 9% lower after refinement; 8 KiB fragments stayed
flat. These operations process 256 KiB through a synchronous counting callback;
they exclude process startup, native I/O and downstream work.

Two real native `cat` repeats, each with 20 measured paired CLI runs per
record size, stayed within 2%. Exact binary output, including NUL, invalid UTF-8,
CR and an unterminated tail, matches the independent input fixture. Every
preserved trace contains one real `/bin/cat` spawn and successful exit. The
harness includes startup, catalog loading, framing and writes to `/dev/null`;
it does not establish a broad task-latency or per-record service improvement.

More than 5,000 comparisons against the original framer check exact emitted
bytes, errors, retained pending bytes, counts and retry finishes. The initial
candidate made tiny fragments 9% slower; its source and measurements remain
separate from the final candidate. A stale core-test artifact rejected the catalog before candidate timing; that
failed pairing is excluded. The corrected comparison uses the explicit executable
from Cargo's current build output. Failed attempts remain in the session evidence.

Five fresh CLI controls stay within 0.8%; unrelated runtime API controls vary
by -3.3% to -0.3%. An unchanged cron microcontrol is 2–3 ns slower in two
repeats, with its cause unestablished. Other core controls stay within 1.2%.
The native executable shrinks by 32 bytes; WASM is byte-identical. Allocation
traffic, peak memory, RSS, cold I/O and Linux/Windows performance were not
measured. Raw paired samples and source, input, output, catalog, harness and
binary hashes are retained. Keep timing separate from builds and stress.

### Widget publication JSON

The [publication report](performance-2026-10-14-ui-publication.json) compares
`cf84e91` with building final JSON directly from renderer data. The old path
first cloned an owned typed UI snapshot and then serialized it into another
owned tree. The exact typed serializer remains a test oracle; publication
comparison, the 8 MiB document budget, rollback and uncertainty handling remain.

Three alternating release rounds measured private JSON-publication medians
39–56% lower for populated list, port and table fixtures, and 10–16% lower for
chart/small controls. These include final JSON construction and destruction;
they exclude setup, validation, document comparison/budget checks, rendering,
IPC and output-oracle construction. They are not whole-widget measurements.

Two real-renderer repeats, each with 20 measured eight-edit trials per variant
and table size, measured 7.3–7.6% lower edit-to-applied medians for the 4 MiB
table. Acknowledgements were about 1% lower. Small cases varied roughly -2% to
+3%. Complete frames and retained table/text/error state match across binaries
and trials; all owned children exit cleanly. Headless applied observation includes
RPC, scheduling and Python/socket costs, rather than event-to-photon latency.

Sixty legacy comparisons require identical JSON and encoded bytes, including
nested rows, Unicode/control text, large integers, negative zero, non-finite
floats serialized as null, and absent focus. Existing capacity-failure rollback
checks pass. Five fresh CLI check controls, 120 samples per variant each, stay
within 0.8%; runtime API controls stay within 1.7%. Unchanged core controls vary
from about -8% to +3% and remain recorded without attributing those changes to
UI publication.

The native executable grows by 17,456 bytes (0.60%). WASM is byte-identical.
Allocation counts, peak memory, RSS, cold startup and Linux timing were not
measured. Source, input, complete-output, catalog, harness and binary hashes and
all raw samples are retained. Keep measurements separate from builds and stress.

### Owned widget handoff

The [widget-handoff report](performance-2026-10-14-ui-handoff.json) compares
`f07b3e2` with transferring already-owned widget values into the renderer.
Inputs, text, items, rows, bar labels and chart series move into place. Changed
input payloads still reach the same ordered queues after inputs are installed;
generation wrapping, deduplication, focus and validation limits remain intact.

Three alternating release rounds measured private handoff medians 35–72% lower
for populated widgets. A 4 MiB table moved from 1.35 to 0.37 ms; unchanged 64 KiB
ports measured 55% lower and the small control 9% lower. Parsed screen and typed
incoming state construction, per-batch input clones and output checks are excluded;
consuming the incoming state and destroying replaced/temporary values are included.
These timings exclude decoding, validation, publication, rendering and IPC.

Two real-renderer repeats, each with 20 measured eight-edit trials per variant
and table size, measured about 6% lower edit-to-applied medians for the 4 MiB
table. Acknowledgements were about 1% higher; small cases stayed within 1.5%.
Complete frame hashes match across binaries and trials, alongside exact retained
rows/text, empty UI errors and clean child exit. These are headless observations
including RPC, scheduling and Python/socket overhead, not event-to-photon latency.

There is a measured tradeoff: an unchanged 256-item manifest-validation control
is consistently about 8% slower, roughly 498 to 539 ns. An unchanged cron
microcontrol is 11–13% faster. The cause is not established; build/code-layout
effects are a possibility. Neither change is claimed as an improvement from UI
ownership. Five fresh CLI check controls, 120 samples per variant each, remain
within 0.5% with identical plans and exact status/output. Runtime API controls
vary -0.4% to +2.6%. All raw results remain in the report.

The old handoff remains a test oracle across 180 full state transitions,
including saturated queues, changed/unchanged/removed ports and generation
wrapping. The native executable shrinks by 112 bytes; WASM is byte-identical.
Allocation traffic, peak memory, RSS, cold startup and Linux timing were not
measured. Keep measurements separate from builds and stress.

### Live widget state decoding

The [UI-state report](performance-2026-10-14-ui-state.json) compares `5ac9361`
with borrowed JSON decoding at validation, normalization and renderer input.
Typed widget state still owns its values; decoding no longer starts by cloning
the entire JSON document. State schemas, exact errors, widget identities, queue
and collection limits, document budgets and transaction boundaries remain.

Three alternating release rounds measured private validation medians 26–85%
lower, including 22% lower for a table with 4 MiB of payload. Normalization
measured 16–56% lower. Fixtures, parsing and hashing are excluded; typed decoding,
validation or normalization and temporary destruction are included. These are
private-entry timings, separate from application responsiveness.

Two final renderer runs, each with 20 measured eight-edit trials per variant
and table size, measured about 2–2.4% lower edit-to-applied medians for the 4 MiB
table. Acknowledgement medians were about 3% lower. Small-console applied results
varied from 4.5% lower to flat; 64 KiB table results were 3–4.5% lower. These
headless measurements include projected RPC, scheduling and Python/socket costs;
they exclude startup and table loading and do not measure event-to-photon latency.

`ui_widget_state.py` checks every complete rendered frame hash across variants
and trials, expected edited text, full retained rows, empty UI errors and clean
owned-child exit. The original validator remains a test oracle across 140
schema/validation cases. Earlier reports used edited-text presence rather than
complete frame hashes; those reports and their corrected interpretation remain
separate from the final verified runs.

The native executable shrinks by 16,512 bytes (0.57%). Allocation traffic, peak
memory, RSS, cold startup and Linux timing were not measured. Unrelated runtime
API controls varied -7% to +2%, and a cron microcontrol varied -5.5%; these are
retained as variation, without attributing them to UI decoding. Source, input,
catalog, harness and binary hashes and all raw paired samples are retained.
Keep measurements separate from builds and stress.

### Borrowed preflight values

The [preflight-state report](performance-2026-10-14-preflight-state.json)
compares `d83078a` with borrowed supplied values during static preflight.
Read-only analysis keeps the caller map borrowed. When a binding becomes
unavailable, validation copies the remaining entries and skips the discarded
value; child program scopes remain owned. Exact legacy errors, type/scope
handling and caller immutability are checked against the old implementation
across 128 differential cases.

Three alternating release rounds measured native runtime API medians about
18–23% lower for 1–4 MiB JSON inputs and 6% lower at 64 KiB. Parsing, fixture
construction and hashing are excluded; validation, resolution, preflight, cwd,
signal registration and pure JSON execution are included. Small API controls
stayed within 1%. Isolated preflight reductions of 81–99.8% describe validation
work alone, not whole-program or event latency.

The 4 MiB API allocation probe repeated 78 to 72 allocation requests and
16,786,257 to 12,590,674 requested bytes in all three runs. A reusable-call
checkpoint control requests 32 additional bytes for scope storage; all other
allocation controls match. Native executable size grows about 0.6%. These are
allocation traffic and artifact sizes, not peak memory or RSS measurements.

Cached native-listener acknowledgements stayed near 0.47/0.51 ms for 8/8,192-byte
records, with exact cache/output and native-spawn oracles. Two managed 32-node
runs did not show a repeatable improvement: large-state medians stayed within
1.7%, while small scheduling-sensitive cases varied by about 6%. Common handlers
without declared inputs or loops already bypass this preflight entry. Neither
service latency nor broad Bash superiority is claimed.

An earlier candidate copied the first invalidated value before removing it;
its overwrite microcase was 3–7% slower. That result motivated the refinement,
which avoids the copy and reduces the same isolated case by over 99%. Earlier
sources, binaries and raw measurements remain separately labelled. Final raw
alternating samples, full output oracles, source/input/catalog/harness/binary
hashes and browser controls are retained. Measurements are local warm-cache
macOS arm64; cold starts and performance on other platforms remain unmeasured.
Keep timing runs separate from builds and tests.

### Browser trace sizing

The [browser trace report](performance-2026-10-14-browser-trace.json) compares
release WASM from `f797524` with a byte-counting serializer for trace limits.
It uses the same JSON serializer and escaping rules while avoiding a temporary
buffer for every trace entry. Full response hashes, exact escaped UTF-8 budget
boundaries, the 4,096-entry/1 MiB caps and lazy trace construction remain intact.

Three alternating rounds in Node's real WASM ABI measured trace-heavy medians
21–29% lower. Repeated Chromium runs measured project-worker time about 21–32%
lower and worker round trips about 21–26% lower. These finite virtual-applet
workloads exclude UI rendering, native processes and network providers. The
pure JSON control stayed approximately flat. WASM grows by 909 bytes (0.175%);
startup, compile time, allocations, peak memory and RSS were not measured.

Tiny worker samples were quantized at 10/20 microseconds, producing an apparent
100% ratio that cannot establish a precise regression. Single-applet round trips
remained about 40 microseconds. Separate browser-main-thread ABI measurements,
using 500-operation batches, repeated about 6.2 to 5.6 microseconds per call;
those exclude worker/RPC overhead. One earlier JSON worker-control round varied
60 to 80 microseconds. Raw samples and these limitations are retained.

`browser_trace.mjs` loads explicit preserved WASM files and compares matching
requests, full responses and independent output/trace-budget oracles. Compilation,
instantiation and fetch are outside timing. Use `--host node` for the Node ABI or
`--host chromium` for the project worker plus the separate batched ABI control:

```sh
node benchmarks/browser_trace.mjs --baseline /path/to/baseline.wasm --candidate /path/to/candidate.wasm --host chromium --report target/browser-paired.json
```

The harness uses the installed playground Playwright dependency. Keep timing
runs separate from builds and tests. Results are local warm-cache macOS arm64,
not cold-start, native-runtime or Bash comparisons. Native CLI bytes are unchanged.

### Compound checkpoint fingerprints

The [checkpoint fingerprint report](performance-2026-10-14-checkpoint-fingerprints.json)
compares `9d56b25` with a temporary serialization buffer reused across a node's
steps. Length framing, serialized bytes, input line normalization, executable
content identities and durable writes remain unchanged. The old serializer is
retained as a test oracle; baseline-created checkpoints resume on the candidate,
completed saves stay skipped, and changed completed nodes are rejected.

Three alternating release rounds measured fingerprint medians 13–14% lower for
64/1,000 JSON steps and about 5% lower for reusable calls. These exclude parsing
and journal I/O. The public API's completed 1,000-step resume allocation probe
measured 1,066 to 65 allocation requests and 132,916 to 4,788 requested bytes.
The reusable-call case also reduced reallocation requests from 798 to 33. Three
repeats had identical counts; all other allocation controls matched exactly.
This is allocation traffic, not measured peak memory or RSS.

Fresh completed-checkpoint CLI medians stayed within 1.2%, and native buffered
input controls within 1.1%; no whole-task speedup is claimed. An unchanged cron
microcontrol varied +9.02%, retained as variation. A separate profile using the
same baseline binary for both labels shows native spawning and persistence
dominate short scripts; it is not an optimization comparison. No persistence
guarantee was weakened to improve those timings.

`checkpoint_compounds.py` measures completed resume with replay, mutation and
exact-output oracles. The ignored `runtime::resume::bench::checkpoint_fingerprints`
test isolates semantic fingerprint work; `opt_allocations` counts public resume
allocation traffic separately. Raw samples and source/input/catalog/harness/binary
hashes are retained. These are local macOS warm-cache observations, without a
cold-cache, other-platform or Bash performance claim. Keep timing runs separate
from builds and tests.

### Managed-live state handoff

The [live-state report](performance-2026-10-14-live-state.json) compares
`6685426` with a change that removes a full copy of incoming live-node state
immediately before the runtime overwrites that copy. The runtime still returns
the final working directory, bindings and output together; failed nodes publish
no success state. The live document's transaction and capacity checks remain.

Three alternating release rounds measured private runtime-entry medians 16–93%
lower with 64 KiB–4 MiB bindings and 10–47% lower with pipeline bytes of those
sizes. These exclude caller input construction and RPC. Two separate managed-app
runs, with 12 and 20 paired samples, include RPC, execution and state publication
for 32 JSON nodes: 1 MiB bindings measured about 4% lower, and 4 MiB bindings
7.5–7.9% lower. Small-state cases were approximately flat. Every trial checks
exact final bindings, bytes, cursor and zero child-process launches.

Fresh CLI controls stayed within 1%. Unchanged microcontrols varied: a short
parse was +3.99%, then -0.41% on repeat; cron matching was -10.07%, then -18.98%.
Those are retained as variation, not gains. A separate compact-index experiment
was rejected: it saved requested allocation bytes but made large-input parsing
consistently 3.7–3.8% slower. Its preceding faster implementation remains.

`live_execution.py` measures the full managed path; the ignored
`runtime::live::tests::live_state_handoff` release test measures its private entry.
Raw samples and source/input/catalog/harness/binary hashes are retained. These
are local macOS warm-cache measurements, with no allocation, peak memory, RSS,
other-platform or Bash performance claim. Run benchmarks without concurrent
builds or tests.

### Typed-input lookup scaling

The [input-interface report](performance-2026-10-14-input-lookup.json) compares
`cc10290` with an index collected during existing declaration validation.
For 1,000 supplied inputs, in-process parsing measured 86% lower median time,
resolution 80% lower, and program-argument validation 66% lower. Two interleaved
120-sample fresh CLI repeats measured check/run/inspect medians 27–33% lower.
These large interfaces are scaling probes. CLI cases with up to 64 inputs stayed
within 1%; native API JSON/no-input controls were approximately flat. Single-input
parsing/resolution helpers measured 11–15% lower, which does not establish a
whole-script gain.

Small interfaces retain linear lookup; zero/singleton validation avoids a
uniqueness-set allocation. Through eight inputs allocation traffic does not
increase. The larger index adds 880 requested bytes at 64 inputs and 14,696 at
1,000 inputs, with unchanged allocation-call counts. This is a CPU/temporary
allocation tradeoff, not measured peak memory or RSS. Invalid-name, duplicate,
default, type, unknown-input and host-local policies retain their error order.

Raw timings, refinement decisions, source/input/harness/binary hashes and bounds
are retained. Some unchanged microcontrols vary: final manifest-list validation
was +8.27%; an earlier cron control changed +9.33%, then -6.76% on repeat. These
are not claimed gains. Results are local macOS with warm filesystem caches;
cold-cache, other-platform and Bash performance remain unmeasured.

### Catalog decoding performance

The [paired catalog report](performance-2026-10-14-catalog-decoding.json)
compares `8cb5e7d` with a strict one-pass argument-node decoder. Across three
alternating release rounds, parsing the catalog from memory measured 51% lower
median time (101 to 50 microseconds); loading its files measured 12.5% lower
(507 to 444 microseconds). Three allocation repeats each show 1,487 to 658
allocation calls and 143,356 to 59,805 requested bytes per load; reallocations
also decrease. All other allocation controls matched exactly. This measures
allocation traffic, not retained memory, RSS or peak memory.

The legacy differential corpus preserves accepted/rejected nodes and mappings;
serialized catalog hashes match, and a baseline-created checkpoint resumes on
the candidate without repeating completed effects. Matching fresh CLI medians
varied -2.53% to +0.22%; these small changes are controls, not claimed whole-task
gains. An unchanged cron control varied +2.67% (about 0.4 ns). Raw samples,
source/catalog/harness/binary hashes and limits are retained. These are local
macOS warm-cache results; cold-cache and other-platform performance remain
unmeasured.

### Loop integration regression controls

The [loop integration report](loop-regression-2026-10-14.json) compares the
unchanged `7348c16` release build with current source after adding bounded loops.
It measures identical **non-loop** workloads: three alternating in-process rounds
and 80 interleaved fresh CLI samples per variant, with warm filesystem caches.
All nine core controls stayed within 2%; three-command validation increased
1.64% (about 5 ns). Matching CLI medians changed by -1.83% to -0.02%. These small
changes are descriptive controls, not claimed speedups. Raw samples, binary and
source hashes, output oracles and local verification are retained. This does not
measure loop dispatch, cold-cache startup, Bash parity or other platforms.

### Supplied JSON input setup performance

The [input setup report](performance-2026-10-14-input-setup.json) compares
`5dc0d59` with a candidate that avoids cloning supplied inputs into an options
copy immediately before replacing them with resolved values. Across three
alternating release rounds, the native Rust API's 64 KiB–4 MiB JSON workloads
measured 14–26% lower median time. The 4 MiB case moved from 656 to 545 microseconds.
Parsing and fixture construction are excluded; validation, input resolution,
signal registration, cwd canonicalization and pure JSON selection/encoding remain
included. No native command launches in these cases.

Three independent allocation probes each measured 88 to 82 allocation calls and
20,982,608 to 16,787,025 requested bytes for the 4 MiB workload: about 4 MiB and
20% fewer requested bytes. This is allocation traffic, not live or peak memory.
All other allocation controls matched exactly. Small/no-input and fresh CLI
workloads were approximately flat overall. A first no-input timing round varied
by +14.9%; an unchanged cron control varied by -12.3%. Both remain visible and
neither is treated as a gain. These are local warm-cache macOS measurements.

The ignored `opt_input_setup` release test provides API timing workloads;
`opt_allocations` measures allocation requests separately from timing.

### Known-input preflight performance

The [paired preflight report](performance-2026-10-14-preflight-parameters.json)
compares `c95495b` with a candidate that borrows literal-only calls during
known-input preflight and resolves reference-containing parameter maps once.
Called reusable-program validation measured 42–74% lower medians across three
alternating release-executable rounds; unused-definition controls stayed within
2%. Other core controls stayed within about 2%.

Three fresh-CLI repeats (120, 160 and 240 samples per variant) measured 6.3–6.7%
lower medians for checking 1,000 literal-argument commands with a declared input,
4.2–4.4% for mixed argument lists, and 2.6–2.9% for scalar references. Whole-list
references and ordinary execution workloads were approximately flat. The no-input
control changed +0.5%, +3.6% and -0.4% across the repeats; the noisy repeat remains
in the report. These are local warm-cache macOS observations, not universal gains.

`preflight_parameters.py` compares identical static-check sources and resolved
plans. Preserve matching release test executables before and after a change,
then use `paired_core.py --baseline BASE_TEST --candidate NEW_TEST --test
reusable_definition_validation --output DIR` for the in-process comparison.
Never run timing comparisons alongside builds or stress tests.

### Reusable definition validation performance

The [October 14 paired report](performance-2026-10-14-definition-validation.json)
compares the unchanged `f742a54` release build with a candidate that borrows
reusable definition inputs and instructions during validation instead of cloning
a temporary program. All definitions, including unused ones, remain validated.

Across three alternating release-executable rounds, the six in-process cases
measured 22–79% lower validation medians. A 1,000-command unused definition fell
from 207 to 42 microseconds; the called version fell from 606 to 441 microseconds.
Parser, catalog and argv controls stayed within 2%. Forty-sample fresh CLI task
comparisons were approximately flat; these results do not establish a whole-task
speedup. Raw batches, workload and binary hashes, controls and limits are retained.
This is local macOS arm64 with warm caches, not a cross-platform or Bash result.

The ignored `opt_reuse_validation` release test measures only validation;
`reusable_programs.py --baseline-programs` compares matching sources end to end.

### Unix command differential matrix (pending source candidate)

`unix_matrix.py` exercises 110 cases across **46 distinct native executables**:
literal argv (including shell-looking text), text and binary tools, multi-stage
native pipes, paths, multiple files, typed CLI inputs and failure contracts.
One hundred cases require exact matching status/stdout/stderr. Ten check explicit
differences: literal wildcard handling, native failure diagnostics, discarded
failed finite output and strict middle-stage pipeline
failure. A declared difference must meet its own oracle; arbitrary mismatches
cannot pass. Independent byte expectations supplement Bash for many cases.

```sh
python3 benchmarks/unix_matrix.py --binary target/release/execute \
  --samples 40 --warmups 5 --report /tmp/unix-matrix.json
python3 tests/unix_matrix.py
```

Sources use absolute native paths. `explain` verifies argv; separate trace runs
verify Execute's native launch identities/count. Bash calls the same external
binaries with literal quoting, except the intentional wildcard comparison.
Fixtures and the sole `tee` output stay in a private temporary directory; there
are no network calls or host configuration changes. The minimal environment
disables startup hooks and user tool lookup. This macOS matrix requires all
listed system tools; missing tools fail rather than silently reduce coverage.

The campaign's `unix-matrix` directory retains 30- and 40-sample paired runs,
raw observations, warmups, sources, executable hashes and harness acceptance
tests. Timings describe fresh processes with warm filesystem caches, a minimal
one-entry unused catalog, Python process/pipe/wait overhead and uncontrolled
background activity. They are not isolated interpreter or pipe latency,
normal-catalog startup, cold-cache or native Linux results. The runtime binary
is unchanged by this proof pass. These are 110 cases, **not 100 distinct tools**
or the held-out 100 real Bash-script/model study; the replacement gate stays
partial. Installed 0.8.0 does not contain the pending source features.

### Typed program inputs, 0.8.0

The [80-sample run](program-inputs-v0.8.0-macos-arm64.json) and
[240-sample repeat](program-inputs-v0.8.0-macos-arm64-repeat.json) compare
0.7.0 literal commands against 0.8.0 literals, defaults, and explicit CLI inputs.
Each case verifies matching native argv through `explain` before timing, then
uses the same data, five warmups and interleaved trials. Exact stdout is checked
outside timing. All variants load the same explicit catalog. This is warm-cache
macOS arm64, with uncontrolled background activity.

Repeat explicit-input median/p95: 5.443/6.001 ms for an 8 KiB checksum
(old literal 5.447/5.892 ms), 5.244/5.806 ms for a two-line preview
(old literal 5.290/5.673 ms). Small differences do not establish a speedup.
The first checksum input p95 was 8.541 ms; the larger repeat measured 6.001 ms.
Both datasets remain visible. No hard latency bound, true cold-cache or native
Linux claim follows from these samples.

```sh
python3 benchmarks/program_inputs.py --baseline /path/to/execute-0.7.0 \
  --candidate target/release/execute --samples 240 --report /tmp/program-inputs.json
```

### Direct executable calls, 0.7.0

The [initial paired report](direct-commands-v0.7.0-macos-arm64.json) and
[120-sample repeat](direct-commands-v0.7.0-macos-arm64-repeat.json) compare the
preserved 0.6.1 executable's named calls, 0.7.0 named calls and 0.7.0 direct calls.
Output oracles match. Cat/tr/wc argv match; printf used output-equivalent but bytewise different format strings. Both reports now carry this explicit method correction. The [original harness](archive/direct_commands_v0_7.py) is retained against their recorded hashes; the current harness constructs its printf format from the manifest. Five warmups precede shuffled paired rounds;
every sample includes fresh interpreter startup and captured output. All variants
load the same explicit catalog. This measures the new call path rather than an
avoided catalog load. It is a warm-cache macOS arm64 observation.

Repeat medians: a direct native printf **5.271 ms** (old named 5.304 ms; new named
5.272 ms); three native stages **9.975 ms** (old named 9.954 ms); a 2 MiB binary
pipeline **7.644 ms** (old named 7.656 ms). These small differences do not establish
a speedup. The initial bulk direct p95 was 24.233 ms; the larger repeat measured
8.292 ms against an old named 8.300 ms. Retain both reports: tail variability in
the first run is not grounds for either a universal regression or speedup claim.
There is no cold-cache or native Linux result here.

```sh
python3 benchmarks/direct_commands.py --baseline /path/to/execute-0.6.1 \
  --candidate target/release/execute --samples 120 --report /tmp/direct-calls.json
```

### 0.6.1 full audit

The [full testing report](../docs/full-testing.md) records the two CLI panic fixes,
237 Rust tests, 31 live scenarios, source coverage, and 1,000 seven-case lifecycle
cycles (7,000 probes). [Raw stress results](replacement-proofs-v0.6.1-macos-arm64.json)
include an exact decimal 1 GB binary-stream check. These instrumented observations
are not a matched Bash throughput comparison.

[Fresh live-control samples](live-control-v0.6.1-macos-arm64.json) use the existing
warm-cache, paused-server methodology: socket read p50/p95 0.073/0.129 ms;
fresh CLI read 3.318/4.291 ms; widget edit through applied headless render
0.205/0.211 ms. They do not measure physical terminal display latency.

The sections below retain historical 0.6.0 and earlier measurements.

### Replacement challenge: streaming and lifecycle proofs

The new [proof runner](replacement_proofs.py) checks exact binary output through
persistent native pipelines and repeats seven cancellation/failure/record-boundary
cases. The recorded 0.6.0 macOS arm64 run covers an 8 MiB fixture, a decimal 1 GB
fixture, and 100 lifecycle cycles (700 probes). Both bulk runs launch exactly two
native applets and match expected SHA-256 values, including invalid UTF-8 and NUL.

[Raw observations and provenance](replacement-proofs-v0.6.0-macos-arm64.json)
include every probe and invocation high-water usage. This is not a matched Bash
throughput comparison, total process-tree RSS, Linux acceptance, or the requested
10,000-cycle run. Timing includes tracing and the incremental output-hashing sink.

```sh
python3 tests/replacement_proofs.py
python3 benchmarks/replacement_proofs.py --binary bin/execute --lifecycle-repetitions 100 --report /tmp/replacement-proofs.json
```

See the [fifteen-gate ledger](../docs/replacement-challenge.md) for the exact
contracts, remaining work, and held-out AI evaluation protocol.

### 0.6.0 live control latency

The local control API and actual headless renderer were measured on macOS arm64.
[Raw samples and provenance](live-control-v0.6.0-macos-arm64.json) record the binary
and harness hashes. These small-payload observations do not establish hard
real-time scheduling or native terminal display latency.

| Operation | Samples | p50 ms | p95 ms |
| --- | ---: | ---: | ---: |
| Socket read | 200 | 0.075 | 0.132 |
| Socket binding update | 200 | 0.087 | 0.170 |
| Fresh CLI read | 20 | 3.530 | 3.865 |
| Fresh CLI update | 20 | 3.743 | 4.725 |
| Fresh CLI version reference | 20 | 3.418 | 3.997 |
| Socket widget update acknowledgment | 50 | 0.084 | 0.099 |
| Widget update through applied headless render | 50 | 0.207 | 0.228 |

RPC includes Python client/JSON overhead and a new Unix socket connection. CLI
includes process startup. The server is paused with no native jobs during these
measurements. Finite-server trials are shuffled/interleaved after warmups; UI
measurements follow as a separate 50-edit phase after ten warmups. Applied latency
includes observation requests until the real renderer publishes the accepted
revision, with frame text verified outside timing. Filesystem/executable caches
are warm, background activity is uncontrolled, and 20-sample CLI tails are
descriptive. Native output correctness is checked outside timing.

Reproduce:

```sh
python3 benchmarks/live_control.py --binary bin/execute --report /tmp/live-latency.json
```

### 0.5.0 optimization and streaming work

See the [measured optimization pass](optimization-pass/README.md) for retained
changes, paired measurements, rejected experiments, and limits. The new
[persistent stream mode](../docs/streaming.md) launches native stages once and
puts the final record limit and stdout sink in the `.program` file.

The following 0.4.6 and earlier tables are historical release measurements.

### Historical 0.4.6 matched Unix workloads

143 Rust tests and two benchmark-contract tests pass on macOS arm64. Formatting,
Clippy and release compilation pass; the installed binary also passes a real
ps/jq file-URI smoke check. Linux CI is configured but has not run locally.

The [Unix compatibility review](../docs/unix-compatibility-review.md) explains
implemented contracts and open acceptance gates. [Raw samples and provenance](unix-workloads-v0.4.6-macos-arm64.json)
record the exact release, installed external binaries and benchmark harness.

#### Wall time, milliseconds

| Workload | Samples/runtime | Execute p50 | Bash p50 | Execute p95 | Bash p95 |
| --- | ---: | ---: | ---: | ---: | ---: |
| Fresh start + external printf | 1000 | 7.847 | 5.062 | 9.847 | 6.109 |
| 1,000 external true launches | 5 | 1303.820 | 1812.607 | 1774.717 | 1957.139 |
| 1,000,000 lines through cat/tr | 5 | 433.480 | 430.768 | 437.270 | 432.737 |
| 100,000 JSON records through cat/jq | 5 | 360.675 | 352.196 | 366.447 | 427.550 |

Fresh-process tests use ten warmups and warm filesystem caches, not OS-cold
caches. The thousand-launch case uses one warmup; bulk pipelines use two. Pairs
are shuffled; background applications are uncontrolled. Five-sample p95/p99
mostly equal the maximum observation. Execute is slower at fresh startup, faster
for these repeated external launches, and close to Bash for the bulk workloads.
These results do not imply a general speedup.

#### Per-record listener, microseconds

A persistent interpreter handles 200 sequential requests with the same external
`tr` binary per record. Ten initial events are excluded. Replies are required
before stdin EOF; a separate Execute trace asserts exactly 200 transform launches.
Bash feeds each transform with its builtin printf; Execute feeds framed stdin bytes.

| Runtime | Samples | p50 | p95 | p99 |
| --- | ---: | ---: | ---: | ---: |
| bash | 190 | 2187.042 | 2712.417 | 3014.000 |
| execute | 190 | 1455.458 | 2808.333 | 3125.875 |

This is per-record process spawning, not one persistent transform. The event
p50 advantage does not remove that architecture cost or establish a tail-latency
advantage.

#### RSS and lifecycle observations

| Workload | Execute reported RSS MiB | Bash reported RSS MiB |
| --- | ---: | ---: |
| Million-line transform | 19.25 | 1.78 |
| 100,000 JSON records | 78.12 | 79.16 |

RSS is one separate `/usr/bin/time` high-water observation, not a distribution
or summed simultaneous process-tree peak. The million-line output is 11,000,000
bytes; Execute buffers final output, whereas Bash streams it to the file. Shared
jq computation dominates the JSON case; these RSS figures do not measure native
language-level structured values.

The gated producer probe observed zero final output bytes from Execute before
producer release, versus six from Bash. Both then returned the exact complete
output. Intermediate Execute pipes still transfer bytes directly.

Both file-following variants emitted exactly three transformed lines from four
source lines and reaped the producer in all five trials. Execute used `--max-runs
3`; Bash used a script counter. The cutoff is not part of the `.program` file.
The time from receipt of the third output to runner exit is:

| Runtime | p50 ms | p95 ms |
| --- | ---: | ---: |
| execute | 0.642 | 0.669 |
| bash | 0.724 | 0.798 |

Cancellation trials have shuffled paired order and no warmup exclusion. Execute
includes tracing here; this is behavioral evidence, not a clean overhead comparison.
Graceful cancellation relies on the runtime cleaning up its separate applet groups;
forced runner-group termination alone does not guarantee that cleanup.

A middle stage exiting 7 prevented later effects in both runtimes. Bash with
pipefail returned 7; Execute returned generic CLI status 1 and included status 7
in its diagnostic. Structured status handling is still a gap.

Nushell was not available and is not measured. The 100-way parallel-command and
ten-source UI workloads are explicitly unsupported; the 100-task AI authoring
benchmark is pending. They are not credited as completed tests.

To reproduce:

```sh
python3 benchmarks/unix_workloads.py --binary target/release/execute --output benchmarks/unix-workloads.json
python3 tests/unix_bench.py
```

The sections below retain measurements from earlier releases.

### Historical 0.4.5 adversarial-refinement measurements

142 tests pass (56 unit, 86 integration), plus the explicitly run release UI
benchmark. Formatting, Clippy, release compilation, and installed real ps/jq
file-URI smoke checks pass on macOS arm64. Linux CI remains configured, not run
locally. These observations do not establish 10/10 production performance.

#### Fresh CLI milliseconds

100 samples per case after 10 warmups, deterministic shuffled ordering. Includes
fresh process startup, manifest loading, commands, output capture and exit. Fixtures
are temporary, outputs are checked, caches warm, background applications uncontrolled.
The shell printf case uses its builtin; Execute uses the external Unix binary.

| Operation | p50 | p95 | p99 |
| --- | ---: | ---: | ---: |
| shell_builtin_printf | 5.159 | 6.644 | 6.952 |
| execute_printf | 8.497 | 11.130 | 14.427 |
| shell_small_pipe | 7.967 | 10.127 | 13.861 |
| execute_small_pipe | 8.953 | 11.831 | 14.348 |
| shell_1MiB_pipe_count | 52.728 | 62.460 | 77.337 |
| execute_1MiB_pipe_count | 53.947 | 60.383 | 88.460 |

[Raw samples, executable/catalog hashes and method](refinement-v0.4.5-cli.json).
Execute is slower than shell on these small one-shot cases. Intermediate native
pipes avoid interpreter copies; final finite output is buffered and cannot promise
shell-equivalent streaming to the final consumer. The warmed persistent `cat`
roundtrip is 3.958 µs at p50, including two pipes, scheduling and Python calls; this is not script dispatch.

#### Application milliseconds

| Operation | p50 | p95 | p99 |
| --- | ---: | ---: | ---: |
| top-cpu.program, real ps/jq | 45.204 | 55.723 | 138.021 |
| ui_refresh_with_mock_gh | 63.443 | 64.099 | 64.568 |
| headless_startup_with_mock_gh | 45.263 | 53.401 | 95.274 |

50 samples per case. CPU uses ten warmups; mock GH uses five. CPU observations
include fresh startup and real commands, exclude physical terminal I/O, and check
rankings on every sample. GH refresh includes PTY input/output observation and a
Python fixture, excludes network. The CPU p99 spike is retained in the raw data.
[CPU data](cpu-macos-arm64.json), [mock GH data](ui-macos-arm64.json).

#### In-memory UI microseconds

| Operation | p50 | p95 | p99 |
| --- | ---: | ---: | ---: |
| parse | 25.345 | 28.442 | 30.315 |
| render_100x30_120_points | 104.153 | 109.352 | 123.775 |
| render_100x30_ten_bars | 105.195 | 109.090 | 112.285 |
| route_text_message | 0.270 | 0.278 | 0.289 |
| validate | 1.857 | 2.062 | 2.162 |

[Release library measurements and method](ui-operations-macos-arm64.json):
100 batch averages of 100 operations, after ten warmup batches. Rendering excludes
terminal I/O; routing excludes applet work. These numbers measure separate costs,
not a Rust-versus-shell end-to-end speedup.

The sections below preserve historical observations and reference archived data.

### Execute 0.4.2 CPU monitor and TUI measurements

126 Rust tests have passed (50 unit and 76 integration), including the 50-script
ladder. The full suite was followed by focused CPU/widget checks for the final
portable ps flags, fractional scale fix, and added live ps smoke test. Formatting,
Clippy with warnings denied, release compilation, and an explicit ignored release
benchmark pass on macOS arm64. Linux CI is configured but has not run locally.

#### CPU program with real ps and jq, in milliseconds

50 fresh one-shot executions after 10 warmups. Every sample checks numeric
ranking and consistency between the bar chart and table. Timing includes process
startup, real ps/jq, the native pipe, JSON projection, component routing, in-memory
100×30 rendering, JSON output capture, and exit. Physical terminal writes are
excluded. Filesystem caches are warm; background processes are uncontrolled.
No artificial CPU load was created.

| Operation | p50 | p95 | p99 |
| --- | ---: | ---: | ---: |
| top-cpu.program --headless | 50.127 | 51.994 | 52.960 |

[Raw CPU measurements and hashes](cpu-macos-arm64-v0.4.2.json),
[live snapshot](top-cpu-live.json). Reproduce with
`python3 benchmarks/cpu.py target/release/execute` after a release build.

#### UI operations in microseconds

Percentiles are 100 batch averages, with 100 operations per batch after 10 warmup
batches. The release unit-test binary calls the optimized UI library functions.
Rendering uses an in-memory 100×30 terminal with a 120-point line graph or a
ten-bar CPU chart. Terminal I/O, JSON input parsing, and applet work are excluded.

| Operation | p50 | p95 | p99 |
| --- | ---: | ---: | ---: |
| parse | 25.503 | 28.017 | 29.824 |
| render_100x30_120_points | 103.640 | 106.667 | 109.488 |
| render_100x30_ten_bars | 104.928 | 107.685 | 108.543 |
| route_text_message | 0.271 | 0.288 | 0.326 |
| validate | 1.566 | 1.692 | 1.899 |

[Raw UI operation measurements](ui-operations-macos-arm64-v0.4.2.json).

#### Historical 0.4.1 GitHub mock refresh in milliseconds

These are 50 observed samples after 5 warmups with a controlled Python `gh`
fixture. Refresh includes a real PTY key event, command worker, subprocess,
JSON projection, chart update, and rendered terminal bytes. Headless startup
includes a fresh interpreter and final JSON output. Neither includes network
work. Percentiles use nearest ranks; p50 is the median. Filesystem caches were
warm and background applications were uncontrolled. The observer adds some
measurement overhead, and the UI can wait up to 20 ms for command completion.

| Operation | p50 | p95 | p99 |
| --- | ---: | ---: | ---: |
| ui_refresh_with_mock_gh | 63.052 | 63.960 | 65.260 |
| headless_startup_with_mock_gh | 44.848 | 50.985 | 51.821 |

[Raw observations and executable/manifest/fixture hashes](ui-macos-arm64-v0.4.1.json).
The real GitHub query is a separate [single smoke observation](github-stars-live.json),
not a latency distribution. Its total is dominated by the CLI and network.
These results do not establish an across-the-board performance advantage over shell.

To reproduce from the project directory:

```sh
cargo build --locked --release
UI_BENCH_REPORT="$PWD/benchmarks/ui-operations-macos-arm64.json" cargo test --locked --release --lib ui::app::tests::benchmark_ui_operations -- --ignored --exact
python3 benchmarks/ui.py target/release/execute
```

#### Historical 0.3.1 measurements after the 50-example stress ladder

All 50 numbered examples pass on macOS. The Rust suite passes 101 tests (41 unit,
37 CLI, 8 adversarial, 14 resume, and 1 integration test that runs the entire
ladder). Formatting, Clippy with warnings denied, and release compilation pass.
The ladder verifies 1,000 sequential live records, a real tail producer, a real
cron minute, long pipes, explicit file lists, failures, and repair/resume. Its
[verification evidence](../examples/ladder/results-macos-arm64.json) contains
single-run phase durations, which are not latency benchmark statistics. Linux CI
is configured but has not been run locally.

The separate latency benchmark below uses 200 samples per case after 10 warmups,
with shuffled CLI cases and separately timed listener batches. It ran after tests
completed, with the packaged release binary on Darwin 25.6.0, arm64. Filesystem
caches were warm. No system caches were flushed, background applications were
not controlled, and every output was checked. All fixtures, saves, checkpoints,
and caches remained in temporary directories. Preparation is outside CLI timing.

##### 0.3.1 CLI milliseconds

| Case | p50 | p95 | p99 |
| --- | ---: | ---: | ---: |
| normal_read | 9.619 | 14.119 | 15.038 |
| checkpoint_read | 78.990 | 91.123 | 98.167 |
| completed_resume | 10.133 | 14.631 | 15.556 |
| normal_pipeline_save | 21.873 | 30.025 | 32.062 |
| checkpoint_pipeline_save | 148.289 | 171.183 | 178.564 |

CLI times include a fresh process through output capture and exit. The read case
loads a 19-byte fixture and launches no applet. The pipeline/save case launches
`printf`, then `tr`, then saves the finite result. Completed resume restores a
finished result without running commands or rewriting the journal; it does not
measure a repaired suffix that starts new work.

##### 0.3.1 listener handler milliseconds

| Case | p50 | p95 | p99 |
| --- | ---: | ---: | ---: |
| normal_read_handler | 0.036 | 0.049 | 0.052 |
| checkpoint_read_handler | 59.160 | 65.541 | 69.317 |

This read-only handler receives one request at a time in one running interpreter,
with startup excluded. It reads the fixture without spawning a child. Durable
mode saves the event, node result, and completion and prunes old snapshots.
Synchronous filesystem writes cost tens of milliseconds on this machine;
ordinary mode remains in tens of microseconds. Checkpointing is opt-in. These
results do not demonstrate generally faster execution than a shell or promise
low-latency durable execution.

[Raw measurements, scripts, and binary hash](recovery-macos-arm64.json).
Reproduce with `python3 benchmarks/recovery.py --binary bin/execute --samples 200
--output benchmarks/recovery-macos-arm64.json` from the project folder. Measurements
are local fsync/rename/directory-sync timings, not a power-loss test.

Historical measurements follow; their raw files retain the recorded versions.

---

### Execute 0.3.0 checkpoint latency measurements (historical)

That release passed 97 tests: 39 unit, 36 CLI, 8 adversarial, and 14 resume
integration tests. Formatting, Clippy with warnings denied, and the optimized
release build pass. A temporary demo fails at `false`, repairs it to `cat`, and
resumes with two completed nodes restored; `printf` and cached `tr` are skipped.

Measured on the same Apple Silicon Mac, Darwin 25.6.0, with the packaged 0.3.0
binary. Each case has 200 samples after 10 warmups. CLI timing includes a new
process through complete output and exit; fixture and checkpoint-directory
preparation are excluded. Cases are shuffled per round. Listener timing excludes
startup and measures a sequential stdin request through the handler's output.
All outputs are checked; all writes occur in temporary directories. These are
local filesystem measurements with warmed caches, not a power-loss simulation.
Background applications and storage activity were not controlled.

#### Current CLI milliseconds

| Case | p50 | p95 | p99 |
| --- | ---: | ---: | ---: |
| normal_read | 10.124 | 14.209 | 15.020 |
| checkpoint_read | 81.881 | 105.007 | 112.168 |
| completed_resume | 10.342 | 14.720 | 15.512 |
| normal_pipeline_save | 23.531 | 31.913 | 34.639 |
| checkpoint_pipeline_save | 152.797 | 195.267 | 208.642 |

The read-only case reads a 19-byte file and launches no applet. The pipeline/save
case runs external `printf`, external `tr`, and a fixed output save as three finite
nodes. A completed resume restores output without running commands or rewriting
the completed journal. It does not measure a repaired suffix that executes new
commands. Checkpoint times include intent writes, binary identity hashing where
applicable, snapshots, completion writes, and directory synchronization.

#### Current listener handler milliseconds

| Case | p50 | p95 | p99 |
| --- | ---: | ---: | ---: |
| normal_read_handler | 0.035 | 0.048 | 0.055 |
| checkpoint_read_handler | 60.895 | 67.712 | 71.594 |

The listener handler reads the fixture without spawning a child. Durable mode
records the event, read result, and handler completion; it also prunes unused
snapshots. The observed cost is tens of milliseconds per event, while ordinary
mode stays in tens of microseconds. Durable checkpointing is explicitly opt-in;
these measurements do not support claiming low-latency durable execution.
Ordinary pipelines retain direct OS pipes. A complete live pipe group remains a
single checkpoint node; there is no per-byte journal traffic.

Raw measurements and script sources are in [recovery-v0.3.0-macos-arm64.json](recovery-v0.3.0-macos-arm64.json).
Recovery smoke evidence is in [recovery-smoke.json](recovery-smoke.json).
Reproduce from the project folder:

```sh
python3 benchmarks/recovery.py --binary bin/execute --samples 200 --output benchmarks/recovery-macos-arm64.json
```

The following 0.2.0 measurements remain a historical baseline; they are not
measurements of the current release.

---

### Execute 0.2.0 latency measurements after adversarial review

Measured on an Apple M1 Pro (10 logical CPUs), macOS 26.6.2, Darwin 25.6.0, arm64, with the optimized release binary. All 71 tests pass: 27 unit and 44 isolated integration tests. Formatting and Clippy checks pass. Linux execution and benchmarks have not been run; macOS/Ubuntu CI is configured.

#### Method

CLI cases use 200 fresh processes after 10 warmups, shuffled each round. OS file/executable caches are warm; no system caches were flushed. Times include Python subprocess launch, complete output capture, and process exit. Fixture/cache preparation occurs outside timing. Every sample's output is checked. Background applications were not controlled; these are local measurements.

The shell pipelines use the same binaries and input as Execute. Builtin `read` supplies the input file without an extra `cat`, matching shell redirection. The example `direct.program` explicitly names `cat` and adds that process. The shell `printf` baseline uses its builtin; Execute launches the external binary. Startup-only shell and Execute check perform different work. Cache misses include hashing and atomic publication; hits are verified by tracing.

p95/p99 are nearest-rank percentiles, with a small tail sample at 200 runs. First-byte cases use 50 samples. The result JSON includes scripts, paths, environment, and binary SHA-256. Each suite ran sequentially after compilation.

#### CLI end-to-end milliseconds

| Case | p50 | p95 | p99 |
| --- | ---: | ---: | ---: |
| shell_startup_only | 4.324 | 5.215 | 5.359 |
| execute_check_no_execution | 5.269 | 6.211 | 7.322 |
| raw_printf | 2.572 | 3.030 | 3.342 |
| shell_builtin_printf | 4.381 | 5.156 | 5.354 |
| execute_printf | 6.804 | 8.248 | 9.653 |
| shell_small_pipe | 6.748 | 8.083 | 9.191 |
| execute_small_pipe | 7.465 | 8.710 | 9.355 |
| shell_1MiB_pipe_count | 49.803 | 52.197 | 53.103 |
| execute_1MiB_pipe_count | 50.466 | 52.804 | 54.109 |
| shell_1MiB_pipe_copy | 50.055 | 52.265 | 54.390 |
| execute_1MiB_pipe_copy | 52.323 | 54.445 | 58.824 |
| execute_1MiB_finite_uncached | 52.996 | 55.929 | 56.957 |
| execute_1MiB_finite_cache_miss | 64.783 | 70.127 | 72.333 |
| execute_1MiB_finite_cache_hit | 12.034 | 14.070 | 15.730 |

The 16-byte native pipeline takes 7.46 ms in Execute and 6.75 ms through `/bin/sh`. The 1 MiB native count pipeline takes 50.47 versus 49.80 ms. Both use native OS pipes; this does not establish generally faster transport or startup.

The 1 MiB sequential workflow takes 53.00 ms uncached and 12.03 ms with a warm transformation cache (4.40× faster). A miss takes 64.78 ms, so caching has a measurable miss cost. This workflow caches `tr`, while generic `wc` caching is disabled because arbitrary arguments can read external files.

#### Running listener round-trip microseconds

One running interpreter handles 200 sequential requests after 20 warmups. Timing includes Python pipe write/read, record framing, named binding, handler work, unbuffered output emission, and scheduling. Startup is excluded. Stdin remains open, verifying delivery before producer EOF.

| Case | p50 | p95 | p99 |
| --- | ---: | ---: | ---: |
| read_fixture_no_applet_spawn | 27.480 | 58.125 | 113.208 |
| tr_external_applet | 1523.708 | 1695.875 | 2646.542 |

The first handler reads a 16-byte fixture without launching an applet; it is event dispatch plus file I/O, rather than pure interpreter overhead. The second launches real Unix `tr` for every record. The producer stays alive; ordinary handler binaries still incur process creation per event.

A 5 ms interval with the fixture handler had positive deadline lateness of **0.991 ms p50**, **1.779 ms p95**, and **2.945 ms p99**, over 200 ticks after 20 warmups. Deadlines are monotonic, missed ticks are skipped, and handlers never overlap. Scheduling is not a real-time guarantee. Calendar minute-boundary firing and live `tail` records were separately smoke-tested.

#### Interpreter and argument binding microseconds

These percentiles describe **batch averages**, not individual request latencies: 100 batches × 1,000 operations after warmup. Validation reuses loaded manifests and a parsed program. No applets launch, and no per-script JIT or native compilation is needed.

| Case | p50 | p95 | p99 |
| --- | ---: | ---: | ---: |
| parser_parse | 1.866 | 2.259 | 2.463 |
| printf_manifest_build_arguments | 0.106 | 0.108 | 0.110 |
| runtime_validate_loaded_registry | 0.291 | 0.295 | 0.313 |

#### Warm native pipe baseline

A persistent `cat` echoes one byte through two OS pipes in **4.71 µs p50**, **5.96 µs p95**, and **8.17 µs p99**, over 2,000 round trips after 200 warmups. This includes cat, scheduling, and Python calls. It excludes startup and is neither Execute event latency nor a one-way pipe estimate.

#### First-byte milliseconds

| Case | p50 | p95 | p99 |
| --- | ---: | ---: | ---: |
| shell_1MiB_pipe_copy | 7.070 | 7.952 | 8.983 |
| execute_1MiB_pipe_copy | 51.450 | 53.066 | 53.725 |

Execute captures final finite output before exposing it: 51.45 ms first-byte latency versus 7.07 ms for shell streaming. Intermediate applet pipes stream directly. Listeners emit after each handler, without waiting for source completion. Streaming finite terminal output and cooperating persistent handlers remain concrete latency improvements.

#### Reproduce

```sh
cargo build --locked --release
python3 benchmarks/latency.py --binary target/release/execute --samples 200 --output benchmarks/results-local.json
python3 benchmarks/services.py --binary target/release/execute --samples 200 --output benchmarks/services-local.json
cargo run --locked --release --example latency > benchmarks/interpreter-local.json
```

Evidence:

- [CLI/pipe benchmark](latency.py), [results](results-macos-arm64.json)
- [Listener/interval benchmark](services.py), [results](services-macos-arm64.json)
- [Interpreter benchmark](../examples/latency.rs), [results](interpreter-macos-arm64.json)
- [Live listener/calendar smoke checks](service-smoke.json)
- [Adversarial review](../DESIGN.md#adversarial-review-october-13-2026)

#### Explicit finite stdin candidate

`finite_stdin.py` compares the candidate's `read from stdin` with the preceding
release candidate's existing `read "/dev/stdin"` file workaround. It checks equal
native argv and exact output for 8 KiB/8 MiB byte counts and a 128 KiB binary
capture. Each sample starts a fresh CLI process with an input descriptor opened
before timing. Warm caches and Python process launch/capture are included.

```sh
python3 benchmarks/finite_stdin.py --baseline /path/to/baseline-execute \
  --candidate target/release/execute --samples 240 --report /path/to/paired-stdin.json
```

Campaign reports, raw samples and exact binary hashes are retained under
`work/optimization-all-dimensions-2026-10-14/finite-stdin/` in the workspace root.
These candidate comparisons establish no speedup claim, cold-cache measurement,
or native Linux execution. The feature adds an explicit input primitive with
bounded cancellation-aware capture; the prior file workaround remains a valid
baseline for data forwarding. Final finite output remains buffered and bounded.

#### Managed interval scheduling candidate

`reactive_timers.py` measures resume-to-completed-ticks through the actual live
Unix socket API. The paired programs use the same `.program` intervals and
native `printf` (or the shared `cd` builtin to isolate scheduling). It verifies
launch counts, output, pause boundaries and owned server/socket cleanup.

```sh
python3 benchmarks/reactive_timers.py --baseline /path/to/baseline-execute \
  --candidate target/release/execute --samples 20 --report /path/to/reactive.json
```

Server startup is excluded with its ready handshake. Each sample includes the
resume request and Python/IPC observation of completed runs; observed overshoot
is retained. Trials use fresh servers, two warmups and shuffled paired order.
This isolates the managed worker's previous fixed 20 ms wait and phase rounding;
it is not a general runtime speedup or hard realtime claim. Background activity
and native process startup still affect results. Campaign preimages, same-binary
baseline probe, paired reports and raw samples are retained under
`work/optimization-all-dimensions-2026-10-14/reactive-timers/` in the workspace root.

#### Buffered stdin writer failure and success probes

`input_writer_failure.py` uses an owned fixture whose parent exits while a
short-lived child retains the generated stdin pipe in its own session. The
child closes stdout/stderr and self-expires within 800 ms, or is released by the
harness cleanup. The probe records whether a 150 ms command deadline returns
a timeout instead of a late success. This is a functional failure probe, not
a successful-workload speed comparison. Native process and CLI startup are
included; the deadline itself applies to the native group.

```sh
python3 benchmarks/input_writer_failure.py --binary target/release/execute \
  --samples 3 --report /path/to/writer-failure.json
python3 benchmarks/input_writer.py --baseline /path/to/baseline-execute \
  --candidate target/release/execute --samples 80 --report /path/to/writer-pairs.json
```

The separate successful-workload harness feeds identical 4 KiB, 64 KiB and
1 MiB payloads through `read ... as input` and native `cat`. It verifies every
output and retains shuffled paired warm-cache release measurements, including
prefilled and threaded-input paths. Do not infer a throughput speedup from the
failure probe's changed status or elapsed time. Reports and preimages are in
`work/optimization-all-dimensions-2026-10-14/error-handling/` in the workspace root.

##### Live state scaling (pending source candidate)

`live_state_scaling.py --baseline PATH --candidate PATH --report PATH` compares
64 paused-listener `send` requests with 0, 1 MiB and 6 MiB of unrelated user
state. Setup, final FIFO/content checks and teardown are outside timing. Trials
use two warmups, shuffled paired order and identical records; RPC/JSON/Python
observation costs are included. The campaign retains raw distributions and
release binary hashes. This measures bounded queue admission, not process RSS,
whole-runtime speed or unbounded workload support. State boundary regressions
are separate functional tests, including rollback and repair before side effects.

##### Completed resume validation (pending source candidate)

`resume_validation.py --baseline PATH --candidate PATH --report PATH` measures
resuming completed checkpoints containing 1, 32 and 256 repetitions of native
`/bin/cat`. Fresh checkpoint creation is outside timing. Both builds validate
the same source/checkpoint, with three warmups and shuffled paired order. Exact
output, restored node counts, absent native respawns and unchanged journal bytes
are checked outside timing. The report hashes `/bin/cat`, both release binaries
and the harness. Results include CLI/trace overhead and use warm caches; they do
not describe fresh application execution or native Linux performance.

##### Checkpoint path security probes

`checkpoint_paths.py --binary PATH --report PATH` runs three functional probes
against directory, trailing-slash directory and blob-directory symlinks. Every
referent and sentinel belongs to a temporary fixture; the report records status,
output, target mode and entries. Baseline/candidate results distinguish target
mutation from rejection. This is a correctness probe, not a speed comparison.
Paired release measurements use `resume_validation.py` separately to quantify
normal completed-resume overhead of the added checks.

##### Static JSON diagnostics (pending source candidate)

`check_diagnostics.py --baseline PATH --candidate PATH --report PATH` interleaves
ordinary static checks from both release binaries and the candidate's JSON mode.
Sources cover valid partial-input programs, parse failures and missing files.
Ordinary output/error bytes must match; JSON framing, statuses and messages must
match the same result, with no applet effects. Five warmups precede timing;
reports include CLI startup/capture and warm caches. The additional JSON format
is measured separately, not treated as an output-identical performance win.

##### Widget publication scaling (pending source candidate)

`ui_state_scaling.py --baseline PATH --candidate PATH --report PATH` compares
actual headless renderer updates with 0, 1 MiB and 6 MiB of unrelated user state.
Each trial sums eight edit-to-ack and edit-to-applied segments; frame-content and
padding-preservation checks are outside timing. Setup is excluded, two warmups
precede shuffled pairs, and IPC/JSON/Python observation is included. The report
retains raw totals, binary/helper/harness hashes and warm-cache scope. This is
neither event-to-photon latency nor an RSS/allocation-count measurement. The
campaign also runs the existing core/allocation probes and broader native CLI,
pipe, sequential-stage, cache, checkpoint and listener comparison harnesses
separately from builds/tests.

### Native status capture, pending source candidate

`native_status.py` compares the pre-status and status-capable release builds on
identical programs/native argv, and separately labels candidate programs that
add `and keep status as name`. All variants use the same project catalog.
The latter programs have different source and retain an integer binding; their
success status and stdout match, but they are feature-overhead observations.
Five warmups precede shuffled interleaved trials, with exact output/status checks.

The campaign's `native-status/paired-latency.json` retains raw samples and binary
hashes. Warm macOS arm64 median observations: printf 5.406 → 5.346 ms (80 pairs),
three native stages 5.808 → 5.821 ms (80), 50 launches 62.152 → 62.197 ms (20),
and static check of 1,000 invocations 3.974 → 3.990 ms (80). Candidate capture
medians were 5.370 ms for printf and 62.233 ms for 50 launches. These small
changes do not establish a speedup, a per-status cost, a regression-free runtime
or a hard bound. Full distributions, including capture-tail variation, remain
visible. No cold-cache, RSS or native Linux measurement follows from this pass.

### Conditional control and validation, pending source candidate

`conditions.py` compares identical ordinary sources across the pre-condition and
condition-capable release builds, plus explicitly different candidate condition
sources with identical selected native argv/output. Trace probes verify actual
launch counts outside timing. It also measures 1,000 skipped conditions against
an empty script; this includes startup, parsing and validation and is not an
isolated per-branch measurement. All cases use the same manifest catalog.

The first conditions implementation exposed binding-table copying in validation.
`conditional_validation.py` then compares **identical conditional sources** before
and after reusing validation state for bodies that define no names. Bodies that
introduce named stdout/status results retain the copying/definite-result path.
Five warmups and shuffled interleaved release CLI trials retain exact status and
stdout checks, raw samples, source/catalog/harness hashes and archived binaries.

Repeat, 160 pairs: checking 1,000 conditions with 100 typed inputs improved
34.857 → 4.766 ms median (about 7.3×), p95 35.691 → 5.371 ms. Thirty-two nested
conditions with 100 inputs improved 4.946 → 3.954 ms. No-input and named-result
control cases remained similar. These are targeted validation gains relative to
the first implementation of conditions; installed 0.8.0 cannot parse these blocks.

The final ordinary-source comparison (100 pairs; 30 for 50 launches) measured
printf 5.204 → 5.180 ms, three native stages 5.727 → 5.695 ms, and 1,000-statement
check 3.985 → 3.967 ms. Such small differences do not establish a broad speedup.
Fifty-launch tails varied: median 63.605 → 62.782 ms, p95 65.109 → 66.898 ms.
Candidate condition variants retain similar selected-native timings. An empty
candidate script measured 3.778 ms versus 4.060 ms for 1,000 skipped conditions,
with zero native launches verified. Do not divide that aggregate difference into
an isolated dispatch latency. No cold-start, native Linux, RSS, file-change or
universal regression-free claim follows from these warm macOS arm64 samples.

Full reports and the pre-optimization sample remain under the campaign's
`conditions/` evidence directory. All three interpreter binaries are identified
by hashes despite their shared pending base version string.

### Native JSON boundary, pending source candidate

`structured_json.py` compares identical native/check controls across archived
release builds, then compares the existing `jq` extraction with the candidate's
parse/select/encode implementation of the same selected JSON result. Inputs have
ordered keys and exact byte/status/stderr oracles. Trace probes verify launches
outside timing. Five warmups precede shuffled interleaved fresh CLI samples on a
warm filesystem; no builds or tests run during timing. Sources, input hashes,
tool/catalog/binary hashes and all raw samples remain in the reports.

The repeat uses 160 trials per variant. For a document with roughly 1 KiB of
padding, the candidate avoids the helper launch and selects/encodes the result
in about 4 ms. At 1 MiB, the previous jq workflow measured 12.504 ms median
versus 4.691 ms for native JSON; at 3 MiB, 24.548 versus 5.930 ms (about 4.1×).
The candidate running the unchanged jq source measured 12.452 and 24.492 ms,
respectively. Exact trace counts are one native helper versus zero.

These are complete task latencies for different explicit implementations of a
simple field extraction, including interpreter startup and helper avoidance.
They are not isolated JSON-parser timings, a general replacement for jq,
arbitrary-precision arithmetic, RSS bounds or a whole-runtime speedup. Native
printf, three-stage pipes and static-check controls remain similar; full tails
are retained rather than claiming a regression-free runtime. JSON is finite and
byte-bounded; streaming queries and CSV transforms remain outside this pass.
The campaign's `structured-json/paired-latency.json` and
`paired-latency-repeat.json` preserve both runs. The installed release is unchanged.

### Reusable finite programs, pending source candidate

`reusable_programs.py` compares archived release builds on identical explicit
native/check controls and on equivalent finite tasks written with inlining or
same-file program calls. It also accepts `--baseline-programs` for **identical
reusable sources** on two function-capable builds. All variants have exact
stdout/status/stderr checks; separate traces verify native launches. Five
warmups precede shuffled interleaved fresh CLI samples, serial on a warm
filesystem without concurrent builds/tests. Reports retain source/input/catalog/
harness/binary hashes and raw distributions.

The first implementation copied resolved arguments through two maps and copied
call names. Binding now validates/moves each resolved value straight into its
local scope and borrows immutable call metadata. The 160-pair identical-source
repeat measured 1,000 scalar calls at 6.164 → 5.908 ms median (about 4.2% lower),
and 100 calls receiving a 64 KiB JSON value at 6.330 → 5.803 ms (about 8.3%
lower). The initial 80-pair run showed the same direction. Fifty native calls
remained similar; these are targeted aggregate task improvements rather than
isolated per-call measurements or a general runtime speedup.

Reuse still has a cost. The final 100-pair feature comparison measured candidate
inlined versus reusable versions at 4.501 versus 5.906 ms for 1,000 scalar
operations, 4.438 versus 4.825 ms for the JSON case, and 71.906 versus 72.044 ms
for 50 native launches. Parsing, bounded expansion, validation, isolated typed
values and execution are all included; do not divide these differences into an
isolated dispatch cost. Native controls were broadly similar with visible
host/tail variation (for example printf median 6.826 → 6.961 ms and p95
7.860 → 8.267 ms across the pre-reuse/final builds). No regression-free,
cold-cache, RSS, native Linux or universal performance claim is established.

The campaign's `reusable-programs/` directory retains the first feature run,
before/after binding comparisons, the final feature comparison, all interpreter
binaries, preimages and five/sixteen-level finite nesting proofs. These are
same-file finite scopes, not imported modules or lifecycle-owned child services.

### Model adapter overhead (source candidate)

`model_adapter.py` compares eight ordinary native configurations against a
pre-adapter release binary and measures fixture-only model startup separately.
Save a known baseline binary, then run from the repository root:

```sh
cargo build --locked --release
python3 benchmarks/model_adapter.py --project . --baseline /path/to/baseline-execute --report target/model-adapter.json
```

The harness uses 80 measured samples per variant, eight excluded warmups,
alternating baseline/candidate order and exact independent stdout/status oracles.
Run it without concurrent builds, tests or benchmarks. The full compatible
catalog removes the new `uncertain_on_failure` field from both variants because
the old parser rejects it; the native controls never call a model. Minimal
controls use one unused manifest. These are warm process-launch measurements
including Python subprocess launch/wait, rather than isolated interpreter/pipe
latency or cold-start tests. Fixture model runs are descriptive, not paired, and
never call a provider.

The two local runs found similar native medians, with no speedup claimed. The
offline applet median was 2.94–2.98 ms; a fixture call through `.program` was
6.09–6.32 ms. See the [validation receipt](../docs/model-integration-validation.json),
[first run](../docs/model-integration-validation/model-adapter-first.json) and
[repeat](../docs/model-integration-validation/model-adapter-repeat.json) for exact
hashes, raw samples, p95 values and limits. Earlier single synthetic live calls
only verified connectivity; their elapsed observations are not API benchmarks.

The 0.9.0-dev source candidate preserves unhandled native CLI status codes. The
current Unix matrix requires the actual native failure status; older reports
retain the generic status-1 contract they measured.

#### Process inputs, source 0.9.0-dev

`process_inputs.py` compares a pre-feature helper pipeline with typed environment
inputs, plus an unchanged literal-command control and equivalent Bash commands.
Every sample checks exact bytes, stderr and status; separate traces verify three
native processes become one. Two checked-in 80-sample runs measure an environment
workflow median of 8.83–8.89 ms before and 5.43–5.52 ms after (about 38% lower).
Bash is 4.94–4.95 ms. Literal controls remain about 5.32–5.34 ms. These are warm
macOS arm64 process measurements including Python launch/capture; no universal
interpreter, cold-start, Linux, memory or desktop improvement is claimed.

```sh
python3 benchmarks/process_inputs.py --baseline /path/to/prior/execute --candidate target/release/execute --report target/process-inputs.json --samples 80
```

Raw samples: [first run](process-inputs-v0.9.0-dev-macos-arm64.json),
[repeat](process-inputs-v0.9.0-dev-macos-arm64-repeat.json).

The source 0.9.0-dev streaming/lifecycle proof is
[recorded separately](replacement-proofs-v0.9.0-dev-macos-arm64.json): exact
1,000,000,000-byte SHA-256 and ten cycles / 70 lifecycle probes on macOS.
Use `--native-failure-code 7` for source 0.9; the harness defaults to packaged
0.8's generic failure code 1 so historical release checks remain explicit.
This is functional integration evidence, not the full 10,000-cycle gate,
Linux parity, memory-scaling proof or a timing benchmark.

`make unix-smoke` runs the real 110-case command matrix on the current host.
Native macOS and Ubuntu CI run this target and retain its JSON plus the
streaming/lifecycle smoke report as `native-proofs-<os>` artifacts for 14 days.
These are functional checks with descriptive timing only; Ubuntu is not an
Arch/Omarchy desktop and CI load is not controlled for performance comparisons.

#### Matched current-source Execute/Bash/Nushell workflows

`matched_language.py` runs the three checked-in public-development reference
workflows (`filesystem_inventory`, `layered_config_merge`, and
`typed_event_normalizer`) through current-source Execute, Bash, and an explicitly
pinned Nushell binary. It runs all six valid fixture cases. The independent
oracle checks exact status, stdout, empty stderr, the declared output file, and
the full workspace delta for each arm before any sample is recorded and after
every warmup/sample. All arms use one controlled working directory per fixture;
Execute receives the explicit immutable manifest catalog. Sources, binaries,
catalog, external tools, fixture payloads, and child environment are retained or
hashed in the report. A missing Nu binary or external workload tool is shown as
an unavailable cell. Execute-only applet matching and typed-graph workloads and
external failure contracts remain explicit unmatched cells.

These public references are fixture artifacts for checking the evaluator, not
model-generated submissions or a held-out task study. The benchmark describes
whole-workflow runtime only; it does not measure programmer effort, generated
tokens, maintainability, isolated interpreter speed, CPU or RSS. The reference
workflows may use different command plans (for example, Execute's internal
`save` versus shell `tee`), so results include those workflow choices.

The Nushell CLI should come from an exact official release archive with its
published archive SHA-256. The [Nushell release page](https://github.com/nushell/nushell/releases)
lists platform artifacts and checksums. Use a private work directory and pass
the explicit paths and release label; the harness never installs Nu globally
or downloads it. One example invocation is:

```sh
python3 benchmarks/matched_language.py \
  --execute-binary /path/to/current/execute \
  --execute-source-root /path/to/current/source \
  --execute-revision current-source-label \
  --manifest-dir /path/to/current/manifests \
  --bash-binary /bin/bash \
  --nushell-binary /path/to/pinned/nu \
  --nushell-archive /path/to/pinned/nu-release.tar.gz \
  --nushell-archive-sha256 published_archive_sha256 \
  --nushell-release-url https://github.com/nushell/nushell/releases/download/version/asset.tar.gz \
  --report /path/to/new-report.json
python3 tests/matched_language.py
```

The default schedule is 30 measured three-arm rounds and five excluded warmups,
balanced across all six language orders. Raw base64 streams, per-sample wall
times, paired differences against Execute, exact environment, artifact hashes,
and unavailable/unmatched cells are retained. Run only in a serialized idle
window. This code addition includes no comparative timing receipt.

#### Retained equal-plan controls and mixed lifecycle run

The [equal-command-plan control](equal_command_plan_control/README.md) retains
1,152 measured invocations over six public fixtures on macOS ARM64 at source
`58b42f6`, plus 120 excluded warmups and 24 preflights. All byte/status/effect
checks passed. Bash had the lowest median in every fixture; full-catalog
Execute medians were lower than Nushell. The one-entry unreferenced catalog
reduced paired Execute medians by 2.824–3.758 ms; it is a diagnostic arm rather
than default runtime behavior. Raw samples, source/tool hashes, independent
public-fixture audit and reproduction sources are retained.

The [Linux mixed lifecycle receipts](mixed-runtime-ci-38833931575/README.md)
retain the successful 32- and 1,000-actor runs and the earlier 846/847-timer
failure. The later 1,000-actor run observed all 847 timers during its measured
1,042,569 µs window and reaped all 16 registry workers. Its 44,191,744-byte
sampled RSS peak covers the Rust probe process only. This bounded run does not
establish independent capability count, process-tree capacity, sustained
operation, desktop behavior, or cross-revision throughput.

The [fixed 3 × 10-second Linux replication](mixed-runtime-ci-38835685785/README.md)
retains every declared trial, each with all 847 timer sources active and all 16
registry workers reaped. It extends the bounded lifecycle evidence; it does not
establish sustained operation or process-tree resource limits.

[Package validation evidence](package-validation-38836416853/README.md) records
all six native targets, both Flatpak targets and the exact twelve-asset hash
set for source `58b42f6`. The newer source candidate remains unpublished.

The [paired JSON-check control](check-json-paired-control-macos-arm64/README.md)
retains all three fixed runs: 2,304 measured invocations, 240 excluded warmups
and 48 preflights. Full 75-manifest `check --json` medians were 24.0–27.2%
lower across the six measured cases. The first one-step p95 regression is
preserved; ordinary-check and inert-catalog controls were mixed. This is a
narrow local static-check result, with no general execution-speed claim.

The [single five-minute mixed-graph observation](mixed-runtime-ci-38840425578/README.md)
passed its registered 300-second window, source coverage, cancellation and
worker-reaping checks. It retains process-local RSS/thread samples and the
original preregistration; the fixed graph and whole-window coverage do not
establish production endurance, churn or process-tree resource limits.

#### October 16 graph and catalog measurements

These fixed paired comparisons retain raw samples, preregistrations, source and binary identities, independent audits, and observed losses. Each has its own workload and baseline.

- [Any-input graph receive on Linux](graph-recv-any-ci-38851233022/README.md): preloaded bounded queues.
- [Named graph receive on macOS arm64](graph-named-recv-macos-arm64-20261016/README.md): preloaded named receive and zero-timeout calls.
- [Graph emission on macOS and Linux](graph-emit-paired-macos-linux-20261016/README.md): four payload/fanout cases, including a macOS eight-sink regression.
- [Explicit packed manifest catalog](manifest-catalog-paired-2026-10-16/README.md): library representation and actual CLI selector comparisons; default discovery is unchanged.
