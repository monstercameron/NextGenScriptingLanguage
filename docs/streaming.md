# Persistent native streams

A stream starts each external CLI once and connects their descriptors with ordinary
Unix pipes. The `.program` file controls the final output limit and sink.

```text
stream from printf with message "one\ntwo\nthree\nfour"
pipe to tr with from "[:lower:]" and to "[:upper:]"
take the first 3 lines
show each line
```

Run `execute examples/stream-three-lines.program`. It prints `ONE`, `TWO`, and
`THREE`, each followed by a newline. `take` is optional; without it the stream
continues until final output reaches EOF and the native stages finish.

For a live macOS file follower:

```text
stream from tail with flags ["-n", "0", "-F"] and path "events.log"
pipe to tr with flags ["-u"] and from "[:lower:]" and to "[:upper:]"
take the first 3 lines
show each line
```

Create `events.log` in the calling directory before starting the program, then
append lines from another terminal. The supplied example is
`examples/stream-events-macos.program`. macOS `tr -u` disables its output buffering;
That flag is specific to this macOS example. On Linux use a native command's own
flush controls or explicitly name a buffering utility installed on that machine.
The runtime cannot force arbitrary native programs to flush a line. The portable
finite `printf` example needs no buffering flag because the source reaches EOF.

## Record and lifecycle contract

- `take` counts final transformed output records, after all native stages. It
  returns at most the requested count; early natural EOF is successful.
- A record ends at LF. Its LF is preserved; a nonempty final suffix at EOF counts
  as one record. Empty EOF produces no record. Bytes, including NUL and non-UTF-8
  values, are preserved. Existing UTF-8 event listeners retain their own contract.
- `show each line` writes raw bytes to stdout as each complete record arrives.
  Slow output consumers backpressure the pipeline. The interpreter retains one
  pending line and a fixed read chunk, independent of total stream length.
- `--max-record-bytes` limits each final record, including LF (default 64 KiB).
  Oversized records fail before that record is emitted. Previously emitted
  records remain visible. `--max-output-bytes` applies to captured finite output;
  streams do not accumulate or impose that total-output cap.
- At the script limit, the supervisor stops and reaps remaining owned applet
  process groups. This intentional stop is successful. Native failures observed
  before cleanup fail the stream; stdout already emitted cannot be rolled back.
  Active stages are stopped immediately rather than awaited for future status.
- Ctrl-C, SIGTERM, and SIGQUIT cancel the stream and clean up owned groups.
  `--command-timeout-ms` bounds the complete native pipeline lifetime, including
  CLI output backpressure. Descendants deliberately escaping their group remain
  outside the existing process-containment contract.

Optional top-level `accept` declarations come first and use the same typed
[program input contract](runtime-reference.md#program-inputs). For example:

```text
accept path as text
stream from "cat" with arguments ["--", path]
show each line
```

Run `execute stream-file.program --input 'path=server log.txt'`. This forwards
file bytes subject to the record limit; it does not follow later file appends.

The source uses `stream from`; later native stages use `pipe to`; optional `take`
precedes the required final `show each line`. Stream statements are unindented.
Applet parameter syntax and manifest validation are shared with finite programs.
There are no hidden per-record transform launches or shell evaluation.

Streams currently contain native applets only. Folder setup, finite `then`
boundaries, named results, caching, schedules, and event handlers are not part of
this mode. Resume, checkpoints, `--max-runs`, and `--headless` are rejected before
launching applets. `execute check` validates a stream; `execute explain` shows its
native argv and script limit.

## Validation evidence

The macOS acceptance probe appended four source records to an owned temporary
file and received three uppercase responses before source EOF. Trace recorded
exactly one `tail` and one `tr` launch; both children were gone after interpreter
exit. The syntax/framing tests cover binary bytes, split records, unterminated
EOF, record bounds, and cancellation during the last callback. Linux compilation
is checked separately; native Linux behavior and latency require a Linux run.
