# Unix language and runtime reference

Execute uses a deterministic English-like grammar. This reference covers finite
programs, listeners, schedules, applet manifests, caching, and checkpoints.
Start with the [project README](../README.md) for installation and runnable examples.
The same grammar runs on native Windows; see [platform differences](windows.md)
for executable lookup, quoting, cancellation, checkpoints and live IPC.
Persistent native streams have a [separate guide](streaming.md); managed runtime
and widget edits are covered by the [live-control API](live-control.md).

Bracketed lists and objects continue across physical lines until their delimiters
balance; blank and comment lines inside a continuation are ignored. Raw text uses
a trailing ` """` opener and a closing line containing only `"""`. The raw body
is inserted as one string without escape processing, and closing it ends that
statement. After an opening bracket, include a space before the raw delimiter.

```program
use value """
first line
second line
"""
then "cat"
```

This reference specifies the language and its native runtime behavior. Release
availability and platform-specific installation are documented in the
[release guide](releases.md). The [browser host](playground.md) has its own
explicit applet and lifecycle limits.

## Language and Unix contracts

One logical statement per line; `#` comments and blank lines are allowed. A
statement can span physical lines while a bracketed list or object remains open.
Triple-quoted raw blocks can contain multiline text; a closing `"""` line ends
the statement, and the block can only appear at its end. When a raw block follows
a bracketed expression, put a space before its opening `"""`. Strings use JSON
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

In a native pipeline, a producer stopped by SIGPIPE because a consumer exited
early is not considered a pipeline failure. A streamed final command that
receives a closed stdout reader exits with status 141.

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
native command stages.

<a id="native-exit-status-pending-source-candidate"></a>

### Language version

A program may begin with `execute 1`, which declares the language version it was
written for. The marker must be the first statement, with no indentation. The interpreter accepts only version 1. Another number fails with
`upgrade execute or remove the marker`. A program without the marker is version 1. `execute inspect` and
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
`grep` status and matching lines. The suffix binds the command's actual exit
status, including zero.

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
To keep resource use bounded, a finite run retains at most 32 named job records at once, with at most 16 active jobs and 16 pipeline stages per job. A resolved record is released after its last possible later use in the source; repeated awaits and later inspections remain available when the source references them. Sequential jobs can therefore exceed 32 over the run. Each job's
captured stdout is limited by
`--max-output-bytes`; all captured job stdout retained during the run shares the
same aggregate byte limit. Exceeding either limit stops and reaps the job and
reports an output-limit failure. Captured stderr is retained per stage up to
65,536 bytes on every platform. The result record's
`stderr_by_stage` contains those exact retained byte prefixes, and its
`stderr_truncated` list says whether the supervisor read additional bytes
beyond each prefix. Inspection and result records report the limit and
truncation flags. This limit applies to diagnostics; explicitly merged or
redirected stderr uses a separate policy.

This finite feature has no detach/disown, `fg`/`bg`, stopped or continued
states, controlling-terminal handoff, or durable service lifetime. Job
operations are rejected inside loops, inside a `try` that has a `catch`, in
`finally` bodies and in `on exit` and `on interrupt` handler bodies, in services,
live programs, TUI handlers and browser execution. A finite module called by one
of those contexts cannot introduce job operations indirectly; the enclosing
preflight rejects the imported source before effects. Those contexts do not yet
provide a matching job-handle lifecycle.

### Job checkpoints

A checkpointed or resumed finite run (`--checkpoint`) accepts named jobs with
these rules:

- A `spawn` and the statement that awaits or cancels its job form one checkpoint
  node. Statements between them belong to that node: they run together and are
  recorded only when the whole node completes. A job handle is never serialized
  and never crosses a checkpoint.
- Each `spawn` must be awaited or cancelled in the same top-level statement,
  including inside nested blocks of that statement. Any other placement is
  refused before the run starts, so no effect runs.
- Jobs inside reusable program definitions and in imported program modules are
  refused under checkpoints, before any effect runs.
