# Execute glue grammar

This is an EBNF-style index of the accepted program forms in the current source.
It summarizes syntax; the [language reference](language.md) explains behavior and
restrictions. Computation belongs to applets, not to this grammar.

Every ```` ```program ```` block below is parsed by the real parser in
`tests/lane_a_grammar_doc.rs`, so an example that drifts from the grammar fails the
test. Blocks marked `text` are grammar fragments and are not parsed.

## Notation

- `"text"` is a literal word or punctuation. `NAME` is an identifier
  (`[A-Za-z_][A-Za-z0-9_]*`). `TEXT` is a double-quoted string. `INTEGER` is an
  optionally signed decimal integer. `BOOL` is `true` or `false`.
- `[ x ]` is optional, `{ x }` repeats, `|` separates alternatives.
- An indented body is one or more statements indented further than the statement
  that opens it. Dedenting ends the block; there is no end marker.

## Program structure

```text
program     = [ version ] { declaration } { statement }
version     = "execute" INTEGER
declaration = "accept" NAME "as" TYPE [ "one" "of" choices ] [ "from" source ] { option }
option      = "with" "default" value | "described" "as" TEXT
choices     = "[" TEXT { "," TEXT } "]"
source      = "environment" TEXT | "arguments" | "argument" INTEGER
            | "program" ( "directory" | "path" )
            | "flag" TEXT [ "alias" TEXT ]          (boolean inputs only)
            | "option" TEXT [ "alias" TEXT ]        (text or integer inputs only)
            | [ "secret" ] "prompt" TEXT
