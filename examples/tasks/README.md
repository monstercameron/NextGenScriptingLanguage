# Bounded task groups

These examples use the normal `.program` parser and CLI runtime:

```sh
cargo run -- run examples/tasks/parallel.program
cargo run -- run examples/tasks/race.program
```

`parallel at most N` joins every declared applet in source order and requires
each to return a declared success outcome. `race at most N` returns the first
declared success outcome and cancels and reaps the remaining owned tasks. Each
task is independent; bindings from sibling tasks are not visible inside the
group. `N` is a per-group limit: nested task groups currently have their own
limits, so this is not yet a workflow-wide concurrency cap.

`parallel for each <name> in <list> at most N as <result>` runs one indented
`run` command once per list element, with at most `N` elements in flight
([for-each.program](for-each.program)). The command may be a named applet or a
quoted executable. Unlike `parallel`, a failing element does not stop its
siblings: the result has one record per element, in list order, with `index`,
`item`, `state`, `status`, `outcome` and the captured output (plus
`output_text`/`stderr_text` when the bytes are UTF-8), and top-level
`succeeded`, `failed`, `cancelled` and `not_started` counts. Every element's
call is resolved and capability-checked before the first one starts.
Cancellation, the total timeout and the output limit stop new elements,
cancel and reap running ones and fail the group. Lists hold at most 10,000
elements; the group is one checkpoint node, like a loop.

Task records preserve stdout bytes. Captured stderr and error messages are
diagnostic presentation fields: exact values registered by declared secret
sources are replaced with `[REDACTED]` before the record is encoded. Direct
`AppletInvocationResult.stderr` remains the exact backend byte capture.