- A job node is retry-safe only when every stage is retry-safe, like an ordinary
  invocation (see Retry safety below). If a run is interrupted while a job node is in flight,
  `resume` refuses the uncertain node unless `--retry-uncertain` is passed. With
  that flag the whole node runs again from its `spawn`, so a native stage may
  repeat its effects.

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
there is no written end marker. Each body uses a consistent positive number of
spaces; the `otherwise` keyword aligns with its matching `if`.
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
existing report after errors. The branch bodies follow the same indentation and
command rules as other conditional command blocks.

<a id="structured-json-pending-source-candidate"></a>

<a id="loops-current-source"></a>

### Loops

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

Validation checks every body before effects, even for zero iterations. Repeat
counts must be non-negative; known counts and supplied inputs are checked before
earlier native or file effects, while derived values are checked when reached.
Each execution of a loop allows at most **1,000,000 entered iterations** by
default. `--max-loop-iterations N` changes the positive limit for `run` and
`resume`; embedding callers can set `RunOptions::max_loop_iterations`. Each
sequential or nested loop execution has its own counter. Nested
loops can therefore execute many more iterations in total; use
`--total-timeout-ms` to bound a whole run. `parallel for each` keeps its own
10,000-element bound. A checkpointed `for each` counts restored elements toward
that loop's limit.
Recorded runs retain the configured limit for replay. Older records without a
limit retain the original shared 100,000-iteration guard during replay.
Cancellation is checked between VM steps, including loops containing only
builtins. Browser execution uses a separate 100,000-iteration per-loop limit
and retains its worker watchdog. `break` and `continue` target the innermost loop in the current
reusable-program scope and cannot cross a reusable call boundary. `break`
leaves the loop; `continue` finishes the current iteration and tests the loop
again. Neither collects iteration output: the pipeline remains the last value
produced before the transfer, subject to any `finally` body run while
unwinding. A reusable program may use `return` to exit early; it preserves its
current pipeline value and restores its caller's bindings and working directory.

