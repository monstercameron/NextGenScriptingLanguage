# Execute: agent card

Execute runs controlled-English `.program` files that call native command-line
tools. This card is for an agent that writes or reviews a program. It lists the
forms that the parser accepts and the review commands the `execute` binary
provides. The [language reference](../language.md) explains behavior, and the
[grammar](../grammar.md) gives the full EBNF. Every ```` ```program ```` block in
this card is parsed by the real parser in `tests/issue_432_agent_pack.rs`.

## File shape

- An optional first line `execute 1`. Only version 1 is accepted.
- Declarations come before statements: `accept NAME as TYPE`, with optional
  `one of [...]`, `from SOURCE`, `with default VALUE` and `described as TEXT`.
- Types are `text`, `integer`, `boolean`, `list of text` and `json`.
- Sources include `arguments`, `argument N`, `environment "NAME"`,
  `program directory`, `flag "--x" alias "-x"` (boolean), `option "--x"` (text or
  integer), and `prompt "..."` or `secret prompt "..."`.
- Keywords are lowercase. `#` starts a comment. A body is the run of statements
  indented further than the line that opens it. There is no end marker.

```program
execute 1
accept name as text
accept count as integer with default 3
accept arguments as list of text from arguments
run "printf" with arguments ["hello %s\n", name]
```

## Native commands

- `run "printf" with arguments ["a", "b"]` launches a quoted executable directly,
  with explicit argv. There is no shell.
- `run NAME with PARAM VALUE as RESULT` calls a manifest applet. Applet names and
  parameters come from the catalog.
- `then "cmd"` waits for the previous step to succeed and passes its output as
  stdin. `pipe to "cmd"` connects two processes with an OS pipe.
- `use value "text"` sets the pipeline input to a text value.
- `read "path" as NAME` reads a file. `then save to "path"` replaces a file.

```program
run "test" with arguments ["-f", "input.txt"] and keep status as present
run "printf" with arguments ["a\nb\n"]
then "wc" with arguments ["-l"]
```

Suffixes, in this order, after the invocation: `as NAME`, `and discard stderr`
(quoted executables only), `and timeout N UNIT`, `and keep status as NAME`,
`and keep result as NAME`, `and keep secret` (needs `as NAME`), `and cache`
(declared applets only), `and retry up to N times`. A misordered suffix is a
`parse.suffix_order` diagnostic with a fix.

## Control

- `if X is Y`, `is not`, `contains`, `does not contain`, or
  `if run "cmd" with arguments [...] succeeds` (or `fails`), with `otherwise if`
  and `otherwise` branches.
- `for each NAME in LIST`, `repeat N times`, `while X is Y`.
- `try` with `catch [NAME]` and `finally`, `fail with message "text"`,
  `exit with status N`, `break`, `continue`, `return`.

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
try
  run "false"
catch failure
  fail with message "step failed"
```

## Results and JSON

`take TYPE at "/pointer" from NAME as NAME` reads one typed value from a JSON
result at a JSON pointer.

```program
run "printf" with arguments ["{\"ok\": true}"] as reply
take boolean at "/ok" from reply as ok
```

## Reusable programs and labels

```program
define a program named greet
  accept name as text
  run "printf" with arguments ["hello %s\n", name]
run program greet with name "world"
```

A step can carry a label, written as one word with no space before the colon:

```program
fetch: run "printf" with arguments ["hello\n"]
run "printf" with arguments ["world\n"]
```

## Triggers and services

A file has at most one trigger: `every N seconds` (also minutes, hours and so
on), `on schedule "CRON"`, or `listen to TARGET whenever a line arrives as NAME`.
A trigger body runs repeatedly, and nothing finite may follow it.

```program
every 5 seconds
  run "printf" with arguments ["heartbeat\n"]
```

## Review loop

Run these in order before a program is used:

1. `execute check FILE` reports diagnostics. `--json` gives structured output,
   `--deny-warnings` turns warnings into errors, and `--input NAME=VALUE`
   supplies a declared input.
2. `execute check --explain CODE` explains a diagnostic code.
3. `execute explain FILE --json` gives the plan, step ids and effect categories.
4. `execute run FILE --what-if` prints the plan and its fingerprint and runs nothing.
5. `execute run FILE --confirm FINGERPRINT` runs only if the reviewed plan still
   matches that fingerprint.
6. `execute applets list`, `search`, `inspect`, `discover` and `match` browse the
   applet catalog.

## Pitfalls

- The grammar has no arithmetic, no string functions and no interpolation.
  Computation belongs in applets.
- Text does not become an integer, boolean or list parameter by itself.
- Paths and arguments have no wildcard, environment or command expansion.
- A bare name is a binding. `"path"` in quotes is literal text.
- A secret binding cannot be passed as an argument. Feed it with `use value NAME`
  or pass it with `and environment`.
- Declarations must precede statements. Only `execute 1` is a valid version line.
