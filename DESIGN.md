# Execute architecture

Execute composes actual Unix CLI programs through a controlled-English language.
Files use `.program`; `execute app.program` selects its lifetime from its contents.
Programs remain plain text and manifests remain declarative argv bindings.

The [source map](CONTRIBUTING.md#find-the-right-code) identifies implementation
boundaries. The [release/source table](README.md#release-and-source-status) and
[language reference](docs/language.md) describe current availability and contracts.
The dated sections below retain the design decisions and evidence for their
recorded versions.

## Implemented

- Explicit persistent native streams: `stream from`, native `pipe to`, optional
  `take the first N lines`, and raw `show each line`; bounded final framing,
  output backpressure, cancellation, and intentional process cleanup at the cap.
- One-shot finite pipelines with native pipes, stdout/stderr conventions, exit
  failure propagation, bounded capture, atomic saves, and text result bindings.
- `cd` as a context builtin; `pwd`, `ls`, `cat`, `printf`, `tr`, `wc`, and `tail`
  as explicitly named program bindings.
- Interval apps using monotonic fixed-phase deadlines, with missed ticks skipped.
- Five-field numeric cron calendar matching in the machine's local timezone.
- Live stdin/applet line listeners; source remains alive, body runs per record,
  output flushes per handler, and bounded synchronous handling backpressures input.
- Whole-program check/explain before execution, finite transformation caching,
  signal cancellation, process group ownership, traces, and isolated tests.
- Direct quoted executable calls and typed CLI inputs in packaged 0.8.0.
- Pending source primitives: finite inherited stdin, inspectable input interfaces,
  structured check diagnostics, native exit-status bindings, scalar conditional
  blocks, finite JSON operations and same-file finite program calls. Calls lower
  into bounded, scoped instructions in the shared engine; conditions/calls are
  whole checkpoint/live nodes. These features have not changed installed bundles.

A service loads the catalog and parses its body once. Each ordinary command is
still a one-shot process. A `listen to` producer remains running between events;
a `stream from` pipeline starts each native stage once for the whole stream.
Persistent request/response worker protocols are a separate future feature.

macOS and Linux share process, descriptor, signal, and `poll` contracts. Tests have
run locally on macOS; CI targets both macOS and Ubuntu. No system cron, launchd,
systemd, or shell-profile configuration is changed by running these apps.

## Next measured improvements

The explicit stream mode supplies line-by-line final output where partial-result
semantics are acceptable; finite capture still delays first-byte delivery.
Keep byte sinks, record framing, and captured values explicit.
Add cooperating worker manifests and a versioned framed request/response protocol
when avoiding process startup per event is necessary. Keep ordinary binaries
supported; a pool cannot make an arbitrary one-shot command reusable.

Extend finite JSON values toward CSV and structured streaming where justified.
Add external modules, structured error results, bounded concurrency and explicit
retry policies. Preserve readable canonical phrases and define scope/order before
expanding the grammar. Native deadlines already exist; broader filesystem/CPU
supervision needs separate evidence and contracts.

Cache only transformations whose dependencies are declared. Add dependency
fingerprints, binary/version pinning, cache eviction, and immutable artifact
reuse. Effects such as commits, HTTP writes, and widget updates remain effects.

Use kqueue/epoll backends only when measured scale warrants replacing portable
`poll`. Add launchd/systemd packaging when supervised installed mode is needed,
while keeping project-local files and the foreground executable portable.

## Performance contracts

Benchmark parser/validation independently from CLI launch and applet cost. Measure
live listener request latency and interval lateness separately. Compare shell
pipelines with the same binaries and input; distinguish finite buffering from
streaming first-byte latency. Report percentiles, sample sizes, build mode, machine,
and warm/cold conditions. Native pipes are the transport baseline; speed gains
come from avoiding shells, repeated startup, repeated parsing, or cached work.

## Adversarial review, October 13, 2026

The review exercised malformed scripts, real Unix command behavior, cached file
operands, executable aliases, and backpressure during shutdown. It found and
fixed the following defects, with regression coverage:

| Failure | Correction |
| --- | --- |
| Resolving executable symlinks changed multicall applet identity. | Preserve the invoked executable path. |
| Generic `wc` could cache external file reads supplied through `flags`. | Disable caching for that binding; fixed stdin-only adapters may cache. |
| Full stdout or trace pipes blocked signal cancellation. | Poll nonblocking writes with cancellation checks, restore descriptor flags, and make final error diagnostics best-effort. |
| Named text results passed static checks for integer/boolean/list parameters, then failed after earlier saves. | Reject statically impossible reference types before execution. |
| Literal NUL paths/arguments passed checks but failed during later file operations or spawn. | Reject these paths and argv values during validation/catalog loading. |
| Single-word, parameterless listener sources were rejected. | Accept `listen to pwd` and similar sources. |
| Results named `true` or `false` could never be referenced as text. | Reserve boolean literal words. |
| The Unix command `as` was parsed as a result modifier, and a second parameter named `cache` was parsed as a cache modifier. | Resolve keyword meaning by command position and recognize `and cache` only as a terminal suffix. |
| Combining `cd` and folder setup passed parsing but failed validation. | Track data-producing steps independently of directory changes. |
| An external command named `cd` passed service setup checks. | Require the explicit `cd` builtin in setup. |
| Trigger-only CLI limits were silently ignored on finite programs. | Reject limits on incompatible program kinds. |
| Recursive list parsing had no depth bound. | Bound nested list parsing at 64 levels. |
| The `tail` binding silently enabled endless follow mode for ordinary runs. | Use Unix defaults and explicit follow flags; retain the older qualified adapter. |

The syntax has three distinct data contracts: `run` begins a command with empty
stdin, `then` passes a completed value to a command that accepts stdin, and
`pipe to` connects concurrent command descriptors. These distinctions should stay
explicit as the grammar grows. `with name value and name value` has a fixed shape;
`as name` precedes `and cache`. Exact Unix flags remain visible where supported.

There are still design limits. A named result is raw UTF-8 stdout, including its
newline: `pwd as folder` followed by `cd with path folder` needs explicit text
normalization and will usually fail as written. Structured text/scalar adapters
are the next appropriate extension; implicit trimming would corrupt payloads.
Only the final finite value is emitted, which differs from shell command lists.
An explicit output statement would improve multi-result scripts. `when_present`
in a manifest tests presence, including a boolean value of `false`; a future
boolean-flag mapping needs an explicit truth condition.

Line handlers are sequential, and slow handlers can lose events from a producer
that cannot tolerate backpressure. Sources must flush their records. Cache purity
is an author contract, and static validation is not a transaction or a guarantee
that later runtime effects succeed. Applets must remain alive until their work
finishes; descendants that escape their process group can retain stdout and keep
the runner waiting for EOF. The runtime does not sandbox arbitrary applets or
enforce a universal command deadline. File/FIFO/filesystem I/O also follows Unix
blocking behavior. Linux CI is configured but has not been executed here.

The final review checks pass: 27 unit tests and 44 integration tests (71 total),
formatting, Clippy with warnings denied, and the optimized release build.

## Durable recovery

Checkpointing is opt-in and separate from the pure-result cache. `execute FILE.program
--checkpoint DIR` refuses to overwrite an existing journal; `execute resume
FILE.program --checkpoint DIR` opens it under an exclusive flock. Identity includes
the canonical program path, starting directory, and finite/service trigger mode.

Before each finite node, the runtime durably writes an intent. After success it
persists content-addressed output and binding blobs, the physical cwd, and a
completed prefix. Files are checksummed and installed by fsync/rename with parent
directory sync. Reads materialize their bounded bytes in this mode. A contiguous
live pipe group is a single node; its intermediate streams remain native pipes.
Compatibility hashes exclude diagnostic line numbers and include step semantics,
manifest serialization, invoked executable paths, and binary contents. Resumption
checks every completed node before effects; edits to failed/unexecuted suffixes
are permitted. The stable semantics hash version must change when runtime semantics
change incompatibly. Global environment and upstream files are intentionally not
re-evaluated: this is continuation of frozen work, not a fresh dependency build.

A failed command retains the last completed snapshot. Retrying a known failure is
explicitly authorized by `resume`; partial effects remain possible. An interrupted
or abandoned running intent for a non-cacheable command requires
`--retry-uncertain`. Persistence failures preserve the earlier intent rather than
marking an uncertain effect as an ordinary failed command. Commands declared
cacheable are trusted to be safe to repeat; manifest declarations are a contract.
Builtins and reads are repeatable nodes. Interrupted saves also require explicit
uncertain retry, since an outside process may have changed the target afterward. No external
transaction can be made atomic with this local ledger, and no PID recovered from a
crash is blindly killed (PID reuse).

Services store a record's initial bytes and binding before executing the handler.
Each successful handler returns the journal to idle after output delivery. A pending
handler is resumed before a producer is restarted. There is no producer acknowledgment,
durable event queue, stream cursor, or exactly-once stdout delivery. Checkpoints
cover the current handler only; new events reset its completed prefix.

Service checkpoint completion also prunes unreferenced blobs under the exclusive
lock, after the Idle journal is durable. Cleanup is best effort; it never removes
current snapshot outputs/bindings or stops a successfully completed handler.

## Utility collection and retained stress regressions

The current `examples/ladder` contains 150 useful utilities with external inputs,
output contracts, exact stdout/file oracles, and 146 alternate-input checks.
The `.program` files own the workflows, including explicit creation of nested
output directories. Python prepares editable sample inputs and verifies results;
it does not compute application outputs. Nine intentionally failing or repaired
programs from the original ladder live in `tests/fixtures/legacy-ladder`.

The original fifty-example stress review exercised actual applet composition,
not proposed language features. It includes checked failures and repaired programs,
binary data, a long pipe group, large streamed input under a small capture limit,
ordered file lists, cache invalidation, 1,000 sequential records before EOF, a real
file-following producer, and a real calendar boundary. Its oracle checked outputs,
files, traces, and skipped effects. CI runs the current utility collection and
retained failure/recovery regressions with bounded processes.

It exposed a parser restriction that allowed only `run cd` setup before a service.
Setup now also accepts chained `then cd`; runtime validation still requires the
actual cd builtin and prevents an external command from masquerading as setup.
It also exposed a stale in-flight record when a repair removed the failed terminal
node. After completed-prefix validation, explicit resume can durably discard that
removed tail. The existing uncertain-side-effect gate applies before the discard;
fresh execution cannot discard an in-flight node. CLI review also found that global
help/version scanning hid missing option values. Help and version are now parsed
as options in order, so an option value such as `--manifest-dir --help` is rejected.

`cat` file lists and the `grep`, `sort`, `uniq`, `cut`, and `head` manifests expand
the catalog without grammar changes. Fixed stdin-only bindings can be cacheable;
generic CLI bindings retain conservative non-cacheable declarations.

## Single-file TUI prototype (0.4.0)

A screen places one or two named component programs. Each program declares text
ports, a vertical widget layout, and event handlers. Connections deliver explicit
messages; cycles, undefined ports, invalid widget targets, and input-field binding
collisions are rejected before any command starts. Native handler steps reuse the
existing manifest registry and finite execution engine, with an explicit binding
map and cancellation token rather than per-worker signal registration.

One renderer owns the terminal. Each component has a bounded, coalescing event
queue and at most one command worker. A generation counter invalidates stale
results when inputs change. Native stdout is captured for explicit text/JSON
projection; stderr is nonblocking, drained, and retained up to 8 KiB for errors.
Failures in a failure handler terminate that handler instead of recursively
re-enqueuing it. Worker cancellation, timeouts, and terminal restoration use RAII.

The tracker implements eight widgets and a session line chart with 120 points
per repository, capped at 16 repository histories. The `gh.repo` manifest places
repository text after an explicit `--` argv terminator and disables result caching.
Read-only live queries have been checked with the installed GitHub CLI. Fixture
and PTY tests cover argv mapping, projections, invalid interfaces, failed/invalid/
missing/negative data, full stderr, keyboard actions, resize, repository changes,
stale-query cancellation, owned-process cleanup, and SIGTERM terminal restoration.
A wider element catalog is documented separately; it is not all implemented.


## Control-flow ownership (0.4.1)

Application control flow is interpreted from `.program` handlers. Rust does not
choose an operation from a component name, button name, repository name, or chart
field. Script-defined key handlers route keys to `press <button>` or `close the
screen`; repository-arrival and timer handlers route to the same declared button
handler. The footer derives shortcut names from the AST. Chart axis labels are
also script configuration. Runtime error fallback no longer mutates a chosen badge.

Terminal editing/activation, emergency interruption, rendering, process lifecycle,
resource bounds, and event scheduling are generic interpreter/widget operations.
Failure recovery provenance follows button dispatch so a failure in recovery
cannot recursively generate a new recovery event. Validation rejects duplicate
screen shortcuts, reserved interruption/editing keys, undefined/non-button press
targets, button-routing cycles, and actions following screen closure.

Regression tests change only program text to rebind keys, rename action targets,
change execution order, remove the initial query, and choose error display. The
same interpreter executes these different flows without a Rust rebuild.

## Generic bar charts and a Unix CPU monitor (0.4.2)

Bar charts project explicit label/value fields from JSON arrays. Values remain
finite nonnegative floating-point numbers; renderer tick normalization does not
change the displayed value or snapshot. Data is validated completely before
replacing a view, with a 256-bar bound and Unicode-aware label truncation. A
successful ordinary UI handler clears its prior diagnostic; recovery handlers
preserve failures until a native retry succeeds.

The CPU monitor is another single-file component program. It invokes `ps` and
`jq.ps`, routes the resulting JSON through a text port, and declares initial load,
a two-second timer, refresh/quit shortcuts, projections, and failure display.
The reusable jq adapter contains text decoding/numeric ranking logic and accepts
its limit from the script. There is no CPU workflow or ranking in the interpreter.


## Native macOS installation (0.4.3)

The PATH command is a symlink into a versioned binary-and-manifest bundle. The
interpreter resolves its executable's launch symlink before locating its sibling
manifest directory, because macOS can return the launch path from current_exe.
A relocation regression supplies a unique manifest absent from the checkout and
runs a copied executable through an unrelated symlink, proving that the bundle
provides its resources. Updating the current link leaves running binaries intact.


## Workflow file URIs (0.4.4)

The workflow argument accepts native paths and local file URIs, decoding percent
escapes into Unix path bytes while preserving literal plus signs. Empty or
localhost authorities are local; remote hosts, relative URI paths, malformed
escapes, NUL bytes, queries, and fragments are rejected with specific diagnostics.
Encoded filename characters remain valid. Lookup and execution after decoding
use the same path behavior, applet registry, and working-directory semantics.

## Adversarial refinement (0.4.5)

The [refinement report](docs/refinement.md) records independent review, fixes,
regressions, operating contracts, and remaining evidence gates. UI handlers now
snapshot referenced widget values, update their own writes in action order, and
wait for actually referenced ports before any actions run. Input fields and text
messages validate bounds/NUL both statically and dynamically. Headless run limits
are rejected instead of ignored. The native runtime supports command-group
deadlines and marks timeout effects uncertain for checkpoint recovery. Trace
writers serialize descriptor flag transitions with cancellable/deadline-aware
lock waiting. Production or usability scores are not inferred from passing tests.

## Unix execution evidence (0.4.6)

The [Unix review](docs/unix-compatibility-review.md) distinguishes persistent
listener producers from per-record handler commands, finite terminal capture from
native intermediate pipes, and supported workloads from proposed syntax. The
matched-workload runner checks output bytes, process counts, real tail cleanup,
JSON semantics, final-consumer streaming, startup hooks, and fixture budgets.
`explain` exposes per-record launch behavior. UI result projections accept the
clear `show the result as rows/bars` spelling while retaining older JSON syntax.
The script itself still cannot declare the example-38 cutoff; that remains a
specific implementation target, rather than a benchmark-runner feature credited
to the language.

## Opt-in live control (0.6.0)

`execute live serve` hosts a local Unix socket beside a supervised execution
worker. The socket accepts bounded JSON CRUD/patch/control requests, authenticates
the peer user, and serializes state changes with monotonic revisions. Mutable
state feeds the ordinary runtime at native group boundaries; native argv is never
changed inside a running process. Cancellation reuses owned process-group cleanup.

The TUI adapter applies typed state to the existing App's actual ports, views,
list items, selections, and chart data. It dispatches existing source-defined
handlers, preserving the `.program` as the control-flow authority. The renderer
publishes its applied revision and fixed-size preview. Source edits rebuild
layouts at a safe boundary, preserving compatible state. Paused UIs stop starting
handlers while finishing active ones; explicit cancellation discards stale worker
completions and requires retry acknowledgment. Regular execution retains its
existing path. Live state is ephemeral and separate from checkpoint journals.

See [the control API](docs/live-control.md) for protocol and lifecycle contracts.

## Model adapter boundary (source candidate)

`execute-model` is a separate supervised Unix process, located beside the calling
interpreter. Five manifests map Jev and OpenAI operations to literal argv; the
`execute model` subcommand delegates to the same helper. This keeps HTTP/TLS and
provider-specific parsing outside ordinary native pipelines while sharing their
existing cancellation, timeout and output-bound machinery. The helper normalizes
results to `execute.model/1`; JSON selection and conditions remain in `.program`.

Fixtures check exact semantic requests and validate provider-native responses
through the live response parser. Presence of `EXECUTE_MODELS_OFFLINE` prevents
fixtureless calls before input, credentials or network access. Live calls use
explicit provider/model settings and bounded input/output/token/time limits. No
automatic retry or cache is enabled. Manifests mark failed requests as uncertain
for checkpoint recovery because an HTTP failure can follow provider-side work.
Completed results restore from private plaintext checkpoints without requesting
inference again. This is a narrow adapter, not a model-driven command executor.
