# Migrating from Bash

This page maps the Bash idioms that appear most often in shell scripts to their Execute form.
Execute is not yet a Bash replacement; the [documentation index](README.md) and the
[language reference](language.md) say what is missing. Use this page to decide, idiom by idiom,
whether a port can be written directly, needs an applet, or has to wait.

Execute deliberately keeps its grammar thin. Arithmetic, string functions, pattern matching and
interpolation are not grammar forms. Where Bash uses one of them, this page names the applet
that does the work, or the native command to call directly.

## How to read the status column

| Status | Meaning |
| --- | --- |
| supported | The grammar has this form. The example shows it. |
| applet | The grammar has no form on purpose. A shipped applet does the work. Run `execute applets inspect NAME` for its parameters. |
| partial | A form exists, but it behaves differently. The notes say what differs. |
| missing | No form exists in this release. Keep the Bash in a native command, or restructure the program. |
| by design | Execute has no such behaviour and needs no replacement. The notes say why. |

Every ```` ```program ```` block below is parsed by the real parser in
`tests/issue_426_migration_guide.rs`. The test checks syntax only. Applet names and parameters
are checked by `execute check FILE`, and the examples have not all been run through it yet.
Blocks marked `bash` show the Bash being replaced and are not parsed.

## Strict mode, status and exit

| Bash idiom | Execute form | Status | Notes |
| --- | --- | --- | --- |
| `set -e` | Default. A native command that exits nonzero stops the run. | supported | Use `and keep status as NAME` to continue after an expected failure. |
| `set -u` | Names come only from `accept`, `as NAME`, and `use value ... as NAME`. | by design | There is no unset-variable state to check. A name is either bound or not written. |
| `set -o pipefail` | A failing stage in a `pipe to` group fails the group. | supported | SIGPIPE from a consumer that exits early is not a failure. |
| `cmd \|\| true` | `run "cmd" ... and keep status as NAME`, then ignore the status. | supported | See the first example below. |
| `cmd1 && cmd2` | Two statements. The second runs only if the first succeeded, because the run stops otherwise. | supported | Use `if run ... succeeds` when the outcome picks a branch. |
| `$?` | `and keep status as NAME`, an integer from 0 to 255. | supported | Native argument lists format the integer in decimal. |
| `exit N` | `exit with status N` | supported | N is a literal from 0 to 255, or an integer binding. |
| `die "msg"` or `echo msg >&2; exit 1` | `fail with message "msg"`, optionally `and status N` | supported | Writes the message to stderr, then exits with status 1 unless `and status` says otherwise. |
| `set -x` | `execute run FILE --trace` | partial | Trace records events for each statement and decision. It is not a shell xtrace of every command line. |

```program
run "grep" with arguments ["-q", "TODO", "notes.txt"] and keep status as found
if found is 0
  run "printf" with arguments ["TODO present\n"]
otherwise
  run "printf" with arguments ["no TODO\n"]
```

`grep` exits 1 for no match and 2 for an error. Both are nonzero. To tell them apart, test the
status for each value, as the [language reference](language.md#native-exit-status) explains.

```program
accept count as integer with default 0
if count is 0
  fail with message "no records" and status 3
run "printf" with arguments ["%s records\n", count]
```

## Quoting, words and globs

| Bash idiom | Execute form | Status | Notes |
| --- | --- | --- | --- |
| `"text"` | A TEXT literal in double quotes, with the escapes shown in the [grammar](grammar.md). | supported | Single-quoted text has no form. |
| `$name` inside a word | No interpolation. Pass the binding as a list item, or join with `text.concat`. | applet | Interpolation is not in the grammar. See the string example below. |
| Unquoted `$x` splitting into words | Not applicable. Each argument is exact bytes. | by design | Spaces, newlines and empty strings survive. Use a list for several arguments. |
| `IFS=` | None. | by design | Nothing is split, so there is no separator to set. |
| `*.log` globbing in arguments | Use `fs.glob`. Glob patterns are not expanded in `run` arguments. | applet | `*`, `?`, `[...]` and a `**` component work. There is no escape syntax. |
| `$'...'` ANSI-C quoting | TEXT escapes only. | partial | Escapes outside the TEXT form are not provided. <!-- feature: ansi_quote --> |

```program
accept name as text
run text.concat with parts ["hello, ", name, "!"] as greeting
run "printf" with arguments ["%s\n", greeting]
```

```program
accept pattern as text with default "*.log"
run fs.glob with pattern pattern as files
for each f in files
  run "cat" with arguments [f]