TYPE        = "text" | "path" | "file" | "directory" | "integer" | "boolean" | "list" "of" "text" | "json"
output      = "produce" OUTTYPE [ "described" "as" TEXT ]
OUTTYPE     = "text" | "integer" | "boolean" | "list" "of" "text" | "json" | "none"
```

Declarations come before statements. A declaration after a statement is an error.
A program has at most one `produce` declaration, and a second one is an error that
names both lines. `produce` is unindented, like `accept`. It declares the program's
result type and changes nothing at run time: the final value is not checked against
it yet. A program without it behaves as before.

```program
produce json described as "Report with per-host status"
```

The optional `version` line is the first statement of the file, with no indentation.
It names the language version the program was written for. Only `execute 1` is
accepted by this release. Any other number is an error, and so is a `version` line
that is not first.

```program
execute 1
accept name as text
```
`with default` and `described as` may follow each other in either order, and each
may appear once. `described as` gives the input a description that `execute inspect`
reports. It does not change how the input is read.

```program
accept name as text
accept count as integer with default 3
accept arguments as list of text from arguments
accept greeting as text described as "Who to greet"
accept repeats as integer described as "Repeat count" with default 3
accept mode as text one of ["light", "dark"] with default "light" described as "Theme"
accept here as text from program directory
accept verbose as boolean from flag "--verbose" alias "-v"
accept output as text from option "--output" alias "-o" with default "out.txt"
```

`one of` applies only to `text` inputs. Its choices are nonempty, unique, NUL-free
quoted text, at most 256 of them. A default must be one of the choices.

A leading comment block may hold one policy header, `# execute-policy: allow
CATEGORY...`. It is an ordinary comment, so it is not part of the grammar. See the
language reference for the categories.

## Logical lines, continuation, and raw text

Statements may continue across physical lines while brackets remain open. The
closing bracket completes the logical statement. A triple-quoted raw block can
also span lines and ends at a line containing `"""`; it can only finish a
statement. When the opening delimiter follows a bracketed expression it needs a
preceding space. See [the language reference](language.md#reading-a-program) for
raw block examples.

```program
run "printf" with arguments [
  "%s\\n",
  "continued"
]
use value """
first line
second line
"""
then capture bytes as payload
use bytes payload
then "cat"
```

## Pipeline input

```text
statement   = ... | "use" "value" value [ "as" NAME ] | "use" "bytes" NAME
```

`use value` sets the pipeline input to a text value. `use bytes` sets it from a
captured byte binding. A following `pipe to` or `then` stage reads those bytes.
Neither form launches a process.

The optional `as NAME` suffix binds the value with its static type: text,
integer, boolean, list of text, or JSON. References (including those inside
lists and records) are resolved before binding. Its pipeline bytes are the text
itself or compact JSON for other values. A bound result is internal and never
becomes implicit program stdout.

```program
use value "hello"
then tr with from "[:lower:]" and to "[:upper:]"
```

## Working directory

```text
statement   = ... | "use" "folder" value
```

`use folder` sets the working directory for the statements that follow, like
`run cd with path`. It may appear anywhere a statement can appear, including in
loops and after data-producing steps. The folder is opened, its resolved path is
authorized and later relative paths join onto it. It may also precede a trigger as
initial setup. See
"Working directory and `use folder`" in [the language reference](language.md).

```program
use folder "work"
run pwd
```

## Native commands and applets

```text
statement   = invocation suffixes
invocation  = "run" target [ "with" argument { "and" argument } ]
            | "then" target [ "with" argument { "and" argument } ]
            | "pipe" "to" target [ "with" argument { "and" argument } ]
target      = TEXT | NAME                      (TEXT is a direct executable)
argument    = NAME value | "arguments" list | "flags" list
value       = TEXT | INTEGER | BOOL | NAME (a binding) | list
list        = "[" [ item { "," item } ] "]"
item        = TEXT | INTEGER | BOOL | NAME
suffixes    = [ "as" NAME ]
              [ "and" "discard" "stderr" ]        (direct quoted commands only)
              [ "and" "timeout" INTEGER UNIT ]
              [ "and" "keep" "status" "as" NAME ]
              [ "and" "keep" "result" "as" NAME ]
              [ "and" "keep" "secret" ]           (requires `as NAME`)
              [ "and" "cache" ]                   (declared deterministic applets only)
              [ "and" "retry" "up" "to" INTEGER "times" ... ]
```

```program
run "printf" with arguments ["hello %s\n", "world"]
run "printf" with arguments ["count"] as label
run "test" with arguments ["-f", "input.txt"] and keep status as present
```

Invocation suffixes have one canonical order, stated once by the `suffixes`
production above: each family at most once, in the order listed. Redirection
(`and stderr to`, `and stdin from`, `and environment`, ...) and the `and retry`
/ `and timeout` restrictions aside, a misordered suffix is a
`parse.suffix_order` diagnostic with a machine-applicable fix that reorders the
suffixes; the reorder preserves meaning. `and retry` cannot be combined with
`and cache`, `and keep status`, or `and keep result`, and `and timeout` cannot
be combined with `and cache`, so those combinations keep their existing errors
and get no reorder fix.

`then` waits for the previous statement to succeed and passes its output as stdin.
`pipe to` connects the previous stage's stdout to this command with an OS pipe.

```program
run "printf" with arguments ["a\nb\n"]
then "wc" with arguments ["-l"]
```

```program
run "printf" with arguments ["a\nb\n"]
pipe to "wc" with arguments ["-l"]
```

`then save to TEXT` replaces a file atomically with the current output:

```program
run "printf" with arguments ["saved"]
then save to "out.txt"
```

Named applets use the manifest's parameter names:

```program
accept text as text
run text.trim_final_lf with text text as trimmed
```

## Results and typed bindings

```text
take        = "take" ( TYPE | "unsigned" "integer" | "usize" ) "at" TEXT "from" NAME "as" NAME
```

`as NAME` binds a result to a name. `take` reads one typed value out of a JSON result
at a JSON pointer.

```program
run "printf" with arguments ["{\"ok\": true}"] as reply
take boolean at "/ok" from reply as ok
```

`then capture bytes as NAME` stores pipeline bytes without decoding them as text;
`use bytes NAME` supplies such a binding as pipeline input. The `read` statement
reads a file into a binding. Pipeline transforms also include JSON parsing and
encoding:

```text
then capture bytes as NAME
use bytes NAME
read = "read" [ "file" ] value [ "as" NAME ]
     | "read" "from" "stdin" [ "as" NAME ]
then parse json as NAME
then encode VALUE as json
```

## Control

```text
conditional = "if" condition body { "otherwise if" condition body } [ "otherwise" body ]
condition   = operand ( "is" | "is not" | "contains" | "does not contain" ) operand
            | "run" invocation ( "succeeds" | "fails" )
loop        = "for" "each" NAME "in" operand body
            | "repeat" INTEGER "times" body
            | "while" operand ( "is" | "is not" ) operand body
operand     = NAME | TEXT | INTEGER | BOOL | list
body        = indented statements
flow        = "break" | "continue" | "return" [ value ]
            | "exit" "with" "status" INTEGER | "fail" "with" "message" TEXT
exception   = "try" body { "catch" [ NAME ] body } [ "finally" body ]
            | "on" ( "exit" | "interrupt" ) body
task_group  = ( "parallel" | "race" ) "at" "most" INTEGER "as" NAME body
            | "parallel" "for" "each" NAME "in" operand "at" "most"
              INTEGER "as" NAME body
task_entry  = "run" NAME [ "with" argument { "and" argument } ] "as" NAME
job         = "spawn" "as" NAME body | "await" NAME | "inspect" NAME "as" NAME
            | "cancel" NAME
```

```program
accept name as text
run "test" with arguments ["-f", name] and keep status as present
if present is 0
  run "printf" with arguments ["file exists\n"]
otherwise
  run "printf" with arguments ["missing\n"]
```

```program
accept arguments as list of text from arguments
for each item in arguments
  run "printf" with arguments ["%s\n", item]
```

```program
repeat 2 times
  run "printf" with arguments ["tick\n"]
```

Other accepted control forms include `otherwise if`, command success conditions,
try/catch/finally, `on exit`, `on interrupt`, `break`, `continue`, `return`,
`exit with status`, and `fail with message`. Background task forms include
`spawn`, `parallel for each`, `race`, `await`, `inspect`, and `cancel`; see the
language reference for complete syntax and restrictions.

## Reusable programs

```text
definition  = "define" "a" "program" "named" NAME body
call        = "run" "program" NAME [ "with" argument { "and" argument } ] [ "as" NAME ]
file call   = "run" "program" "file" TEXT [ "with" argument { "and" argument } ] [ "as" NAME ]
```

A `file` call runs another `.program` file's top level in-process, with the same
explicit inputs as a named call. Its path resolves from the calling file's directory.
The call is checked when the program loads, before any step runs.

```text
run program file "install/config/fonts.program" with theme theme as fonts
```

```program
define a program named greet
  accept name as text
  run "printf" with arguments ["hello %s\n", name]
run program greet with name "world"
```

## Step labels

```text
statement   = [ label ] statement-body
label       = LABEL ":"
LABEL       = letter { letter | digit | "_" | "-" }     (not a statement keyword)
```

The label is one word, with no space before the colon. It names the statement's first
step. A label must be unique in its program body. It cannot precede a declaration, a
trigger or a clause, and a statement takes at most one label. The
[language reference](language.md) explains step ids, including the derived ids that
unlabelled steps receive.

```program
fetch: run "printf" with arguments ["hello\n"]
run "printf" with arguments ["world\n"]
```

## Triggers

A trigger wraps its body and runs it repeatedly. A file has at most one trigger, and
a trigger cannot be combined with ordinary finite statements after it.

```text
interval    = "every" INTEGER ( "millisecond" | "milliseconds" | "second" | "seconds"
            | "minute" | "minutes" | "hour" | "hours" ) body
schedule    = "on" "schedule" TEXT body
listener    = "listen" "to" target [ "with" argument { "and" argument } ]
              "whenever" "a" "line" "arrives" "as" NAME body
```

```program
every 5 seconds
  run "printf" with arguments ["heartbeat\n"]
```

```program
listen to stdin
whenever a line arrives as message
  then "cat"
```

## Suffixes and scoped forms

Accepted command suffix families include environment changes (`and environment
{...}` and `and without environment [...]`), `and retry ...`,
`and timeout ...`, stderr/stdin/stdout
redirections, `then append to`, `interactively`, `hand over to`, and
`launch detached`. Scoped statements include `with temporary file as NAME` and
`with temporary directory as NAME`; lifecycle blocks include `on exit` and
`on interrupt`. Their complete shapes and context restrictions are in the
[language reference](language.md).

## Modifier contexts

Which modifiers may follow each statement. A `yes` cell is accepted by the parser;
`no` cells are rejected or unsupported in that context.

| Statement | `as NAME` | `and keep status as` | `and keep result as` | `and keep secret` | `and discard stderr` | `and cache` | `and timeout` |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `run TEXT` (direct executable) | yes | yes | yes | yes, after `as NAME` | yes | no | yes |
| `run NAME` (manifest applet) | yes | yes | yes | yes, after `as NAME` | no | yes, when the manifest declares it | yes, native pipelines only |
| `then` | yes | yes | yes | yes, after `as NAME` | no | no | yes |
| `pipe to` | yes, on the group's final stage (see note) | no | no | yes, after `as NAME` | no | no | yes, bounds the whole group |
| `run program NAME` (reusable) | yes | yes | no | no | no | no | no |
| `take` | no | no | no | no | no | no | no |
| `produce` (output declaration) | no | no | no | no | no | no | no |
| `every`, `on schedule`, `listen to` | no | no | no | no | no | no | no |

`pipe to ... as NAME` binds the pipe group's final output, the terminal stage's
stdout, to NAME. A group with a binding does not stream to the terminal, and a later
statement can read NAME. `pipe to` stages cannot capture status or results. The test
`pipe_to_as_binds_the_terminal_stage_output_and_runs` checks this against the real
binary.

Reusable calls can keep their completion status with
`run program NAME and keep status as NAME`. Normal completion and `return` report
status 0; `exit with status N` inside a reusable body ends only that call and
reports N. A failing native command in a call reports its native exit status when
the call captures status, then the caller continues.

## Run options outside the grammar

These options are not part of the program text. The language reference describes each
one in full.

- `run FILE --what-if` prints the plan and its fingerprint, and runs nothing.
- `run FILE --confirm FINGERPRINT` runs only when the current fingerprint matches. It
  covers the plan, the resolved inputs (an environment value as an HMAC under a per-user
  key, never the value), the working directory, the capability and data-flow policy
  file contents, the manifest files, and the program's version. See the language
  reference for the exact list.
- `run FILE --single-instance LOCK` refuses to start while another run holds LOCK.
- `run FILE --control` hosts the run on a live control endpoint, and `attach` reads or
  controls that endpoint.
- `run FILE --daemon` (or `EXECUTE_DAEMON=1`) runs locally with a notice unless `EXECUTE_DAEMON_ACCEPT_CONTEXT=1` is set for both the client and the daemon, explicitly accepting the daemon context at both ends.
- `run FILE --checkpoint DIR` and `resume FILE --checkpoint DIR` record and continue a
  run; `execute checkpoint status DIR` reports a checkpoint.

See "Review a plan before it runs" and "Attach to a live program" in
[the language reference](language.md).

## Machine-readable grammar (GBNF)

`flow::parser::gbnf_grammar()` returns a GBNF grammar (the llama.cpp grammar format
for constrained decoding). It is built from `flow::parser::GRAMMAR_FORMS`, one table
row per statement form. `tests/lane_a_grammar_doc.rs` checks that each row's example
parses and appears in this document, so the table and this index cannot disagree
about the forms they cover.

The generated grammar covers only the forms in that table: `accept` and `produce`
declarations, `run` (direct and applet targets), `then`, `pipe to`, `use value` and `use folder`,
each on one unindented line. It does not yet encode indented blocks, `take`,
suffixes other than `as NAME`, declaration options, or context rules such as "`then`
needs a preceding step". It is therefore looser than the parser in those places. The
applet name form (`text.trim_final_lf`) allows dots, which the NAME notation above
does not show.

## Stream, screen and graph files

A `.program` file can instead be one of three other kinds. Each has its own
parser, and none of them is covered by the `program` table or the GBNF above.
The grammars below were written from the parsers, not from the language
reference:

- stream: `flow::stream::parse` in `src/stream.rs`, detected by `is_stream`
- screen: `flow::ui::syntax::parse` in `src/ui/syntax.rs`, detected by `is_screen`
- graph: `flow::graph::parse` in `src/graph/syntax.rs`, detected by `is_graph`

Every stream, screen and graph example below uses its own fence (```` ```stream ````,
```` ```screen ````, ```` ```graph ````). `tests/issue_476_grammar_file_kinds.rs` parses each
one with the parser for its kind and fails with that parser's message. Blocks
marked `text` are fragments and are not parsed.

### Stream files

```text
stream      = { declaration } header { stage } [ take ] sink
header      = "stream" "from" source
source      = target [ "with" argument { "and" argument } ]
stage       = "pipe" "to" target [ "with" argument { "and" argument } ]
take        = "take" "the" "first" INTEGER ( "line" | "lines" )
sink        = "show" "each" "line"
```

`declaration` is an `accept` line as in the program structure, and it must come
before the header. `target`, `argument` and `INTEGER` have the meanings given in
the native commands section. The header and each stage are parsed as the
finite `run` and `pipe to` forms, with the `run` or `pipe to` word removed.

- Statements are not indented. Blank lines and `#` comments are allowed.
- `take` appears at most once, and only after every stage. `show each line`
  appears exactly once and is the last statement.
- `take the first N lines` needs N to be a positive integer.
- Validation, which is separate from parsing, requires the header to be a
  direct or applet `run` and every later stage to be `pipe to`. It rejects
  `as NAME`, `and cache` and status or result bindings on any stage, and it
  requires external applets.

```stream
stream from "printf" with arguments ["alpha\nbeta\ngamma\n"]
pipe to "tr" with arguments ["a-z", "A-Z"]
take the first 2 lines
show each line
```

```stream
accept word as text
stream from "printf" with arguments ["%s\n", word]
show each line
```

### Screen files

```text
screen      = title { layout } { component }
title       = "open" "a" "screen" "titled" TEXT
layout      = "divide" "the" "screen" "into" "two" "columns"
            | "place" NAME "on" "the" ( "left" | "right" )
            | "give" NAME INTEGER "percent" "of" "the" "width"
            | "connect" NAME "from" NAME "to" NAME "in" NAME
component   = "define" "a" "program" "named" NAME { member }
member      = ( "accept" | "offer" ) NAME "as" "text"
            | "show" "a" KIND "named" NAME [ property ] { item }
            | "give" NAME INTEGER "rows"
            | "label" "the" ( "horizontal" | "vertical" ) "axis" "of" NAME TEXT
            | handler
KIND        = "heading" | "text" "field" | "list" | "table" | "text" "panel"
            | "badge" | "button" | "line" "chart" | "bar" "chart"
property    = "with" "text" TEXT | "with" "value" TEXT
            | "labelled" TEXT | "with" "key" TEXT
item        = "include" TEXT                        (list only)
            | "column" TEXT "from" TEXT             (table only)
            | "label" "each" "bar" "from" TEXT      (bar chart only, required)
            | "size" "each" "bar" "from" TEXT       (bar chart only, required)
            | "label" "bar" "values" "with" TEXT    (bar chart only)
handler     = event { action }                      (at least one action)
event       = "whenever" "the" "program" "opens"
            | "whenever" "key" TEXT "is" "pressed"
            | "whenever" NAME "arrives"
            | "whenever" NAME "is" ( "submitted" | "pressed" )
            | "whenever" "an" "item" "is" "chosen" "in" NAME
            | "whenever" "a" "command" "fails"
            | "every" INTEGER UNIT
action      = "close" "the" "screen"
            | [ "then" ] "press" NAME
            | "show" "JSON" "as" ( "rows" | "bars" ) "in" NAME
            | "show" "the" "result" "as" ( "rows" | "bars" ) "in" NAME
            | "show" "field" TEXT "in" NAME
            | "add" "field" TEXT "to" NAME
            | "send" VALUE "as" NAME
            | "show" VALUE "in" NAME
            | finite-statement
VALUE       = TEXT | "the" "value" "of" NAME | "that" "item" | "the" "result"
            | "the" "error" | "its" "error"
```

Indentation carries the structure. Each `define` member is indented exactly two
spaces. Each widget item, event, and action is indented exactly four spaces.
Any other statement, or a statement with a bad indent, is an error. A
`finite-statement` is any single statement from the native commands and control
sections. An `if`, `repeat`, `for` or `while` block is also accepted as an
action, and its body is indented further than the action.

Limits the parser enforces: the title must be nonempty, and there must be one or
two placed components. `give NAME N percent of the width` takes N from 10 to 90 and
must name the left component. `give NAME N rows` takes N from 1 to 40. A component
needs 1 to 32 widgets and at most 32 handlers. A screen has at most 16 components
and 32 `connect` lines. A text field's initial value is at most 512 bytes, and a
list has at most 256 items. A table needs at least one column and at most 16. The
`every` interval uses the same units as the trigger form in the program
section.

The `connect` line reads `connect OUTPUT from SOURCE to INPUT in TARGET`. The name
after `from` is a placed component, and the name after `in` is the component that
receives the value. Whether a name is a placed component or a declared port is
checked by the screen's validation step, not by the parser.

```screen
open a screen titled "Greeter"
place form on the left
place log on the right
give form 40 percent of the width
connect greeting from form to greeting in log
define a program named form
  offer greeting as text
  show a text field named name labelled "Name"
  show a button named send labelled "Send"
  give name 1 rows
  whenever send is pressed
    send the value of name as greeting
define a program named log
  accept greeting as text
  show a text panel named panel with text "waiting"
  whenever greeting arrives
    show the result in panel
```

```screen
open a screen titled "Report"
place summary on the left
define a program named summary
  show a heading named title with text "Totals"
  show a table named results
    column "Name" from "name"
    column "Count" from "count"
  give results 8 rows
  show a bar chart named bars
    label each bar from "name"
    size each bar from "count"
    label bar values with " items"
  show a line chart named trend
  label the vertical axis of trend "count"
  show a list named choices
    include "one"
  whenever the program opens
    run "printf" with arguments ["a\n"] as result
    show the result as rows in results
  whenever an item is chosen in choices
    show that item in title
  every 5 seconds
    show field "name" in results
```

### Graph files

```text
graph       = [ version ] { declaration } "graph" NAME { statement } "end" "graph"
version     = "execute" INTEGER                     (first statement only)
statement   = run | every | listen | render | stream | emit | bind
run         = "run" target [ "with" argument { "and" argument } ] "as" NODE
every       = "every" INTEGER UNIT "as" NODE        (literal interval)
listen      = "listen" "to" ( "stdin" | target [ "with" argument { "and" argument } ] ) "as" NODE
render      = "render" BACKEND ":" COMPONENT [ "with" "properties" RECORD ] "as" NODE
BACKEND     = "quickshell" | "tui"
COMPONENT   = "text" | "button"
stream      = "stream" "from" source "as" NODE
              { "pipe" "to" target [ "with" argument { "and" argument } ] | "take" ... }
              "show" "each" "line"                (the stream lines above)
emit        = "emit" "result" "from" NODE "to" PORT
bind        = "bind" NODE "." PORT "to" NODE "." PORT
              [ "with" "capacity" INTEGER [ POLICY ] ]
POLICY      = "lossless" | "drop" "newest" | "drop" "oldest" | "latest"
UNIT        = "millisecond" | "milliseconds" | "second" | "seconds"
            | "minute" | "minutes" | "hour" | "hours"
NODE, PORT  = identifier (a letter or "_", then letters, digits, "_" or "-")
```

Each statement is one line. Every node statement ends with `as NODE`, and
the node name must be unique. A `run` must be one applet invocation, and it
rejects `and retry`, `and cache` and status or result bindings. The `every`
interval must be a positive integer followed by one unit, and it cannot name
a graph input. `bind` defaults to `with capacity 16 lossless` when the capacity
clause is absent. The capacity must be positive. `stream` takes the same stage and
`take` lines as a stream file. Its header and stages must be literal, so a
graph input cannot appear in them. The file ends with `end graph`, and nothing may
follow it. At least one node is required. Edges are checked against the port
contracts after every node is added, so a `bind` can fail even when it parses.

```graph
graph ticks
every 1 second as clock
listen to stdin as input
run text.trim_final_lf with text "hello\n" as trimmed
end graph
```

```graph
graph panel
render tui:text with properties {"text": "Hello"} as label
stream from text.trim_final_lf with text "a\nb\n" as lines
take the first 1 lines
show each line
end graph
```

## Reading the examples

The examples use `"` for text, and each example is one complete program. The test
checks syntax only. Names used by applets (for example `text.trim_final_lf`) are
checked against the manifest catalog by `execute check`, not by this grammar.
