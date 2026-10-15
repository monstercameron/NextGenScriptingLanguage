# Browser playground

[Open the live playground](https://monstercameron.github.io/NextGenScriptingLanguage/playground/).

Edit a `.program`, press **Run program** (Cmd/Ctrl+Enter), and inspect the terminal,
virtual files and execution trace. **Check** and **Explain** use the Rust validator.
Nothing calls an LLM, logs into a server, or reads your computer's files.

Ten workflows have fixtures and checked stdout and generated files. Start with
**Incident command**, **Launch control**, or **Support signal**: each combines
several inputs, branches and reusable programs into reports and a dashboard.
Edit a regional incident log, release candidate, or support queue and rerun to
change the actual counts, decisions and generated artifacts. Dashboard cards
render bounded JSON written by the current execution; retained files from an
older run cannot masquerade as fresh results. Direction-control characters are
shown as visible escapes, and markup stays literal text.

The other seven workflows cover log triage, a typed release gate, a reusable
shortlist, explicit caching, exit-status branching, stdin cleanup, and a batch file loop.
Try changing `checks_passed` in `release.json`, or enter `{"limit":"2"}` in
**Inputs & stdin**. Input values use the same CLI type parser as native Execute.
Click any virtual file to edit it; `.program` files open in the main editor.
Import reads only the file you select. Download saves the open script. Share puts
only the source into a URL fragment: recipients must supply any additional files
and inputs. A shared script loads for inspection and never runs automatically.

The terminal supports quoted literal arguments and command history:

```sh
help
ls
cat app.log
execute log-triage.program --input severity=WARN
execute check shortlist.program
execute explain shortlist.program
execute shortlist.program --input limit=2
```

This launcher does not parse shell pipelines, substitutions, wildcard expansion or
redirections. Express pipelines and branches in the `.program` editor.

## What actually executes

The compiled artifact is a real Rust `wasm32-unknown-unknown` module, not a
JavaScript translation of the language. `src/ast.rs`, `src/parser.rs`,
`src/inputs.rs`, `src/structured.rs`, `src/manifest.rs`,
`src/runtime/shared.rs`, and the control/call helpers are shared source with the
native runtime. The browser host in `src/browser.rs` supplies a separate finite
execution loop, virtual files, memory caching, and Rust applet implementations.
Application branching, finite loops and program calls execute in Rust from the parsed script.
JavaScript supplies xterm.js, the editor, worker transport, and UI rendering.

The browser cannot spawn your installed executables. Its applets are deliberately
small Rust ports, **not native binaries, BusyBox, a Linux VM, or full coreutils**.
Finite pipelines buffer bytes between stages; native Execute keeps native
concurrent pipes and backpressure. No browser result proves native performance.
Schedules, listeners, streaming mode, TUI screens, model calls, checkpoint/resume
and live-control sockets are native-only and are rejected here.

| Browser applet | Supported contract |
| --- | --- |
| `pwd`, `cd` | Virtual working directory; `cd` takes one existing directory. Terminal `cd` persists; workflow cwd stays local to that execution. |
| `ls` | One path, `-1`, `-a`, `-A`, `--`; one name per line, sorted. Both hidden-file flags include hidden names but omit `.` and `..`. |
| `mkdir` | One or more paths, `-p`, `--`. |
| `cat` | Literal file paths or pipeline input, `--`, `-` for the provided input. No other options. |
| `echo` | Literal argv joined by spaces, optional `-n`; no escape expansion. Quoted direct calls only. |
| `printf` | A single format pass: `%s`, signed `%d`, `%%`, `\n`, `\r`, `\t`, `\\`. No width/precision, repeated format, or `%b`. |
| `sort` | Pipeline input, `-r`, `-u`, `--`; UTF-8 byte order, no locale or numeric mode. |
| `uniq` | Adjacent input lines, optional `-c`; counts use a seven-column field. |
| `wc` | Pipeline input; one of `-l`, `-w`, `-c`, or all three by default. Compact decimal output; byte count for `-c`, Unicode whitespace for `-w`. |
| `head` | Pipeline input; `-n COUNT`, default 10; nonnegative counts only. |
| `grep` | Pipeline input and one literal pattern; **`-F` required**, separate flags `-i`, `-v`, `-n`, `--`; no regular expressions. No match exits 1. Case-insensitive matching uses Unicode lowercase. |
| `true`, `false` | No arguments; status 0 or 1. |

Named applets embed the repository's original manifests, including parameter
validation and argv mapping. `count lines` maps to `wc -l`. Quoted direct calls
use literal argument lists; unavailable names fail before any workflow effects.
Supported names and grammar do not imply support for every native flag. A later
runtime flag or I/O failure does not roll back earlier completed file writes.

`cache` remains explicit and requires a cacheable manifest. The browser cache
hashes binary name, argv, cwd, input and all virtual file contents, so changed
file arguments cannot reuse stale data. It is an in-memory cache, separate from
native disk caches. File changes may invalidate unrelated cached applets too.

## Boundaries and lifecycle

The module has **zero host imports**: no filesystem, process, socket, timer or
network capabilities. The page downloads its assets only from the Pages origin;
no runtime CDN, provider key, or backend is required. Dependencies and license
notices are bundled at build time. The page uses a restrictive CSP. Program
output is text, never HTML, and control characters are stripped for terminal
display. Raw bytes remain inside the Rust pipeline; the UI displays UTF-8 with
replacement for invalid sequences.

Limits are 64 KiB source, 4,096 expanded steps, 1 MiB output/file, 128 files,
128 directories, 4 MiB total file bytes, and a 256-entry / 4 MiB payload cache.
Loops share a 100,000-iteration guard per invocation. Trace collection stops at
4,096 entries or 1 MiB with an explicit `trace truncated` marker; execution
continues within its other bounds.
The linker caps WASM linear memory at 64 MiB. Exceeding memory or stack capacity
can trap; the UI replaces that worker. Each worker request has an 8-second
watchdog. **Stop** or terminal Ctrl+C terminates the worker and restores the last
completed request's files, directories and cwd; cache and in-flight effects are
lost. This is not native resumable checkpointing. Reload clears the session;
**Reset files** restores all fixtures and clears the cache.

Worker timing includes JSON request decoding, parse/validation, applet execution,
response serialization, and a complete virtual-file snapshot. It excludes
initial module download/compilation, transport and UI work. The optional browser
benchmark records both worker and round-trip distributions with raw samples;
it is not a Bash comparison or a native-process benchmark.

## Build and verify

Requires stable Rust with the WASM target, Node 24, npm, and Python 3:

```sh
rustup target add wasm32-unknown-unknown
npm --prefix web ci --ignore-scripts --no-audit --no-fund
cargo build --locked --release --bin execute
python3 tools/build_playground.py
npm --prefix web test
npx --prefix web playwright install chromium
npm --prefix web run test:browser
python3 -m http.server 8080 --directory docs
```

Open `http://localhost:8080/playground/`. The helper builds from `browser/` so
its checked-in linker memory limit applies. Opening `index.html` with `file://`
cannot load a module worker; use HTTP. Generated JS, CSS, WASM and `build.json`
are ignored. `docs/playground/index.html`, Rust, fixtures and UI sources are
tracked. `build.json` contains the WASM SHA-256 and source hashes.

The WASM tests execute the module in Node, including six exact-output native
comparisons. Playwright checks real module workers, file editing, terminal input,
cache state, errors, Stop/restart, downloads, shared URLs, CSP, and mobile width.
Stop's integration test deliberately holds a worker request at the transport
boundary; it proves termination/recovery, not the timing of a busy WASM workload.
Screenshots and reports go into ignored `target/playground/`.

`.github/workflows/pages.yml` builds and tests before uploading `docs/` and
publishing via the official Pages actions. PRs run the same checks without
publishing. Native release packages and their SemVer pipeline remain independent.
