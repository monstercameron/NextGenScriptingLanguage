# Unix compatibility review and benchmark gates

The [supplied review](unix-review-source.md) evaluates example syntax and proposes acceptance tests. Its high
readability/AI scores are hypotheses, not measured properties of the Rust runtime.
This document relates its claims to the implementation and the new benchmark run.
The [replacement challenge ledger](replacement-challenge.md) now tracks the
fifteen broader acceptance gates and adds versioned 1 GB streaming/lifecycle
probes. Its evidence does not retroactively change these historical measurements.

The product remains a small Unix glue language: native applets compute, `.program`
declares application control flow, and Rust supplies generic supervision/widgets.

## Existing contracts and genuine gaps

| Review concern | Current implementation | Remaining gap |
| --- | --- | --- |
| Persistent `tail` | A listener starts its producer once, retains it, and reaps owned groups when the service ends | Transform applets in the handler start once per record, not once per stream |
| Stop after three lines | `--max-runs 3` limits listener handlers and stops the producer | Original ladder example 38 used a runner-supplied cutoff; current utilities use documented bounds |
| Backpressure | Native intermediate pipes use bounded OS buffers; sequential listeners stop reading while handling/emitting a record | Finite final output is captured, bounded by `--max-output-bytes`, and delivered after completion |
| Record value | Named record is UTF-8 text including its newline; its bytes become the handler's initial input | Ports/results lack schema-checked structured types |
| Where results go | CLI emits completed final values on stdout; listeners emit each completed handler result on stdout | No script-directed general stderr/stdout redirection or explicit streaming final sink |
| JSON display chaining | Rows, fields, bars and points read the same worker-output bytes; UI actions do not consume those bytes | UI JSON projections share one parsed result; earlier named native results cannot be used as arbitrary UI values |
| Refresh overlap | One native worker per placed component; pending handler events coalesce | No general parallel-task syntax or selectable per-handler concurrency policy |
| Stale repository results | Changed incoming ports cancel old workers, advance generation, and discard old results | Local draft edits use snapshots and do not cancel work until the script sends a new input |
| History ownership | In-memory per-widget series keyed by the script's chart key, at most 120 points/series and 16 series | Limits/retention are runtime defaults; persisted or configurable history is absent |
| Quit and polling | Script close exits the renderer and cancels/joins owned workers; service signals stop owned groups | Descendants deliberately escaping groups are not sandboxed or contained |
| Middle-command failure | Any observed native failure stops execution, cancels peers, and prevents later saves | CLI returns generic status 1; structured status values and recoverable shell-style branches are absent |
| Counters/ordinary loops | Schedules and listeners provide fixed reactive loops; finite sequences and named text results exist | Conditions, counters, ordinary loops and CLI parameter binding are not implemented |

`execute explain listener.program` now tells authors that producer lifetime and
per-record native launches differ. This distinction is part of the language's
performance contract, not a Rust implementation detail to hide.

The canonical UI spelling now names the shared result explicitly:

```text
run gh.repo with repository repository
then show the result as rows in metrics
then add field "stargazerCount" to history
then show field "description" in description
```

`show JSON as rows/bars` remains compatible. A regression checks both spellings,
one command invocation, and the table/chart/description reading one result.

## Nine requested benchmark categories

| Requested category | Evidence gathered now | Interpretation |
| --- | --- | --- |
| 1,000 trivial starts | 1,000 fresh interpreters/runtime, ten warmups, identical external printf | Filesystem/executable caches are warm; this is not a true cold-cache measurement |
| 1,000 external commands | Five measured batches/runtime, one warmup, same external true; separate trace counts exactly 1,000 launches | Execute uses unrolled statements; Bash uses a loop; source sizes are recorded |
| One million lines through tr | Five measured batches/runtime, two warmups, same cat/tr, 11 MB output checked by SHA-256; separate RSS observation | This is a finite native pipeline; final Execute output is buffered. It does not prove a persistent line listener |
| Follow file and stop after 3 | Five paired trials, four source lines, exactly three outputs and no live owned producer afterward | Execute uses a CLI limit, Bash a script counter. The script-only acceptance gate remains open |
| 100,000 JSON records | Same jq predicate/sort in both runtimes, independent Python output oracle, five samples/runtime plus separate RSS | Measures orchestration and the shared applet; it does not prove typed language-level values |
| 100 independent commands concurrently | Explicitly unsupported | No Python fan-out is credited as `.program` parallelism |
| Fail middle command | Read-only fixture returns 7; both runtimes must report failure and prevent later effects | Bash with pipefail returns 7; Execute currently returns 1 |
| AI generation of 100 scripts | Pending | Requires held-out tasks, actual model outputs, token accounting, semantic checks and recorded repair turns |
| Poll 10 sources while switching | Exact workload unsupported | Current UI places at most two components. Existing PTY switching/cancellation coverage is narrower evidence |

