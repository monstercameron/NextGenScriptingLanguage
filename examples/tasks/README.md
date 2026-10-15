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

Task records preserve stdout bytes. Captured stderr and error messages are
diagnostic presentation fields: exact values registered by declared secret
sources are replaced with `[REDACTED]` before the record is encoded. Direct
`AppletInvocationResult.stderr` remains the exact backend byte capture.
