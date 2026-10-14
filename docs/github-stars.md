# A single-file GitHub star tracker

From this project directory, with GitHub CLI installed and authenticated:

```sh
bin/execute examples/github-stars.program
```

The default repository is `morluto/rea` (Reverse Engineer Anything). Type any
`owner/repo` and press Enter, or choose from the saved list. Tab and Shift-Tab
move focus; arrows move the list selection; Enter activates fields and buttons.
Ctrl-U clears the repository field, Ctrl-R refreshes, and Ctrl-Q quits.
Use a terminal at least 80 columns by 24 rows for the full layout.

The layout comes first in the `.program` file. Each named program declares its
own widgets, incoming/outgoing text ports, and event handlers. `connect` sends
the chooser's repository name to the tracker. Applet parameters refer directly
to incoming port names or text-field names; native steps keep existing
`run`, `then`, and `pipe to` syntax. JSON projections are explicit.

```text
show a line chart named history with key "nameWithOwner"
label the horizontal axis of history "session seconds"
label the vertical axis of history "stars"

whenever repository arrives
  run gh.repo with repository repository
  then add field "stargazerCount" to history
```

`gh.repo` maps to the real command below, with the repository passed as one argv
entry after an explicit `--` terminator, so repository text cannot become a CLI
flag. No shell is involved.

```sh
gh repo view --json nameWithOwner,stargazerCount,forkCount,description,url,updatedAt -- morluto/rea
```

The table shows stars and forks. The line graph plots star-count samples against
elapsed session seconds, adding a point after each successful manual or 60-second
refresh. It retains the latest 120 points per repository and at most 16 repository
series. A new session starts with one point; it does not fetch historical counts.

Queries run off the UI thread, with a 30-second timeout. Repository changes cancel
old queries and discard stale results. Repeated events coalesce in bounded queues.
Failures appear in the status area; stderr is captured and limited so it cannot
corrupt the terminal. Quitting cancels owned commands and restores terminal mode.

For a single live query and a machine-readable snapshot, including a 100×30
rendered frame:

```sh
bin/execute examples/github-stars.program --headless
bin/execute check examples/github-stars.program
bin/execute explain examples/github-stars.program
```

`check` and `explain` launch no commands. Interactive `--max-runs N` exits after
N completed command handlers, once queues drain. Headless mode runs initialization
once without interval ticks. UI checkpoint/resume is not supported yet.

This tracker uses eight widget types: heading, text field, list, table,
text panel, badge, button, and line chart. Widgets stack in their component's
column; `give <widget> <number> rows` sets a height. Root layout supports one or
two component columns and a percentage width for the left component. Connections
carry text, and cycles are rejected before execution. The `.program` explicitly
binds Ctrl-R to `press refresh` and Ctrl-Q to `close the screen`. Button names
have no special meaning to the runtime. The wider [element catalog](tui-catalog.md)
is a design inventory; those other elements and properties are proposals.

The app uses read-only repository queries. GitHub authentication remains with
`gh`; the tracker neither stars repositories nor changes remote data.

## Control flow belongs to the program

Rust provides parsing, rendering, widget editing, event delivery, applet execution,
process cleanup, and bounded-state operations. The `.program` chooses which events
invoke which actions, when to query, how actions are sequenced, which ports connect,
where results go, and how application failures are displayed. A button named
`refresh` does nothing until a script handler gives it behavior.

```text
whenever repository arrives
  press refresh

whenever key "Ctrl-R" is pressed
  press refresh

every 60 seconds
  press refresh

whenever refresh is pressed
  run gh.repo with repository repository
  then show the result as rows in metrics
  then add field "stargazerCount" to history

whenever key "Ctrl-Q" is pressed
  close the screen
```

Shortcuts are global to the placed components and must be unique. They accept
ASCII letters/digits, `Ctrl-`/`Alt-` plus a letter/digit, `Escape`, and `F1` through
`F12`. Unmodified letter bindings also consume text-field keystrokes, so this app
uses Ctrl bindings. Tab, Enter, arrows, Backspace, and Ctrl-U retain generic widget
editing/activation behavior. Ctrl-C and OS termination signals remain host-level
emergency interruption. Ctrl-Q is an application binding, with no implicit action.

`press` dispatches the named button's script handler. Button-action cycles are
rejected. `close the screen` must be the final action and cancels owned workers on
exit. Application status updates require explicit script actions; unhandled errors
remain available as runtime diagnostics. A failing failure-handler chain stops
instead of recursively generating further failure events.

## Handler values and required inputs

A handler captures its input ports and referenced widget text when it starts.
Typing a draft while a command runs does not alter that handler's later `send`
or `show` values. Submitting the draft starts the corresponding script handler.
A script's own `show` or JSON field write updates both its widget and the current
handler's value, so a subsequent applet receives the value just written.
Changed linked ports still cancel and invalidate the old component worker.

Before any handler action runs, all ports referenced by its native steps must
exist. A pending handler waits for those ports; unused declared ports do not
block it. Ready handlers run in queue order, skipping handlers still waiting for
inputs. Repeated pending events coalesce by handler, retaining the latest event.
Headless initialization reports missing required inputs if no worker can make
progress. Interactive waiting permits editing, other ready handlers, and quit.

Text fields hold at most 512 UTF-8 bytes and cannot contain NUL. Text port messages
hold at most 64 KiB and cannot contain NUL. Known invalid literals fail `check`
before any applet runs; dynamic values are checked before updating a field or
delivering a message. Native binary pipes continue to carry arbitrary bytes.
`--headless --max-runs` is rejected: initialization and the interactive job limit
have different semantics.

Headless initialization has a separate total deadline: the native pipeline budget
plus five seconds (35 seconds with the default budget). A multi-step initialization
can reach this total before an individual pipeline reaches its own budget. Neither
limit bounds a blocking filesystem operation or an escaped descendant.
