# Live state control for agents

`execute live` exposes real execution state through a local Unix socket, or a
native owner-checked named pipe on Windows. Every
client command prints one JSON response and returns status 0 on success or 1 on
failure. `execute live schema` describes the commands, fields, limits, and supported
program kinds without starting a program. No shell, HTTP server, or extra daemon
is required.

Start a controlled instance in its own terminal/process:

```sh
cd examples/live
execute live serve queue-monitor.program --state-file queue-state.json \
  --socket /tmp/execute-queue.sock --paused
```

`live serve` keeps the existing manifest discovery by default. For an explicit
packed catalog, add `--manifest-catalog FILE`; it cannot be combined with
`--manifest-dir`.

The foreground server emits a JSON `ready` event. It stays alive after a finite
program completes, allowing inspection, repair, and another explicitly requested
execution. `control stop` stops its owned native groups, joins the workers, removes
its socket, and exits. Ctrl-C/SIGTERM does the same cleanup. An existing socket or
file at the socket path is never overwritten.

From another process, an agent can inspect and change the live monitor:

```sh
execute live read --socket /tmp/execute-queue.sock
execute live read /bindings --socket /tmp/execute-queue.sock
execute live create /user/change_reason --json '"Review shorter queue delays"' --socket /tmp/execute-queue.sock
execute live update /bindings/threshold --json '"10"' --socket /tmp/execute-queue.sock
execute live delete /user/change_reason --socket /tmp/execute-queue.sock
execute live control resume --socket /tmp/execute-queue.sock
```

The `.program` still specifies the timer, native commands, jq predicate, and saved
report. Rust supervises execution. The next report uses the changed threshold;
Python or an LLM does not compute substitute application results. The supplied
sample produces one overdue job with threshold 30 and two with threshold 10.
`queue-report.json` is an actual saved output, relative to the program's working
directory. In managed mode, app output is also available through state reads;
server stdout contains control events rather than raw app output.

## State and CRUD

Paths are RFC 6901 JSON pointers. `~1` encodes `/` in a key and `~0` encodes `~`.
A root read returns `{state, runtime, events}` inside the response's `result`.
Writes return compact acknowledgments with status, cursor, and revision; read
only the fields needed next to avoid filling an agent context with large outputs.
State paths such as `/bindings/threshold` address the editable state directly.
`/runtime` and `/events` address observed, read-only information.

| Editable field | Meaning |
| --- | --- |
| `/bindings` | Actual named values used to resolve applet parameters and paths |
| `/input` | Actual bytes supplied to the next consuming node: `{"text":"UTF-8"}` or `{"bytes":[0,255,10]}` |
| `/cwd` | Existing absolute UTF-8 working directory for subsequent execution |
| `/source` | Current in-memory `.program` source |
| `/cursor` | Zero-based next instruction; native pipeline groups and complete candidate conditionals cannot be split |
| `/inbox` | FIFO listener records; insert at `/inbox/-`, inspect, replace, or remove entries |
| `/user` | Arbitrary JSON metadata owned by the controlling application/agent |

`create` requires an absent object key; for arrays it inserts an item. `update`
requires an existing path. `delete` requires an existing path. Required document
fields cannot be deleted, but their valid contents can be replaced or cleared.
Removing a binding still referenced by pending steps is rejected. Previously
produced result bindings can be changed before subsequent nodes consume them.
Statements that subsequently assign a binding recompute that binding; a listener
also replaces its event binding and pipeline input when it accepts the next record.
Managed bindings, cwd, and pipeline data otherwise persist across ticks/handlers;
this opt-in stateful mode lets subsequent iterations use agent-edited values or
previous results. A `run`/`read` step still replaces data as usual.

Strings, booleans, integers, and lists become the corresponding native argument
values. Installed 0.8.0 converts objects, null, fractional numbers and numbers
outside i64 to JSON text at the native argument boundary. **The pending source
candidate changes this boundary:** those values remain structured; use `take`
to select a typed scalar or `then encode value as json` to produce explicit
pipeline bytes. Nested `/bindings/document/field` edits affect subsequent
selections. JSON numbers use the signed/unsigned integer and finite floating-point
model described in the language guide; clients can also round numbers before
sending. Unchanged external bindings retain
their JSON types in state. Applets still enforce their manifest parameter types;
for example, values in a `flags` list declared as strings must be strings. The
example's `threshold` is JSON numeric *text* passed to `jq --argjson`.

