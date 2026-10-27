# Runtime reference

This companion documents operational interfaces and runtime lifecycle behavior.

## Finite stdin and Unix composition

Finite programs accept explicit stdin input. For example:

```text
read from stdin
pipe to "sort" with arguments ["-u"]
```

From the project directory, run
`printf 'b\na\nb\n' | target/release/execute examples/unique-lines.program`.
The result is `a` and `b` on separate lines. One `.program` can feed another
through an ordinary Unix pipe. Standard output retains the finite result contract.

Without a named binding, stdin passes directly to the next consuming native
stage through its inherited descriptor. Large input does not need to fit the
finite output limit when the native result is small; `checksum-stdin.program`
uses this to checksum a binary stream. A terminal capture, save, cached call,
checkpoint boundary, or `read from stdin as name` materializes bytes subject to
`--max-output-bytes`. Named results require UTF-8; unnamed bytes remain exact.
Uncaptured stdout is a separate rule, described under
[Uncaptured stdout](#uncaptured-stdout).

Finite stdin captures poll for input, observe cancellation and
`--command-timeout-ms`, and preserve the inherited descriptor's flags. Native
stages retain their existing pipeline supervision and timeout. Neither validation
nor inspection reads stdin. A completed checkpoint restores captured bytes;
resuming an interrupted read requires `--retry-uncertain`, since consumed input
cannot be rewound. The caller must supply appropriate replacement input when
explicitly retrying.

### Uncaptured stdout

A top-level `run` from the CLI's `run` or `resume` command writes its stdout to
the terminal as it runs when nothing consumes that stdout. Consumers are a
binding (`as`), `keep result`, `keep status`, `cache`, a following `pipe to`, or
any reachable later statement that reads the pipeline value before it is
replaced. Consumed output stays buffered. A step whose result is bound never
becomes implicit program stdout, including file reads, JSON selections, byte
captures, and task results. The end-of-run print applies only to an unbound
final value. Buffered bytes remain available to subsequent pipeline consumers.

A `run` is treated as unconsumed when its continuations replace the value or
end the program without reading it. This includes block boundaries when no
reachable continuation reads the bytes. Bound reusable-program returns and exceptional
transfers conservatively retain capture. Native commands inherit
the terminal stdout. Other applets write their captured bytes after success. A
failing step has already written any inherited output; its error goes to stderr.

Data-flow policy governs remote and persistence sinks, not the terminal, so
streamed bytes reach the caller's stdout without a policy check. Only the CLI's
top-level finite run streams. Library runs, program modules, TUI
handlers, services, and listeners keep capture. Streamed output is not bounded by
`--max-output-bytes`, because it is not captured. A checkpointed run streams the
same way, but resume does not replay committed output: a completed run resumes
with empty output, including when its final value was explicitly emitted.
An unbound `await` emits its job's stdout when no continuation consumes it;
`await ... keep status/result as ...` withholds it until explicitly reused.

Migration: replace literal assignments such as `run "printf" with arguments
["CLEAR"] as state` with `use value "CLEAR" as state`. Programs that relied on a
final bound value being printed must explicitly reuse it with unbound `use value`
(text) or `then encode ... as json` (structured values). `check` warns about a
final bound value that the old rule would have printed; `explain` marks stdout
as streamed, a possible unbound final value, or absent. Empty commands used only
to hide internal final values can be removed. A resumed checkpoint withholds the
restored final value until an unbound producing step executes. Library callers,
services and listeners also receive only unbound final output in their run result;
buffered bytes remain available during execution and in live pipeline state.

The syntax is distinct from `read stdin`, which still refers to a binding named
`stdin`. Services use `listen to stdin` for UTF-8 records, managed programs use
their pipeline state/inbox, and TUIs use component ports/widgets. These execution
forms reject `read from stdin` before application effects.

## Program inputs

Declare inputs before any folder directive, command, or trigger:

```text
accept path as text
accept lines as integer with default 20
read path
pipe to "head" with arguments ["-n", lines]
```

Run `execute preview.program --input 'path=server log.txt' --input lines=10`.
Repeat `--input NAME=VALUE` for different names; duplicate and unknown names fail.
The first `=` separates name from value, preserving later `=` characters. Text
values are literal argument text, with no shell expansion by Execute. Shell users
must still quote their own command-line arguments in the calling shell.

Input types are `text`, `integer` (signed 64-bit), `boolean` (`true`/`false`),
`list of text`, `list of integer`, `list of json`, and `json`. List inputs are
supplied as JSON arrays and each item is checked before execution. Integer lists
contain signed 64-bit integers; JSON lists contain JSON values. A text list is supplied as JSON:

```text
accept files as list of text with default ["notes.txt"]
run "cksum" with arguments files
```

Run `execute checksums.program --input 'files=["first file.txt","second.txt"]'`.
`execute report.program --input 'ports=[80,443]'` supplies an integer list.
Empty strings and lists are valid at the input-schema level; an applet manifest
can impose tighter constraints. Defaults must be literal values of the declared
type, including literal text lists. Scalar and text-list inputs reject references,
nested lists, records, null, floating point and NUL bytes. The `json` type retains nested
JSON, numbers and escaped NUL; references remain invalid input values. Names cannot
duplicate other inputs or be shadowed by ordinary result bindings.

Declared values can feed file paths, cwd setup, applet parameters, native argv,
listener sources, handler bodies and stream pipelines. They persist across
otherwise fresh service handler contexts. Native tools still enforce their own
option semantics; for example, BSD `head` rejects a zero line count. No implicit
integer-to-text conversion occurs for an applet's declared string parameter;
direct argv performs the scalar formatting described above.

`check` and `explain` validate any values supplied and any known defaults without
requiring every required input. `explain --input ...` resolves known inputs into
the displayed native argv; values produced by commands remain unresolved. Neither
command probes tools or executes effects. Normal execution requires all inputs
and checks their schema and known manifest constraints before starting commands.
Values that can only be computed by running commands are checked when available.

Checkpoint identity includes effective values and types. Resume must use the same
effective inputs; an explicit value equal to its original default is compatible.
Changes require a fresh checkpoint rather than silently reusing old results.
Programs with no declarations keep their previous checkpoint representation.

### Describe inputs

`described as "TEXT"` gives an input a human-readable description. It may appear
before or after `with default`, and each appears at most once. The description is
reported by `execute inspect` and does not change how the input is read or checked.

```program
accept name as text described as "Who to greet" with default "you"
```

### Allowed values

`one of [...]` after a `text` input limits it to a fixed list of choices:

```program
accept mode as text one of ["light", "dark"] with default "light" described as "Theme"
```

The choices must be nonempty, unique, NUL-free quoted text, at most 256 of them, and
a `with default` value must be one of them. These rules are checked when the program
is read. A supplied value is checked the same way from the command line and from
environment or argument sources, before any step runs. A value outside the list is refused with an error that names the allowed
values, for example `input "mode" must be one of "light", "dark"; got "blue"`.
`execute inspect` lists each input's `choices`, and `execute explain --json` lists the
program's inputs under `inputs` with the same field. `described as` works with
`one of` in either position relative to `with default`. `one of` is not allowed on
`integer`, `boolean`, any list input, or `json` inputs.

### Program location

`from program directory` and `from program path` bind a text input to where the
program file lives, not to where it runs. `program directory` is the canonical
directory of the file, and `program path` is its canonical path. Scripts can read
files shipped beside them without `$(dirname "$0")`:

```program
accept here as text from program directory
accept self as text from program path
```

The value is the input's default, so an explicit `--input` or `with` value still
wins, as with environment inputs. A location input must be `text` and cannot declare
its own `with default`. A called file's location inputs name that file's directory,
not the caller's. Programs run from text with no file path have no location, so the
input is required there.

### Prompted inputs

`from prompt "QUESTION"` declares an input that a person answers on the terminal.
`from secret prompt "QUESTION"` does the same with echo off. A prompt is a declared
input, so `execute inspect` and `execute explain --json` list its question, and an
agent can see every question before the run starts.

```program
accept name as text from prompt "Your name?"
accept token as text from secret prompt "Access token?"
accept mode as text one of ["light", "dark"] from prompt "Theme?" with default "light"
```

An answer is supplied in one of three ways, and the first that applies wins:
`--input NAME=VALUE`, then an answers file given as `--answers FILE` (a JSON object
whose values are text, numbers, or booleans), then the terminal. The answers file
must be a private regular file owned by the current user; symbolic links and
reparse points are refused. On Unix it must grant no group or other access; on
Windows it must have a non-inherited owner-only ACL. A prompt with a
default takes it for an empty answer. A prompt that has no answer and no terminal
fails the run before any step starts, and the error names the question. Set
`EXECUTE_NO_PROMPT=1` to turn terminal prompts off, so an unattended run fails the
same way. Only `run` asks; `check`, `inspect`, `explain`, and service deployment never
prompt.

A secret answer is never printed or recorded. `inspect` reports `value_withheld`
instead of the value, and a run's record keeps a `withheld` marker in place of it. A
checkpoint records a keyed HMAC of the answer under the per-user key, not the answer,
and `--checkpoint` is refused for a run with a secret prompt input.
A replay of a run with a withheld answer is refused, because the answer is needed
again. A secret prompt must be `text` and cannot declare `one of`, since a choice
error would quote the answer. The answer is still on the command line when given
with `--input`, as any `--input` value is, so unattended use should prefer
`--answers`.

### Flags and options

An input can be set from the command line after `--`, by a named flag or option
instead of by position:

```text
accept verbose as boolean from flag "--verbose" alias "-v" described as "Print progress"
accept output as text from option "--output" alias "-o" with default "out.txt"
accept count as integer from option "--count"
```

A `flag` is a `boolean`: it is `true` when present and `false` when absent, unless it
has a default. It takes no value. An `option` is `text` or `integer`, and takes its
value as the next argument or as `--name=VALUE`. Its `alias` is one dash and one letter
or digit. An option without a default must be given.

```text
execute tool.program -- --verbose --output=report.txt --count 3
```

Arguments are split before positional inputs are bound. A flag or option that the
program does not declare is refused before any step runs, and the error lists the
declared spellings. A flag or option given twice, a value given to a flag, or an
option without its value is refused the same way. An undeclared argument that starts
with `--` is an error. An undeclared single-dash argument stays positional, so negative
numbers still work. A program with no flags or options keeps every argument positional.

Names must start with `--` and use lowercase letters, digits and hyphens. The spellings
`--help` and `-h` belong to the generated help. `--help` after `--` prints it for a
program that declares flags or options, even though arguments after `--` are normally
literal. `-h` does the same only for programs that declare flags. `inspect` reports each
flag with its source kind, name and alias. `explain --json` lists the flags in its
`flags` array, with the input each one sets, its type, its default and its description.

### Agent interface discovery

Use `execute inspect FILE [--input NAME=VALUE]`.
one JSON object using protocol `execute.inspect/1`, after the same static syntax,
binding, type and known manifest-constraint validation as `check`. It starts no
applets, services or UI handlers, reads no application input files, and creates
no cache or checkpoint. Missing required inputs are reported rather than failed:

```sh
execute inspect examples/preview-if-present.program
execute inspect examples/preview-if-present.program --input 'path=server log.txt'
```

| Field | Contract |
| --- | --- |
| `protocol` | `execute.inspect/1` identifies this response format. |
| `validation` | `static`; tool existence and runtime results are not verified. |
| `kind` | `finite`, `interval`, `calendar`, `listener`, `stream`, or `screen`. |
| `stdin_mode` | Ordinary CLI input: `bytes` for finite stdin, `utf8_lines` for a stdin listener, `terminal` for a screen, or `unused`. Managed/headless modes have their own input transports. |
| `inputs` | Declarations in source order: `name`, language-spelled `type`, `line`, and `required`. Literal `default` and effective `value` are present only when available. Values use ordinary JSON types. |
| `missing_inputs` | Names with neither a supplied value nor a default. |
| `inputs_complete` | Whether all root CLI inputs have values; this is not a runtime readiness check. |
| `line_limit` | Stream-only field: record limit or JSON null for unlimited records. |
| `title`, `placements`, `components` | Screen-only fields. Components are sorted by name and expose text input/output ports, separate from root CLI inputs. |

`required` describes whether the declaration has a default; supplying a value
does not change that contract. An override changes `value` and retains the
original `default`. The output includes supplied values, so treat it like the
program's configuration. Invalid syntax or supplied values return status 1 with
a stderr diagnostic and no partial JSON. Root inputs cannot be supplied to a
screen. Use `explain` for the command/argv plan and `live schema` for the control
protocol. Inspection is an interface description, not an exhaustive side-effect
or permissions declaration.

Managed programs materialize defaults under `/bindings` at startup. Required
inputs can be supplied through `live serve --bindings` JSON. Live edits retain
the declared types, and deleting a declared binding is rejected atomically. When
replacing source with new input declarations, include their binding values in
the same patch. Existing stream/producer argv remains fixed for that producer's
lifetime; restart it to apply source-argument changes. Screen programs continue
to use component input ports and live widget state; root `--input` is not exposed
for screens in this release.

<a id="machine-readable-checks-pending-source-candidate"></a>

## Machine-readable checks

```sh
execute check report.program --json
execute check report.program --json --input count=20
```

This runs the same static validation as ordinary `check`. It does not launch
applets, check installed tools, read application input, create caches/checkpoints
or require missing declared input values. Use `inspect` to discover the missing
inputs, and ordinary execution to verify native behavior.

A correctly parsed check invocation writes one JSON document to stdout and no
stderr. Exit status is 0 for a valid check and 1 for a failed check:

```json
{
  "protocol": "execute.check/1",
  "validation": "static",
  "ok": false,
  "path": "report.program",
  "path_bytes": [114, 101, 112, 111, 114, 116, 46, 112, 114, 111, 103, 114, 97, 109],
  "diagnostics": [{
    "severity": "error",
    "code": "check_failed",
    "message": "could not parse workflow report.program: line 1: unterminated quoted string",
    "causes": ["could not parse workflow report.program", "line 1", "unterminated quoted string"]
  }]
}
```

The message/causes above illustrate the envelope; exact prose depends on the
failing parser or validator. `path` is a display string; `path_bytes` preserves
the original Unix path even when it is not UTF-8. Success has an empty diagnostic
list. Static analysis can return multiple diagnostics. Each has a stable code,
severity, message, and source span; token precision is used when the token can be
located reliably, otherwise the span covers its line. Byte offsets are zero-based
UTF-8 offsets with an exclusive end; line and column numbers are one-based, with
columns counting Unicode scalar values. Optional fixes contain independent text
edits over the same source. Some validation failures that do not have a source
span use `check_failed` and include their cause chain. The
[JSON schema](check-result.schema.json) defines the response envelope.

Semantic validation failures use specific `check.*` codes with token spans when
the source token can be located. They include duplicate names, invalid scope,
unknown reusable programs and program files, unknown or required program inputs,
range and value-constraint failures, and named parameters passed to direct
executables. Program-name and program-input diagnostics can include suggested
edits. Use `execute check --explain CODE` to print a registered code's meaning,
example, and typical repair. The registry is documented in the
[diagnostic code reference](language-server.md#diagnostic-codes).

Warnings report common mistakes without changing a check's success status.
They include literal shell-like expansion, shell metacharacters in quoted
executable names, unused bindings and inputs, unreachable statements, empty
loops, and model-flow safety issues. Some warnings provide machine-applicable
fixes when the edit is safe in context. Add `# execute-allow: CODE` to a line to
suppress one warning code there. `--deny-warnings` makes warnings fail `check`;
the JSON response retains each diagnostic's warning severity.

`--json` is accepted only by `check`. Malformed CLI arguments, duplicate options,
help/version output and output-write failures retain normal CLI behavior;
errors from a correctly parsed check use this protocol. Human-readable checks
are unchanged.

`execute check --resolve` opts into checking quoted executable names against the
current PATH without starting those commands. Missing executables produce an
`exec.not_found` warning with the PATH used and a suggested correction when a
close executable or catalog applet name is available. Without `--resolve`,
checks remain independent of the host's installed tools. `explain --json`
reports `resolved: false` when a statically named executable was not found.

## Review a plan before it runs

Three options confirm what runs, and they change nothing in the program itself.

**Policy header.** A leading comment block may hold one line
`# execute-policy: allow CATEGORY...` that declares the categories the program intends
to use. The categories are `writes`, `network`, `reads`, `process`, `cwd`,
`environment`, `privileged`, `system` (package installs and service control, which
`process` does not cover) and `undeclared`. `allow none` declares that the program
uses no effects. The header must be in the leading comments, there may be only one,
and an unknown category is an error. `execute explain FILE --json --policy-header`
reports every effect category the program does not declare as a violation. The
header never approves anything. A category passes only when `--allow` approves it
and the header declares it, so a header-only check without `--allow` denies every
effect. The header grants nothing at run time: enforcement stays with the capability
policy.

**What-if and confirm.** `execute run FILE --what-if` prints the plan the run would
execute, as the explain JSON, with its fingerprint (`execute.what_if/2`) and the
settings the fingerprint covers. It runs no program; when sensitive inputs are present it may create the private review key described below. `execute run FILE
--confirm FINGERPRINT` runs only when the current fingerprint equals the one given.
Otherwise it stops before any effect with `plan changed since --what-if`. The
fingerprint is a SHA-256 over the explain plan and these settings: the resolved input
names, types, and values; the working directory; the contents of the
`--capability-policy` and `--data-flow-policy` files; the contents of each `.json`
manifest in the `--manifest-dir` directory, or of the `--manifest-catalog` file; and
the program's version. The bundled manifests are covered by that version only. A
sensitive input (an environment value or a secret prompt answer) is a candidate secret.
The report shows it as
`{"sensitive": true, "hmac_sha256": ...}`, an HMAC-SHA256 under a per-user key, and the
fingerprint covers that HMAC. A changed secret therefore needs a new review, and an
offline guess at a short secret cannot be checked without the key. The key is 32
random bytes, created on first use at `$XDG_STATE_HOME/execute/what-if.key` (or
`~/.local/state/execute/what-if.key`), or `%LOCALAPPDATA%\execute\what-if.key` on
Windows. On Unix it must be a regular file with no group or other permission bits and
exactly 32 bytes. A key that fails these checks is refused, and a review with a
sensitive input then fails rather than falling back to an unkeyed value. On Windows
the file must be owned by the current user and have an explicit, non-inherited DACL
that grants access only to that user, SYSTEM, or local Administrators; otherwise the
key is refused.
A policy file appears as its path and digest. Changing any other covered setting
requires a new `--what-if` review. Both options apply to finite programs and to `run`
only, and `--what-if` takes precedence when both are given.

**Single instance.** `execute run FILE --single-instance LOCK` takes an exclusive
lock on the file LOCK without waiting. A second run that names the same lock fails
at once with `another instance holds the single-instance lock`. The lock is held
for the whole run, including graph, screen and stream programs, and the lock file
is created if it is missing and is never written.

## Attach to a live program

`execute live serve FILE --socket PATH` runs a program that accepts control. From
another shell, `execute attach PATH` prints one JSON object (`execute.attach/1`)
with the current revision and runtime state. `execute attach PATH --control ACTION`
reads that revision first and then sends the action with it, so an instance that
changed in between refuses the action and applies nothing. The actions are `pause`,
`resume`, `step`, `cancel`, `retry`, `reset`, `restart`, `restart-source` and `stop`.

`execute run FILE --control` runs the program under a live control endpoint instead
of a plain run. It prints `execute: control endpoint PATH` on standard error, and the
endpoint path is fresh for each run and removed when the run ends. Output streams as
a plain run's does, and the process exits with the program's status. `--control`
cannot be combined with `--checkpoint`, `--trace`, `--record`, `--what-if`, `--confirm`,
`--once`, `--headless`, `--allow`, `--policy-header`, `--retry-uncertain`,
`--total-timeout-ms`, `--max-runs`, `--missed-ticks`, `--max-catch-up`,
`--calendar-dedup`, `--max-record-bytes`, positional program arguments, or a
triggered program. Each of these is refused by name.

## Listener and schedule semantics

There is one trigger per program. Initial `run cd` or `use folder` statements may
precede it. Setup executes once before the producer or scheduler starts. Each
handler starts with that configured directory and fresh result bindings; a `cd`
inside the body applies within that invocation.

Line handlers run sequentially. A slow handler backpressures the producer through
its pipe, rather than building an unlimited event queue. The bound record and
implicit input preserve the newline; a final unterminated record is handled at
EOF. Records must be UTF-8. `--max-record-bytes` limits each record, including its
newline; default 64 KiB. Each handler emits and flushes its terminal output before
waiting for the next record or the producer's exit. Stdin EOF ends the listener.
An applet source is kept alive until it finishes, is cancelled, or a run limit is
reached. Its stdout must flush records if low first-event latency matters.

Intervals accept positive integer milliseconds, seconds, minutes, or hours. The
first tick occurs after one full interval. Deadlines keep their original monotonic
phase; slow jobs skip missed ticks and never overlap.

Managed intervals use the same phase calculation and wait until their deadlines
rather than sleeping in fixed 20 ms chunks.
Managed normal ticks retain the initial full-interval delay; diagnostic `step`
remains an explicit immediate tick. OS scheduling and native runtime costs still
limit actual latency.

Calendar schedules use five numeric fields: minute, hour, day of month, month,
day of week. Supported forms are `*`, lists, ranges, and `/steps`. Weekday 0 or 7
means Sunday. Restricted day-of-month and weekday fields use cron OR semantics.
Calendar matching uses the machine's local timezone and starts at the next minute.
Missed minutes are skipped. Repeated local minutes during a DST fall-back can
match twice; skipped local minutes do not run. Names, `@daily`, and seconds fields
are unsupported.

Unhandled failures stop the app and return nonzero; explicit native status capture
continues after ordinary exit codes. There is no automatic retry or restart.
Ctrl-C, SIGTERM, and SIGQUIT cancel execution and stop owned process groups. An
applet must stay alive until its descendants finish; remaining descendants in its
group are stopped when it exits. Deliberately escaped process groups are outside
that cleanup contract. Binaries run with the caller's privileges.

## Process input sources

Programs can declare explicit environment and positional argument inputs:

```text
#!/usr/bin/env -S execute --
accept root as text from environment "OMARCHY_PATH" with default "/usr/share/omarchy"
accept count as integer from argument 1 with default 10
accept arguments as list of text from arguments
run "printf" with arguments ["%s|%s\n", root, count]
```

Run `execute tool.program -- 20`, or make the file executable and invoke it
directly with `./tool.program 20`. The shebang's `--` separates interpreter
options from the workflow filename and its argv. Without that shebang, put
runtime options before `--`: `execute tool.program --input count=30 -- 20`.
An explicit named input overrides its environment/argument source; a source
overrides the literal default. Positional numbers start at 1. Extra arguments
are rejected unless a `from arguments` list is declared. That list preserves
empty values, option-looking strings and literal argv boundaries; it is empty
when no arguments are supplied and cannot have a default. No shell expansion
or interpolation occurs.

Environment names are literal ASCII identifiers. An unset variable uses the
default; an empty value remains empty and must satisfy the declared type.
Integers, booleans, JSON and text lists use the same parsing as `--input`.
Invalid UTF-8 or invalid typed input fails before commands run, with no value
echoed in the error. `check`, `explain` and `inspect` do not read the process
environment; inspection describes its source without exposing its value.
Runtime values can appear in argv, live state or plaintext checkpoints, so only
read the environment values that the program is intended to handle.

Resolved values become ordinary declared inputs: checkpoints freeze them and
reject incompatible resume values; live type constraints remain unchanged.
Reusable finite program inputs remain explicit caller arguments. Read process
inputs at the top level and pass them into helpers. General environment
assignment/unset, shell sourcing and automatic string interpolation remain open.

Unhandled native command failures now preserve their exit code through the CLI;
a child terminated by signal N yields status 128+N. `and keep status as code`
handles an ordinary nonzero exit and continues, so the CLI can still finish
successfully. Syntax, validation, spawn/I/O, timeout and interpreter-cancellation
errors retain status 1. Native failure diagnostics and failed finite-output
discard remain explicit differences from Bash; pipeline failure policy is
stricter than Bash without `pipefail`.

## CLI and manifests

Source builds enable `graph`, `screens`, `records` and `tooling` by default. Use
`--no-default-features` for the core interpreter; rebuild with `--features graph`,
`screens`, `records` or `tooling` to add the corresponding optional programs and
commands. `tui` includes `screens` and adds interactive terminal support.

```sh
execute app.program
execute check app.program
execute explain app.program
execute app.program --trace --cache-dir ./cache --max-output-bytes 16777216
execute listener.program --max-runs 10 --max-record-bytes 65536
```

Explicit `execute run app.program` also works. `check` and `explain` validate the
whole program without starting producers, running commands, or writing files.
`explain` shows binary/argv bindings and trigger semantics. CLI arguments must be UTF-8; percent-encoded file URIs can represent workflow
filename bytes where the filesystem permits them. Invalid encodings and closed
stdout return ordinary CLI errors. The caller's working
directory is the initial program directory, regardless of where the source file
lives. `cd` resolves physical directories. An explicit relative cache directory
resolves from the initial caller directory.

Workflow, registry-backed applet, service-generation, and live-serve commands
accept `--manifest-dir DIR` (or one legacy JSON file). They also accept the
explicit, mutually exclusive `--manifest-catalog FILE` selector for a strict,
versioned packed catalog. A packed catalog is never selected automatically and does not
merge with a directory. Catalog paths are relative to the caller's working
directory; packed entries use the canonical catalog file as their origin, and
relative program-module sources resolve from its parent directory. Service
definitions store the canonical catalog path. Without either selector,
discovery checks `manifests` beside the executable's `bin` directory, then
`share/execute/manifests` there (the native package layout), then the development source location in debug builds. Release builds never search
the build machine’s source tree. If no installed directory exists, use an explicit
selector for named applets; quoted direct commands can run without a catalog. The
working directory is never searched, so a directory a script runs in cannot shadow
bundled applets.

**Bundled manifest index.** The shipped manifests are also embedded in the binary as
one deflate stream per manifest with a generated index (names, phrases, sizes and
offsets). The default directory must contain exactly the bundled JSON file set;
extra, missing or non-regular entries are refused. A manifest's on-disk content is
checked against the bundled SHA-256 digest before it is first used; `check` verifies
all bundled entries. Use `--manifest-dir` or `--manifest-catalog` to load edited or
custom manifests explicitly. Daemon cache validation hashes explicit sources on each
request, so restored modification times do not hide edits. A test fails when
the checked-in index no longer matches `manifests/`. The catalog includes `cd`, `pwd`, `ls`, `cat`, `printf`, `tr`,
`wc`, `tail`, `grep`, `sort`, `uniq`, `cut`, `head`, `false`, and a `git.commit` mapping. Older aliases remain available.
Git bindings are provided for inspection; the tests and demos do not commit/push.
`tail` uses ordinary CLI defaults; following requires explicit `-F` flags. The
older `follow file` alias maps to the `tail.follow` adapter. Executable symlinks
retain their invoked names, including for multicall applets.

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

This maps `then tr with from "a-z" and to "A-Z"` directly to argv
`["tr", "a-z", "A-Z"]`. Argument nodes support literals, scalar parameters,
list spreads, and optional `when_present`/`append` mappings. Positional file paths
use `--` where appropriate. `cat` accepts an ordered `files` list in addition to
its optional single `path`; when both are supplied, `path` comes first. The explicit `builtin: "cd"` declaration is validated
against its single path parameter; ordinary manifests always launch their binary.

String parameters may declare `min_length` and `nonempty`. `min_length` counts
Unicode scalar values (not UTF-8 bytes or grapheme clusters); `nonempty: true`
also rejects strings whose Unicode whitespace-trimmed value is empty. Known
literals are checked by `execute check`, and referenced values are checked after
resolution before argv is built. The contract projection exposes these rules as
JSON Schema `minLength` and a whitespace-only exclusion using Rust's exact
`str::trim` Unicode whitespace set. For example, U+0085 is whitespace while
U+FEFF is not.

A non-cacheable external manifest may set `"uncertain_on_failure": true` when a
failed request may already have caused an external effect, including provider
billing. Checkpoint resume then requires explicit `--retry-uncertain` after a
failed attempt. Existing manifests default to false and retain their serialized
identity. The bundled model manifests use `"binary": "@execute-model"`, resolved
only to the sibling helper beside the running interpreter. See
[model operations](models.md) for the provider and offline-fixture contract.

Finite captures default to 16 MiB. This covers terminal results, finite boundaries,
named reads, and cache input. Intermediate native pipe traffic streams directly.
An overflow stops the active commands and skips later saves. This version buffers
each finite terminal result until completion; a live listener emits after each
record handler, rather than waiting for the unbounded source to finish.
For output larger than the capture limit, stream a final `pipe to` stage such as
`tee` so bytes flow without collecting the whole result. `parallel for each`
divides the available output allowance by its concurrency limit.

Cache keys include the manifest, resolved argv, directory, executable path/content,
environment, and exact input. Failed results are never published. Cache files use
private temporary files and atomic renames. Default state is `.flow-cache` in the
current program directory. External files, libraries, network state, and time
aren't tracked automatically; `cacheable` is an author contract. `cat` with an
optional file path or file list is therefore not declared cacheable. Cache
publication evicts older entries to keep at most 128 entries and 64 MiB of
cached output and checksum data. A single output larger than the byte budget is
returned to the caller but not retained.
`wc` also permits arbitrary CLI arguments in `flags`, including file operands,
so its generic binding is not cacheable; the fixed stdin-only `count.lines`
adapter remains cacheable.

Cache directories and entries must be real directories/regular files; symlinks,
Windows reparse points, FIFOs, sockets and devices are rejected. Unix cache
paths must belong to the effective user with no group/other write mode bits.
New Unix cache directories are 0700 and result files are 0600. Rejected paths are not
chmodded; use a real private directory instead of a symlink/shared writable cache.
Unsafe cache state is an error, not a cache miss that repeats the applet. Parent
directories and same-user cache writers remain trusted. These checks do not
provide a command sandbox, authenticated cache contents, complete ancestor-race
protection or Unix extended ACL/Windows cache ACL enforcement; see
[security boundaries](../SECURITY.md).

`--trace` emits JSON lifecycle events, cache events, interval lateness, calendar
ticks, and handler timings. Applet diagnostics share stderr. Parameter values and
environment contents are omitted from traces.
Full stdout and trace pipes backpressure execution while remaining responsive
to cancellation. If stderr is full during shutdown, the final error diagnostic
may be omitted. Trigger-only limits are rejected on programs that cannot use them.

## Warm daemon (`--daemon`)

Native applet processes and the bundled model helper inherit the host environment by default. To limit ambient variables without a capability policy, set `EXECUTE_CHILD_ENV_ALLOW` to a comma-separated list of names, such as `PATH,HOME,LANG,TERM`. The same setting is available per invocation with `--env-allow PATH,HOME`; repeat the flag or use a comma-separated list for several names. Use `--clear-env` to pass no ambient variables. Only listed names may be inherited or explicitly set in a child environment; the allowlist intersects with any capability-policy environment grant, and dynamic-loader variables are rejected. Malformed lists fail closed. These options apply to finite runs, listeners, services, `run-parts`, daemon-forwarded runs, and the bundled `execute model` helper. Service generation records the selected option in the generated service command. A model helper that reads an API key from the environment needs that variable explicitly listed. This does not confine the interpreter or remove secrets explicitly supplied as program input. Unset the environment variable or omit the flags to retain full inheritance.

`execute daemon serve` keeps a per-user process running behind a Unix socket, by default under `$XDG_RUNTIME_DIR`. `execute daemon status` reports whether it is running, and `execute daemon stop` stops it. Nothing runs in the background unless you start the daemon. The daemon limits concurrent forwarded runs to twice the available CPU count, capped at 64. Set `EXECUTE_DAEMON_MAX_RUNS` on the daemon to an integer from 1 through 4096 to choose another limit. At capacity the daemon declines new runs and the client runs them locally; existing forwarded runs continue. `daemon status` reports the current `runs` and `max_runs`.

The socket defaults to `$XDG_RUNTIME_DIR/execute-<uid>/daemon.sock` (the temporary directory when unset). `--socket PATH` selects another socket for `serve`, `status` and `stop`. The daemon is Unix only, and checks socket ownership, directory permissions and peer identity.

By default, `run --daemon` and `EXECUTE_DAEMON=1` run locally with a notice. The daemon declines requests from older clients. The protocol cannot reproduce or prove equivalence of namespaces, cgroups, security filters and other OS confinement, so a daemon may be less restricted than its client. Local execution preserves the client's execution mode and policies.

To explicitly accept execution in the daemon's OS context, set `EXECUTE_DAEMON_ACCEPT_CONTEXT=1` together with `--daemon` or `EXECUTE_DAEMON=1`. Only do this when you trust that context for the program; the client does not verify equivalence. Arguments, execution mode and policy paths are forwarded and the run applies those policies in the daemon context. Do not set the acceptance variable in sandboxed environments unless you intend to let runs leave their sandbox. Avoid `EXECUTE_DAEMON=1` in sandboxed environments when using older versions, which forward without this opt-in.

## Recover after a failure

The harmless [`examples/repair.program`](../examples/repair.program) intentionally runs `false` after a cached
`tr` result. Copy it to a temporary directory, run it below, then replace
`then false` with `then cat` to recover:

```sh
execute repair.program --checkpoint .execute-state/repair --trace
# Fix the failed command in repair.program, then:
execute resume repair.program --checkpoint .execute-state/repair --retry-uncertain --trace
```

The first run fails intentionally. Inspect any external effects before retrying;
a failed command may have completed part of its work. Even after editing the failed
node, resume requires `--retry-uncertain` to authorize repeating it. Use a fresh
checkpoint directory when starting the example again.

Resume restores the latest completed node's output, named results, and working
directory. It skips completed commands and saves, including commands that are not
cacheable. `and cache` still provides reusable pure results across fresh runs;
`--checkpoint` records progress for one execution. Ordinary runs do no checkpoint
I/O.

You can edit the failed node and subsequent nodes, including removing a failed
terminal suffix. Completed nodes must retain the
same syntax, manifests, executable identities, and pipe grouping. Comments and
line-number changes are fine: each completed node records the id of its first
step (see [step labels and step ids](language.md#step-labels-and-step-ids)) and the line it
had, and resume matches nodes by that id at the position the completed prefix
gives them. An edit below the last completed node, or a comment above a completed
node, resumes without rerunning it. A statement inserted or removed above a
completed node is refused, and the error names the node and both lines; use a
fresh checkpoint. Upstream file contents are frozen at a checkpointed
`read`; resume uses that snapshot even if the original file changes. Environment
changes apply only to commands that still run. Use a new checkpoint directory to
start over or change the completed prefix. A completed finite resume returns its
stored final output without running commands again.

On Unix, the runtime reuses executable content digests within a completed
prefix validation pass. Each reuse still checks the selected path's device,
inode, size, modification time and change time; a changed identity is rehashed.
New validation attempts and execution boundaries use fresh digests. Fingerprint
bytes and existing journals stay compatible. Hashing detects metadata changes
while reading and fails rather than recording that unstable content. This is
not an atomic filesystem snapshot or protection against a binary being replaced
after validation.

Each adjacent live `pipe to` group is one node. A failed group retries as a whole;
there is no checkpoint halfway through a live byte stream. Checkpointed `read`
buffers its input and applies `--max-output-bytes` to that snapshot. Checkpoints add file
hashing and synchronous disk writes, so use ordinary execution when minimal
latency matters more than crash recovery.

For listeners and schedules, the current event is saved before its handler starts.
Resume finishes a failed handler before restarting the producer or waiting for the
next tick. This preserves the current event, not the producer's unread queue or
file offset. Restarting a listener can lose or repeat other events; intervals
restart their timing and cron waits for the next matching minute. Output delivery
is not transactional and a partly delivered result can be emitted again.

If a process crashes or is interrupted during a command with side effects, its
outcome can be unknown. Resume refuses that node until you explicitly add
`--retry-uncertain`. Inspect the external effect first. Even a reported command
failure can have partial effects; retrying it is an explicit decision. Checkpoints
do not guarantee exactly-once external effects. An abruptly killed interpreter
can also leave its applet running; stop or inspect it before retrying.

Services discard unreferenced checkpoint blobs after each successful handler.
The pure-result cache has its own lifecycle.

### Per-element checkpoints

A top-level `for each` or `parallel for each` under `--checkpoint` is one in-flight
node whose elements are recorded one by one. Each element gets a `begin` line before
it starts and a `commit` line once it has succeeded, in the append-only
`elements.jsonl` beside the journal. A commit line holds the element's fingerprint
(the node's fingerprint plus the item's canonical JSON), its output, and references
to its bindings and working directory. Resume replays the log: committed elements
are skipped and their results restored, so a sequential loop continues after its
last committed element and a parallel group takes the recorded results of the
committed ones. Entries appended to the list after the committed ones run normally.

- A committed element whose list entry changed refuses the resume with `for each list
  changed at element N; use a fresh checkpoint`.
- Commit lines are appended through one handle and synced in groups, at most every
  250 ms and always before any journal write. A commit lost in a crash reruns that
  element.
- The journal records the log's line count and chain digest at its last write. Resume
  refuses a log shorter than that count or with a different chain at it.
- An element that is not [retry-safe](language.md#retry-safety) writes a durable `begin` record
  first. If the run died inside such an element, only that element is uncertain, and
  `--retry-uncertain` reruns it. The rest of the loop is not retried.
- Nested loops, `repeat` and `while` keep whole-node behavior.

### Journal versions

Journal versions, migration behavior, keyed digest encoding and filesystem
protections are implementation details documented in the
[checkpoint format internals](internals/checkpoint-format.md).

## Bound command lifetimes

```sh
execute my.program --command-timeout-ms 2000
```

The optional positive millisecond limit applies separately to each launched native
pipeline group, including startup, pipe draining, and trace writes. It also applies
to service handler commands. Listener producers retain their intended lifetime;
this flag does not shut them down after one interval. UI native pipelines default
to 30 seconds; the option overrides that default. Very small limits may expire
during process startup.

The runtime also keeps buffered stdin writers inside that
deadline/cancellation contract. Their private pipe uses cancellable writes,
and error cleanup tells the writer to stop without blocking on a join. This
fixes a case where an exited applet's escaped descendant retained stdin and the
interpreter joined a blocked writer after the owner had finished. Escaped
descendants remain outside process-group containment; this change bounds the
interpreter's own input worker rather than claiming to contain those processes.

Timeouts kill and reap owned process groups. A timed-out effectful invocation is
recorded as uncertain in a durable checkpoint; resuming it requires the existing
explicit `--retry-uncertain` gate. File I/O, cache hashing, filesystem stalls, and
final output delivery are outside this command lifetime limit. This is cooperative
process supervision, not a sandbox: a command that escapes its group with `setsid`
can retain descriptors or survive, and arbitrary applets have the user's host
permissions. Only execute trusted scripts and manifests.


The default machine-readable `explain --json` emits `execute.explain/2`; `run --what-if` emits `execute.what_if/2` with that plan. The v2 schema is `docs/schemas/explain.v2.schema.json`. Consumers of the original closed `execute.explain/1` schema can select `--explain-version 1` with `explain --json` or `run --what-if`. Legacy plans omit direct argv and unresolved arguments, so migrate reviewers to v2 to inspect them. A legacy what-if emits `execute.what_if/1`; pass the same `--explain-version 1` to `run --confirm`, since protocol versions produce different fingerprints.

Version 2 `explain --json` and `run --what-if` include an `argv` array for each invocation and a `parameters` object for named applets. Values known from inputs are shown; unresolved runtime bindings appear as `{"ref":"name"}`. Sensitive environment and secret-prompt inputs are represented by a keyed HMAC marker. The first such `explain --json` or what-if review creates a private per-user what-if key if one does not already exist (`$XDG_STATE_HOME/execute/what-if.key`, or `~/.local/state/execute/what-if.key` on Unix; `%LOCALAPPDATA%/execute/what-if.key` on Windows). Literal JSON records in parameters and structured templates are encoded as `{"record":{...}}` recursively, so a literal field named `ref` or `sensitive` cannot be mistaken for a marker. Some manifest argument forms use structured template entries when their final native argument cannot yet be rendered. The plan is static: bindings produced during execution and called-program inputs may remain unresolved.
