# Thesis and implementation progress

> Unix provides the applets. Rust provides the runtime. `.program` provides the glue. AI provides the programmer.

The authoritative thesis is retained in [thesis-source.md](thesis-source.md).
This ledger tracks the entire objective; a completed feature does not establish
the complete Bash-replacement claim. The language should stay compact while
making the thesis's capabilities real. GUI rendering is a later phase.

| Requirement | Implemented evidence | Remaining work |
| --- | --- | --- |
| Zero friction | A `.program` runs through an installed interpreter without generated application source or a build step. Direct quoted calls in 0.7.0 need no applet wrapper. | Verify clean distribution/install environments across supported platforms. |
| Shell dominance | Direct commands, native byte pipes, typed declared inputs, named results, file read/save, cwd, timers, signals, and supervised child groups. Pending candidate adds integer native exit-status capture and nested scalar conditions. | Environment overrides, substitution, redirection variants and process/job control. |
| Structured data | JSON can reach TUI widgets; typed inputs include text, integers, booleans and text lists. Pending candidate adds native JSON parse, typed pointer selection, explicit encoding, JSON inputs and structured checkpoint/live bindings. | CSV, record mapping/filtering, structured streams and broader transformations; existing demos still use jq/awk/sed where needed. |
| Composability | Applet contracts, typed CLI input interfaces and same-file TUI text ports. Pending candidate adds finite stdin and same-file finite program definitions/calls with explicit inputs, local scope and byte results through the shared native runtime. | External modules, reusable finite helpers inside screens, lifecycle-owned nested services and unbounded terminal-output streaming composition. |
| Low latency | Native spawning and OS pipes, bounded streams, optimization probes and paired benchmark harnesses. | Complete cold-start, file-change latency, allocation/memory and representative service measurements on macOS and Linux. |
| AI-native syntax | Deterministic parser, whole-program validation, line errors, check/explain, 150 tested utilities, editor grammar and snippets. | Formal grammar/discovery sufficient for held-out generation, structured diagnostics, repair evaluation and demonstrated productivity. |
| Robust execution | Timeouts, cancellation, process-group cleanup, sequential event handlers, durable checkpoints and explicit uncertain-effect retry. | First-class retries, concurrency/jobs, error branches and complete signal semantics; wider fault injection. |
| Universal integration | `run "tool" with arguments [...]` executes installed tools directly; named manifests remain optional validated adapters. Pending candidate has a 110-case Bash differential matrix across 46 native tools, with exact matches and explicit contract differences. | A broader distinct-command/path/platform matrix; keep explicit shell invocation as an opt-in native command. |
| Security | Literal argv, local peer-checked private control sockets, bounded queues/captures and inspectable execution plans. | Explicit permissions, enforceable capability boundaries and inspectable structured side-effect contracts. Caller privileges are still inherited. |
| Incremental complexity | Finite commands, pipelines, services and TUI handlers share invocations and runtime supervision. Pending candidate adds scalar conditions, finite JSON operations and same-file finite reuse in that engine. | Modules, jobs and structured streaming from the same primitives; audit existing programs as these arrive. |

The five architectural decisions remain explicit: direct process spawning,
bounded streaming, a hybrid byte/text/record model, runtime-owned concurrency,
and actionable type-aware diagnostics. The current implementation establishes
parts of native spawning, bounded streaming and structured values; it does not
establish the full five-part architecture.

Follow the thesis's phase order: finish command/control-flow foundations, then
structured data and reuse, async jobs and supervision, reactive state/program
composition, complete TUIs, and finally GUI support. Existing TUI work should
continue using the shared core rather than hiding missing control flow in Rust.

The candidate's compact JSON boundary replaces simple helper-based extraction
without adding an expression language. Parsing retains original bytes; typed
selection feeds native argv/conditions; encoding is explicit and byte-bounded.
Objects, arrays, null and supported numbers survive checkpoint/live
state. The same instructions run in finite, listener, scheduled and native TUI
bodies. These are finite transforms rather than general query/record operators;
CSV, external modules and structured streaming remain missing. The
installed 0.8.0 binaries and packaged artifacts do not include this candidate.

The reuse candidate lowers same-file finite program calls into scoped
instructions, preserving native command execution, byte transport and local cwd
restoration. Explicit typed inputs replace implicit variable capture; native
stdout remains the return interface. Recursion, instruction count and serialized
expansion size are bounded. Calls share cancellation, native deadlines, caches
and failures, and form conservative whole checkpoint/live nodes. This is useful
finite reuse across scripts and services, rather than external modules, async
jobs, nested service lifecycle ownership or helper definitions in screen files.
Large typed values still copy for isolation, and live controls expose the caller
scope. Complete body changes can shift expanded cursor indexes; richer node
inspection remains open.