Finite `read from stdin` belongs to ordinary CLI runs. Managed programs reject
it; use `/input` pipeline state or a `listen to stdin` service's inbox instead.

Top-level `accept` declarations constrain their bindings: required values must
exist, defaults materialize at startup, and edits must retain the declared JSON
type. Declared bindings cannot be deleted. A source edit that adds required inputs
must add their values in the same atomic patch. Component TUI input ports retain
their separate text-port contract. The source candidate's
[`execute inspect FILE`](runtime-reference.md#agent-interface-discovery) exposes program
input types, defaults, missing values and TUI ports without launching the program;
the installed 0.8.0 release does not include `inspect` yet. `live schema` also
describes these declared-input restrictions.

## Atomic changes and concurrency

Every accepted mutation and execution transition advances a monotonically
increasing revision. Read the response's `revision`, then use `--if-revision N`
on a write to avoid overwriting intervening work. A conflict returns
`{"ok":false,"error":{"code":"conflict",...},"revision":...}`.

Apply related edits atomically with RFC 6902 `add`, `replace`, `remove`, and `test`:

```sh
execute live patch --socket /tmp/execute-queue.sock --if-revision 12 --json '[
  {"op":"replace","path":"/bindings/threshold","value":"15"},
  {"op":"add","path":"/user/request_id","value":"agent-42"}
]'
```

Use an actual revision from a read; `12` above is illustrative. The whole candidate
state and pending workflow validate before commit. Invalid syntax, missing
references, unknown applets, bad cursor boundaries, capacity violations, or failed
patch tests leave both state and revision unchanged. Resolved native argument
values are checked by the ordinary runtime when their node executes.

For workflow replacement, pause and wait for a boundary, then replace `/source`
and `/cursor` together if the new program has a different position. Use
`--text-file repaired.program` to supply a source string without manually escaping
its newlines. Source replacement does not rewrite the original file. Completed
statements are not replayed unless the agent moves the cursor backward or requests
restart. Validation applies to the pending suffix against the current bindings.

`execute live wait --after N --timeout-ms 1000 --socket PATH` waits for a revision
change or timeout and returns a snapshot. A timeout is successful with
`changed:false`. This is bounded long polling, so an agent need not busy-loop.
An optional pointer, such as `wait /runtime/status`, projects the returned snapshot.
The latest 64 transition events are retained; they are not a durable event log.

## Execution controls

| Action | Behavior |
| --- | --- |
| `pause` | Finish the active native group, then pause; a waiting service pauses immediately |
| `step` | Run one node/native group and pause; for an interval/calendar service waiting for a tick, authorize one immediate diagnostic tick |
| `resume` | Continue pending execution or resume waiting for events/ticks |
| `cancel` | Cancel and reap the active native group; retain the pre-node state and cursor |
| `retry` | Explicitly authorize retry after an error/cancellation that may have partial effects |
| `reset` | Pause, return cursor/cwd/input to the initial context, restore script-assigned names from initial bindings (or clear them if unseeded); preserve other bindings, inbox, and metadata |
| `restart` | Reset and run again; completed external effects may be repeated |
| `restart-source` | Restart an owned native listener producer; input may be replayed |
| `reload-agents` | Re-read the startup `--agent-policy` file so revocations and new grants (including `expires_at`) take effect without restarting; a broken file keeps the old policy and fails the request |
| `stop` | Stop the whole managed instance and clean up its owned processes/socket |

Pause is cooperative at a node boundary, not an arbitrary instruction freeze.

The pending source candidate makes managed interval waits follow the actual
monotonic deadline, preserving submillisecond phase after a handler finishes and
skipping missed ticks. The first normal tick still waits one full interval;
handlers remain serial. Control and source edits notify the waiting worker.
This removes the installed 0.8.0 worker's fixed 20 ms timer-wait granularity.
It does not provide hard realtime guarantees: native commands, OS scheduling and
control/rendering work still contribute latency. Calendar/listener polling and
TUI event-loop timing retain their existing contracts.

The response to `pause` can still show `in_flight:true`; wait until it is false
before editing execution fields. During an active group, `/user` and `/inbox`
remain editable. Execution-state changes return `busy` until the group finishes.
Cancellation cannot undo native file/API/Git effects. Failures expose
`effects_may_be_incomplete:true`; ordinary `resume` is refused until explicit
`retry`, reset/restart, or a deliberate cursor change. No silent retry is performed.

The source candidate also treats each complete `if`/`otherwise` block as one
execution node, including its nested commands. A step selects and runs that block;
interior cursor edits are rejected, and pause/source edits wait for its boundary.
Native status bindings are JSON integers. See the [conditional contract](language.md#conditional-commands)
for comparison types and conservative checkpoint recovery.

The candidate's same-file reusable finite calls are also one whole node. Their
local bindings stay private; arguments and returned names live in the caller's
state. Failed calls require explicit `retry`, and cancellation reaches their
native children. Source edits that change expanded body size may require an
atomic cursor update. See the [reuse contract](language.md#reusable-finite-programs).

Native pipeline groups are single scheduling units. Separately buffered `then`
steps and reads/saves form their own nodes. Runtime observations include the active
statement range, completed nodes/runs, completed native launches, cache hits,
last output, errors, and producer completion. Launch counts cover successfully
completed groups, including a producer after its successful completion; they are
not an inventory of currently running PIDs.

## Listener and stream behavior

For a managed `listen to stdin` program, the inbox is its input transport:

```sh
execute live send --socket /tmp/events.sock --json '{"text":"deployment complete\n"}'
```

Each queued item is one record/handler invocation; bytes/newlines are preserved.
Listener bindings require UTF-8, matching the ordinary listener runtime. Native
applet listeners start one real producer, enqueue bounded records, and run the
script's handler sequentially. Pausing handlers applies backpressure once the
inbox fills. Pausing does not terminate the producer. Editing the native listener source or cwd restarts
that producer and may replay records. Handler-only source edits preserve the producer. Binding edits affect future handlers;
use `restart-source` if the producer's captured parameter bindings must change.

Persistent native streams run one real pipeline. They publish each final record
immediately to live state. Pause/step operate at emitted-record boundaries;
`in_flight` stays true while a paused pipeline remains alive. Resume/step can
release it. Native argv cannot change inside an already running process: cancel,
wait for cleanup, edit bindings/source/cursor, and retry/restart the pipeline.
Native stdio buffering still governs when records arrive.

## Live TUI state and events

Start a real terminal UI with the same namespace:

```sh
execute live serve examples/live/widget-console.program --socket /tmp/console.sock --paused
```

For an agent-owned background process, add `--headless`. This keeps the same
renderer and component/event engine alive without requiring a terminal; the
100×30 rendered preview is available at `/runtime/frame`. Interactive mode draws
to the real terminal and accepts normal keyboard input too.

```sh
execute live read /ui --socket /tmp/console.sock
execute live update /ui/programs/console/widgets/message/text --json '"hello from an agent"' --socket /tmp/console.sock
execute live create /ui/programs/console/widgets/choices/items/- --json '"new choice"' --socket /tmp/console.sock
execute live event --json '{"type":"submit","program":"console","widget":"message"}' --socket /tmp/console.sock
execute live control resume --socket /tmp/console.sock
execute live read /ui/programs/console/widgets/result/text --socket /tmp/console.sock
```

Submitting invokes the actual `.program` handler, which runs the real `printf`
manifest and displays its native result. The CLI never substitutes application
logic for that handler. Button events use `type:"press"`, list activation uses
`type:"choose"` with an `index`, and `type:"open"` explicitly queues a component's
initialization handlers. These events can also be created/edited/deleted in
`/ui/events` before dispatch. Once dispatched, ordinary per-component handler
queues coalesce repeated events as they do for keyboard input; they are not a
durable delivery log.

| Path | Actual renderer state |
| --- | --- |
| `/ui/programs/NAME/inputs/PORT` | Create/update/delete a declared component input; changed values queue its arrival handler |
| `/ui/programs/NAME/widgets/ID/text` | Input draft, heading, text panel, badge, or button contents |
| `/ui/programs/NAME/widgets/ID/items` and `/selected` | List entries and current selection |
| `/ui/programs/NAME/widgets/ID/rows` | Table objects with the columns required by the source |
| `/ui/programs/NAME/widgets/ID/bars` | Bar objects `{label,value}` with finite nonnegative values |
| `/ui/programs/NAME/widgets/ID/series/KEY` | Editable `[x,y]` points for a chart series |
| `/ui/programs/NAME/widgets/ID/active_series` | Visible chart series |
| `/ui/focus` | `{program,widget}` naming an interactive widget |

CRUD on entries, rows, bars, points, and series uses the same atomic patch and
revision rules. For example, deleting the active series also requires replacing
`active_series` in the same patch. Removing a list entry may require adjusting
its selection. Invalid updates roll back the entire transaction. Text-field
limits, table columns, 256-item/bar limits, and 16-series/120-point chart limits
match the renderer's normal contracts. Required widget fields/identities are
structural; add/remove widgets and components by editing `/source`.

Source replacement rebuilds the layout and handlers at an idle boundary. It
preserves compatible edited view state, applies changed defaults to untouched
fields, creates new widget state, and removes deleted widget state. Rebuilt
components queue their open handlers again, so initialization effects may repeat
when execution resumes. Changing source/cwd or reset/restart can rebuild the UI.
Root `/bindings` must be empty for UI programs: their live variables are component
inputs and input-widget text, which are the actual values used by UI handlers.

UI edits to text/list/table/chart data are applied to the actual `View` objects,
not an independent JSON copy. Writes return an accepted revision; wait until
`/runtime/ui_applied_revision` reaches that revision to observe application and
rendering. `/runtime/frame` is a fixed-size preview; the interactive display uses
the real terminal dimensions. `/runtime/ui_errors` reports component errors.
UI counters describe native worker completions, while view-only handlers appear
as state/revision changes.

UI `pause` finishes an active handler and stops starting new handlers; `step`
starts one queued handler and then pauses. UI events remain enqueueable while a
worker is active, but execution-state/widget edits return `busy` until an idle
boundary. `cancel` kills/reaps current native workers, discards stale completions,
and requires explicit retry before re-executing their queued handlers. Stop and
signals restore terminal settings and join owned workers.

## Resource and compatibility contract

On Windows, `--socket PATH` is a local named-pipe namespace key rather than a
filesystem socket. Both peers check user SIDs; protected DACLs and rejection of
remote clients enforce the local boundary. Use identical absolute path spelling
for the server and clients. See [native Windows semantics](windows.md). The
Unix permission/UID details below apply to macOS and Linux.

The socket is mode 0600; macOS/Linux peer credentials must match the server's
user. The pending source candidate also verifies the server peer UID in the CLI
before transmitting a request. This prevents sending values to a socket owned
by another user; same-user processes remain inside the trust boundary. On Unix
the socket's parent directory must be owned by you and private (mode 0700):
`serve` refuses any other parent, and `run --control` hosts its endpoint under
a private per-user directory with a randomized name. Installed 0.8.0 only
recommended a private directory. Limits are 1 MiB/request,
8 MiB/editable state, 64 KiB/source, 128 queued records, 16 simultaneous clients
(`live schema` limits keep the deprecated `concurrent_clients` field as an alias
for `general_clients`, both 16; new readers should use `general_clients` +
`priority_spare`)
plus a 2-slot reserve so `control stop` and `read` are still served while every
general slot holds a long poll (other requests past the slot limit fail fast
with `busy`; reserve-exhausted priority also answers `busy` on Unix so callers
retry instead of seeing EOF, and sheds use non-blocking writes that never
stall admission — a saturated client may still observe EOF or a truncated
`busy` prefix instead of the full frame. The CLI performs a single attempt
and does not auto-retry: callers should retry `busy`, EOF, and
truncated/unparseable responses exactly like `busy`. `read` and
`control stop` are safe to retry; for mutations an EOF/truncated response is
ambiguous (a shed connection never ran the handler, but a response-write
failure after the handler ran looks the same), so retry mutations only with
`if_revision` or after a confirming `read`), a burst-tolerant flood budget admitting 2048 back-to-back
non-priority requests and refilling 64 per second (a sustained flood past the
burst sheds with `busy`; reserve-eligible priority never spends the budget),
a bounded 64-connection pre-admission queue on Unix so partial senders hold no
thread or slot, a documented 8 KiB priority-classification window (only a
newline-terminated request line within the first 8 KiB is considered for the
reserve; a larger-but-valid `read`/`stop` still joins the general lane and is
served whenever a general slot is free), an authorization gate on the reserve
(when `--agent-policy` is configured, only a request already authorized for
`read` or `control stop` may claim a reserve slot; unauthenticated,
under-permissioned, or malformed-`auth` fake-priority frames are demoted to
the general lane and spend the flood budget there, so they cannot monopolize
the reserve — with no policy the socket stays same-user trusted and every
well-formed priority claim without an `auth` field stays reserve-eligible (a
no-policy claim carrying `auth` is demoted because the handler rejects any
`auth` without a policy), preserving `stop` availability; preclassification
additionally mirrors the handler's pre-state field checks so an
authorized-but-invalid `read`/`stop` with extra fields, a bad `if_revision`,
or wrong field types is demoted rather than occupying a reserve slot), 64 retained events,
and 10 seconds per long poll. Live node/record output defaults
to 256 KiB and can be raised to 1 MiB with `--max-output-bytes`. Native groups
support `--command-timeout-ms`. Established connections have absolute two-second
budgets for request reads and
response writes. The CLI also bounds request writes to two seconds and response
reads to twelve seconds. Progress does not reset these budgets. The separate
`wait` handler may run for up to ten seconds before its response write begins.
Connection setup, CPU/mutex work and filesystem operations do not acquire these
transport deadlines. Expired incomplete requests do not mutate state; a peer
still sending may observe connection closure instead of an error frame. Each
socket connection carries one
newline-terminated JSON request and one JSON response, then closes.

Windows is best-effort and does not claim Unix parity. Named pipes have no
pre-admission classifier: every accepted connection spawns a handler thread
that holds its slot through the 2-second request read, so up to 18 slow or
partial same-user senders can delay `stop` until their reads time out (the
server self-heals as each read expires; no slot is held past the timeout).
The flood budget is enforced post-parse but pre-auth/state, so sustained
floods still shed with `busy` within the 18-connection cap at the cost of
thread churn rather than no thread at all. Past the hard 18-connection cap
Windows closes the connection without a `busy` frame (the client sees EOF and
should retry); a reply there would block the accept loop or spawn unbounded
shed threads. Lane admission never bypasses authentication on either
platform: classification only schedules, and every admitted request still
passes the agent policy before touching state. Windows has no pre-admission
authorization gate: a constrained-lane handler still answers `busy` to
non-priority shapes after parsing, but a fake-priority frame costs a handler
thread and its slot until the handler rejects it, so the Unix demotion bound
above does not apply there. The saturation tests in
`tests/live.rs` run on Unix only, so Windows thread/slot behavior has no
automated coverage; Windows acceptance is the documented behavior above, not
a test gate. The new deterministic gates are unit tests over the Unix lane
decision and reserve-eligibility predicate in `src/live/mod.rs`, which need
no timing; the reserve-exhausted `busy` path is covered there rather than by
a timing-sensitive integration test because fast `read`/`stop` handlers
cannot deterministically hold both reserve slots.

The pending source candidate enforces the document budget when a stream installs
its next record and when a listener promotes an inbox record into its input and
binding. An oversized stream record fails without replacing the previous input
or last output. An oversized listener promotion fails before starting its handler
and preserves the queued record, prior binding and input; remove unrelated user
state, then resume to retry. Invalid UTF-8 listener records retain their existing
consume-and-reject behavior. The 8 MiB bound counts compact serialized document
bytes, including JSON escaping and byte-array expansion; it is not a bound on the
entire process RSS or response envelope. Installed 0.8.0 does not include this fix.

This opt-in mode supports finite workflows, interval/calendar services, managed
stdin listeners, native applet listeners, persistent native streams, interactive
TUIs, and persistent headless TUIs. Regular `execute FILE.program` behavior is
unchanged. Existing processes must be started in live mode to opt in; this API
does not attach to arbitrary existing processes. State is ephemeral and is not
mixed with the durable checkpoint/resume ledger. There is no claim of hard
real-time scheduling or arbitrary mutation of another binary's private memory.