Two additional probes target the critical architecture questions: a live stdin
listener and equivalent Bash loop record event latency, and a trace asserts one
`tr` launch per Execute event; a controlled
producer emits early bytes, waits for an explicit release, then exits. The latter
checks whether the final consumer sees output before producer completion.

Nushell was not installed on this host and has not been measured. There is no
Nushell performance claim. OS cache flushing and scheduler tuning were not done;
background activity is uncontrolled. Do not compare historical runs as if they
were matched trials from this run.

## Method, safety, and interpretation

The runner is `benchmarks/unix_workloads.py`. All fixtures, output files and helper
applets live in a temporary directory. Benchmarks use the same installed external
binaries, inherited PATH, sanitized startup environment, and `LC_ALL=C`. Bash/POSIX
startup-hook variables and imported functions are removed; dedicated regression
checks verify the hook cannot execute and an over-budget line count fails before
creating fixtures. Wall-time and cancellation pairs are shuffled deterministically.
RSS observation order is shuffled separately. Fixtures and correctness checks are
outside timed regions. Phases run sequentially.

RSS is one descriptive `/usr/bin/time` high-water observation per runtime/case,
converted to bytes. It is not a sampled distribution or summed simultaneous peak
memory of the process tree. Low-sample bulk/cancellation p95 and p99 values are
mostly the maximum observation and do not establish a reliable tail distribution.
Execute cancellation trials include trace collection; Bash trials do not, so
these timings prove behavior and are not a clean interpreter-overhead comparison.

Cleanup first requests graceful termination of the owned runner. Execute's own
supervisor then kills/reaps its separate applet groups. Forced runner-group
termination alone cannot guarantee cleanup of those separate groups if the
interpreter is wedged; escaped sessions remain outside the contract. The harness
never signals remembered descendant PIDs during fallback cleanup. Normal trials
assert producer disappearance and no extra fourth output.

The release hash, harness hash, versions, applet/catalog hashes, counts, raw timing
samples, outputs and unsupported statuses are saved in the JSON report. A partial
report is written after each phase; only a successfully completed run establishes
all its recorded acceptance checks.

## Persistent stream implementation

The explicit `stream from` mode now supplies a native pipeline, a script-declared
final-record limit, and a raw stdout line sink. See [the streaming contract](streaming.md).
The macOS acceptance probe uses `tail` once and `tr -u` once, receives three live
uppercase records, drops the fourth, and verifies both children are gone at exit.
Native buffering remains explicit; Linux runtime acceptance is still pending.
The historical example 38 measurements used per-record handlers and a runner
limit. The current 150-utility collection replaces that original numbered ladder;
those historical benchmark results retain their original workload meaning.

## Remaining small-language work

The next targets are numeric exit status and stderr as explicit result metadata,
then small conditions or counters where ordinary glue requires them. Schema-checked structured
pipeline values remain unimplemented. Release 0.6.0 adds opt-in ephemeral
[managed runtime/widget state](live-control.md); it is distinct from durable
checkpoint recovery and does not add general shell variables or structured ports. The new
stream mode does not add general shell control flow or claim that arbitrary
native applets flush each line promptly. Linux native stream acceptance remains
an open platform check.

The completed release observations are in [the benchmark report](../benchmarks/README.md)
and [raw JSON](../benchmarks/unix-workloads-v0.4.6-macos-arm64.json). The next target
is supported by observed per-record launches and delayed final output, rather
than inferred from syntax.
