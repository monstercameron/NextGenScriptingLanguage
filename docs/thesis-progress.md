# Thesis and implementation progress

> Unix provides the applets. Rust provides the runtime. `.program` provides the glue. AI provides the programmer.

The authoritative thesis is retained in [thesis-source.md](thesis-source.md).
This ledger tracks the entire objective; a completed feature does not establish
the complete Bash-replacement claim. The language should stay compact while
making the thesis's capabilities real. GUI rendering is a later phase.

| Requirement | Implemented evidence | Remaining work |
| --- | --- | --- |
| Zero friction | A `.program` runs through an installed interpreter without generated application source or a build step. Direct quoted calls in 0.7.0 need no applet wrapper. | Verify clean distribution/install environments across supported platforms. |
| Shell dominance | Direct commands, native byte pipes, typed declared inputs, named results, file read/save, cwd, timers, signals, integer status capture, scalar conditions, bounded loops, named finite background pipeline handles and per-applet environment overlays. | Workflow-scoped environment mutation, substitution, redirection variants and interactive/detached process job control. |
| Structured data | JSON parse/get/set/map/filter helpers, recursive typed applet ports, explicit encodings, JSON inputs, structured checkpoint/live values, comment-preserving JSONC operations, finite typed CSV parse/stringify and bounded typed graph transport are in current source. | Richer general-purpose record transforms and general structured streaming; existing demos still use jq/awk/sed where needed. |
| Composability | Versioned applet contracts use shared call syntax across commands, in-process operations and JSON-lines workers; typed CLI inputs, same-file finite calls, versioned external modules, scheduled services, bounded native-command listener module nodes with typed startup inputs and one decoded event per sequential handler, and typed graphs with ordinary process inputs. | Listener sources on remote, worker, builtin or stdin backends; event joins and broader structured streaming; unbounded terminal-output streaming composition. |
| Low latency | Native spawning and OS pipes, bounded streams, optimization probes and paired source measurements. A local macOS synthetic 1,000-manifest/top-100 matching workload improved; ordinary-task medians are mixed and some tails worsened. In six public reference workflow cases, Bash medians are lower than Execute and Execute medians are lower than Nu. The equal-command-plan control preserves that median ordering. A separate bounded Linux 1,000-actor run observed all 847 timer sources during its one-second active window and reaped 16 registry workers; its resource samples cover only the Rust process. Neither result proves 1,000 independent capabilities or sustained desktop capacity. A same-code applet-boundary harness still has no result. | Complete cold-start, file-change latency, allocation/memory and representative service measurements on macOS and Linux. |
| AI-native syntax | Deterministic parser, whole-program validation, structured source diagnostics, check/explain/inspect, safe executable discovery, typed port matching with bounded multistep converter plans, 150 tested utilities, editor grammar and snippets. | Formal grammar/discovery sufficient for held-out generation, repair evaluation and demonstrated productivity. |
| Robust execution | Timeouts, cancellation, process-group cleanup, sequential handlers, durable checkpoints, explicit uncertain-effect retry, typed outcome branches, bounded parallel/race task groups, scope-owned named jobs with individual cancellation and reap, and named jobs under checkpoints (a spawn and its await form one resumable node; an interrupted job needs `--retry-uncertain`). | Detached and interactive job control, comprehensive signal/trap semantics and wider fault injection. |
| Universal integration | `run "tool" with arguments [...]` executes installed tools directly; named manifests remain optional validated adapters. The Bash differential matrix covers 110 cases across 46 native tools, with exact matches and explicit contract differences. | A broader distinct-command/path/platform matrix; keep explicit shell invocation as an opt-in native command. |
| Security | Literal argv, local peer-checked private control sockets, bounded queues/captures and inspectable execution plans. Current source adds opt-in strict v1 capability and data-flow policies, environment/path grants, cache-purity checks, exact-origin and persistence controls, and fail-closed macOS/Linux command sandboxes. | Wider platform and adversarial verification, complete ancestor-race protection, filesystem deadlines and controls for untrusted in-process operations. Without `sandbox_required`, native children still run with caller filesystem/network privileges. |
| Incremental complexity | Finite commands, pipelines, services and TUI handlers share invocations and runtime supervision. Current source includes scalar conditions, finite JSON operations, bounded loops, same-file finite reuse, external modules, task groups, scope-owned named native jobs and typed graph interfaces. | Bring job handles into persistent, browser and live lifecycles; add checkpoint and resume for finite graphs (#71); add general structured streaming from the same primitives and continue auditing real programs as they arrive. |

### Current applet-contract source candidate

The current source candidate adds recursive typed applet values and ports,
explicit status/outcome routing and effect certainty, optional result envelopes,
stdio dispositions, and command, in-process, JSON-lines-worker, program-module
and remote backends. The bundled `execute-applet` provides path, text, numeric,
color, JSON and JSONC operations; single-file compare-and-swap writes; and a
bounded journaled file-only transaction with explicit recovery. Worker framing
is bounded. Ordinary worker pools belong to the registry; scheduled and
bounded native-command listener module graph nodes own separate pools with the
configured capacity and release them on shutdown. Single-file
updates are atomic; transaction files become visible one at a time, and
external effects are outside its journal. Command timeout and output-capture
limits do not bound arbitrary file I/O or inherited streams. The
[contract guide](applet-contracts.md) and [security guide](../SECURITY.md)
define these boundaries. This is source-candidate behavior, not a
published-release claim; no boundary benchmark result has been recorded yet.

The five architectural decisions remain explicit: direct process spawning,
bounded streaming, a hybrid byte/text/record model, runtime-owned concurrency,
and actionable type-aware diagnostics. The current implementation establishes
parts of native spawning, bounded streaming and structured values; it does not
establish the full five-part architecture.

Follow the thesis's phase order: finish command/control-flow foundations, then
structured data and reuse, async jobs and supervision, reactive state/program
composition, complete TUIs, and finally GUI support. Existing TUI work should
continue using the shared core rather than hiding missing control flow in Rust.

### Long-term direction, clarified October 14, 2026

The long-term objective includes complete Linux desktop applications and shell
components, particularly within Omarchy. The immediate priority remains a
superior Bash replacement with a compact Unix core and explicit proof gates.
Desktop integration should build on that foundation in this order:

1. Fast, AI-friendly Unix scripting backed by Rust.
2. Reactive orchestration through events, timers, streams, state and composable programs.
3. Interactive TUI applications through generic Rust widgets.
4. Optional Quickshell GUI integration, retaining its Wayland and Hyprland integrations.
5. Native desktop utilities, panels, launchers and dashboards defined in `.program` files.
6. Desktop shell replacement as a stretch goal, potentially replacing QML/JavaScript
   application logic while reusing established desktop infrastructure.

The guiding principle is one language for Unix automation, interactive
applications and desktop interfaces, with no traditional application source-code
generation or user-facing compilation. Current source includes a narrow graph
adapter for generic Quickshell text and button components, with one renderer
process per node. Shared renderer sessions and real Quickshell/Hyprland/Omarchy
acceptance remain open; broader desktop-shell integration is a roadmap goal.

AI-capable CLIs may also become explicit applets for classification and control
decisions. Begin with the existing native invocation, structured JSON, typed
selection and conditional primitives: the CLI returns a result, and `.program`
owns the branch. A compact dedicated AI syntax should follow a real, verified
CLI contract and demonstrated need. Provider/model selection, timeouts, failure
behavior, data sent and any cache use must stay explicit; there is no implicit
model invocation in ordinary control flow. The separate 100-workflow model
collection is authored and fixture-backed. Two bounded synthetic live requests
checked Jev and `gpt-6-luna` connectivity through `.program`; neither establishes
model accuracy, calibration, productivity or latency.

The candidate's compact JSON boundary replaces simple helper-based extraction
without adding an expression language. Parsing retains original bytes; typed
selection feeds native argv/conditions; encoding is explicit and byte-bounded.
Objects, arrays, null and supported numbers survive checkpoint/live
state. The same instructions run in finite, listener, scheduled and native TUI
bodies. These are finite transforms rather than general query/record operators;
External modules and finite typed CSV applets are now available in current
source; general structured streaming remains missing. The
installed 0.8.0 binaries and packaged artifacts do not include this candidate.

The same-file finite reuse candidate lowers program calls into scoped
instructions, preserving native command execution, byte transport and local cwd
restoration. Explicit typed inputs replace implicit variable capture; native
stdout remains the return interface. Recursion, instruction count and serialized
expansion size are bounded. Calls share cancellation, native deadlines, caches
and failures, and form conservative whole checkpoint/live nodes. Current source
also provides versioned external program modules (see
[program modules](program-modules.md)); detached or persistent job lifecycles,
nested service ownership and helper definitions in screen files remain separate
gaps.
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

The remaining entries in this section preserve the status of earlier candidate
reviews at the time they were recorded. For present source status, use the table
above and the [current applet-contract candidate](#current-applet-contract-source-candidate);
phrases such as “pending”, “not yet” and “still open” below are time-bound and
do not supersede those summaries. Published artifacts and historical reports
remain unchanged.

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

## Explicit model applets (source candidate)

The human-requested model layer reuses native invocations, typed JSON selection
and existing conditions. A bundled `execute-model` helper implements Jev choice,
judgment and score, and OpenAI generation/strict extraction. Five named manifests
map explicit parameters to its literal argv. No AI grammar or implicit decision
engine was added. `.program` owns every branch and downstream native command.

Provider requests have bounded inputs/responses/timeouts, sanitized error classes,
explicit output token limits and no automatic retries or caching. Checkpoints
restore completed results and mark failed requests uncertain before retries.
Fixtures validate exact semantic requests, use the same response validation, and
remain distinct from actual inference. The offline environment guard blocks every
fixtureless request before credentials or network access. Model quality, prompt
robustness and confidence calibration still need representative live evaluation.

The separate 100-workflow collection adds authored fixture inputs and output
oracles; it does not satisfy the held-out Bash replacement study or platform
proof gates. Batch Jev questions, token streams, embeddings and provider failover
remain future work. Installed 0.8.0 artifacts remain unchanged.

### Source 0.9.0-dev production work

The [current production gates](production-readiness.md) retain the complete
Bash/Omarchy objective. [Local validation](production-validation-0.9.0-dev.json)
covers native exit propagation and typed environment/positional/variadic input
sources, including executable shebangs and checkpoint input identity. A pinned
444-command Omarchy inventory and ten isolated interface ports now expose real
porting requirements. These do not establish desktop acceptance. Two paired
measurements show helper-process avoidance improves one workflow while Bash
remains faster; no universal performance or readiness claim is allowed.

### Bounded loops (unreleased source)

Current source adds `repeat`, `for each` and scalar-equality `while` blocks in
the shared native/browser grammar, with same-type outer updates, local iteration
names and a shared iteration guard. Native checkpoint/live handling treats a
whole loop as one conservative recovery node. This closes the basic finite-loop
gap; jobs, richer redirection and desktop acceptance remain separate proof gates.
See the [language contract](language.md#loops). Published release
artifacts remain unchanged.


The later security focus additionally reproduces pure-cache symlink substitution
and FIFO acceptance in private fixtures. The candidate rejects linked cache
roots/entries and special files, checks opened file descriptors, and rejects
Unix paths writable by other users or owned by another UID. New cache directories
are private; existing raw cache bytes and key identities remain compatible.
This is filesystem-state hardening, not cache authentication or a sandbox.
Windows reparse rejection is implemented; Unix UID/mode checks do not establish
Windows ACL enforcement. Parent-path races and same-user tampering remain open.
See [security boundaries](../SECURITY.md) and the retained cache security evidence.


Live transport hardening reproduces a partial-request client that keeps a server
alive after an acknowledged stop by renewing per-syscall timeouts. The candidate
uses fixed elapsed read/write budgets for established connections while keeping
valid handler waits separate. LF/EOF framing and byte limits remain; expired
incomplete requests cannot mutate state. This addresses the demonstrated same-user
availability fault without claiming connect/CPU/filesystem deadlines or a separate
application authorization boundary.

A subsequent actual-CLI proof found that macOS can keep a single blocking
socket write active beyond its requested timeout while a peer drains bytes.
The Unix transport now temporarily uses a nonblocking connection and polls only
for the remaining monotonic budget, restoring descriptor flags afterwards.
Owned slow response and slow request-drain fixtures verify the actual CLI boundaries;
Windows retains its overlapped I/O implementation. See the
[follow-up security evidence](../benchmarks/security-2026-10-15-live-client.json).
