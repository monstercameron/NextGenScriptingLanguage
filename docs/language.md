# Unix language and runtime reference

Execute uses a deterministic English-like grammar. This reference covers finite
programs, listeners, schedules, applet manifests, caching, and checkpoints.
Start with the [project README](../README.md) for installation and runnable examples.
The same grammar runs on native Windows; see [platform differences](windows.md)
for executable lookup, quoting, cancellation, checkpoints and live IPC.
Persistent native streams have a [separate guide](streaming.md); managed runtime
and widget edits are covered by the [live-control API](live-control.md).

This reference describes the current native 0.9.0-rc.5 source candidate, which
has not been released. Features added since the published 0.9.0-rc.4 candidate,
including loop blocks, named background jobs and per-command stderr discard,
require a source build. Historical `bin/execute` 0.8.0 lacks several newer forms;
use a matching [release or source build](releases.md). The [browser host](playground.md) has its
own explicit applet and lifecycle limits.

## Language and Unix contracts

One statement per line; `#` comments and blank lines are allowed. Strings use JSON
double-quote escaping. Identifiers reference named results. Trigger bodies use
consistent space indentation, with nested conditional and loop command blocks.

Keywords are lowercase and case-sensitive. Finite service bodies accept any
consistent positive space indentation; examples use two spaces. TUI component
declarations require two spaces and their widget items and actions require four;
native conditional command bodies deepen from the action indentation.
Input and result names
match `[A-Za-z_][A-Za-z0-9_]*`; `true` and `false` are reserved boolean values.
Bare names used as values refer to bindings: `path` reads a value, while
`"path"` is literal text. Paths and command arguments do not expand wildcards,
environment variables, or shell expressions.

### Reading a program

| Structure | How to read it |
| --- | --- |
| `accept path as text` | This file expects a named CLI input. Top-level declarations come first. |
| `run "cat" with arguments [path]` | Start an independent native command with explicit argv. |
| `then "wc" with arguments ["-l"]` | Wait for success and pass the complete finite output as stdin. |
| `pipe to "wc" with arguments ["-l"]` | Keep both native commands alive, connected by an OS pipe. |
| `every 5 seconds` followed by an indented body | Run the body on each interval; this is a persistent service. |
| `listen to ...` and `whenever a line arrives as event` followed by a body | Keep one input producer alive and run the body sequentially for each record. |
| `stream from ...` through `show each line` | Keep native stages alive and forward complete byte records, without a per-record handler. |
| `open a screen ...` with `define a program named ...` | A TUI file contains component layouts, ports, and event handlers; see the [TUI catalog](tui-catalog.md). |

Each file selects one execution form: finite commands, one service trigger, one
native stream, or a screen. Finite statements and service handlers share command
syntax. Stream stages use the same invocation syntax with a narrower lifecycle
contract. TUI component ports such as indented `accept processes as text` belong
to that component; they are separate from top-level CLI input declarations.

| Form | Behavior |
| --- | --- |
| `run pwd` | Run an independent command with closed stdin. |
| `run "git" with arguments ["status", "--short"]` | Launch an executable directly, bypassing the applet catalog. |
| `run cd with path "folder"` | Change the program's directory; the calling shell stays unchanged. |
| `then ls with path "."` | Wait for prior success, then run the command. |
| `pipe to wc with flags ["-l"]` | Connect adjacent applet stdout/stdin directly with OS pipes. |
| `as name` | Name a finite UTF-8 stdout result, including its trailing newline. |
| `and keep status as code` | Bind an external command's exit code as an integer. |
| `and keep result as execution` | Bind an execution record with outcome, captured bytes and actual pipeline stage metadata. |
| `and cache` | Reuse a declared deterministic finite transformation by content. |
| `read "path"` | Builtin file input without launching a `cat` process. |
| `read from stdin` | Read inherited stdin in a finite program; native stages consume it directly. |
| `then save to "path"` | Atomically replace a file with the current finite output. |
| `every 5 seconds` | Reuse the indented body at a monotonic interval. |
| `on schedule "0 9 * * 1-5"` | Reuse the body on matching local calendar minutes. |
| `listen to stdin` | Listen for UTF-8 lines from stdin. |
| `listen to tail with flags ["-n", "0", "-F"] and path "events.log"` | Follow new lines from the file with one producer process. |
| `whenever a line arrives as message` | Name each record and provide it as the handler's initial pipeline input. |

Parameters use `with name value and name value`. Values are strings, integers,
booleans, references, or lists such as `["-l", "-a"]`. Manifests define allowed
parameters and types. There is no shell interpolation or command substitution.
`true` and `false` are boolean literals and cannot name results. `as name` binds stdout text; `and keep status as name` binds an integer; `and keep result as name` binds a JSON execution record.
Text does not implicitly become an integer, boolean, or list parameter.
Lists of string arguments may contain named text results. `as name` precedes
`and cache` when both are used.

Quoted direct native invocations may add `and discard stderr` after an optional
`as name` binding and before `and keep status/result as name`. It connects only
that command's stderr to the null device; stdout and exit-status behavior stay
unchanged. The modifier is unsupported for named applets, reusable-program
calls, task groups, background jobs, graph nodes, and listener sources, and it
cannot be combined with `and cache`. It is supported on finite and stream
native command stages. This modifier requires the unreleased source candidate.

<a id="native-exit-status-pending-source-candidate"></a>

### Language version

A program may begin with `execute 1`, which declares the language version it was
written for. The marker must be the first statement, with no indentation. This release
accepts only version 1. Another number fails with `upgrade execute or remove the
marker`. A program without the marker is version 1. `execute inspect` and
`execute explain --json` report the version as `language_version`.

### Native exit status

```text
accept path as text
run "test" with arguments ["-f", path] and keep status as file_status
then "printf" with arguments ["test exit: %s\n", file_status]
```

The explicit `and keep status as name` suffix waits for the native command,
keeps its complete bounded stdout as the pipeline value, binds its exit code
(0–255) as an integer and continues after an ordinary nonzero exit. It does not
change Execute's own final exit code. Without this suffix a nonzero command
still stops execution. Timeout, cancellation, signal termination, missing tools,
I/O failures and output-limit errors still stop execution; none becomes an
ordinary status. Without `and discard stderr`, native stderr keeps the existing
foreground/managed transport.

The explicit `and keep result as name` suffix binds a JSON record after an
applet invocation. On a pipeline, put it on the terminal `pipe to` call to
retain one record for the whole group. The record includes the actual terminal
status and outcome, combined effect certainty, per-stage statuses and capture
flags, plus raw captured stdout and stderr byte arrays. A decoded contract value
is included when the output is typed or separately named with `as`. If stdout
was inherited or discarded, `stdout_captured` is false and the raw stdout array
is empty. Stage metadata is bounded; `stages_dropped` reports any omitted
stages. On its own, this suffix preserves fail-fast behavior: failures still
stop execution and can be handled with `try`/`catch`. When combined with
`and keep status as`, an ordinary non-success outcome is retained in both
bindings and execution continues; its result record preserves the actual
failure or absence outcome and does not include a typed value unless the
contract's result envelope declares one. Infrastructure errors such as timeout,
cancellation, signal termination, transport and decode failures still stop
execution. Result capture cannot be combined with `and cache`, an intermediate
pipeline stage, task entries, or reusable-program calls.

`as output` may precede the status suffix to name stdout as well; the names must
be distinct. Status capture is limited to a single external command outside a
`pipe to` group and cannot be cached. Builtins, stream stages and listener
producers cannot capture status; finite, scheduled, listener and TUI **handlers**
can. Checkpoints persist the integer binding, and live state exposes it as a JSON
integer. Native argument lists format the integer in decimal. Integer status
bindings cannot serve as file paths. Status capture handles every ordinary exit
code, so tools such as `grep` need explicit treatment of error codes as well as
"no matches". Use the scalar conditional blocks below to choose the next commands.

