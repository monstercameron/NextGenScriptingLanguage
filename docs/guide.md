# Practical guide

Build your first program with the [quickstart](../README.md#quickstart), then use
this tour for inputs, pipelines, services, terminal apps, model decisions and
recovery. The [language reference](language.md) defines exact contracts. Examples
run from the repository root with the current native runtime and required applets.
The [browser playground](playground.md) supports a bounded subset.

## The language in one minute

| Syntax | Meaning |
| --- | --- |
| `run pwd` | Start an independent command with closed stdin. |
| `run "git" with arguments ["status", "--short"]` | Launch any executable directly with exact argv, without a manifest. |
| `then tr with from "a-z" and to "A-Z"` | Wait for success, then pass finite output to the next command. |
| `pipe to wc with flags ["-l"]` | Connect adjacent native commands with an OS pipe. |
| `read "input.txt"` | Read a file without starting `cat`. |
| `read from stdin` | Inherit stdin for a finite pipeline stage. |
| `as report` | Bind finite UTF-8 output to a name. |
| `and keep status as code` | Bind a native exit code as an integer and continue after ordinary nonzero exits. |
| `if code is 0` / `otherwise` | Choose an indented native command body using scalar equality. |
| `and cache` | Reuse the result of a declared cacheable transformation. |
| `then save to "report.txt"` | Atomically replace a file with the current output. |
| `run cd with path "reports"` | Change the program's working directory. |

One logical statement per line. Bracketed lists and objects may continue across
physical lines, and triple-quoted raw blocks hold multiline text at the end of a
statement. Use double-quoted JSON strings, `#` comments, named parameters, and
consistently indented event bodies. Strings, integers, booleans,
references, and lists are supported. The runtime also provides
[JSON parsing, typed field selection and encoding](../docs/language.md#structured-json),
with JSON inputs and editable structured bindings. Broader computation stays in tools such as `jq`,
`awk`, and `sort`.

The runtime also supports [same-file reusable finite programs](../docs/language.md#reusable-finite-programs):
define a program with typed inputs, then call it with `run program name` or
`then program name`. Calls keep local bindings/cwd and return bytes through the
shared runtime; no additional interpreter process is launched.

`then` creates a finite, buffered boundary. `pipe to` keeps stages running
concurrently with native backpressure. A finite program buffers its final output
until completion; choose a stream or listener when results must arrive live.

Multiple files are explicit and ordered:

```text
run cat with files ["notes/intro.txt", "notes/checklist.txt"]
then save to "handoff.txt"
```

Named results can supply later parameters. Their bytes, including a trailing
newline, are preserved. Wildcards, `$variables`, and command substitution do not
expand automatically. See the [language reference](../docs/language.md) for exact
value, quoting, error, and output contracts.

## Explicit AI decisions, ordinary program control flow

The model adapter supports TypeSafe Jev classification, yes/no judgment and
scoring, plus OpenAI text generation and strict JSON extraction. Rust handles
the provider protocol. Your `.program` checks typed results and chooses the branch:

```text
accept message as text
accept env_file as text
run "printf" with arguments ["%s", message]
then jev.classify with question "Does this message describe a storage failure?" and labels ["storage", "other"] and minimum 85 and env_file env_file
then parse json as result
take boolean at "/accepted" from result as accepted
take text at "/label" from result as label
if accepted is true
  if label is "storage"
    run "printf" with arguments ["Queue a storage investigation\n"]
  otherwise
    run "printf" with arguments ["Queue general review\n"]
otherwise
  run "printf" with arguments ["Request human classification\n"]
```

Confidence and label are separate decisions. A confident negative result must
still take its own branch. Calls have explicit limits, sanitized errors and
checkpoint protection against accidental repeat requests. There are no automatic
model calls, retries, or model-result caches.

Explore [100 runnable model workflows](../examples/models/README.md), covering
operations, development, research, accessibility, education and more. Each has
input fixtures, expected output artifacts, and multiple routing cases. They run
offline by default; a typed `live` switch enables real inference. Build both
`execute` and `execute-model` beside each other, then see the
[model operations guide](../docs/models.md) for credentials, limits and examples.

## One file, several lifetimes

Programs can declare typed inputs and pass them into ordinary native argv:

```text
accept path as text
accept lines as integer with default 20
read path
pipe to "head" with arguments ["-n", lines]
```

Save this as `preview.program`, then run
`execute preview.program --input 'path=server log.txt' --input lines=10`.
Programs also support `accept root as text from environment "OMARCHY_PATH"`
and `accept message as text from argument 1`; pass Unix arguments after `--`.
Use `#!/usr/bin/env -S execute --` for an executable `.program` command.
[Process input contracts](../docs/language.md#process-input-sources) explain precedence,
static inspection and checkpoint behavior.

Text, integers, booleans, and lists of text are supported. Missing required inputs,
bad types, unknown names, and known manifest constraint violations fail before
commands run. `check` and `explain` can inspect a signature without supplying
required values. See the [input reference](../docs/language.md#program-inputs).

The command stays the same: `execute app.program`. The script declares its lifetime.

### Persistent native pipeline

```text
stream from printf with message "one\ntwo\nthree\nfour"
pipe to tr with from "[:lower:]" and to "[:upper:]"
take the first 3 lines
show each line
```

This prints `ONE`, `TWO`, and `THREE`. Each native stage starts once; final records
are forwarded as they arrive. Native tools must flush their own output. See
[streaming](../docs/streaming.md) for an actual file-following example, binary record
handling, bounds, and cancellation.

### A handler for each incoming record

```text
listen to stdin
whenever a line arrives as message
  then tr with from "[:lower:]" and to "[:upper:]"
```

Save it as `uppercase-events.program`, then run:

```sh
printf 'first event\nsecond event\n' | execute uppercase-events.program
```

The interpreter stays alive and launches the handler's `tr` for each record. Use
`listen to tail with flags ["-n", "0", "-F"] and path "events.log"` instead of
`listen to stdin` to keep a file-following producer alive. Handlers run serially;
slow processing backpressures the source.

### Intervals and calendar schedules

```text
every 30 seconds
  run pwd
```

```text
on schedule "0 9 * * 1-5"
  run ls with flags ["-la"] and path "."
```

Intervals first run after one full interval. Calendar apps match five-field cron
expressions in the local timezone. Both run in the foreground; Ctrl-C stops
them. Missed ticks default to `skip`: overdue work is dropped while an interval
keeps its original phase. `--missed-ticks coalesce` reduces overdue work to the
latest due run. `--missed-ticks catch-up --max-catch-up N` allows at most 1 to
64 pending runs; older interval work and cron matches beyond the seven-day cron
lookback are discarded. Calendar matching counts each absolute minute by
default, including both instances of a repeated daylight-saving-time minute.
`--calendar-dedup local` treats the same local wall-clock minute as one event,
using a bounded two-day duplicate-key cache. These options apply to triggered
`run` and `resume` commands; see `execute --help` for the flags.

### Generate a reviewable service definition

Current source can print a per-user systemd unit or launchd plist for a
validated triggered workflow:

```sh
execute service generate systemd examples/heartbeat.program \
  --name com.example.nightly-report \
  --working-dir "$PWD" \
  --manifest-dir "$PWD/manifests"
```

The command writes the rendered definition to stdout only. It does not create a
file, install or enable a unit, load a launchd job, or start the workflow.
Review the output and move it into the service manager's user configuration
yourself when ready. Arguments are literal argv entries; generation uses no
shell. Paths are resolved and validated. Any required input sourced from the
caller environment must be supplied explicitly with `--input NAME=VALUE`, so
the service does not depend on an interactive login environment. A stdin
listener is rejected because the service manager does not supply a durable
event producer.

The generated systemd unit uses `Type=simple`, an explicit working directory,
`Restart=on-failure` with a five-second delay, `KillMode=control-group`, and
journal stdout/stderr. The launchd plist sets `ProgramArguments`,
`WorkingDirectory`, `RunAtLoad`, and `KeepAlive` with `SuccessfulExit=false`, so
it restarts failed runs but leaves successful completion stopped. It routes
stdout/stderr to per-user files under `~/Library/Logs`. It does not configure an OS-level timer;
the workflow's own interval or calendar trigger runs inside the foreground
Execute process. The upstream [systemd.service(5)](https://github.com/systemd/systemd/blob/main/man/systemd.service.xml)
and [systemd.kill(5)](https://github.com/systemd/systemd/blob/main/man/systemd.kill.xml)
manuals describe those unit settings; Apple's archived
[launchd job guide](https://developer.apple.com/library/archive/documentation/MacOSX/Conceptual/BPSystemStartup/Chapters/CreatingLaunchdJobs.html)
documents `ProgramArguments`, `KeepAlive`, standard-output paths and foreground
process expectations. Apple’s upstream [`launchd.plist(5)` manual](https://github.com/apple-oss-distributions/launchd/blob/main/man/launchd.plist.5)
defines `SuccessfulExit=false` as restarting when the process does not exit
successfully.

These definitions do not allocate a TTY or implement terminal handoff; use
noninteractive applets. The generated systemd cgroup and the runtime's process
supervision handle ordinary child processes, but a deliberately daemonized or
escaped descendant is not guaranteed to stop with the service. Apple's guide
requires launchd-managed programs to remain in the foreground; `KeepAlive`
supervises the job's main process and does not promise cleanup of detached
children. This is service startup/restart integration, not interactive-session
or universal descendant management.

## Terminal apps with control flow you can read

A screen defines its layout first. Components declare their widgets, input/output
ports, and event handlers in the same file. Rust renders the widgets; `.program`
decides when commands run and where results go. Screens need the terminal UI, which is
in the `full` build (`cargo build --locked --release --features full`) and in release
packages, but not in the lean default build.

For example, this complete CPU display uses actual `ps` output and the shipped
`jq.ps` adapter, which invokes `jq`:

```text
open a screen titled "CPU usage"
place monitor on the left

define a program named monitor
  show a bar chart named usage
    label each bar from "label"
    size each bar from "cpu"
    label bar values with "%"
  show a button named refresh labelled "Refresh"

  whenever the program opens
    press refresh

  every 2 seconds
    press refresh

  whenever refresh is pressed
    run ps with flags ["-A", "-o", "pid=", "-o", "pcpu=", "-o", "comm="]
    pipe to jq.ps with limit 10
    then show the result as bars in usage

  whenever key "Ctrl-Q" is pressed
    close the screen
```

From the project directory, try the fuller applications:

```sh
execute examples/top-cpu.program
execute examples/github-stars.program
```

The [CPU monitor](../docs/top-cpu.md) connects a bar chart to a separate process table.
The [GitHub star tracker](../docs/github-stars.md) connects a repository chooser to
live stars/forks, description, and a session history graph. Its requests use `gh`.
Ctrl-R refreshes; Ctrl-Q quits.

Nine widget types are implemented: headings, text fields, lists, tables, text
panels, badges, buttons, line charts, and bar charts. Current layouts place one or
two components. The [UI catalog](../docs/tui-catalog.md) distinguishes the broader
design inventory from what is implemented today.

## Inspect and edit a running app

Agents can inspect and change **actual runtime and widget state** through a local
Unix socket. This needs the `live` feature, part of the `full` build and of release
packages. Start a managed instance from the project directory:

```sh
execute live serve examples/live/widget-console.program --socket /tmp/execute-console.sock --paused
```

In another terminal, with `execute` on its PATH:

```sh
execute live schema
execute live read /ui --socket /tmp/execute-console.sock
execute live update /ui/programs/console/widgets/message/text --json '"hello from an agent"' --socket /tmp/execute-console.sock
execute live event --json '{"type":"submit","program":"console","widget":"message"}' --socket /tmp/execute-console.sock
execute live control resume --socket /tmp/execute-console.sock
```

The input visibly changes; the declared submit handler runs real `printf` with
that text. Stop it with `execute live control stop --socket /tmp/execute-console.sock`.
Add `--headless` to `live serve` for a persistent UI without a terminal.

The CLI supports create/read/update/delete, atomic JSON patches, revision checks,
bounded waits, UI events, and pause/step/cancel/retry/restart controls. Editable
state includes bindings, pipeline input, queued records, cwd, in-memory source,
and widget fields. Changes take effect at execution boundaries; derived runtime
observations remain read-only. Every client command returns one JSON response.

Managed execution is opt-in at startup. Live state is ephemeral; durable recovery
uses checkpoints. See the [live-control API](../docs/live-control.md) for field paths,
concurrency, access control, and accepted-versus-rendered revisions.

## Cache work. Repair failures. Resume progress.

```text
read "service-names.txt"
then tr with from "[:upper:]" and to "[:lower:]" and cache
then save to "service-names-normalized.txt"
```

`and cache` reuses a pure transformation across fresh runs. Keys include exact
input, arguments, manifest, executable identity, cwd, and environment. External
files and network state are not tracked automatically: cacheability is a manifest
author's contract. Cache publication keeps at most 128 entries and 64 MiB of
cached output and checksum data by evicting older entries. An output larger than
the byte budget is returned but not retained.

Checkpoints serve a different purpose: record completed nodes so a repaired
program can continue after failure.

```sh
execute repair.program --checkpoint .execute-state/repair --trace
# Edit the failed node or remaining suffix, then:
execute resume repair.program --checkpoint .execute-state/repair --trace
```

The [repair example](../examples/repair.program) deliberately fails after a cached
transformation. Copy it into a scratch folder to try this. Resume restores saved
output, named values, and cwd, and skips the completed prefix. A native pipe group
is one checkpoint node. Ordinary runs perform no checkpoint I/O.

A top-level `for each` or `parallel for each` records each element, so resume
continues after the committed elements rather than rerunning the loop. A comment added
above a completed node, or an edit below the last completed node, does not force a
rerun, because resume matches completed nodes by step id. Whether a step may run again
without `--retry-uncertain` follows its declared effects: a pure or read-only in-process
applet may, a native command may not. `execute checkpoint status DIR` shows what a
resume would do without running anything.

Checkpoints do not make external effects transactional. An interrupted command
with an uncertain outcome requires explicit `--retry-uncertain`; completed nodes
cannot be rewritten in place. See [recovery contracts](../docs/language.md#recover-after-a-failure),
[per-element checkpoints](language.md#per-element-checkpoints) and
[retry safety](language.md#retry-safety).

## Faster repeated runs with the warm daemon

If you run many short programs that load a large manifest directory or policies, start
a per-user daemon once and forward runs to it:

```sh
execute daemon serve &          # leave it running; `execute daemon stop` ends it
execute run report.program --daemon
EXECUTE_DAEMON=1 execute run report.program    # the same, from the environment
```

When no daemon answers, the program simply runs in the client. The forwarded run sees
your working directory, environment, umask, resource limits and standard streams. A
program that may take the terminal (`interactively`, `hand over`, a prompted input) is
declined and runs in the client. For a one-line program the daemon is level with a
cold run; it saves manifest loading and policy setup, so measure your own program
first. Unix only. The details, measured latencies and socket checks are in the
[language reference](language.md#warm-daemon---daemon).

## Bring another CLI

An applet manifest maps readable parameter names to an executable's argument
vector. No shell evaluation is involved. This is a complete minimal adapter:

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

`then tr with from "a-z" and to "A-Z"` becomes `argv = ["tr", "a-z", "A-Z"]`.
An adapter may choose a subcommand, add fixed arguments, and spread ordered lists.
For example, [`git.commit`](../manifests/git.commit.json) binds a message and optional
file list to `git commit -m …`. The [shipped catalog](../manifests) supplies the
ordinary Unix tools and specialized `gh.repo` and `jq.ps` adapters. Quoted direct calls work without a manifest. Named parameter adapters use a
manifest; neither requires a new Rust implementation.

## Testing programs

`execute test [--json] [--manifest-dir DIR] [PATH...]` runs every `*.test.program`
under each PATH. With no PATH it uses `tests/` when that directory exists, and
otherwise the current directory. Each test runs in its own `execute` process.

```text
tests/
  login.test.program       the program under test
  login.test.json          optional sidecar: inputs and expectations
  fixtures/
    bin/                   fake tools, first on PATH for this test
```

The sidecar sits beside the program and has the same stem. Unknown fields are
rejected. Every field is optional.

```json
{
  "inputs": { "name": "world" },
  "arguments": [],
  "expect": { "status": 0, "stdout": "hello world", "files": { "out.txt": "hi" } }
}
```

- `inputs` become `--input NAME=VALUE` arguments.
- `arguments` are passed after `--`.
- `expect.status` defaults to 0.
- `expect.stdout` is an exact byte comparison.
- `expect.files` maps paths relative to the test's working directory to their exact
  contents.

Without a sidecar, a test expects exit status 0. Each test runs in a fresh working
directory created with random name and mode 0700 on Unix. The directory is removed
afterwards, without following symlinks. The `fixtures/bin` directory beside the
test is prepended to `PATH`, and `EXECUTE_TEST_FIXTURES` names the `fixtures`
directory.

Fake tools are ordinary executables. On Unix they can be shell scripts. On Windows,
process creation finds only `.exe` files on `PATH`, so fake tools there must be
compiled `.exe` files; shell-script fixtures are Unix-only.

The default output is TAP 13, one line per test with a YAML block for failures. With
`--json`, the output is one document with protocol `execute.test/1`, summary counts,
and a `tests` array giving each name and `pass` or `fail`. The exit status is 0 only
when every test passes. A run that finds no tests is an error.

## Running a directory of programs

`execute run-parts DIR [--once STATE_DIR] [--continue] [--json] [--manifest-dir DIR]`
runs each `*.program` directly inside DIR, in lexical byte order, as its own
`execute` process. Hidden files and subdirectories are skipped.

The run stops at the first failure. `--continue` runs the remaining programs
anyway, and the programs after a failure are reported as `not_run` when it is not
given.

`--once STATE_DIR` records each completed program's name and SHA-256 content hash
in `STATE_DIR/state.json`. The state file is replaced atomically. A later run skips
any program whose hash is unchanged, reporting it as `skipped`. A program whose
content has changed since it completed is reported as `changed` and is not run.
To run it again, remove its entry from `state.json`.

Each program's status is one of `ran`, `skipped`, `changed`, `failed`, or `not_run`.
The exit status is 1 when any program fails, and otherwise 0. A `changed` program
does not count as a failure.

With `--json`, one document is printed with protocol `execute.run-parts/1`, summary
counts for each status, and a `results` array giving each program's name, status,
hash, and exit code. The programs' own output then goes to standard error, so
standard output stays parseable.

## Record a run, show what failed, and replay it

`execute run FILE --record DIR` keeps a record of one finite run in a new or empty
directory. The directory is private (mode 0700 on Unix, files 0600). The record has
`run.json` (program and plan hashes, redacted inputs, result, limits), `program.program`
(a copy of the program as it ran), `trace.jsonl` (one line per applet group, with
argv, status, declared outcome, wall time, and stdout and stderr byte counts and
hashes), and `blobs/` (the captured bytes, content-addressed). Records are bounded:
at most 16 MiB of captured output, after which steps keep their hashes but not their
bytes and the record is marked `truncated`.

Values that the data-flow policy declares secret (`--data-flow-policy`) are redacted
from every record file, from `show-run`, and from replay output. Redaction is by exact
value, so a secret the policy does not declare is recorded as it was. Recording
refuses programs with triggers, background jobs (`spawn`), and cached steps
(`and cache`), and it cannot be combined with checkpoints, since their steps cannot
all be observed.

```sh
execute run deploy.program --record .runs/deploy
execute show-run .runs/deploy            # failed step, outcome, certainty, slowest steps
execute show-run .runs/deploy --json
```

`execute replay DIR` reruns the recorded program with each applet group answered from
the record. It launches no process and writes no file, and it reports
`N steps replayed, no divergence, 0 processes launched`. Replay compares each group's
line, applet, binary and argv with the record. An edited program stops at the first
group that differs:

```sh
execute replay .runs/deploy --program deploy-edited.program
execute replay .runs/deploy --until 40          # stop before line 41
execute replay .runs/deploy --as-test tests/deploy-recorded   # then: execute test tests
```

`--as-test DIR` writes `NAME.test.program` and `NAME.test.json` with the recorded
inputs, exit status, and terminal stdout. The fixture runs the real program, so it
needs the tools the program calls.

## Try statements in a session

`execute repl` reads statements from standard input, one statement at a time. An
indented body, or a statement that opens one (`if`, `for`, `repeat`, `every`, ...),
continues until a blank line. Each block is validated against the whole session, with
the same checks as a file. A block that fails validation is reported with `rejected:`
and dropped, and the session continues. A block that runs and then fails is reported
with `error:`, and its statements are dropped too.

Statements before a new block are not run again: the session answers them from its
own record, so bindings carry over and earlier file writes and printed output are not
repeated. Commands:

- `:explain` shows the plan of the session program.
- `:bindings` lists top-level `as NAME` bindings with the start of their last value.
- `:save FILE` writes the session program to a new file. An existing file is never
  replaced.
- `:quit` ends the session.

```sh
printf 'run printf with message "a" as first\nrun printf with message first\n:save session.program\n:quit\n' | execute repl
```