The required comparative study remains open: pin 100 real Bash scripts, recreate
them with a fixed model/prompt, and compare functional correctness, runtime,
generated tokens, repair iterations and human readability. The existing 150
utilities and their source lengths are not that study. The separate
[replacement gates](replacement-gates.json) preserve the broader challenge's
unmet requirements.

The bounded Unix proof adds 110 command/argv cases across 46 distinct system
executables. One hundred match Bash status/stdout/stderr exactly; ten verify
intentional differences in wildcard expansion and failure/output/status policy.
Two paired runs retain 7,700 measured correctness pairs plus excluded warmups,
with argv plans, native spawn identities and independent byte oracles. This is
local macOS candidate evidence, not 100 distinct tools, native Linux parity, a
model study, or proof that arbitrary Bash scripts are interchangeable. The native
runtime and installed release are unchanged by this evidence pass.

## Current refinement

0.7.0 adds direct executable calls in finite programs, native pipelines,
listeners, intervals, stream sources/stages and TUI handlers. These use the same
supervisor, rather than a second shell-based execution path. Quoted executable
names and an explicit `arguments` list distinguish ordinary CLI argv from named
applet parameters. Existing invocation serialization remains unchanged when
`direct` is false, preserving checkpoint identities for existing named applets.

Regression coverage includes binary output, literal argument boundaries,
relative executable paths with spaces, named argument items, early validation,
stream framing, listener launch counts, failure, timeout, cancellation, live
bindings/widgets and completed-checkpoint identity. Evidence from this change
is recorded separately from the preceding 0.6.1 full audit.


0.8.0 adds top-level typed input declarations, literal defaults, repeated
`--input NAME=VALUE` arguments, early schema/known-parameter validation, and
inspection of resolved native argv without execution. Inputs propagate into
finite pipelines, cwd setup, listener producers, fresh service handlers and
native streams. Live bindings preserve declared types. Checkpoints include
an input signature while retaining compatible earlier invocation identities.

The 150-utility collection now exposes 161 path inputs across 144 programs; the
six stdin programs retain their streaming interfaces. Defaults preserve their
original tasks, inputs and output oracles. Four migrated utilities also exercise
CLI overrides with original source files absent. The Git/checksum/file-following
examples use the same interfaces. This is progress toward program composition;
ordinary finite stdin stages were still absent in that release. Functions/modules
and the remaining control-flow requirements are still open.

The pending source candidate adds single-position input parse diagnostics and
`execute inspect FILE`: static JSON discovery of typed inputs, defaults, missing
values, execution form and TUI component ports. All 171 example interfaces can
now be checked and inspected, including the live queue monitor's newly declared
inputs. Its real managed report still comes from the `.program` native pipeline.
These changes are not yet in installed 0.8.0. Formal grammar coverage, structured
error diagnostics and the held-out generation/repair study remain open; interface
discovery alone does not establish AI generation efficiency.

The next candidate adds `read from stdin`, direct descriptor forwarding into
native stages and bounded, cancellable finite capture. Two ordinary program
files now compose through a Unix pipe without the `/dev/stdin` file workaround.
Checkpoints restore completed input captures and require explicit uncertain retry
for interrupted consumption. Services, live state and TUIs retain their own
input transports. JSON inspection identifies the ordinary CLI stdin mode. Useful
unique-line and binary-checksum stages bring the example corpus to 173 files;
the 150-utility collection remains the same collection. Finite final output is
still bounded and buffered, and this does not implement functions/modules or
general streaming program composition.

The reactive review found a managed interval defect: a fixed 20 ms condition
wait postponed short ticks, and completion rounded away submillisecond phase.
The candidate now waits for the actual deadline and shares the foreground
service's phase calculation. Handlers remain serial, missed ticks are skipped,
and control/source edits wake the waiting worker. Paired release-build evidence
is retained in the campaign's `reactive-timers` directory. This is a targeted
timer correction, not a hard realtime or general application-speed claim;
calendar/listener polling, TUI event-loop granularity, parallel jobs and broader
reactive program composition still need further work.