The useful [search report](../examples/search-status.program) records both real
`grep` status and matching lines. The suffix is available in 0.9.0-rc.4; the historical 0.8.0 binary does not
recognize it.

When retaining both status and execution result, place `and keep status as`
before `and keep result as`; output, status and result bindings must all use
distinct names.

### Named background jobs

```text
spawn as build
  run "cargo" with arguments ["build"]
  pipe to "tee" with arguments ["build.log"]
inspect build as build_state
await build and keep status as build_status and keep result as build_result
```

`spawn as <name>` starts one native command pipeline that can be inspected,
awaited or cancelled by that name in later finite-program statements. Pipeline
stages use the ordinary named applet or quoted executable syntax. The parser
requires one indented `run` stage followed by zero or more `pipe to` stages at
the same indentation. Before starting any stage, Execute resolves every argv
value, executable and capability check, and snapshots the current directory and
bindings. Job stdin is closed; later changes to the caller's bindings or
directory do not change the running job. Spawn returns after every pipeline
child has been created and acknowledged by the existing process supervisor,
not after the commands finish. Spawn leaves the current pipeline value
unchanged; awaiting the job makes its captured stdout the current value. If a
later stage cannot start, already-created
stages are terminated and reaped, and the spawn error reports whether any
process started and the resulting effect certainty.

`inspect <name> as <binding>` is nonblocking. Its JSON record contains the job
state, each known stage's PID, state, exit status or signal, effect certainty,
and captured diagnostic byte counts. `await <name>` waits for all output to be
drained and every stage to be reaped, then makes the job's exact captured
stdout bytes the current pipeline value. Optional `and keep status as <name>`
and `and keep result as <name>` suffixes bind the terminal exit code and
structured execution record. Repeated awaits return the same terminal record
and output. As with a foreground pipeline, a non-success outcome stops the
workflow unless status capture explicitly handles an ordinary exit outcome.
`cancel <name>` requests cancellation of that job alone and returns after its
process tree is terminated and reaped; other jobs continue. The terminal
cancellation result remains available to inspection.

The handle belongs to the current finite root or reusable-program call and
cannot be serialized or returned as a value. Every handle must be explicitly
awaited or cancelled before normal scope exit, even if its process already
finished; inspection alone does not resolve ownership. An explicit reusable
program `return`, an error unwind, or runtime cancellation cancels and reaps
still-owned jobs before leaving that scope. Names are unique within a scope.
To keep resource use bounded, a finite run can create at most 32 named jobs
total, with at most 16 active jobs and 16 pipeline stages per job. Each job's
captured stdout is limited by
`--max-output-bytes`; all captured job stdout retained during the run shares the
same aggregate byte limit. Exceeding either limit stops and reaps the job and
reports an output-limit failure. Captured stderr is retained per stage up to
8,192 bytes on Unix and 65,536 bytes on Windows. The result record's
`stderr_by_stage` contains those exact retained byte prefixes, and its
`stderr_truncated` list says whether the supervisor read additional bytes
beyond each prefix. Inspection and result records report the platform limit and
truncation flags.

This finite feature has no detach/disown, `fg`/`bg`, stopped or continued
states, controlling-terminal handoff, or durable service lifetime. Job
operations are rejected inside loops and `try`/`catch`/`finally`, in services,
live programs, TUI handlers, browser execution and checkpointed/resumed runs.
A finite module called by one of those contexts cannot introduce job operations
indirectly; the enclosing preflight rejects the imported source before effects.
Those contexts do not yet provide a matching job-handle lifecycle.

<a id="conditional-commands-pending-source-candidate"></a>

### Conditional commands

```text
accept path as text
run "test" with arguments ["-f", path] and keep status as file_status
if file_status is 0
  run "head" with arguments ["-n", "10", "--", path]
otherwise
  run "printf" with arguments ["Missing regular file: %s\n", path]
```

The grammar supports `if <value> is [not] <value>`, `if <value> contains
<value>`, and `if <value> does not contain <value>`, followed by an indented
command body and optionally `otherwise` at the same indentation with another
body. `otherwise if <condition>` adds a clause with its own indented body; a
chain runs at most one body, in order, and may end without a final `otherwise`.
Each clause nests in the previous else branch, so the chain counts toward the
64-level nesting limit. Dedenting ends a block;
there is no written end marker. Both bodies use the same space indentation.
Nested conditionals are allowed to 64 levels; use a nested `if` within
`otherwise` for another case. Scalar equality is exact text, integer or boolean
equality, including text newlines. JSON arrays and objects use deep equality,
and `null` can be compared directly. There is no truthiness or implicit
conversion. A JSON-typed binding can be null, so its kind is checked when the
condition runs: a non-null record, list, or other value compared with a different
kind, such as `verdict is true`, is a type error, not false. `contains` checks a substring in text, a value in an array, or a
key in an object. List ordering, arithmetic operators and boolean expression
combinators are not implemented. These operators are contextual; use quoted
strings for literal words.

Only the selected body executes. A conditional does not consume or copy the
incoming pipeline value. `then` inside the chosen body may consume that value;
its last produced value becomes the next finite pipeline value. A skipped block
without `otherwise` preserves the incoming value. Native `pipe to` groups stay
inside one body; use `then` across a conditional boundary. Conditions and native
commands share the finite, service, managed and TUI handler engine. TUI command
branches precede the handler's ordinary display/routing actions.

Validation checks both bodies before any command starts, even when the condition
is literal. A new result is usable after the block only when every path defines
it with the same type. Inputs and existing results retain normal naming rules;
there is no new assignment operator. External live bindings without a declared
type are checked when the condition executes. A failure inside the selected body
stops the handler; `otherwise` chooses the false comparison path and is not an
error-catching clause. Capture a native exit status explicitly to branch on it.
Trace emits the comparison decision and source line, without operand values.

A complete conditional is one checkpoint/live execution node. Live `/cursor`
cannot enter its interior, and edits/pause take effect at its outer boundary.
Checkpoint restoration skips the entire completed conditional and restores its
output/common bindings. Its identity includes both bodies and their available
executable identities, or a marker for an unresolved optional executable;
changing an unused body or installing a previously missing tool requires a fresh
checkpoint. Directory changes within a conditional are supported in ordinary
runs, but checkpoint mode requires moving that setup before the block so binary
identities have an unambiguous cwd. An unsuccessful conditional may have completed
some effects: unsafe retry needs `--retry-uncertain`, and retries repeat the whole
block. This is conservative recovery, not exactly-once execution or per-command
resume inside a branch.

#### Command conditions

`if run <command> succeeds` and `if run <command> fails` branch on one command's
declared outcome instead of a value. `<command>` is an applet or quoted
executable with its arguments, written as in `run`, with no pipe, binding, status
or result capture, cache, or discarded stderr. Builtins such as `cd` are not
conditions. `while run <command> succeeds|fails` checks the same way at each loop
header.

```text
if run "test" with arguments ["-f", path] succeeds
  run "head" with arguments ["-n", "10", "--", path]
otherwise
  run printf with message "missing"
```

The command runs with closed stdin, and its stdout and stderr are read and
dropped; they never become the pipeline value, which is unchanged. A nonzero
exit selects the `fails` branch; a success selects `succeeds`. A spawn failure,
a missing executable, a timeout, cancellation, or a process that ends without an
exit status is an error, not false. Output is still bounded by the normal output
limit. `explain` shows the step, and `--trace` emits a `command_condition` event
with the match. A command condition is one checkpoint node, like a value
conditional, and its binary identity is part of that node. Graph nodes and live
programs reject it at validation; a native stream pipeline rejects it because it
allows only applet stages.

[preview-if-present.program](../examples/preview-if-present.program) uses native `test` and
`head`; [search-report.program](../examples/search-report.program) uses real
`grep` codes to distinguish matches, no matches and an error, preserving an
existing report after errors. The historical 0.8.0 binary does not support these blocks; current release
validation is linked from the [installation guide](releases.md).

