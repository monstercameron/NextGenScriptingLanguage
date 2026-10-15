# Typed applet graphs

Current source accepts a `graph` block as a workflow file. Use the ordinary
`execute check`, `inspect`, `explain` and `run` commands; `inspect` returns an
`execute.graph/1` topology, `explain` prints Graphviz DOT, and `run` returns
terminal port events as JSON. Graph node inputs are declared in the graph file,
so `--input` and positional workflow arguments do not apply. Graphs do not
support checkpoints/resume, trigger/run limits or UI options. The small example
is [`examples/graph/composition.program`](../examples/graph/composition.program).

The same parser and executor are public through `flow::graph`. A Rust host can
parse with `graph::parse(source, &registry)`, preflight with
`GraphPlan::validate()` or `validate_with_registry()`, inspect with `to_json()`
or `to_dot()`, and execute with `graph::run(plan, &registry, &run_options,
cancelled)`. `run` validates against the registry and preflights applet
capabilities and data-flow policy before starting nodes. Graphs are finite
plans, but timers and listeners can remain active until cancellation or a
configured total timeout.

## Source form

Every graph begins with a name and ends explicitly:

~~~text
graph greeting
  run text.concat with parts ["hello", "graph"] and separator " " as joined
  run text.trim_final_lf as cleaned
  bind joined.value to cleaned.text with capacity 1 lossless
end graph
~~~

`run` creates a named applet node. Graph nodes use named applets rather than
direct executable commands. `every 10 seconds as ticker` creates a timer whose
`tick` event carries an integer. `listen to stdin as lines` or `listen to
<applet> as lines` produces byte `line` events. `stream from <source> as
records` accepts native stream pipeline directives such as `pipe to ...` and
`take ...` and must end with `show each line`; it exposes byte `stdout` records
and a single integer `status` output.

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

## Runtime limits

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
