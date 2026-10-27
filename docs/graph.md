# Typed applet graphs

Current source accepts a `graph` block as a workflow file. Use the ordinary
`execute check`, `inspect`, `explain` and `run` commands; `inspect` returns an
`execute.graph/1` topology, `explain` prints Graphviz DOT, and `run` returns
terminal port events as JSON: one closing `execute.graph-run/1` document for
finite graphs, and one `execute.graph-event/1` JSON line per terminal event as
it arrives for graphs with persistent nodes (timers, listeners, native
streams, program services, or renderers). `--events document` keeps the
closing document and `--events jsonl` streams lines for any graph without a
checkpoint; streamed runs hold no collected outputs, so a persistent graph can
run indefinitely, and closing the reader (`| head -n 3`) cancels the session.
The event schema is `docs/schemas/graph-event.v1.schema.json`.
Graphs can declare the same typed top-level inputs
as finite programs, then use those names in applet parameters. `--input` and
declared positional arguments follow the ordinary workflow rules. Finite applet
graphs can be checkpointed with `run --checkpoint DIR` and continued with
`resume --checkpoint DIR` (see [Checkpoints](#checkpoints)). Trigger/run limits
and UI options are not supported for graphs. The small example is
[`examples/graph/composition.program`](../examples/graph/composition.program).

The [runtime composition proof](../examples/composition/graph-runtime/README.md)
combines a scheduled module, a loopback remote event source, a persistent worker
and a native stream. Its test injects a headless renderer and checks generation
changes and cleanup; it does not establish actual terminal or desktop rendering.

The same parser and executor are public through `flow::graph`. A Rust host can
parse with `graph::parse(source, &registry)`, preflight with
`GraphPlan::validate()` or `validate_with_registry()`, inspect with `to_json()`
or `to_dot()`, and execute with `graph::run(plan, &registry, &run_options,
cancelled)`. `run` validates against the registry and preflights applet
capabilities and data-flow policy before starting nodes. Graphs are finite
plans, but timers and listeners can remain active until cancellation or a
configured total timeout.

`GraphSession::start` is the lower-level activation API. It checks graph
topology and runtime admission limits, then starts the supplied executor; it
does not resolve applets or perform capability, data-flow policy, or program
module preflight. A host that starts a `RuntimeGraphExecutor` directly must run
those checks and preload the matching `ProgramModuleSession` first. The
`graph::run` convenience function performs registry, policy and module
preflight before activating the graph.

Hosts that reconfigure a live session can use `GraphSession::try_reconfigure`:
static admission failures leave the current generation running. After
admission succeeds, the current generation is stopped before replacement
activation, so a later activation error does not restore it. The consuming
`reconfigure` method drops its old session if it returns an error.

## Source form

Every graph begins with a name and ends explicitly:

~~~text
graph greeting
  run text.concat with parts ["hello", "graph"] and separator " " as joined
  run text.trim_final_lf as cleaned
  bind joined.value to cleaned.text with capacity 1 lossless
end graph
~~~

## Top-level inputs

Put ordinary `accept` declarations before the graph header. They support the
same types, defaults, environment sources and positional argument sources as a
finite program. Use a declared name as a typed reference in an applet's `with`
parameters:

~~~text
accept greeting as text from argument 1
accept separator as text with default " "
graph greeting
  run text.concat with parts [greeting, "graph"] and separator separator as joined
  run text.trim_final_lf as cleaned
  bind joined.value to cleaned.text with capacity 1 lossless
end graph
~~~

For example, `execute run greeting.program -- "Hello"` supplies the positional
input, while `execute run greeting.program --input greeting=Hello` supplies it
by name. List parameters may mix literal items with compatible references, and
JSON inputs can supply a record parameter as a whole. Input names in `with` are
resolved as typed values and preserved as literal argv elements at the process
boundary.

`check`, `inspect` and `explain` validate supplied values and defaults while
allowing required values to remain unset, as they do for ordinary programs.
`inspect` includes the graph input declarations and any known values. `run`
resolves every required input and validates the resulting applet parameters,
ports and graph topology before starting nodes. Graph inputs apply to applet
parameters; graph topology, timer intervals, native stream parameters and
renderer seed properties remain literal declarations.

`run` creates a named applet node. Graph nodes use named applets rather than
direct executable commands. `every 10 seconds as ticker` creates a timer whose
`tick` event carries an integer. `listen to stdin as lines` or `listen to
<applet> as lines` produces byte `line` events. `stream from <source> as
records` accepts native stream pipeline directives such as `pipe to ...` and
`take ...` and must end with `show each line`; it exposes byte `stdout` records
and a single integer `status` output.

Remote listener applets use their manifest-declared `Events` ports and connect
with ordinary `bind` edges. The remote adapter uses the declared endpoint's
GET event protocol without graph caller parameters. Event frames retain the
source event name, opaque resume cursor, and duplicate-cursor marker for
at-least-once delivery. Hosts that accept stdin must inject a `GraphStdinSource`
(for example, a bounded `ChannelStdinSource`); graphs never read ambient
process stdin. Cancelling or reconfiguring a `GraphSession`, or reaching its
total deadline, cancels and joins listener workers and closes their bounded
queues.

A finite applet can opt into publishing its result on one declared event port:

~~~text
graph result_events
  listen to feed as source
  run records.normalize as normalized
  emit result from normalized to changed
  bind source.updates to normalized.payload with capacity 1 lossless
end graph
~~~

`emit result` is a graph projection over the ordinary finite invocation. The
applet contract must declare exactly one `Events` port, that port's schema must
exactly equal the canonical decoded result schema (or the declared result
envelope schema), and the applet must declare no ordinary output ports. The
graph suppresses its generated `status` output in this mode, so every successful
invocation publishes exactly one event and cannot also publish a result or
status output. A stream-triggered invocation emits one event for each incoming
message; a returned list remains one list payload rather than being split into
several events. Byte results cross unchanged as bytes. Non-success outcomes
without a result envelope fail the graph with the applet's outcome and effect
certainty and publish no event. With a result envelope, its typed value,
including failure metadata, is the one event payload. Projection uses the
existing bounded queues, revision and sequence accounting, sensitivity flow,
invocation context, module session, and graph cancellation path; it does not
give a finite applet a persistent event-source lifecycle.

`render tui:text`, `render tui:button`, `render quickshell:text` and
`render quickshell:button` describe the current generic renderer components.
`text` has a typed text property. `button` has text and enabled properties and
emits an empty-record `clicked` event. Seed the required properties with a
record literal:

~~~text
graph action
  render tui:button with properties {"text": "Refresh", "enabled": true} as refresh
end graph
~~~

Renderer input ports also expose `property_updates` as record deliveries for
partial property updates. These graph components are a narrow current-source
surface, not a claim that general Quickshell applications or desktop shell
integration are complete.

## Typed edges and delivery

Connect one output or event to a named input with `bind`:

~~~text
bind source.value to transform.text with capacity 8 drop oldest
~~~

An omitted capacity defaults to 16; the default policy is `lossless`. Available
policies are `lossless`, `drop newest`, `drop oldest` and `latest`. Lossless
queues backpressure their producer. The lossy policies discard new, oldest or
all queued messages respectively when full. Every edge has a bounded queue.

An interface-2 scheduled or listener `.program` applet runs as a persistent
graph node using the ordinary `run <applet> as <node>` form; `listen to` is the
trigger inside the module source. Its handler output is decoded through the
applet contract and published as one typed event per successful tick or input
line; its declared `Single` inputs are read when the graph starts. A listener service starts one
registered native command producer, rejects ambient stdin and other backend
protocols, and runs handlers sequentially. The graph's bounded queues provide
backpressure before the next line is read. Cancellation, deadlines and
reconfiguration stop and join the service, terminate/reap its source child,
and close downstream event ports. See the
[scheduled queue monitor](../examples/modules/queue_monitor/README.md) for a
schedule package, graph source and typed event contract.

Each port declares a canonical value schema and a delivery mode: `single`,
`records` or `events`. Validation checks names, required inputs, schemas,
nullable values and delivery compatibility before execution. A single output
may feed a records input; a records output connects to records, and events
connect only to events. No edge guesses a text/JSON, string/path or bytes/value
conversion. Use an applet with an explicit conversion contract when types do
not match. Applet stdout remains its raw output boundary; graph ports carry the
declared typed payload.

An applet node with one-shot inputs runs once. A `single` input is read once
before invocation; a `records` or `events` input can trigger one invocation per
incoming message. More than one streaming input to an applet is rejected rather
than implicitly joined. A graph cycle is also rejected because the source form
does not yet define a feedback seed operation. Applet outputs not bound to an
edge are collected as terminal events. Each event carries graph generation,
revision, sequence and sensitivity metadata.

Sensitivity labels flow with payloads. An emitted value joins the node's
declared output label with observed input labels, so a public transform cannot
erase a Secret or Unknown input label. Remote applet nodes are authorized using
that effective label before graph execution; see the
[data-flow policy](security-dataflow.md).

## Checkpoints

`execute run GRAPH --checkpoint DIR` runs a finite applet graph and records each
completed node in `DIR`. If the run is interrupted, `execute resume GRAPH
--checkpoint DIR` continues it. A node that was in flight when the run stopped
has side effects that may have happened, so resume refuses it unless
`--retry-uncertain` is passed. `execute checkpoint status DIR` reports the
committed nodes and any node in flight.

The on-disk journal is journal version 5, mode `graph`. `journal.json` holds
the graph's identity, the count and chain digest of the committed nodes, and the
nodes in flight. A node enters the in-flight list before its first applet
invocation, and `journal.json` is written before that invocation starts.
`graph.jsonl` holds one line per completed node, and a node's line is durable
before `journal.json` names it. Lines past the committed count are ignored, and
a chain mismatch refuses the resume. A program checkpoint is refused by a graph
resume, and a graph checkpoint is refused by a program resume.

The identity covers the canonical plan, including its inputs, and the working
directory. A resume must present the same identity. Completed payloads are written
only after the program journal's persistence policy accepts their sensitivity
labels.

The Rust API is `graph::run_checkpointed(plan, &registry, &run_options,
cancelled, store, retry_uncertain)`. `GraphCheckpointStore` is the storage trait,
and `MemoryGraphStore` implements it in memory for tests.

Phase 1 covers these graphs:

- Finite applet nodes only. Timer, listener, native stream, program service and
  renderer nodes are refused before any effect runs, and the refusal names the
  node.
- At most one stream input (`records` or `events` port) per node. A node with
  two or more is refused, and the refusal names the node. Multi-stream ordering is
  not supported yet.
- Acyclic graphs. Program-module nodes need a complete module identity; a node
  whose module identity cannot be fully computed is refused, and the refusal names
  the node.

A node's outputs are held back until the node completes. Its completion record is
stored before any of its outputs reach downstream nodes. A later run replays a
completed node's recorded outputs, with their recorded sensitivity, and does not
run the node again. Replayed events get new sequence numbers.

Each node's fingerprint covers its definition without source lines, its incoming
edges, its invocation's manifest and executable digest, its upstream fingerprints,
and every preloaded program module's fingerprint. Changing a module behind a
completed node therefore refuses the resume, before any effect runs.

A node is in flight from its first applet invocation, not from when it starts
waiting for inputs. An interrupted node that is not retry-safe needs
`retry_uncertain`; a node is retry-safe when its applet is retry-safe (see Retry safety
in the language reference).
Retrying reruns that node only.

Under checkpoints, terminal outputs come in node completion order, and within a
node in emission order. Graph runs without checkpoints keep arrival order.

## Runtime limits

`GraphRunOptions.max_listener_readers` defaults to 64 and cannot exceed the
absolute limit of 128. Admission sums the declared event-port count for every
applet listener before allocating graph queues or starting node workers.
Each applet listener is also limited to 64 declared event ports, matching the
per-source reader scope; this check runs before any listener starts. The
aggregate limit can therefore admit two 64-port listeners when configured to
128.
Remote listeners use one additional pump worker per declared event port. A
local applet listener is conservatively charged for its synthetic `line` port,
although it reads through its graph node actor and creates no additional pump;
`listen to stdin` is excluded. This budget is separate from graph node actor
parallelism and the finite applet task scheduler.

Default graph limits are 64 nodes and parallel tasks, 4,096 edges, 65,536 total
edge and terminal queue slots, and 8 MiB per message. Terminal queues hold 64
events each. Collected output is limited to 100,000 events and 64 MiB. Graph
execution returns an error if a collected-output limit is reached; lossy edge
queues report dropped-message counts in the run result. These caps bound the
graph transport surface, not arbitrary memory or I/O used inside an applet.

The `--command-timeout-ms` and `--total-timeout-ms` flags apply through the
ordinary run options. Pressing Ctrl-C cancels the graph. Native child cleanup
still follows the process-group/platform supervision limits described in the
[security boundaries](../SECURITY.md); cancellation is not a universal deadline
for filesystem operations.