<a id="structured-json-pending-source-candidate"></a>

### Loops (current source)

```text
accept count as integer with default 3
repeat count times
  run "printf" with arguments ["Worker ready\n"]
```

```text
accept files as list of text
for each file in files
  run "cksum" with arguments [file]
```

```text
accept path as text
run "test" with arguments ["-r", path] and keep status as ready
while ready is not 0
  run "sleep" with arguments [1]
  run "test" with arguments ["-r", path] and keep status as ready
run "cat" with arguments [path]
```

`repeat <integer> times`, `for each <name> in <text list or JSON array>` and
`while <scalar> is [not] <scalar>` introduce an indented body. Dedenting ends
it. Loops and conditionals can nest to a combined depth of 64, including inside
same-file reusable programs. `while` uses the same exact, type-matched equality
as `if`; there is no truthiness or arithmetic. An external command or typed
selection can update a condition's binding, as the `test` example does.

The current pipeline flows through each iteration. The final iteration's output
becomes the next pipeline value; outputs are **not concatenated**. A zero-count,
empty-list or initially false loop preserves its incoming bytes (empty when
there are none). Use explicit file appends or another native applet to accumulate
results. `then` works within and after blocks; an OS `pipe to` group cannot cross
a control-flow boundary.

A loop can rebind an existing outer name only with the same statically known
type. Each write is checked, including status and stdout bindings. Names created
inside the body are local to that iteration. The `for each` name cannot shadow
an existing name, and its input is snapshotted once at entry. A text-list input
binds each item as text; a JSON array may contain any JSON values and its
iteration binding has type JSON. A literal list of strings is a text list; a
heterogeneous or non-text literal list is JSON. `repeat` also resolves its
count once; `while` reevaluates its comparison each iteration.
Reading an
externally supplied binding with an unknown static type is supported, but
rebinding it in a loop requires establishing a known type before entry. Ordinary
loop directory changes persist; checkpoint mode requires directory setup outside
control blocks so executable identities remain unambiguous.

Validation checks every body before effects, even for zero iterations. Known
counts must be between 0 and 100,000; supplied input counts, lists and comparisons
are checked before earlier native or file effects. Derived values are checked
when execution reaches them. A shared guard allows at most **100,000 entered
iterations per finite invocation**, across nested loops and reusable calls.
Services and TUI handlers start a new guard per handler; managed live execution
starts one per outer node. Resume counts newly entered iterations and does not
persist a lifetime quota. Cancellation is checked between VM steps, including
loops containing only builtins. Browser execution also retains its worker
watchdog. `break` and `continue` target the innermost loop in the current
reusable-program scope and cannot cross a reusable call boundary. `break`
leaves the loop; `continue` finishes the current iteration and tests the loop
again. Neither collects iteration output: the pipeline remains the last value
produced before the transfer, subject to any `finally` body run while
unwinding. A reusable program may use `return` to exit early; it preserves its
current pipeline value and restores its caller's bindings and working directory.

A whole loop is one checkpoint/live node, including its body fingerprint.
Completed loops are skipped on resume. A failure may have performed partial
external effects: `--retry-uncertain` explicitly retries the **entire loop**;
there is no per-iteration recovery. Live cursor changes cannot enter a loop's
interior, and pause/source replacement take effect at its outer boundary.
Finite scripts, schedule/listener handlers, reusable finite programs and native
TUI command handlers accept loops. Persistent stream-stage grammar remains
separate. The browser supports finite loop blocks with virtual applets, but its
pre-effect validation currently rejects `break`, `continue`, reusable-program
`return`, `try/catch/finally`, and dynamic JSON pointers.

Runnable utilities: [batch checksums](../examples/batch-checksums.program),
[repeat log](../examples/repeat-log.program), and
[wait for a readable file](../examples/wait-for-file.program). These blocks require
current source; published 0.9.0-rc.4 and historical 0.8.0 binaries do not include them.
[Regression controls](../benchmarks/loop-regression-2026-10-14.json) retain paired
release measurements for existing non-loop workloads.

### Parallel for each (current source)

```text
accept urls as list of text
parallel for each url in urls at most 8 as results
  run "curl" with arguments ["-sf", "--max-time", "10", url]
take integer at "/succeeded" from results as succeeded
take integer at "/failed" from results as failed
```

`parallel for each <name> in <text list or JSON array> at most <1-64> as
<result>` runs exactly one indented `run` command per element, with at most
the given number in flight. The command can be a named applet or a quoted
direct executable; it cannot use `as`, `and cache`, `and discard stderr` or
status/result capture, because each element's status and output are already
in its record. The element name is visible only inside that command, follows
the same typing as a `for each` binding and cannot shadow an existing name.

All element calls are resolved, validated and capability-checked before any
element starts. A non-success outcome or launch failure is recorded for that
element and does not stop its siblings. The result binding is JSON with `mode`
`"for_each"`, `items` in list order and the counts `succeeded`, `failed`,
`cancelled` and `not_started`. Each item has `index`, `item`, `state`,
`status`, `outcome`, `outcome_kind`, `uncertain`, `launched_processes`, the
captured `output`/`stderr` bytes, `output_text`/`stderr_text` copies when the
bytes are valid UTF-8, and `value` for typed applet results. The encoded record
also becomes the pipeline value.

Interpreter cancellation, the total timeout or exceeding `--max-output-bytes`
stops new elements, cancels and reaps running ones and fails the group; running
elements are marked `cancelled` and the rest `not_started`. Each element's
captured output shares the byte limit evenly. Lists hold at most 10,000
elements. The group is a single checkpoint node, so a failed group retries as a
whole. Like `parallel` and `race`, it is unsupported in the browser host.

### Transfers and cleanup

`break` and `continue` are valid only inside a loop. `return` is valid only in
a same-file reusable program. They target the nearest loop or current reusable
call and cannot escape into a caller's loop. Each transfer unwinds crossed
`finally` blocks before reaching its destination.

```text
try
  run "false" with arguments []
catch failure
  then encode failure as json
finally
  run "printf" with arguments ["cleanup finished\n"]
```

The finalizer runs after the handler and can replace its pipeline output, as in
this example. Save a caught record before cleanup when both it and the cleanup
output must be retained.

`try` requires a nonempty body and at least one `catch` or `finally`. A single
`catch <name>` may precede a single `finally`; each block has an indented,
nonempty body. A caught error is a JSON record. Its fields include `kind`,
`outcome`, observed `status` and `signal`, captured stdout/stderr/value with
separate capture flags, per-stage results, `message`, and `effect_certainty`.
Missing status is `null`, not a fabricated exit code. `then encode failure as
json` makes the record available as pipeline bytes.

Ordinary failures in the try or catch body enter the catch when present, then
run finally. Cancellation bypasses catch but still attempts finally. Cleanup is
best-effort: nested finalizers share one budget of 128 VM steps and five seconds,
and the parent workflow deadline remains a hard upper bound. If that deadline
has expired, cleanup can be skipped or stopped. A cleanup failure cannot replace
the original cancellation; with an ordinary pending failure, a finally failure
becomes the reported error and retains the earlier failure as context. A transfer
inside finally overrides a pending ordinary failure or transfer. These rules do
not make external effects reversible or exactly once.

### Explicit exit and failure

`exit with status <value>` ends the finite run with that process status. The
value is an integer literal from 0 to 255 or an integer binding; a binding is
range-checked when it runs, before the exit takes effect. `fail with message
<value> [and status <value>]` writes the text to stderr followed by a newline
and ends the run with status 1 unless `and status` gives 1 to 255. The message
is a quoted text literal or text binding.

```text
accept partial as boolean with default true
run printf with message "processed 10 records, 2 skipped"
if partial is true
  exit with status 2
fail with message "no records" and status 3
```

Both steps are valid in finite programs, including inside `if`, `repeat`, `for`
and `while` bodies, `try` blocks and reusable programs. Inside a reusable
program they end the whole run, not just that call. Services and listeners
reject them during validation, because a handler failure is per record and an
exit would end the whole service. Imported program modules reject them as well,
since a module runs as one applet inside its caller.

