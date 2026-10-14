# Execute

**English for the glue. Unix for the work.**

Execute turns existing command-line tools into readable programs, live services,
and terminal apps. Write the workflow in a `.program` file; run it with `execute`.

```text
run "cat" with arguments ["contacts.txt"]
pipe to "tr" with arguments ["[:upper:]", "[:lower:]"]
pipe to "sort"
pipe to "uniq"
then save to "contacts-clean.txt"
```

Those are real `cat`, `tr`, `sort`, and `uniq` processes, connected by native Unix
pipes. A small Rust interpreter handles orchestration, supervision, caching, and
terminal widgets. **The `.program` file owns application control flow.**

The goal is a small language that people can read and agents can compose: reuse
the tools that already do the work, and make the connections explicit. The syntax
is deterministic; running a program requires no LLM, API key, or model inference.

**0.8.0 · macOS + Linux · Rust · [MIT](LICENSE)**

[Quickstart](#quickstart) · [Language](#the-language-in-one-minute) ·
[Live services](#one-file-several-lifetimes) · [Terminal apps](#terminal-apps-with-control-flow-you-can-read) ·
[Agent control](#inspect-and-edit-a-running-app) · [Performance](#performance-with-receipts) ·
[Documentation](#documentation)

## Quickstart

From the project directory, build with a current stable Rust toolchain:

```sh
cargo build --locked --release
export PATH="$PWD/target/release:$PATH"
execute --version
```

The package also includes `bin/execute`, built for Apple Silicon macOS. On that
platform, use `export PATH="$PWD/bin:$PATH"` to try it without building. Linux and
other architectures build from source. Individual programs require their native
tools; the example below uses standard macOS/Linux utilities. JSON utilities and
the CPU monitor additionally need `jq`; the GitHub tracker needs authenticated `gh`.

Create and run a useful program in a fresh workspace:

```sh
workspace=$(mktemp -d)
cd "$workspace"
printf '%s\n' 'SAM@example.com' 'lee@example.com' 'sam@example.com' > contacts.txt

cat > clean-contacts.program <<'PROGRAM'
run "cat" with arguments ["contacts.txt"]
pipe to "tr" with arguments ["[:upper:]", "[:lower:]"]
pipe to "sort"
pipe to "uniq"
then save to "contacts-clean.txt"
PROGRAM

execute check clean-contacts.program
execute explain clean-contacts.program
execute clean-contacts.program
```

Both stdout and `contacts-clean.txt` contain:

```text
lee@example.com
sam@example.com
```

`check` validates syntax and bindings. `explain` also shows the execution plan and
native arguments. Neither runs the applets. Relative paths resolve from your
**calling directory**, including when the script lives elsewhere.

The current source candidate also adds [`execute inspect FILE`](docs/language.md#agent-interface-discovery)
for JSON input types, defaults, missing values, and TUI component ports. It runs
no applets. It is not yet included in the installed 0.8.0 release.

The source candidate also supports [`read from stdin`](docs/language.md#finite-stdin-and-unix-composition)
for finite Unix pipeline stages. Try `examples/unique-lines.program` or
`examples/checksum-stdin.program` with the candidate build; this syntax is pending
release integration too.

Local file URIs work too: `execute "file:///absolute/path/clean-contacts.program"`.
Quoted executable calls need no applet manifests: `run "git" with arguments
["status", "--short"]` launches `git` directly. Use a quoted relative or absolute
path for your own tools. The argument list is literal; no shell is launched.

Unquoted names select named applets, whose manifests validate parameters and can
declare cacheability. For those calls, keep `bin/execute` and `manifests/` together
under one directory, or select a catalog with `--manifest-dir PATH`.

Editor support is available in the [VS Code extension](editors/vscode/README.md):
syntax highlighting, folding, comment/bracket support, and snippets for `.program`
files. Install its VSIX through **Extensions: Install from VSIX…**.

## The language in one minute

| Syntax | Meaning |
| --- | --- |
| `run pwd` | Start an independent command with closed stdin. |
| `run "git" with arguments ["status", "--short"]` | Launch any executable directly with exact argv, without a manifest. |
| `then tr with from "a-z" and to "A-Z"` | Wait for success, then pass finite output to the next command. |
| `pipe to wc with flags ["-l"]` | Connect adjacent native commands with an OS pipe. |
| `read "input.txt"` | Read a file without starting `cat`. |
| `as report` | Bind finite UTF-8 output to a name. |
| `and keep status as code` | Bind a native exit code as an integer and continue after ordinary nonzero exits (source candidate). |
| `if code is 0` / `otherwise` | Choose an indented native command body using scalar equality (source candidate). |
| `and cache` | Reuse the result of a declared cacheable transformation. |
| `then save to "report.txt"` | Atomically replace a file with the current output. |
| `run cd with path "reports"` | Change the program's working directory. |

One statement per line. Use double-quoted JSON strings, `#` comments, named
parameters, and consistently indented event bodies. Strings, integers, booleans,
references, and lists are supported. The pending source candidate also provides
[JSON parsing, typed field selection and encoding](docs/language.md#structured-json-pending-source-candidate),
with JSON inputs and editable structured bindings. Broader computation stays in tools such as `jq`,
`awk`, and `sort`.

The source candidate also supports [same-file reusable finite programs](docs/language.md#reusable-finite-programs-pending-source-candidate):
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
expand automatically. See the [language reference](docs/language.md) for exact
value, quoting, error, and output contracts.

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
Text, integers, booleans, and lists of text are supported. Missing required inputs,
bad types, unknown names, and known manifest constraint violations fail before
commands run. `check` and `explain` can inspect a signature without supplying
required values. See the [input reference](docs/language.md#program-inputs).

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
[streaming](docs/streaming.md) for an actual file-following example, binary record
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

Intervals first run after one full interval and skip missed ticks. Calendar apps
match five-field cron expressions in the local timezone. Both run in the
foreground; Ctrl-C stops them. Use launchd or systemd for startup and restart
policies. Execute does not install operating-system scheduler entries.

## Terminal apps with control flow you can read

A screen defines its layout first. Components declare their widgets, input/output
ports, and event handlers in the same file. Rust renders the widgets; `.program`
decides when commands run and where results go.

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

The [CPU monitor](docs/top-cpu.md) connects a bar chart to a separate process table.
The [GitHub star tracker](docs/github-stars.md) connects a repository chooser to
live stars/forks, description, and a session history graph. Its requests use `gh`.
Ctrl-R refreshes; Ctrl-Q quits.

Nine widget types are implemented: headings, text fields, lists, tables, text
panels, badges, buttons, line charts, and bar charts. Current layouts place one or
two components. The [UI catalog](docs/tui-catalog.md) distinguishes the broader
design inventory from what is implemented today.

## Inspect and edit a running app

Agents can inspect and change **actual runtime and widget state** through a local
Unix socket. Start a managed instance from the project directory:

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
uses checkpoints. See the [live-control API](docs/live-control.md) for field paths,
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
author's contract. Cache eviction is not implemented.

Checkpoints serve a different purpose: record completed nodes so a repaired
program can continue after failure.

```sh
execute repair.program --checkpoint .execute-state/repair --trace
# Edit the failed node or remaining suffix, then:
execute resume repair.program --checkpoint .execute-state/repair --trace
```

The [repair example](examples/repair.program) deliberately fails after a cached
transformation. Copy it into a scratch folder to try this. Resume restores saved
output, named values, and cwd, and skips the completed prefix. A native pipe group
is one checkpoint node. Ordinary runs perform no checkpoint I/O.

Checkpoints do not make external effects transactional. An interrupted command
with an uncertain outcome requires explicit `--retry-uncertain`; completed nodes
cannot be rewritten in place. See [recovery contracts](docs/language.md#recover-after-a-failure).

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
For example, [`git.commit`](manifests/git.commit.json) binds a message and optional
file list to `git commit -m …`. The [shipped catalog](manifests) supplies the
ordinary Unix tools and specialized `gh.repo` and `jq.ps` adapters. New tools
require a manifest; they do not require a new Rust implementation.

## 150 useful programs, with inputs and outputs

The [utility collection](examples/ladder/README.md) covers file operations, log
triage, CSV/JSON reports, configuration checks, CI failures, release preparation,
reconciliation, and live event processing. Each utility declares its inputs and
outputs and has exact fixture expectations.

From the project directory:

```sh
python3 examples/ladder/run.py --list
python3 examples/ladder/run.py --case 76
python3 examples/ladder/run.py --case 76 --prepare /tmp/failed-test-report
cd /tmp/failed-test-report
execute 076-ci-failed-test-summary.program
```

The export destination must be new or empty. Replace the prepared CI JSON with
your own data; `.utility/README.md` describes its contract. Python prepares and
verifies fixtures; the `.program` performs the work. Most cases have alternate
inputs too. CSV cases specify unquoted delimiter-separated records, rather than
claiming a general CSV parser.

## Performance, with receipts

There is no JIT or LLVM compilation stage. Execute parses once and reuses the
program for services. Native pipe payloads flow between applets without being
interpreted byte by byte. Process startup, native buffering, and finite capture
boundaries still matter; persistent streams avoid per-record process launches.

The 0.8.0 input benchmark's 240-sample repeat measured a supplied-path checksum at
**5.44 ms** median (0.7.0 literal path: 5.45 ms) and a supplied
integer/file preview at **5.24 ms** (old literals: 5.29 ms), including fresh
Execute startup. Native argv is verified through `explain` before timing.
These are comparable measured workloads, not a speedup claim.
[Raw input measurements](benchmarks/program-inputs-v0.8.0-macos-arm64-repeat.json)
include paired samples and limitations; the benchmark guide retains the first run's tail variability.

The earlier 0.7.0 paired native-call benchmark measured a direct `printf` at **5.27 ms**
median, a three-stage pipeline at **9.97 ms**, and a 2 MiB binary pipeline at
**7.64 ms**, including fresh Execute startup. Measured medians were comparable
to 0.6.1 named calls; this is a capability improvement, not a demonstrated speedup.
[Raw reports and methodology](benchmarks/README.md#direct-executable-calls-070)
retain both runs and their tail variability.

Earlier **0.6.0 macOS arm64** control-plane measurements, in milliseconds:

| Operation | Samples | p50 | p95 |
| --- | ---: | ---: | ---: |
| Local socket state read | 200 | 0.075 | 0.132 |
| Local socket binding update | 200 | 0.087 | 0.170 |
| Fresh CLI state read, including startup | 20 | 3.530 | 3.865 |
| Widget edit through applied headless render | 50 | 0.207 | 0.228 |

These are small-payload measurements with warm filesystem caches. The server was
paused with no native jobs; the widget measurement includes polling for the
applied revision. It does not measure physical terminal display latency or
establish hard real-time guarantees. Full methodology, raw samples, binary hashes,
and historical Bash comparisons are in the [benchmark guide](benchmarks/README.md).
The historical comparisons show tradeoffs, not a universal speedup over Bash.

Reproduce the live-control measurements from the project directory:

```sh
python3 benchmarks/live_control.py --binary target/release/execute --report /tmp/execute-latency.json
```

## Scope and current limits

Execute is a focused orchestration language for trusted Unix workflows. It is
not yet a general Bash replacement. There are no general-purpose loops or
conditionals, arbitrary parallel job syntax, or reusable imported/nested programs.
Terminal components currently live in one file.

Applets run with your permissions; process supervision is not sandboxing. Native
pipeline failures stop ordinary execution and produce CLI status 1 with the child
status in diagnostics. Finite captures default to 16 MiB; listener/stream records
default to 64 KiB. Timeouts and capture limits are configurable. Durable recovery
does not capture an unread source queue or guarantee exactly-once external effects.

See the [Unix compatibility review](docs/unix-compatibility-review.md) for the
implemented contracts, known gaps, and acceptance criteria.

## Build and verify

From the project directory, with current stable Rust, Python 3, and `jq` available:

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo build --locked --release
```

Recorded 0.8.0 validation on macOS arm64: **262 regular Rust tests**, including
38 live CLI/socket/TUI scenarios, plus 150 utilities, 146 alternate-input checks,
one invalid-input guard, and nine failure/recovery fixtures. All 14 opt-in probes also passed, including the isolated signal
compatibility check. These counts describe overlapping suites, not
independent totals. [Current validation evidence](docs/validation-v0.8.0.json)
includes source/binary hashes and paired latency reports. The preceding
[0.6.1 full audit](docs/full-validation-v0.6.1.json) retains coverage and longer
lifecycle-stress evidence; those measurements were not repeated for 0.7.0.

Tests use owned temporary workspaces and bounded subprocesses. The calendar case
can wait up to 75 seconds for a real minute. macOS and Ubuntu CI are configured;
native Linux runtime validation has not been performed on this development host.
Linux-target Clippy has passed.

For contributions, start with the relevant runtime contract, add a regression for
the behavior you change, and run the checks above. Performance changes should
include reproducible measurements with output correctness checked separately.

## Documentation

| Guide | What it covers |
| --- | --- |
| [Thesis and progress](docs/thesis-progress.md) | Full project objective, implemented capabilities, and unfinished requirements |
| [VS Code extension](editors/vscode/README.md) | Installable highlighting, editor support, snippets, and grammar tests |
| [Language and runtime](docs/language.md) | Syntax, values, pipes, manifests, scheduling, limits, cache, and recovery |
| [Persistent streams](docs/streaming.md) | Live native pipelines, framing, buffering, and cleanup |
| [Live control](docs/live-control.md) | Runtime/widget CRUD, JSON protocol, revisions, and lifecycle controls |
| [Live examples](examples/live/README.md) | An editable queue monitor and operator console |
| [150 utilities](examples/ladder/README.md) | Searchable programs, data contracts, fixtures, and exports |
| [CPU monitor](docs/top-cpu.md) / [GitHub tracker](docs/github-stars.md) | Complete single-file terminal apps |
| [UI element catalog](docs/tui-catalog.md) | Widget design inventory and implementation status |
| [Architecture](DESIGN.md) | Interpreter design and implementation decisions |
| [Benchmarks](benchmarks/README.md) | Methods, raw results, comparisons, and reproducibility |
| [Full testing report](docs/full-testing.md) | Adversarial checks, fixes, source coverage, raw logs, and remaining limits |
| [Replacement challenge](docs/replacement-challenge.md) | Fifteen proof gates, current evidence, stress runner, and AI evaluation protocol |
| [Unix compatibility](docs/unix-compatibility-review.md) | Bash-replacement potential and remaining gaps |
| [Adversarial refinement](docs/refinement.md) | Reproduced issues, fixes, and evidence gates |

Licensed under the [MIT License](LICENSE).

The pending source candidate also supports `execute check FILE --json`: one
versioned static result for validation/repair tools, including the complete error
chain and lossless Unix path bytes. Ordinary check output stays unchanged. See
[the diagnostic protocol](docs/language.md#machine-readable-checks-pending-source-candidate)
for its first-error/static-only scope. Installed 0.8.0 does not include this option.