The error-handling candidate fixes background stdin writes that could outlive
the native pipeline's deadline after its owning process exited. Private pipe
writes now observe cancellation/deadlines, and error cleanup stops the worker
without waiting on an escaped reader. An owned, bounded fixture reproduces the
old success-after-deadline behavior; successful pipeline measurements remain
separate from that functional failure probe. This does not add script-visible
exit statuses, conditions, first-class retries or stronger descendant containment.

The scalability candidate closes document-budget bypasses at live stream record
publication and listener input/binding promotion. Listener capacity failure keeps
its queued record for repair before any handler effects. A bounded serialization
counter checks the exact JSON size without a second state-sized output buffer;
queue admission also avoids cloning unrelated document fields. Payload-scaling
measurements and boundary regressions are retained in the campaign's scalability
evidence. This preserves the 8 MiB document/128-record limits; it does not establish
a process-memory bound, cache eviction, parallel jobs or general scalability.

The determinism review preserves sorted manifest/environment identities, atomic
revision-checked live writes, frozen checkpoint inputs and the documented rule
that environment changes apply to pending commands. Its candidate shares
executable digests within completed-prefix validation while rechecking file
identity and discarding the memo at the next attempt/execution boundary. Identity
bytes stay compatible; changed files and cwd-specific relative executables retain
distinct fingerprints. Paired completed-resume measurements are retained under
`determinism`. This does not promise an atomic filesystem/environment snapshot,
exactly-once external effects or deterministic arbitrary native tools.

The security review reproduced checkpoint-directory and blob-directory symlink
following in owned fixtures: the baseline wrote through them and chmodded their
targets. The candidate rejects these leaves before target mutation, checks opened
checkpoint file descriptors and uses no-follow/nonblocking flags. The CLI now
checks the live server's peer UID before sending data, complementing existing
server checks. Regression evidence covers ordinary/trailing-slash directory
links, fresh/resume blob links, file links, FIFO rejection and credential matching.
Caller privileges, trusted checkpoint/cache parents and same-user socket trust
remain explicit. Capability policy, complete ancestor-race protection, cache
integrity envelopes and a command sandbox remain unimplemented.

The developer-experience candidate adds `execute check FILE --json` and a
versioned `execute.check/1` envelope/schema. Correctly parsed checks report static
success or the first failure with a stable general code, complete error chain
and exact Unix path bytes, preserving ordinary output and exit behavior. This
makes validation results consumable without scraping stderr, while missing input
interfaces remain discoverable through `inspect`. It does not yet add typed
semantic codes/spans, error recovery, formal grammar completeness, automatic
editor diagnostics or evidence of model/human productivity.

The runtime-efficiency candidate removes full-document cloning and a second
serialized state buffer when the renderer publishes live widget state. It uses
the existing exact-budget counter with rollback under the state mutex. Capacity
failure preserves prior widgets/user data and still marks potentially completed
handler effects uncertain. Core/allocation profiles and paired large-state widget
measurements are retained under `runtime-efficiency`; these are targeted costs,
not proof that all feasible optimizations are exhausted. Cold startup, file-change
latency, native Linux execution and broader memory/RSS work remain open.

The command-control follow-up adds `and keep status as name`: a single native
command may expose its ordinary exit code as an integer while preserving bounded
stdout and the shared supervisor. Timeouts, cancellation, signal termination,
spawn/I/O failures and output-limit errors remain failures. Existing implicit
fail-fast behavior is preserved unless the suffix is explicit. Native status
bindings flow through finite, scheduled, listener and TUI handlers, checkpoint
restoration and live state. They cannot be cached or belong to a native pipe
group, whose multiple-exit policy needs a separate design. A useful search-report
program records real `grep` status and matches. This is pending source work;
installed 0.8.0 remains unchanged. Conditions/error branches, structured transforms
and reusable programs still need implementation rather than a documentation-only
completion claim.

The next command-control candidate adds nested `if value is [not] value` and
`otherwise` command blocks. Both paths validate with definite-result/type merging;
only the selected commands run, through the existing pipeline supervisor. Scalar
comparisons borrow values and preserve incoming byte/file/stdin transport. No
shell or generated application code selects the branch. The same parser/control
instructions run in finite, scheduled, listener, managed and native TUI handlers.
Complete conditionals form conservative checkpoint/live nodes; unfinished unsafe
blocks require explicit retry, and checkpointed cwd changes must move before the
block. Conditions remain scalar equality rather than an arithmetic language.
Structured transforms, reusable programs, environment/redirection/job primitives
and wider comparative evidence remain core gaps. Installed 0.8.0 is unchanged.