An exit is not an error. It is not caught by `catch`, but it runs every pending
`finally` block, like `return`. Owned background jobs are cancelled and reaped,
not awaited, so an exit satisfies the rule that every job is observed. The
pipeline output at that point is still delivered. `fail` writes its message
after that output. `exit with status 0` exits zero. Cancellation, a timeout and a
failing cleanup step still take precedence over an exit that is pending in a
`finally` block. The trace records an `exit` or `fail` event, and `explain`
shows the step.

Checkpointed programs (`--checkpoint`) accept an exit or fail only as a
top-level statement. A completed exit is a terminal checkpoint node: `resume`
does not run any later step and reports the same status, re-derived from the
restored bindings. Validation rejects nested exits in a checkpointed program
before any effect.

### Structured JSON

```text
accept path as text
read path
then parse json as repository
take text at "/full_name" from repository as name
take integer at "/stargazers_count" from repository as stars
then "printf" with arguments ["%s: %s stars\n", name, stars]
```

Source values include bounded JSON literals: `null`, JSON numbers, nested
arrays and objects with quoted keys, for example
`{"user": {"name": "Ada"}, "tags": ["rust", "execute"]}`. Lists may mix
JSON value types. Literal object keys must be quoted; duplicate keys and
trailing commas are errors, and nested values are limited to 64 levels. Literal
numbers retain signed 64-bit integers when possible and otherwise use the JSON
number representation. This value grammar also applies to loop inputs and
command parameters.

`then parse json as name` materializes the current finite bytes within
`--max-output-bytes`, validates one complete UTF-8 JSON value and binds its typed
tree. It preserves the original pipeline bytes exactly. A subsequent `take`
reads a binding without changing those bytes. Neither step launches a helper.

`take <text|integer|boolean|list of text|json> at "<pointer>" from <value> as name`
selects a value using a literal JSON pointer. An unquoted identifier after `at`
uses a text binding as the pointer, for example `take text at pointer from
repository as name`; that binding must contain a valid pointer when the step
executes. `/items/0/name` selects an array
element's field; `~1` means `/`, `~0` means `~`, and `""` selects the root.
Empty object keys work. Array indexes are unsigned decimal with no leading
zeroes; `-` does not append or select. Missing fields, invalid pointers, null
where text is requested, mixed text lists and wrong scalar types fail with the
source line. There is no automatic parsing of a text binding: parse its bytes
first. Conditions and native argv require a selected typed scalar or text list.

`then encode value as json` replaces the pipeline bytes with compact JSON,
without a trailing newline. It accepts a literal, reference or list of values
and supports native pipes and `then save`. Encoding writes directly from the
typed tree and checks the byte limit as it writes. Object keys are ordered;
whitespace, duplicate keys and numeric spelling are not preserved. Parsing
duplicate object keys keeps the last value. JSON integers through signed/unsigned
64-bit ranges retain exact values.
Fractional and larger numbers use finite 64-bit floating point and may round;
use JSON strings for values requiring greater precision. Nonfinite/overflowing
floating point input is rejected; exponent formatting can normalize. Only signed
64-bit JSON integers
can be selected as `integer`; there is no numeric coercion or arithmetic.
JSON null, nested objects/arrays and escaped NUL can be retained with `json` and
encoded again. Selecting `text` or `list of text` rejects NUL at that boundary.

`use value <value>` sets the pipeline input to a text literal or text binding's
UTF-8 bytes, so a following `pipe to` or `then` native stage reads it. It launches
no process; `explain` shows it as launching no process. Only text is accepted.
Structured values are rejected before effects; encode them first with
`then encode value as json`.

`accept data as json` accepts a complete JSON value through `--input`, for example
`--input 'data={"count":7}'`. Inspection and live state expose ordinary JSON
objects and arrays. Object, null and numeric literals are also available in
ordinary value positions. Known input selections are checked
before earlier effects; fields from runtime command output are checked when
reached. Both conditional bodies receive static validation and compatible result
types must be defined on every path before later use.

Finite runs, scheduled/listener bodies and native TUI handlers use these same
operations. A listener can parse each incoming JSON line directly. Checkpoints
persist structured values and completed nodes; live CRUD can edit object fields
before the next selection. Live failures retain the existing explicit `retry`
control. Parsing and encoding are finite bounded operations, not streaming JSON
queries; native stream stages remain native commands. Some bounded in-process
loops, including `json.map`, `json.filter`, `text.concat`, `text.matches` and the
JSONC scanner, cooperatively check cancellation, the per-call
`--command-timeout-ms` budget and the parent total deadline. A single
parser/serializer or synchronous filesystem call can still return after the
budget because those library and OS calls do not expose checkpoints. The byte
bound is not an RSS limit. CSV, arbitrary `jq`-style transformations and
streaming JSON queries remain future work. The bounded `json.map` and
`json.filter` applets provide JSON Pointer projection across an array and
declarative array predicates; they are finite operations, not general-purpose
transformation programs. See
[github-repo-summary.program](../examples/github-repo-summary.program),
[json-event-names.program](../examples/json-event-names.program) and
[json-compact.program](../examples/json-compact.program).

<a id="reusable-finite-programs-pending-source-candidate"></a>

### Reusable finite programs

```text
define a program named stars
  accept path as text
  read path
  then parse json as repository
  take integer at "/stargazers_count" from repository as count
  then encode count as json

accept first as text
accept second as text
run program stars with path first as first_stars
run program stars with path second as second_stars
then "printf" with arguments ["%s: %s stars\n%s: %s stars\n", first, first_stars, second, second_stars]
```

Declare same-file programs before executable root statements or a service
trigger. Their bodies use two spaces and begin with any typed `accept`
declarations. Forward references work. All definitions, including unused ones,
are syntax/type checked before effects. Definitions are finite; nested
definitions, triggers inside them, recursion and external module imports are
unsupported. A file containing only definitions performs no actions.

`run program name with input value and other value as result` calls the named
program with closed byte input. `then program name ...` forwards current pipeline
bytes. Each invocation gets only its declared inputs, supplied explicitly or by
literal defaults. Unknown, missing and statically incompatible arguments fail
before earlier effects. There are no implicit parent-variable captures. Local
bindings disappear on return; an optional `as result` binds complete UTF-8 output
in the caller. Without it, binary bytes or a file input descriptor can return
directly to a later native stage. Structured results can be encoded as JSON and
parsed by the caller. Native argv still comes from the explicit CLI calls inside
the reusable body.

A reusable body's first `read from stdin` reads **call input**, not the process's
inherited stdin. It is optional and must be its first executable statement.
`read from stdin as name` performs bounded UTF-8 capture; unnamed input retains
the native transport. A `run program` call provides EOF; `then program` forwards
the caller's data. Finite root `read from stdin` keeps its ordinary Unix meaning.
Program calls use `run`/`then`, outside native `pipe to` groups; their bodies can
contain native pipes. They execute in the same interpreter and supervisor, with
no extra interpreter subprocess or generated application code.

Successful return restores the caller's cwd. Native commands and file writes
inside a body still have real side effects, and caller privileges are inherited.
Timeouts remain per native group/capture, not a deadline for the entire call.
Cancellation is shared. Native failures retain their typed error and call
context. Call-level native status capture and caching are unsupported; explicit
status handling and declared pure applet caches work inside the body.

Each complete call is one checkpoint/live execution node, including nested
calls. Completed checkpoints pin expanded body semantics, arguments, input
contracts and native executable identities. Diagnostic line movement preserves
identity; changing a completed body requires a fresh checkpoint. Checkpointed
calls cannot change cwd internally, so move that setup outside. An unfinished
call may have partial effects; retry explicitly authorizes repeating the whole
call. This is not per-command resume inside the body or exactly-once execution.