A whole loop is one checkpoint/live node, including its body fingerprint.
Completed loops are skipped on resume. A failure may have performed partial
external effects: `--retry-uncertain` explicitly retries the **entire loop**.
The exception is a top-level `for each` (and `parallel for each`) under
`--checkpoint`, which records each element and resumes after the committed ones;
see [per-element checkpoints](#per-element-checkpoints). `repeat`, `while` and
nested loops have no per-iteration recovery. Live cursor changes cannot enter a loop's
interior, and pause/source replacement take effect at its outer boundary.
Finite scripts, schedule/listener handlers, reusable finite programs and native
TUI command handlers accept loops. Persistent stream-stage grammar remains
separate. The browser supports finite loop blocks with virtual applets, but its
pre-effect validation currently rejects `break`, `continue`, reusable-program
`return`, `try/catch/finally`, and dynamic JSON pointers.

Runnable utilities: [batch checksums](../examples/batch-checksums.program),
[repeat log](../examples/repeat-log.program), and
[wait for a readable file](../examples/wait-for-file.program).

[Regression controls](../benchmarks/loop-regression-2026-10-14.json) retain paired
release measurements for existing non-loop workloads.

<a id="parallel-for-each-current-source"></a>

### Parallel for each

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
elements are marked `cancelled` and the rest `not_started`. The elements in
flight share the byte limit evenly, and all retained element output counts
against one group-wide limit: a native element's stdout is charged as it
arrives, so the element that crosses the limit is stopped at that point rather
than when it finishes. Lists hold at most 10,000
elements. Without `--checkpoint` the group is one unit. Under `--checkpoint`, a
top-level group records each element, so resume skips the committed elements; see
[per-element checkpoints](#per-element-checkpoints). A nested group is one
checkpoint node and retries as a whole. Like `parallel` and `race`, it is
unsupported in the browser host.

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
For a failed native command whose stdout is captured, `stdout` holds what it
printed before it failed, up to `--max-output-bytes`, as with `keep result`;
stderr is included when it is captured.
Missing status is `null`, not a fabricated exit code. `then encode failure as
json` makes the record available as pipeline bytes.

Ordinary failures in the try or catch body enter the catch when present, then
run finally. Cancellation bypasses catch but still attempts finally. Cleanup is
best-effort: nested finalizers share one budget of VM steps and five seconds,
and the parent workflow deadline remains a hard upper bound. If that deadline
has expired, cleanup can be skipped or stopped. A cleanup failure cannot replace
the original cancellation; with an ordinary pending failure, a finally failure
becomes the reported error and retains the earlier failure as context. A transfer
inside finally overrides a pending ordinary failure or transfer. Nested cleanup
has a 16,384 VM-step budget as well as a five-second budget and the parent
workflow deadline; the step budget also covers `on exit` and `on interrupt`
bodies. A cleanup that exceeds the step budget after every statement before it
completed fails the run with status 1 and a message naming the budget, but the
run's output at that point is still delivered, as with `fail`. With a failure
already pending, the overrun is reported with that failure as context. These
rules do
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
before any effect; the statements of a program with `on exit` count as nested.

### Step labels and step ids

A statement may start with a label: one word ending in a colon, such as `fetch:`.
The label names the statement's first step, so a compound statement is named by its
opening step. Labels start with a letter and use only letters, digits, `_` and `-`.
They must be unique in their program body, and they cannot be statement keywords
such as `if`, `run` or `then`. A label cannot precede a declaration, a trigger or a
clause, and a statement takes at most one label.

```program
fetch: run "printf" with arguments ["hello\n"]
run "printf" with arguments ["world\n"]
```

Every step has an id. A labelled step's id is its label. An unlabelled step's id
is derived from its kind, a short hash of its canonical content, and its occurrence
among identical steps, for example `Invoke:7e9b5db767aa#1`. Derived ids never
contain line numbers. Inserting a statement above a labelled step leaves its id
unchanged, even though the step's line number changes. A derived id changes when
the step's own content changes, or when an identical earlier step is added or
removed. Use a label when a tool must refer to a step across edits.

Ids appear in `execute explain --json` (each step row's `id`), in `--trace` events
(`id`), in the recorded step events of `--record` (`id` in `trace.jsonl`), and in
execution records (`step_id`). A program checkpoint records the id of each completed
node's first step and matches it again on resume; see
[Recover after a failure](#recover-after-a-failure). Live control does not expose step ids yet.

### Exit and interrupt handlers

A top-level `on exit` block runs on every exit path: normal completion, `exit`,
`fail`, and an error. It wraps the program's statements in a try/finally, so
its output is the run's final output, as with `finally`.

A top-level `on interrupt` block runs when an interrupt starts a drain. The
first interrupt (SIGINT, SIGTERM, SIGQUIT or SIGHUP on Unix; Ctrl-C on Windows)
stops admitting new work: a running command or `parallel for each` element
finishes, the next statement boundary unwinds through the cleanup blocks, and
the run ends with status 130 unless the interrupt's own status applies. A
second interrupt cancels as before: running children are stopped and the
`on interrupt` body is skipped. A `parallel for each` that is drained reports
the elements it did not start as `not_started`; a `parallel` task group with
`join` or `race` fails instead, as it does when cancelled.

Both blocks are unindented headers with indented bodies, appear at most once
each, and are allowed only in one-shot programs: a triggered program
(`every`, `on schedule`, `listen`) rejects them. A program with a handler needs
at least one statement to wrap. `on interrupt` is rejected with `--checkpoint`
and in live programs. Explain shows the interrupt body as an `on_interrupt` row.

The program's statements may spawn, await and cancel named jobs as they would
without a handler; the handler bodies may not. When a failure or `exit` ends the
statements, the `on exit` body runs first, and the jobs the run still owns are
cancelled and reaped as the run ends.

Checkpointed runs accept `try` and `on exit`. A `try` with a `catch` is one
checkpoint node, like `if` and loops: a resume runs the whole block again. A
`try` without a `catch`, and an `on exit` program, keep one node per statement
in the body, and the cleanup body is one node. A resume inside the body enters
the block again, so the cleanup runs once at its end. When a statement fails or
the run is cancelled, the cleanup runs at once but neither the failed statement
nor the cleanup is recorded as complete: a resume retries that statement, then
runs the rest of the body and the cleanup again.

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
`then capture bytes as name` binds the current bytes without parsing them;
`use bytes name` later supplies that exact byte value as pipeline input. A leading
UTF-8 BOM is stripped by text parsers.

`take <text|integer|boolean|list of text|json> at "<pointer>" from <value> as name`
selects a value using a literal JSON pointer. An unquoted identifier after `at`
uses a text binding as the pointer, for example `take text at pointer from
repository as name`; that binding must contain a valid pointer when the step
executes. `/items/0/name` selects an array
element's field; `~1` means `/`, `~0` means `~`, and `""` selects the root.
Empty object keys work. Array indexes are unsigned decimal with no leading
zeroes; `-` does not append or select, and a nonnumeric array index is treated
as an absent value. Missing fields, invalid pointers, null
where text is requested, mixed text lists and wrong scalar types fail with the
source line. There is no automatic parsing of a text binding: parse its bytes
first. Conditions and native argv require a selected typed scalar or text list.

`then encode value as json` replaces the pipeline bytes with compact JSON,
without a trailing newline. It accepts a literal, reference or list of values
and supports native pipes and `then save`. Duplicate object keys are refused.
Encoding writes directly from the
typed tree and checks the byte limit as it writes. Object keys are ordered;
whitespace and numeric spelling are not preserved. JSON integers through signed/unsigned
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

Add `as NAME` to name a literal or another binding without an applet call:

```program
use value "CLEAR" as state
use value 3 as attempts
use value true as enabled
use value ["one", "two"] as names
use value {"state": state, "attempts": attempts} as report
then encode report as json
```

The binding retains its static type: text, integer, boolean, list of text, or
JSON for other structured values. References inside lists and records resolve
when the statement runs. The pipeline representation is text bytes for text,
and compact JSON for other values, subject to the existing output limit.
`use value VALUE as NAME` launches no process and produces no implicit stdout.
Use `use value NAME` to emit text or `then encode NAME as json` to emit JSON.

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

`number.round` rounds to `digits` decimal places (zero by default); negative
`digits` round to tens, hundreds, and so on. Halfway values round away from
zero. Integer input uses exact integer arithmetic and permits `digits` from
-38 through 38. Floating-point input permits -308 through 308 and rounds the
shortest decimal representation of its binary64 value, then converts the
result back to binary64. A result outside the finite float range returns an
`overflow` error; non-finite inputs are invalid JSON numbers.

`number.format` accepts an integer `precision` from 0 through 15, defaulting to
0 when omitted. `number.format_size` accepts 0 through 6, defaulting to 1.
A supplied precision outside the applicable range or of another type is invalid.

`text.count_lines_by` groups input lines by a zero-based field separated by a
nonempty delimiter. An omitted `field` defaults to zero; a supplied field must
be an integer from 0 through 16,777,216, the maximum possible index within
its 16 MiB input limit. A line without the selected field is an error.

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

### Call another program file

`run program file "PATH" with NAME VALUE as RESULT` runs another `.program` file in
the same process, without a module manifest. The path is relative to the calling
file's directory. It runs with explicit inputs only, as a named call does, and its
result binds the same way. The file is read and checked when the caller loads, so
a missing file, a cycle, or a literal call input of the wrong type is refused before
the first effect. Values computed at run time are checked when their call starts.

The read of a called file is a host read. The capability policy must allow it, or the
call is refused. A called file must be a regular file of at most 1 MiB. Its load
errors name the file and the line, and never quote the file's text, so a refused or
unparseable file does not leak its contents.

```text
accept name as text with default "world"
run program file "install/config/fonts.program" with theme name as fonts
```

A called file is an ordinary finite program with two limits. It cannot declare a
trigger or define `define a program` blocks, and its inputs cannot come from the
environment or from arguments: callers pass them. Line numbers in a diagnostic
from a called file refer to that file, not to the caller. Calls nest to 16 files
and a program may reach 256 distinct files. A file call needs the loader, so
programs run from text with no path (live editing, the REPL, and language-server
text) report it as an error, not run it.

### Retry a command

`and retry up to N times` on a `run` command runs it again after a failure that may
be temporary. N is 1 to 100 retries after the first attempt. Optional clauses follow
it, in this order: `even if uncertain`, then `with backoff AMOUNT UNIT` with an
optional `doubling`. The unit is milliseconds, seconds, minutes, or hours. A backoff
is at most one hour.

```program
run flaky.service and retry up to 3 times with backoff 2 seconds doubling
```

```text
run "./sync-mirror" with arguments ["--quick"] and retry up to 2 times even if uncertain
```

A failed attempt is retried only when its outcome is declared retryable. An applet
declares this with an outcome of kind `retryable` in its contract. A quoted
executable has no contract, so any nonzero exit counts as retryable. Other outcomes
are never retried, and a retry that does not apply is refused with a message naming
the reason.

In the gate (`explain --json --allow`), a retried step is reported with its attempts,
because each attempt may repeat its effects. Its violations carry `attempts`, and the
verdict lists every retry under `retries`. A retry with `even if uncertain` on a step
with network or writes effects also needs `--allow undeclared`.

Retrying also needs the effects to be safe to repeat. The attempt is safe when its
outcome declares that nothing started (`effect_certainty: not_started`), or when the
command is retry-safe and has no redirection (see Retry safety below). These are
the same rules
checkpoint resume uses. Otherwise the retry is refused unless the step says
`even if uncertain`. A quoted executable is never retry-safe, so its
retries always need that clause.

After the last allowed attempt the command fails with `retry exhausted after N+1
attempts`. Without a backoff, retries follow the failure at once. With a backoff, the
first wait is the given amount. `doubling` doubles each later wait. The wait stops
early on cancellation or the program deadline.

Only a plain `run` command can retry. It is refused on `then`, `pipe to`, a
pipeline stage, `interactively`, hand-over and detached commands, command conditions,
listener sources, task groups, background jobs, program calls, and graph nodes. It
cannot be combined with `and cache`, `and keep status`, or `and keep result`. Output
redirection is kept, and each attempt reopens the file.

Each retry adds a `retry` event to the `--trace` output, with the attempt number and
the delay. The execution record keeps every failed attempt's outcome. `execute
explain --json` shows the policy on the command's step as `retry`, with `attempts`,
`backoff_ms`, `doubling`, `even_if_uncertain`, and `idempotent`.

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
the pipeline empty. A native command writes a stderr file target directly, as
`2>file` does: bytes land as they are written, with no size cap, and survive a
failure, a timeout or a cancellation. A replaced file is truncated when the
command starts, so a command that never starts leaves it unchanged. `stdout to
stderr` likewise gives the command this process's stderr as its stdout, so its
output appears as it is written, in order with the command's own stderr unless
that stderr is captured, and survives a failure or timeout. A merged stderr is
captured in memory, up to 64 MiB. Only one stderr or stdout redirection applies per
command, and it cannot be combined with `discard stderr`, `cache`, `keep result`,
or a pipe stage. `and stdin from <path>` reads the file as the command's stdin; it
applies to a command that starts a statement, and it is a `read` followed by the
command as a consumer. `then append to <path>` adds the pipeline value to the file
and keeps it as the pipeline value. Unlike `save`, it does not replace the file.
Per-command environment suffixes use `and environment {"NAME": "value"}` to
set values and `and without environment ["NAME"]` to remove inherited values.
Names beginning with `LD_` or `DYLD_` are refused unless an applicable capability
policy authorizes them.
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

### Working directory and `use folder`

`use folder "path"` sets the program's working directory, like `run cd with path
"path"`; either may precede a trigger as initial setup. Both resolve the physical
directory and go through one path:

1. The folder is opened.
2. The path it resolves to is authorized. With a capability policy that is a host
   filesystem check against the policy's grants; without one nothing is denied.
3. On Unix its device and inode are checked against the opened handle.
4. The resolved path is recorded as the working directory, so later relative paths join
   onto the directory that was authorized, not onto a path that could be re-resolved.

On Unix the opened handle is the authority: launched children enter the opened directory
(`fchdir`) before `exec`, call frames carry a duplicate of the handle, and background jobs
and hand-overs use the run's handle, so a symbolic link swapped after `use folder`
cannot move them. On other platforms the directory is a path. With a
policy, an absolute path is checked in preflight, but a relative `read`, `save`,
`append` or redirect after a directory change cannot be, because preflight does not
know the directory yet. The runtime authorizes each one when its step runs, joined
onto the authorized directory, with the same open-and-verify checks.

Listeners and persistent workers still start in a path-based directory, and sandboxed
children get the sandbox's own directory; see [threat model](threat-model.md) H12 and
H18. Windows has no device and inode check (H13).

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
use separate programs or explicitly save the results you need. `then save to`
preserves the existing file mode and refuses symlinks, hard links, non-regular
files, and Windows device names, matching append and redirect safety.

### Hand over and detached launch

`hand over to "program" with arguments [...]` replaces the script with one native program, so it must be the last statement of the program. On Unix the process is replaced with `execve`, keeping its process id and exit status. Windows has no in-place replacement, so the program starts with inherited stdio, the script waits for it, and its exit code becomes the script's exit code; codes outside 0-255 are reported as 1.

`launch detached "program" with arguments [...]` starts the program and continues without waiting. The script does not own the child: exiting the script neither waits for nor cancels it. An optional `and output to "file"` sends stdout and stderr to that file, which is truncated and checked exactly like a redirect target: the write grant and persistence policy apply, and a link is refused. A detached command produces no pipeline data, so it cannot feed a `then` or `pipe to` statement.

Both forms take one fresh native program named by a quoted path. They reject redirection, stdin, environment changes, result bindings, `discard stderr`, and caching. They are refused under `--checkpoint`, under `run --control`, in live programs (including live source edits, which check the whole program), in triggered, service and listener programs, in browser programs, and in TUI handlers. A run may start at most 64 detached programs; the next one is an error. On Unix a waiter thread reaps each detached child when it exits. On Windows, `CREATE_BREAKAWAY_FROM_JOB` escapes an outer job only when that job permits breakaway; where it does not, the child stays in the outer job and that job's close can end it.

### Temporary files and directories

`with temporary file as NAME` and `with temporary directory as NAME` bind NAME to a fresh, private, empty path for the indented body. The path is under the system temporary directory, is created owner-only, and is removed when the body ends. The removal also happens when the body fails, when an `exit` or `fail` leaves it, and when the run is drained or interrupted. A process killed with SIGKILL cannot run that cleanup, so the first temporary scope a later process creates removes scope directories whose owner process no longer exists. That sweep runs lazily, not at startup, and touches only directories owned by the current user.

Process substitution has no direct form. `diff <(sort a) <(sort b)` ports as two saves into temporaries followed by one `run "diff"` on their paths:

```text
with temporary file as left
  with temporary file as right
    run "sort" with arguments ["a"]
    then save to left
    run "sort" with arguments ["b"]
    then save to right
    run "diff" with arguments [left, right]
```

FIFOs and stdout-to-path job routing are not part of this version. Temporary scopes are refused under `--checkpoint`, in live programs, in program modules, in browser programs, and in TUI handlers, because a checkpoint cannot replay a path that a previous run created.

### Compile to POSIX sh

`execute compile --to sh FILE [--out PATH]` writes a POSIX sh script for the part of a program that has a faithful sh form. Without `--out` the script goes to standard output; with it, only that path is written. The compiler never drops or approximates a statement: every statement it cannot translate is refused by name and line, and nothing is written.

The compiled forms are native commands (`run "program" with arguments [...]` and named applets whose manifest renders a native command), `pipe to`, `and stderr to`, `and stderr appended to`, `and stdout to stderr`, `and discard stderr`, `and environment {"NAME": "value"}` on one command, `and without environment ["NAME"]` on one command, `if run ... succeeds` or `fails` with `otherwise`, `repeat N times`, `for each NAME in [...]` over literal text, `break`, `continue`, `exit with status`, and `fail with message`. `and stderr merged` and environment names beginning `LD_` or `DYLD_` are refused by the compiler.

These are refused: in-process applets such as `number.to_text`, built-in commands, applets whose contract remaps exit statuses, programs with inputs, triggers, or reusable definitions, bindings and `keep` options, retries, caching, `save`, `append`, `read`, `while` loops, try and catch, background jobs, parallel groups, temporary scopes, hand over, detached launch, and interactive commands.

Commands run with stdin closed unless a pipeline supplies it. The script keeps
execute's output rule. A command's stdout becomes the pipeline value, except that
a command followed by another fresh command or by the end of the program streams
straight to stdout, as it does under `execute run`. Multi-stage pipelines enable
`set -o pipefail`; if the selected shell does not support it, the script exits
with status 70. A pipeline producer terminated by SIGPIPE because its consumer
finished early is not a failure. A streamed final command whose reader closed
stdout exits 141. A failed command leaves the previous value in place, and the
final value is printed before the script exits, on success or failure.

The script must pass `dash -n`. The integration tests also compare its stdout and success with `execute run` under dash on Unix, and run shellcheck when it is installed.

Two limits follow from sh itself. Applet arguments are expanded when the script is compiled, so a leading `~` takes the home directory of the compiling user. Output containing NUL bytes cannot pass through a sh variable.

### Retry safety

Retry safety asks one question: does running a stage again repeat an effect the runtime
cannot account for? It decides two things. Under a checkpoint, an element or node that
is retry-safe needs no durable begin record before it runs, so a loop of such elements
costs no flush per element. On resume, a retry-safe node runs again without the
`--retry-uncertain` override. A stage is retry-safe when:

- it is builtin, or
- it is cacheable (deterministic and reusable), or
- it is an in-process applet and every effect it declares is a read (`fs.read*`,
  `clock.read`), or it declares no effects at all.

Anything else is uncertain. That includes a write, lock, process, network, system, or
undeclared effect, and every native or external command, whose effects the runtime
cannot see. A manifest that sets `"uncertain_on_failure": true` is uncertain even when
its effects are reads. Cacheable is an author contract: the result is deterministic and reusable, and
retry safety relies on it. Declare `cacheable` only for applets whose re-run has no
external effect.

A loop over a pure in-process applet such as `text.upper` therefore writes no begin
records, while a loop over a native command writes one fsynced begin record per element
before that element runs.

## Operational reference links

The following legacy section links point to their full descriptions in the
[runtime reference](runtime-reference.md).

<a id="finite-stdin-and-unix-composition"></a>[Finite stdin and Unix composition](runtime-reference.md#finite-stdin-and-unix-composition)

<a id="uncaptured-stdout"></a>[Uncaptured stdout](runtime-reference.md#uncaptured-stdout)

<a id="program-inputs"></a>[Program inputs](runtime-reference.md#program-inputs)

<a id="describe-inputs"></a>[Describe inputs](runtime-reference.md#describe-inputs)

<a id="allowed-values"></a>[Allowed values](runtime-reference.md#allowed-values)

<a id="program-location"></a>[Program location](runtime-reference.md#program-location)

<a id="prompted-inputs"></a>[Prompted inputs](runtime-reference.md#prompted-inputs)

<a id="flags-and-options"></a>[Flags and options](runtime-reference.md#flags-and-options)

<a id="agent-interface-discovery"></a>[Agent interface discovery](runtime-reference.md#agent-interface-discovery)

<a id="machine-readable-checks"></a>[Machine-readable checks](runtime-reference.md#machine-readable-checks)

<a id="review-a-plan-before-it-runs"></a>[Review a plan before it runs](runtime-reference.md#review-a-plan-before-it-runs)

<a id="attach-to-a-live-program"></a>[Attach to a live program](runtime-reference.md#attach-to-a-live-program)

<a id="listener-and-schedule-semantics"></a>[Listener and schedule semantics](runtime-reference.md#listener-and-schedule-semantics)

<a id="process-input-sources"></a>[Process input sources](runtime-reference.md#process-input-sources)

<a id="cli-and-manifests"></a>[CLI and manifests](runtime-reference.md#cli-and-manifests)

<a id="warm-daemon---daemon"></a>[Warm daemon (`--daemon`)](runtime-reference.md#warm-daemon---daemon)

<a id="recover-after-a-failure"></a>[Recover after a failure](runtime-reference.md#recover-after-a-failure)

<a id="per-element-checkpoints"></a>[Per-element checkpoints](runtime-reference.md#per-element-checkpoints)

<a id="journal-versions"></a>[Journal versions](runtime-reference.md#journal-versions)

<a id="bound-command-lifetimes"></a>[Bound command lifetimes](runtime-reference.md#bound-command-lifetimes)

## Runtime reference

The detailed contracts for stdin composition, declared inputs, machine-readable
checks, planning, live control, listeners, process inputs, CLI and manifests,
checkpoint recovery, daemon use, and command lifetimes are in the
[Runtime reference](runtime-reference.md). This document keeps the core language
syntax and semantics together; the companion reference preserves the stable
section anchors for those operational topics.