```

## Inputs, variables and defaults

| Bash idiom | Execute form | Status | Notes |
| --- | --- | --- | --- |
| `x=value` | `use value "value" as x` | supported | Text, integer, boolean, list and JSON literals all bind with `as`. |
| `$1` | `accept first as text from argument 1` | supported | Argument 1 is the first. <!-- feature: positional_arguments --> |
| `"$@"` | `accept items as list of text from arguments` | supported | Each item is one argument. |
| `$HOME`, `$VAR` | `accept home as text from environment "HOME"` | supported | Environment values are declared inputs, not ambient names. <!-- feature: environment_variable --> |
| `read -p "prompt" x` | `accept x as text from prompt "..."` | supported | Use `from secret prompt "..."` to hide the answer. |
| `${x:-default}` for an input | `accept x as text with default "default"` | supported | |
| `${x:-default}` for a value | `run text.default_if_empty with text x and default "default" as y` | applet | Returns the default only when the text has zero bytes. A space is not empty. <!-- feature: parameter_expansion --> |
| `local x` | Bindings inside a reusable program. | partial | Scope is the reusable program, not a shell function. <!-- feature: local_variable --> |
| `arr=(a b c)` | `use value ["a", "b", "c"] as arr` | supported | Lists are values. Iterate them with `for each`. |
| `"${arr[@]}"` | `for each item in arr`, or pass a declared list of text as the argument list. | supported | |
| `${#arr[@]}` | None. | missing | No length or count form is documented. |
| `declare -A map` | `use value {...} as map`, read with `take` at a JSON Pointer. | missing | Workaround only: keys are fixed JSON Pointers. There is no form for a computed key. <!-- feature: associative_array --> |
| `shift` | Declare each argument, or iterate the list. | partial | Positional names are fixed at declaration. |

```program
accept first as text from argument 1
accept rest as list of text from arguments
run "printf" with arguments ["first: %s\n", first]
for each file in rest
  run "cat" with arguments [file]
```

```program
accept home as text from environment "HOME"
run "printf" with arguments ["%s\n", home]
```

```program
accept verbose as boolean from flag "--verbose" alias "-v"
accept output as text from option "--output" alias "-o" with default "out.txt"
run "printf" with arguments ["%s\n", output]
```

`getopts` maps to one declaration per option. Each flag or option is named in the declaration,
so a parser that reads clustered short flags such as `-abc` is not provided.

```program
use value ["alpha", "beta"] as names
for each name in names
  run "printf" with arguments ["item %s\n", name]
```

```program
use value {"host": "example", "port": 8080} as settings
then encode settings as json
```

## Tests and conditionals

| Bash idiom | Execute form | Status | Notes |
| --- | --- | --- | --- |
| `[[ -f p ]]`, `[ -d p ]`, `[ -r p ]` | `run fs.test with path p and kind "file" as is_file`, or `if run "test" with arguments ["-f", p] succeeds` | applet | `fs.test` kinds are exists, file, directory, symlink, readable, writable, executable and empty. |
| `[[ a == b ]]` | `if a is b` | supported | Exact text, integer or boolean equality. There is no truthiness. |
| `[[ a != b ]]` | `if a is not b` | supported | |
| `[ -z "$x" ]` | `if x is ""` | supported | |
| `[ -n "$x" ]` | `if x is not ""` | supported | |
| `[[ a < b ]]`, numeric compare | `run number.compare ...` | applet | Returns -1, 0 or 1. Inspect it for parameter names. |
| `[[ s == pat* ]]` | `if s contains "text"` for a substring. | partial | Conditions do not match glob patterns. |
| `[[ s =~ regex ]]` | `run "grep" with arguments [...] and keep status as found` | partial | No regex operator. `text.matches` takes a literal needle. Use the native grep. <!-- feature: regex_match --> |
| `if cmd; then ... elif ...; fi` | `if run "cmd" ... succeeds` with `otherwise if` | supported | <!-- feature: conditional --> |
| `case $x in ...` | `accept x as text one of [...]` with an `if` and `otherwise if` chain. | partial | No case statement, and no glob patterns in arms. |
| `command -v tool` | None. | missing | In a command condition a missing executable is an error, not false. Probe the expected path with `fs.test` instead. |
| `while [[ a == b ]]` | `while a is b` | supported | Scalar operands only. |

```program
accept target as text
run fs.test with path target and kind "directory" as is_dir
if is_dir is true
  run "printf" with arguments ["%s is a directory\n", target]
```

```program
accept mode as text one of ["start", "stop", "status"]
if mode is "start"
  run "printf" with arguments ["starting\n"]
otherwise if mode is "stop"
  run "printf" with arguments ["stopping\n"]
otherwise
  run "printf" with arguments ["status\n"]
```

## Loops and lines

| Bash idiom | Execute form | Status | Notes |
| --- | --- | --- | --- |
| `for x in a b c` | `for each x in ["a", "b", "c"]` | supported | <!-- feature: loop --> |
| `for x in $(cmd)` | `run text.lines with text listing as items`, then `for each`. | applet | `text.lines` splits on LF and drops one final terminator. |
| `for ((i=0; i<n; i++))` | `repeat N times`, or `for each` over a list. | partial | `repeat` has no counter binding. |
| `while read line; do ...; done < file` | `read "file"`, `then text.lines as lines`, `for each line in lines`. | applet | Whole-file read. For a large or unbounded stream, use `listen`. <!-- feature: read_builtin --> |
| `while read line` from stdin | `listen to stdin` and `whenever a line arrives as message` | supported | Runs the body once per record. |
| `break`, `continue` | `break`, `continue` | supported | Valid only inside a loop. |
| `while true; do ...; sleep 5; done` | `every 5 seconds` | supported | A persistent service with a monotonic interval. |
| cron entry | `on schedule "0 9 * * 1-5"` | supported | Runs on matching local minutes. |
| `sleep 5` | `run "sleep" with arguments [5]` | supported | Direct executable. |

```program
repeat 2 times
  run "printf" with arguments ["tick\n"]
```

```program
read "input.txt"
then text.lines as lines
for each line in lines
  run "printf" with arguments ["%s\n", line]
```

```program
listen to stdin
whenever a line arrives as message
  then "cat"
```

```program
every 5 seconds
  run "printf" with arguments ["heartbeat\n"]
```

## Functions

| Bash idiom | Execute form | Status | Notes |
| --- | --- | --- | --- |
| `f() { ...; }` | `define a program named f` with its own `accept` lines. | supported | Declare it before the root statements. <!-- feature: function_definition --> |
| `f arg` | `run program f with name arg` | supported | Inputs are explicit. There are no ambient variables. |
| `result=$(f arg)` | `run program f with name arg as result` | supported | |
| `return N` | `return VALUE` in a reusable program. | partial | `return` is valid only in a program defined in the same file. |
| `local` | Bindings inside the reusable program. | partial | See the variable section above. |

```program
define a program named greet
  accept name as text
  run "printf" with arguments ["hello %s\n", name]
run program greet with name "world"
```

```program
define a program named label
  accept name as text
  run "printf" with arguments ["build: %s\n", name]
  return
run program label with name "app"
```

## Strings and numbers

| Bash idiom | Execute form | Status | Notes |
| --- | --- | --- | --- |
| `$((a + b))` | `run number.add ...` | applet | No arithmetic operators in the grammar, on purpose. Operands are JSON numbers. Inspect `number.add` for parameter names. <!-- feature: arithmetic --> |
| `$((a - b))`, `$((a * b))`, `$((a / b))` | `number.subtract`, `number.multiply`, `number.divide` | applet | Division by zero fails. Fractional results use binary64, so decimals are not exact. |
| `((i++))` | `number.add` with the next value, then bind the result. | applet | |
| `${x#prefix}`, `${x%suffix}` | `text.strip_prefix`, `text.strip_suffix` | applet | Removes one literal matching end. There are no glob patterns. |
| `${x/old/new}` | `text.replace` with `old` and `new` | applet | Replaces every literal occurrence. |
| `${x^^}`, `${x,,}` | `text.upper`, `text.lower` | applet | Unicode default case mapping. |
| `${x:offset:len}`, `${#x}` | None. | missing | No substring or length applet is documented. |
| `basename`, `dirname` | `path.basename`, `path.dirname` | applet | String transforms. They do not touch the filesystem. |
| `printf "%s\n" x` | `run "printf" with arguments ["%s\n", x]` | supported | The output of `printf` follows the platform's `printf`. |
| `x=$(cmd)` then trimming | `run "cmd" ... as x`, then `run text.trim_final_lf with text x as y` | applet | A command's bound output keeps its trailing newline. |

```program
accept name as text
run text.strip_suffix with text name and suffix ".log" as stem
run text.replace with text stem and old "-" and new "_" as safe
run text.upper with text safe as shout
run "printf" with arguments ["%s\n", shout]
```

```program
run "jq" with arguments ["-r", ".name", "package.json"] as name
run "printf" with arguments ["%s\n", name]
```

`jq` is not in the grammar. It is a native executable, so it runs as a direct call. For JSON that
Execute already holds, use `then parse json as NAME` and `take`.

## Command substitution and temporary files

| Bash idiom | Execute form | Status | Notes |
| --- | --- | --- | --- |
| `x=$(cmd)` | `run "cmd" with arguments [...] as x` | supported | The binding is the stdout text, including its trailing newline. <!-- feature: command_substitution --> |
| Nested `$(...)` | One binding per command, in statement order. | supported | Execute has no nested expression. |
| `mktemp` | `with temporary file as tmp` | supported | The file is removed when the block ends, even after a failure. |
| `mktemp -d` | `with temporary directory as dir` | supported | Owner-only and removed on exit. |
| `<(cmd)` process substitution | Save each stream to a temporary file, then pass the paths. | missing | There is no FIFO form. See the example below. <!-- feature: process_substitution --> |

```program
run "git" with arguments ["rev-parse", "HEAD"] as head
run text.trim_final_lf with text head as commit
run "printf" with arguments ["%s\n", commit]
```

```program
with temporary file as scratch
  run "printf" with arguments ["draft\n"]
  then save to scratch
  run "cat" with arguments [scratch]
```

```program
with temporary file as left
  with temporary file as right
    run "sort" with arguments ["a"]
    then save to left
    run "sort" with arguments ["b"]
    then save to right
    run "diff" with arguments [left, right]
```

## Redirection and pipes

| Bash idiom | Execute form | Status | Notes |
| --- | --- | --- | --- |
| `cmd1 \| cmd2` | `run "cmd1" ...`, then `pipe to "cmd2" ...` | supported | Both stages run at once, connected by an OS pipe. <!-- feature: pipeline --> |
| `cmd1 \| cmd2` where `cmd2` needs the text | `then "cmd2" ...` | supported | `then` waits for success and passes complete output as stdin. |
| `cmd > file` | `then save to "file"` | supported | Atomically replaces the file with the pipeline value. <!-- feature: redirection --> |
| `cmd >> file` | `then append to "file"` | supported | |
| `cmd 2> file` | `and stderr to "file"` | supported | |
| `cmd 2>/dev/null` | `and discard stderr` | supported | Direct executables only. |
| `cmd 2>&1` | `and stderr merged` | supported | Captured in memory, up to 64 MiB. |
| `cmd >&2` | `and stdout to stderr` | supported | |
| `cmd < file` | `and stdin from "file"` | supported | |
| `cmd <<< "text"` | `use value "text"`, then the command. | supported | |
| `cat <<EOF` with a quoted body | `use value """`, body, `"""`, then the command. | partial | The body is literal. Values that Bash would expand must be bound separately. <!-- feature: heredoc --> |
| `exec 3>file` and numbered descriptors | None. | missing | Use `stderr` and `stdout` suffixes instead. |
| `PIPESTATUS` | None. | missing | Each stage's status is not bound. |

```program
run "printf" with arguments ["a\nb\n"]
pipe to "wc" with arguments ["-l"]
```

```program
run "printf" with arguments ["hello\n"]
then save to "out.txt"
run "printf" with arguments ["again\n"]
then append to "out.txt"
run "tool" with arguments ["-v"] and stderr to "err.log"
run "tool" with arguments ["-q"] and discard stderr
run "wc" with arguments ["-l"] and stdin from "input.txt"
```

```program
use value """
server {
  listen 8080;
}
"""
then "tee" with arguments ["server.conf"]
```

```program
run "sed" with arguments ["s/old/new/", "config.txt"]
then save to "config.txt"
```

## Working directory

| Bash idiom | Execute form | Status | Notes |
| --- | --- | --- | --- |
| `cd dir` | `use folder "dir"` | supported | Applies to the statements that follow. |
| `( cd dir && make )` subshell | None. | partial | There is no subshell block. Set the folder before the statements that need it. |
| `pwd` | `run pwd` | supported | |

```program
use folder "build"
run pwd
```

## Processes, jobs and signals

| Bash idiom | Execute form | Status | Notes |
| --- | --- | --- | --- |
| `cmd &` | `spawn as name` with the command, then `await name` | partial | The job is owned by the run and must be awaited or cancelled before scope exit. <!-- feature: background_job --> |
| `wait $pid` | `await name` | supported | Returns the captured stdout and the status when kept. |
| `kill $pid` for an owned job | `cancel name` | supported | Cancels only that job. |
| `jobs`, `$!`, process ids | `inspect name as state` | supported | The record includes each stage's pid, state and status. |
| `nohup cmd &` | `launch detached "cmd" with arguments [...]` | partial | The run does not wait for or own the child. It produces no pipeline data. |
| `exec cmd` | `hand over to "cmd" with arguments [...]` | partial | Must be the last statement. On Windows the child runs in a job object. <!-- feature: exec_builtin --> |
| `timeout 5 cmd` | `and timeout 5 seconds` | supported | |
| Retry loop around a command | `and retry up to 3 times with backoff 2 seconds doubling` | supported | Retries only retry-safe commands unless `even if uncertain` is given. |
| `VAR=x cmd` | `and environment {"VAR": "x"}` | supported | |
| `env -u VAR cmd` | `and without environment ["VAR"]` | supported | |
| `bg`, `fg`, stopped jobs | None. | by design | Jobs have no stop, continue or terminal handoff states. |

```program
spawn as build
  run "make" with arguments ["all"]
  pipe to "tee" with arguments ["build.log"]
await build and keep status as build_status
run "printf" with arguments ["make exit %s\n", build_status]
```

```program
spawn as watcher
  run "sleep" with arguments [60]
inspect watcher as watcher_state
cancel watcher
```

```program
launch detached "sleep" with arguments [60]
```

```program
run "printf" with arguments ["preparing\n"]
hand over to "tail" with arguments ["-f", "app.log"]
```

```program
run "curl" with arguments ["-fsS", "https://example.test/health"] and retry up to 3 times with backoff 2 seconds doubling
run "tool" with arguments ["--check"] and environment {"LOG_LEVEL": "debug"}
```

## Traps and cleanup

| Bash idiom | Execute form | Status | Notes |
| --- | --- | --- | --- |
| `trap cmd EXIT` | `on exit` with an indented body | supported | Runs on normal completion, `exit`, `fail` and error. |
| `trap cmd INT`, `TERM`, `HUP`, `QUIT` | `on interrupt` with an indented body | partial | Covers SIGINT, SIGTERM, SIGQUIT and SIGHUP on Unix, and Ctrl-C on Windows. Other signals have no form. <!-- feature: trap --> |
| `trap '' SIGPIPE` and other ignores | None. | missing | |
| Cleanup after a failing block | `try` with `finally` | supported | The `finally` body runs after a failure in the block. |

```program
on exit
  run "printf" with arguments ["cleanup\n"]
run "printf" with arguments ["working\n"]
```

## Sourcing, eval, locks and privilege

| Bash idiom | Execute form | Status | Notes |
| --- | --- | --- | --- |
| `source file`, `. file` | `run program file "file" with name value as result` | partial | Runs in process with explicit inputs. Shell variables do not cross the boundary. <!-- feature: source --> |
| `source /etc/os-release` | `read "/etc/os-release"`, `then env.load as osrelease`, `take text at "/ID" from osrelease as distro` | applet | `env.load` reads `KEY=VALUE` files without running them. |
| `eval` | None. | missing | Write the command as a program. <!-- feature: eval --> |
| `flock file cmd` | `execute run FILE --single-instance LOCK` | partial | A run option that refuses to start while another run holds the lock. It is not a per-statement lock. |
| `sudo cmd` | `run "sudo" with arguments ["-n", ...] and keep status as NAME` | partial | Privileged steps are flagged. A capability policy with `deny_privileged` refuses them. Closed stdin means a password prompt cannot be answered. |

```program
read "/etc/os-release"
then env.load as osrelease
take text at "/ID" from osrelease as distro
run "printf" with arguments ["%s\n", distro]
```

```text
accept theme as text with default "light"
run program file "config/fonts.program" with theme theme as fonts
```

The file form loads the named `.program` file while the caller is parsed, so it needs that file to
exist. It is shown as text here and is not part of the parse test.

```program
run "sudo" with arguments ["-n", "systemctl", "is-active", "nginx"] and keep status as active
if active is 0
  run "printf" with arguments ["nginx is running\n"]
```

```program
accept token as text from secret prompt "API token: "
```

## Idioms without a form

These idioms are absent by design or not yet implemented. The Execute route is in the notes.

| Bash idiom | Execute form | Status | Notes |
| --- | --- | --- | --- |
| Arithmetic in expressions | `number.*` applets | applet | Listed above. |
| Regex matching | `grep` or another native matcher | partial | Listed above. |
| `select` menus | None. | missing | |
| Associative arrays with computed keys | JSON objects read by pointer | partial | Listed above. |
| Coprocesses (`coproc`) | `listen` or stream forms | missing | <!-- feature: coproc --> |
| Process substitution as FIFOs | Temporary files | partial | Listed above. |

## Walkthroughs

The line counts of ported Omarchy scripts, and worked before and after examples, are not in this
release. The port report in `tools/omarchy_port_report.py` has the feature table these statuses
are drawn from.