Live CRUD exposes caller bindings and returned names; function-local bindings
are private. Interior cursor edits are rejected, and pause/source replacement
wait for a boundary. Editing a body may change expanded indexes, so source/cursor
changes may need an atomic patch. Source/node discovery remains a developer
experience follow-up. `execute inspect` exposes each defined program's input
interface, definition line, finite lifetime, local scope and byte output.

Expansion is bounded to 16 call levels, 100,000 instructions and a conservative
16 MiB serialized-payload budget shared by definitions and root calls. Size is
charged before cloning repeated schemas/defaults; these are expansion bounds,
not process RSS limits. Typed argument trees are copied for local isolation;
native byte/file input transport is preserved. Calls work in finite scripts,
foreground schedules/listeners and managed versions of those lifetimes.
Screen component `define a program` blocks retain their separate layout/port
meaning; reusable finite bodies inside screen files and general lifecycle-owned
nested services remain future work. See
[compare-repository-stars.program](../examples/compare-repository-stars.program),
[compare-file-line-counts.program](../examples/compare-file-line-counts.program) and
[normalize-stdin-with-program.program](../examples/normalize-stdin-with-program.program).

### Direct executable calls

A quoted command name is an executable, resolved through the inherited `PATH`
or relative to the program's working directory when it contains `/`:

```text
run "git" with arguments ["status", "--short", "--branch"]
```

The only parameter is the optional `arguments` list. Items can be text, integers,
or booleans; integers become decimal strings and booleans become `true`/`false`
at the native argv boundary. Named results and declared scalar inputs can supply
individual items. A declared list of text can supply the complete argument list.
Nested lists are rejected. An omitted or empty list passes
zero arguments. Empty strings, spaces, Unicode, newlines, and shell-looking text
remain exact argument bytes. Executable names must be nonempty literal strings;
NUL is forbidden in names and arguments. The runtime does not split a quoted
command into words, expand globs or `$variables`, insert `--`, or invoke a shell.
Use the tool's own option rules and a safe format such as `%s` when passing data
to `printf`. Direct calls run with the caller's environment and privileges.

Before a fresh finite run starts any command, Execute resolves every statically
known native executable, including those in branches, loops, and task groups,
and reports all unresolved ones together. A run that resumes from a checkpoint
does not repeat this check. Once a working-directory change makes relative names
unknown, later calls are resolved when they run.

### Redirection

A native command accepts these trailing suffixes. Each is checked before effects
and appears in `explain`:

```text
run "tool" with arguments ["-v"] and keep status as code and stderr to "err.log"
run "tool" with arguments ["-v"] and stderr appended to "run.log"
run "tool" with arguments ["-v"] and stderr merged
run "tool" with arguments ["-v"] and stdout to stderr
run "wc" with arguments ["-l"] and stdin from "input.txt"
then append to "run.log"
```

`and stderr to <path>` replaces the file with the command's stderr. `appended to`
adds it to the file, and `merged` appends stderr after stdout in the pipeline
value. `and stdout to stderr` writes stdout to this process's stderr and leaves
the pipeline empty. Stderr is captured under the normal capture limit, so a very
large stderr is truncated as before. A failing command still writes its captured
stderr to a file target. Only one stderr or stdout redirection applies per
command, and it cannot be combined with `discard stderr`, `cache`, `keep result`,
or a pipe stage. `and stdin from <path>` reads the file as the command's stdin; it
applies to a command that starts a statement, and it is a `read` followed by the
command as a consumer. `then append to <path>` adds the pipeline value to the file
and keeps it as the pipeline value. Unlike `save`, it does not replace the file.
Redirection to a file is a persistence sink for data-flow policy. Live programs
reject these forms. Under a capability policy, a redirect or append target needs a
filesystem write grant, and the check runs before any command starts. A target is
opened before the command runs, and the open file is what gets written. A link in
the final component is refused on Unix (`O_NOFOLLOW`) and on Windows (reparse
points), and the resolved path must still lie inside the grant.

`run`, `then`, `pipe to`, `stream from`, and `listen to` accept these calls, as do
native statements inside schedules and TUI handlers. Direct calls share the
existing process supervision, timeouts, cancellation, output bounds and native
pipe implementation. `check` validates argument shape and bindings without
probing or running the executable. `explain` displays the executable and literal
argv; unresolved named values remain visible as parameters.

Unquoted names retain their existing meaning: named applets resolved through a
manifest. This catches misspelled applet names and preserves their parameter
contracts. The two forms can share a pipeline. `run cd with path "folder"` is
the interpreter's directory change; `run "cd"` attempts an external executable.
Direct calls cannot use `and cache`, because an arbitrary tool's purity is
unknown. Define a manifest with an explicit cache contract when caching is safe.

Programs made entirely of direct calls work with an empty `Registry` through the
Rust API and without a catalog installed beside the CLI binary. A missing
automatically discovered catalog is allowed; a malformed catalog or an explicit
missing `--manifest-dir` or `--manifest-catalog` remains an error. Existing
manifests remain optional adapters, rather than a prerequisite for integrating
a new executable.

`run` ignores previous stdout. `then` buffers finite output and feeds it only to
commands that accept stdin. `pipe to` joins a concurrently running group with
native OS backpressure. Output naming and caching belong at finite boundaries,
rather than between members of a live pipe group. `cd` has no output and cannot
belong to a pipe group or serve as a listener source.

```text
run printf with message "hello"
then tr with from "[:lower:]" and to "[:upper:]" as loud and cache
then save to "hello.txt"
```

The final finite result goes to stdout, including after a save. Intermediate
results remain pipeline values. To inspect multiple command results separately,
use separate programs or explicitly save the results you need.

## Finite stdin and Unix composition

Finite programs accept explicit stdin input. The historical 0.8.0 binary
does not include this syntax:

```text
read from stdin
pipe to "sort" with arguments ["-u"]
```

From the project directory, run
`printf 'b\na\nb\n' | target/release/execute examples/unique-lines.program`.
The result is `a` and `b` on separate lines. One `.program` can feed another
through an ordinary Unix pipe. Standard output retains the finite result contract.

