# Bounded task groups

These examples use the normal `.program` parser and CLI runtime:

```sh
cargo run -- run examples/tasks/parallel.program
cargo run -- run examples/tasks/race.program
```

`parallel at most N` joins every declared applet in source order and requires
each to return a declared success outcome. The join is fail-fast: when a task
fails or returns a non-success outcome, the group stops admitting pending tasks,
cancels and reaps the tasks still running, and returns a task-group error
instead of a result. That error keeps `partial_results`, a list with one record
per declared task in source order. A record's `state` is `success` or
`non_success` for a task that finished with an outcome, `failed` for a task that
raised an execution error, `cancelled` for a task stopped while running, and
`not_started` for a task that was never admitted. Tasks that finished before the
failure keep their results. The error's `uncertain` field is true when the
failing task or any other record is marked uncertain, so a caller can see which
tasks may have produced external effects.

`race at most N` returns the first declared success outcome and cancels and
reaps the remaining owned tasks; tasks that had not started are recorded as
`not_started`. Each task is independent; bindings from sibling tasks are not
visible inside the group. `N` is a per-group limit: nested task groups currently
have their own limits, so this is not yet a workflow-wide concurrency cap.

`parallel for each <name> in <list> at most N as <result>` runs one indented
`run` command once per list element, with at most `N` elements in flight
([for-each.program](for-each.program)). The command may be a named applet or a
quoted executable. Unlike `parallel`, a failing element does not stop its
siblings: a non-success outcome or launch failure is recorded for that element
alone. The result is a record with `mode` (`"for_each"`), `schema_version`
(`3`), `items` in list order, and the counts `succeeded`, `failed`, `cancelled`
and `not_started`. Each item has `index`, `item`, `state`, `uncertain`,
`launched_processes`, `output`, `output_encoding`, `stderr` and
`stderr_encoding`. It also has `status`, `outcome` and `outcome_kind` when the
element ran to an outcome, `error` when the element failed, and `value` for
typed applet results. Every element's call is resolved and capability-checked
before the first one starts.

`output` and `stderr` hold the captured bytes. Valid UTF-8 is stored as a string
with its encoding field set to `"utf8"`. Any other bytes are stored as base64
with the encoding field set to `"base64"`, and decoding the base64 recovers the
exact bytes. There are no separate `output_text` or `stderr_text` fields.

Records from earlier versions differ. Schema version 2 also carried
`output_text` and `stderr_text` convenience strings, which schema version 3
removes so that captured output is stored only once. Results with no
`schema_version` store `output` and `stderr` as arrays of bytes (`u8_array`).
Check `schema_version` before reading these fields, including in results saved
by an earlier run.

Cancellation, the total timeout and the output limit stop new elements, cancel
and reap running ones, and fail the group. Running elements are recorded as
`cancelled` and the rest as `not_started`. A failed `parallel for each` group
keeps the same record encoding in its `partial_results` list and sets
`partial_results_schema_version` to `3`. Lists hold at most 10,000 elements; the
group is one checkpoint node, like a loop.

Captured stdout and stderr in task records are presented after redaction: exact
values registered by declared secret sources are replaced with `[REDACTED]`
before the record is encoded, and the same applies to error messages. Direct
`AppletInvocationResult.stderr` remains the exact backend byte capture.