Without a named binding, stdin passes directly to the next consuming native
stage through its inherited descriptor. Large input does not need to fit the
finite output limit when the native result is small; `checksum-stdin.program`
uses this to checksum a binary stream. A terminal capture, save, cached call,
checkpoint boundary, or `read from stdin as name` materializes bytes subject to
`--max-output-bytes`. Named results require UTF-8; unnamed bytes remain exact.
Uncaptured stdout is a separate rule, described under
[Uncaptured stdout](#uncaptured-stdout).

Finite stdin captures poll for input, observe cancellation and
`--command-timeout-ms`, and preserve the inherited descriptor's flags. Native
stages retain their existing pipeline supervision and timeout. Neither validation
nor inspection reads stdin. A completed checkpoint restores captured bytes;
resuming an interrupted read requires `--retry-uncertain`, since consumed input
cannot be rewound. The caller must supply appropriate replacement input when
explicitly retrying.

### Uncaptured stdout

A top-level `run` from the CLI's `run` or `resume` command writes its stdout to
the terminal as it runs when nothing consumes that stdout. Consumers are a
binding (`as`), `keep result`, `cache`, a following `pipe to`, or any later
statement that could read the pipeline value. Consumed output keeps the buffered
behavior and is printed once when it is the program's final value.

A `run` is treated as unconsumed when the next statement starts a fresh command
or the program ends, because a fresh command replaces the value. Inside `if`,
`repeat`, `for`, `while`, and `try` bodies, output is buffered unless that rule
holds inside the body, so a block boundary never streams. Native commands inherit
the terminal stdout. Other applets write their captured bytes after success. A
failing step has already written any inherited output; its error goes to stderr.

Data-flow policy governs remote and persistence sinks, not the terminal, so
streamed bytes reach the caller's stdout without a policy check. Only the CLI's
top-level finite run streams. Library runs, program modules, TUI
handlers, services, and listeners keep capture. Streamed output is not bounded by
`--max-output-bytes`, because it is not captured. A checkpointed run streams the
same way, but resume does not replay streamed output: a completed run whose final
output streamed resumes with empty output.

The syntax is distinct from `read stdin`, which still refers to a binding named
`stdin`. Services use `listen to stdin` for UTF-8 records, managed programs use
their pipeline state/inbox, and TUIs use component ports/widgets. These execution
forms reject `read from stdin` before application effects.

## Program inputs

Declare inputs before any folder directive, command, or trigger:

```text
accept path as text
accept lines as integer with default 20
read path
pipe to "head" with arguments ["-n", lines]
```

Run `execute preview.program --input 'path=server log.txt' --input lines=10`.
Repeat `--input NAME=VALUE` for different names; duplicate and unknown names fail.
The first `=` separates name from value, preserving later `=` characters. Text
values are literal argument text, with no shell expansion by Execute. Shell users
must still quote their own command-line arguments in the calling shell.

Input types are `text`, `integer` (signed 64-bit), `boolean` (`true`/`false`),
`list of text` and `json` as described above. A text list is supplied as JSON:

```text
accept files as list of text with default ["notes.txt"]
run "cksum" with arguments files
```

Run `execute checksums.program --input 'files=["first file.txt","second.txt"]'`.
Empty strings and lists are valid at the input-schema level; an applet manifest
can impose tighter constraints. Defaults must be literal values of the declared
type, including literal text lists. Scalar and text-list inputs reject references,
nested lists, records, null, floating point and NUL bytes. The `json` type retains nested
JSON, numbers and escaped NUL; references remain invalid input values. Names cannot
duplicate other inputs or be shadowed by ordinary result bindings.

Declared values can feed file paths, cwd setup, applet parameters, native argv,
listener sources, handler bodies and stream pipelines. They persist across
otherwise fresh service handler contexts. Native tools still enforce their own
option semantics; for example, BSD `head` rejects a zero line count. No implicit
integer-to-text conversion occurs for an applet's declared string parameter;
direct argv performs the scalar formatting described above.

`check` and `explain` validate any values supplied and any known defaults without
requiring every required input. `explain --input ...` resolves known inputs into
the displayed native argv; values produced by commands remain unresolved. Neither
command probes tools or executes effects. Normal execution requires all inputs
and checks their schema and known manifest constraints before starting commands.
Values that can only be computed by running commands are checked when available.

Checkpoint identity includes effective values and types. Resume must use the same
effective inputs; an explicit value equal to its original default is compatible.
Changes require a fresh checkpoint rather than silently reusing old results.
Programs with no declarations keep their previous checkpoint representation.

### Describe inputs

`described as "TEXT"` gives an input a human-readable description. It may appear
before or after `with default`, and each appears at most once. The description is
reported by `execute inspect` and does not change how the input is read or checked.

```program
accept name as text described as "Who to greet" with default "you"
```

### Agent interface discovery

Use `execute inspect FILE [--input NAME=VALUE]`.
The installed 0.8.0 release does not include this command yet. Inspection prints
one JSON object using protocol `execute.inspect/1`, after the same static syntax,
binding, type and known manifest-constraint validation as `check`. It starts no
applets, services or UI handlers, reads no application input files, and creates
no cache or checkpoint. Missing required inputs are reported rather than failed:

```sh
execute inspect examples/preview-if-present.program
execute inspect examples/preview-if-present.program --input 'path=server log.txt'
```

| Field | Contract |
| --- | --- |
| `protocol` | `execute.inspect/1` identifies this response format. |
| `validation` | `static`; tool existence and runtime results are not verified. |
| `kind` | `finite`, `interval`, `calendar`, `listener`, `stream`, or `screen`. |
| `stdin_mode` | Ordinary CLI input: `bytes` for finite stdin, `utf8_lines` for a stdin listener, `terminal` for a screen, or `unused`. Managed/headless modes have their own input transports. |
| `inputs` | Declarations in source order: `name`, language-spelled `type`, `line`, and `required`. Literal `default` and effective `value` are present only when available. Values use ordinary JSON types. |
| `missing_inputs` | Names with neither a supplied value nor a default. |
| `inputs_complete` | Whether all root CLI inputs have values; this is not a runtime readiness check. |
| `line_limit` | Stream-only field: record limit or JSON null for unlimited records. |
| `title`, `placements`, `components` | Screen-only fields. Components are sorted by name and expose text input/output ports, separate from root CLI inputs. |

`required` describes whether the declaration has a default; supplying a value
does not change that contract. An override changes `value` and retains the
original `default`. The output includes supplied values, so treat it like the
program's configuration. Invalid syntax or supplied values return status 1 with
a stderr diagnostic and no partial JSON. Root inputs cannot be supplied to a
screen. Use `explain` for the command/argv plan and `live schema` for the control
protocol. Inspection is an interface description, not an exhaustive side-effect
or permissions declaration.

Managed programs materialize defaults under `/bindings` at startup. Required
inputs can be supplied through `live serve --bindings` JSON. Live edits retain
the declared types, and deleting a declared binding is rejected atomically. When
replacing source with new input declarations, include their binding values in
the same patch. Existing stream/producer argv remains fixed for that producer's
lifetime; restart it to apply source-argument changes. Screen programs continue
to use component input ports and live widget state; root `--input` is not exposed
for screens in this release.

<a id="machine-readable-checks-pending-source-candidate"></a>

## Machine-readable checks

```sh
execute check report.program --json
execute check report.program --json --input count=20
```

This runs the same static validation as ordinary `check`. It does not launch
applets, check installed tools, read application input, create caches/checkpoints
or require missing declared input values. Use `inspect` to discover the missing
inputs, and ordinary execution to verify native behavior.

A correctly parsed check invocation writes one JSON document to stdout and no
stderr. Exit status is 0 for a valid check and 1 for a failed check:

```json
{
  "protocol": "execute.check/1",
  "validation": "static",
  "ok": false,
  "path": "report.program",
  "path_bytes": [114, 101, 112, 111, 114, 116, 46, 112, 114, 111, 103, 114, 97, 109],
  "diagnostics": [{
    "severity": "error",
    "code": "check_failed",
    "message": "could not parse workflow report.program: line 1: unterminated quoted string",
    "causes": ["could not parse workflow report.program", "line 1", "unterminated quoted string"]
  }]
}
```

The message/causes above illustrate the envelope; exact prose depends on the
failing parser or validator. `path` is a display string; `path_bytes` preserves
the original Unix path even when it is not UTF-8. Success has an empty diagnostic
list. Failure contains the first error, with its complete message and individual
cause strings. No line/column spans are guessed from prose, and `check_failed`
is currently a general code. Typed semantic errors and multiple-error recovery
remain open. The [JSON schema](check-result.schema.json) defines the envelope.

`--json` is accepted only by `check`. Malformed CLI arguments, duplicate options,
help/version output and output-write failures retain normal CLI behavior;
errors from a correctly parsed check use this protocol. Human-readable checks
are unchanged. Installed 0.8.0 does not include this option.

## Review a plan before it runs

Three options confirm what runs, and they change nothing in the program itself.

**Policy header.** A leading comment block may hold one line
`# execute-policy: allow CATEGORY...`. The categories are `writes`, `network`,
`reads`, `process`, `cwd`, `environment`, `privileged` and `undeclared`, and
`allow none` approves nothing. The header must be in the leading comments, there
may be only one, and an unknown category is an error. `execute explain FILE --json
--policy-header` merges the approved categories into the effects report. The header
grants nothing at run time: enforcement stays with the capability policy.

**What-if and confirm.** `execute run FILE --what-if` prints the plan the run would
execute, as the explain JSON, with its fingerprint (`execute.what_if/1`). It runs
nothing and writes nothing. `execute run FILE --confirm FINGERPRINT` runs only when
the current plan has that fingerprint. Otherwise it stops before any effect with
`plan changed since --what-if`, so a changed program, manifest or input must be
reviewed again. Both options apply to finite programs and to `run` only, and
`--what-if` takes precedence when both are given.

**Single instance.** `execute run FILE --single-instance LOCK` takes an exclusive
lock on the file LOCK without waiting. A second run that names the same lock fails
at once with `another instance holds the single-instance lock`. The lock is held
for the whole run, including graph, screen and stream programs, and the lock file
is created if it is missing and is never written.

## Attach to a live program

`execute live serve FILE --socket PATH` runs a program that accepts control. From
another shell, `execute attach PATH` prints one JSON object (`execute.attach/1`)
with the current revision and runtime state. `execute attach PATH --control ACTION`
reads that revision first and then sends the action with it, so an instance that
changed in between refuses the action and applies nothing. The actions are `pause`,
`resume`, `step`, `cancel`, `retry`, `reset`, `restart`, `restart-source` and `stop`.

`execute run FILE --control` runs the program under a live control endpoint instead
of a plain run. It prints `execute: control endpoint PATH` on standard error, and the
endpoint path is fresh for each run and removed when the run ends. Output streams as
a plain run's does, and the process exits with the program's status. `--control`
cannot be combined with `--checkpoint`, `--trace`, `--record`, `--what-if`, `--confirm`,
`--once`, `--headless`, `--allow`, `--policy-header`, `--retry-uncertain`,
`--total-timeout-ms`, `--max-runs`, `--missed-ticks`, `--max-catch-up`,
`--calendar-dedup`, `--max-record-bytes`, positional program arguments, or a
triggered program. Each of these is refused by name.

## Listener and schedule semantics

There is one trigger per program. Initial `run cd` or `use folder` statements may
precede it. Setup executes once before the producer or scheduler starts. Each
handler starts with that configured directory and fresh result bindings; a `cd`
inside the body applies within that invocation.

Line handlers run sequentially. A slow handler backpressures the producer through
its pipe, rather than building an unlimited event queue. The bound record and
implicit input preserve the newline; a final unterminated record is handled at
EOF. Records must be UTF-8. `--max-record-bytes` limits each record, including its
newline; default 64 KiB. Each handler emits and flushes its terminal output before
waiting for the next record or the producer's exit. Stdin EOF ends the listener.
An applet source is kept alive until it finishes, is cancelled, or a run limit is
reached. Its stdout must flush records if low first-event latency matters.

Intervals accept positive integer milliseconds, seconds, minutes, or hours. The
first tick occurs after one full interval. Deadlines keep their original monotonic
phase; slow jobs skip missed ticks and never overlap.

Managed intervals use the same phase calculation and wait until their deadlines
rather than sleeping in fixed 20 ms chunks.
Managed normal ticks retain the initial full-interval delay; diagnostic `step`
remains an explicit immediate tick. OS scheduling and native runtime costs still
limit actual latency. These corrections are included in 0.9.0-rc.4.

Calendar schedules use five numeric fields: minute, hour, day of month, month,
day of week. Supported forms are `*`, lists, ranges, and `/steps`. Weekday 0 or 7
means Sunday. Restricted day-of-month and weekday fields use cron OR semantics.
Calendar matching uses the machine's local timezone and starts at the next minute.
Missed minutes are skipped. Repeated local minutes during a DST fall-back can
match twice; skipped local minutes do not run. Names, `@daily`, and seconds fields
are unsupported.

Unhandled failures stop the app and return nonzero; explicit native status capture
continues after ordinary exit codes. There is no automatic retry or restart.
Ctrl-C, SIGTERM, and SIGQUIT cancel execution and stop owned process groups. An
applet must stay alive until its descendants finish; remaining descendants in its
group are stopped when it exits. Deliberately escaped process groups are outside
that cleanup contract. Binaries run with the caller's privileges.

## Process input sources

Programs can declare explicit environment and positional argument inputs:

```text
#!/usr/bin/env -S execute --
accept root as text from environment "OMARCHY_PATH" with default "/usr/share/omarchy"
accept count as integer from argument 1 with default 10
accept arguments as list of text from arguments
run "printf" with arguments ["%s|%s\n", root, count]
```

Run `execute tool.program -- 20`, or make the file executable and invoke it
directly with `./tool.program 20`. The shebang's `--` separates interpreter
options from the workflow filename and its argv. Without that shebang, put
runtime options before `--`: `execute tool.program --input count=30 -- 20`.
An explicit named input overrides its environment/argument source; a source
overrides the literal default. Positional numbers start at 1. Extra arguments
are rejected unless a `from arguments` list is declared. That list preserves
empty values, option-looking strings and literal argv boundaries; it is empty
when no arguments are supplied and cannot have a default. No shell expansion
or interpolation occurs.

Environment names are literal ASCII identifiers. An unset variable uses the
default; an empty value remains empty and must satisfy the declared type.
Integers, booleans, JSON and text lists use the same parsing as `--input`.
Invalid UTF-8 or invalid typed input fails before commands run, with no value
echoed in the error. `check`, `explain` and `inspect` do not read the process
environment; inspection describes its source without exposing its value.
Runtime values can appear in argv, live state or plaintext checkpoints, so only
read the environment values that the program is intended to handle.

Resolved values become ordinary declared inputs: checkpoints freeze them and
reject incompatible resume values; live type constraints remain unchanged.
Reusable finite program inputs remain explicit caller arguments. Read process
inputs at the top level and pass them into helpers. General environment
assignment/unset, shell sourcing and automatic string interpolation remain open.

Unhandled native command failures now preserve their exit code through the CLI;
a child terminated by signal N yields status 128+N. `and keep status as code`
handles an ordinary nonzero exit and continues, so the CLI can still finish
successfully. Syntax, validation, spawn/I/O, timeout and interpreter-cancellation
errors retain status 1. Native failure diagnostics and failed finite-output
discard remain explicit differences from Bash; pipeline failure policy is
stricter than Bash without `pipefail`.

## CLI and manifests

```sh
execute app.program
execute check app.program
execute explain app.program
execute app.program --trace --cache-dir ./cache --max-output-bytes 16777216
execute listener.program --max-runs 10 --max-record-bytes 65536
```

Explicit `execute run app.program` also works. `check` and `explain` validate the
whole program without starting producers, running commands, or writing files.
`explain` shows binary/argv bindings and trigger semantics. CLI arguments must be UTF-8; percent-encoded file URIs can represent workflow
filename bytes where the filesystem permits them. Invalid encodings and closed
stdout return ordinary CLI errors. The caller's working
directory is the initial program directory, regardless of where the source file
lives. `cd` resolves physical directories. An explicit relative cache directory
resolves from the initial caller directory.

Workflow, registry-backed applet, service-generation, and live-serve commands
accept `--manifest-dir DIR` (or one legacy JSON file). They also accept the
explicit, mutually exclusive `--manifest-catalog FILE` selector for a strict,
versioned packed catalog. A packed catalog is never selected automatically and does not
merge with a directory. Catalog paths are relative to the caller's working
directory; packed entries use the canonical catalog file as their origin, and
relative program-module sources resolve from its parent directory. Service
definitions store the canonical catalog path. Discovery checks
`./manifests`, then `../manifests` beside the executable, then the development
source location. The catalog includes `cd`, `pwd`, `ls`, `cat`, `printf`, `tr`,
`wc`, `tail`, `grep`, `sort`, `uniq`, `cut`, `head`, `false`, and a `git.commit` mapping. Older aliases remain available.
Git bindings are provided for inspection; the tests and demos do not commit/push.
`tail` uses ordinary CLI defaults; following requires explicit `-F` flags. The
older `follow file` alias maps to the `tail.follow` adapter. Executable symlinks
retain their invoked names, including for multicall applets.

```json
{
  "name": "tr",
  "binary": "tr",
  "accepts_stdin": true,
  "cacheable": true,
  "parameters": {
    "from": { "type": "string", "required": true },
    "to": { "type": "string", "required": true }
  },
  "arguments": [{ "parameter": "from" }, { "parameter": "to" }]
}
```

This maps `then tr with from "a-z" and to "A-Z"` directly to argv
`["tr", "a-z", "A-Z"]`. Argument nodes support literals, scalar parameters,
list spreads, and optional `when_present`/`append` mappings. Positional file paths
use `--` where appropriate. `cat` accepts an ordered `files` list in addition to
its optional single `path`; when both are supplied, `path` comes first. The explicit `builtin: "cd"` declaration is validated
against its single path parameter; ordinary manifests always launch their binary.

A non-cacheable external manifest may set `"uncertain_on_failure": true` when a
failed request may already have caused an external effect, including provider
billing. Checkpoint resume then requires explicit `--retry-uncertain` after a
failed attempt. Existing manifests default to false and retain their serialized
identity. The bundled model manifests use `"binary": "@execute-model"`, resolved
only to the sibling helper beside the running interpreter. See
[model operations](models.md) for the provider and offline-fixture contract.

Finite captures default to 16 MiB. This covers terminal results, finite boundaries,
named reads, and cache input. Intermediate native pipe traffic streams directly.
An overflow stops the active commands and skips later saves. This version buffers
each finite terminal result until completion; a live listener emits after each
record handler, rather than waiting for the unbounded source to finish.

Cache keys include the manifest, resolved argv, directory, executable path/content,
environment, and exact input. Failed results are never published. Cache files use
private temporary files and atomic renames. Default state is `.flow-cache` in the
current program directory. External files, libraries, network state, and time
aren't tracked automatically; `cacheable` is an author contract. `cat` with an
optional file path or file list is therefore not declared cacheable. Cache
publication evicts older entries to keep at most 128 entries and 64 MiB of
cached output and checksum data. A single output larger than the byte budget is
returned to the caller but not retained.
`wc` also permits arbitrary CLI arguments in `flags`, including file operands,
so its generic binding is not cacheable; the fixed stdin-only `count.lines`
adapter remains cacheable.

Cache directories and entries must be real directories/regular files; symlinks,
Windows reparse points, FIFOs, sockets and devices are rejected. Unix cache
paths must belong to the effective user with no group/other write mode bits.
New Unix cache directories are 0700 and result files are 0600. Rejected paths are not
chmodded; use a real private directory instead of a symlink/shared writable cache.
Unsafe cache state is an error, not a cache miss that repeats the applet. Parent
directories and same-user cache writers remain trusted. These checks do not
provide a command sandbox, authenticated cache contents, complete ancestor-race
protection or Unix extended ACL/Windows cache ACL enforcement; see
[security boundaries](../SECURITY.md).

`--trace` emits JSON lifecycle events, cache events, interval lateness, calendar
ticks, and handler timings. Applet diagnostics share stderr. Parameter values and
environment contents are omitted from traces.
Full stdout and trace pipes backpressure execution while remaining responsive
to cancellation. If stderr is full during shutdown, the final error diagnostic
may be omitted. Trigger-only limits are rejected on programs that cannot use them.

## Recover after a failure

The harmless [`examples/repair.program`](../examples/repair.program) intentionally runs `false` after a cached
`tr` result. Copy it to a temporary directory, run it below, then replace
`then false` with `then cat` to recover:

```sh
execute repair.program --checkpoint .execute-state/repair --trace
# Fix the failed command in repair.program, then:
execute resume repair.program --checkpoint .execute-state/repair --trace
```

Resume restores the latest completed node's output, named results, and working
directory. It skips completed commands and saves, including commands that are not
cacheable. `and cache` still provides reusable pure results across fresh runs;
`--checkpoint` records progress for one execution. Ordinary runs do no checkpoint
I/O.

You can edit the failed node and subsequent nodes, including removing a failed
terminal suffix. Completed nodes must retain the
same syntax, manifests, executable identities, and pipe grouping. Comments and
line-number changes are fine. Upstream file contents are frozen at a checkpointed
`read`; resume uses that snapshot even if the original file changes. Environment
changes apply only to commands that still run. Use a new checkpoint directory to
start over or change the completed prefix. A completed finite resume returns its
stored final output without running commands again.

On Unix, the runtime reuses executable content digests within a completed
prefix validation pass. Each reuse still checks the selected path's device,
inode, size, modification time and change time; a changed identity is rehashed.
New validation attempts and execution boundaries use fresh digests. Fingerprint
bytes and existing journals stay compatible. Hashing detects metadata changes
while reading and fails rather than recording that unstable content. This is
not an atomic filesystem snapshot or protection against a binary being replaced
after validation. Installed 0.8.0 retains its original per-node hashing behavior.

Each adjacent live `pipe to` group is one node. A failed group retries as a whole;
there is no checkpoint halfway through a live byte stream. Checkpointed `read`
buffers its input and applies `--max-output-bytes` to that snapshot. Checkpoints add file
hashing and synchronous disk writes, so use ordinary execution when minimal
latency matters more than crash recovery.

For listeners and schedules, the current event is saved before its handler starts.
Resume finishes a failed handler before restarting the producer or waiting for the
next tick. This preserves the current event, not the producer's unread queue or
file offset. Restarting a listener can lose or repeat other events; intervals
restart their timing and cron waits for the next matching minute. Output delivery
is not transactional and a partly delivered result can be emitted again.

If a process crashes or is interrupted during a command with side effects, its
outcome can be unknown. Resume refuses that node until you explicitly add
`--retry-uncertain`. Inspect the external effect first. Even a reported command
failure can have partial effects; retrying it is an explicit decision. Checkpoints
do not guarantee exactly-once external effects. An abruptly killed interpreter
can also leave its applet running; stop or inspect it before retrying.

Services discard unreferenced checkpoint blobs after each successful handler.
The pure-result cache has its own lifecycle.

Checkpoint directories contain private outputs and named values (0700 directories,
0600 files), checksummed state, and an exclusive writer lock. They are local trusted
state, not a sandbox or an encrypted secrets store. Keep them until recovery is
complete; choose a separate directory for each execution.

The runtime rejects symlinks at the requested checkpoint
and blob directory leaves, including a trailing `/` or `/.` on the requested
checkpoint path. Lock, journal and blob files must be regular files; opens use
`O_NOFOLLOW` and descriptor checks, with nonblocking opens to reject substituted
FIFOs without waiting for a peer. These changes prevent following those symlink
leaves or changing their targets' permissions. Parent aliases remain supported
(for example macOS temporary-directory aliases), so use an owner-controlled
parent directory. This does not pin every ancestor with directory descriptors,
exclude hard links, enforce capabilities or make concurrent hostile filesystem
replacement safe. Installed 0.8.0 does not include these guards.

## Bound command lifetimes

```sh
execute my.program --command-timeout-ms 2000
```

The optional positive millisecond limit applies separately to each launched native
pipeline group, including startup, pipe draining, and trace writes. It also applies
to service handler commands. Listener producers retain their intended lifetime;
this flag does not shut them down after one interval. UI native pipelines default
to 30 seconds; the option overrides that default. Very small limits may expire
during process startup.

The runtime also keeps buffered stdin writers inside that
deadline/cancellation contract. Their private pipe uses cancellable writes,
and error cleanup tells the writer to stop without blocking on a join. This
fixes a case where an exited applet's escaped descendant retained stdin and the
interpreter joined a blocked writer after the owner had finished. Escaped
descendants remain outside process-group containment; this change bounds the
interpreter's own input worker rather than claiming to contain those processes.

Timeouts kill and reap owned process groups. A timed-out effectful invocation is
recorded as uncertain in a durable checkpoint; resuming it requires the existing
explicit `--retry-uncertain` gate. File I/O, cache hashing, filesystem stalls, and
final output delivery are outside this command lifetime limit. This is cooperative
process supervision, not a sandbox: a command that escapes its group with `setsid`
can retain descriptors or survive, and arbitrary applets have the user's host
permissions. Only execute trusted scripts and manifests.

