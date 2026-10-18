# Browser client

This directory owns the playground UI, worker bridge, bundled xterm.js terminal,
fixtures and browser acceptance harness. Application syntax and control flow run
in Rust; JavaScript does not interpret `.program` statements.

From the repository root, with stable Rust, the WASM target, Node 24 and Python 3:

```sh
make setup
rustup target add wasm32-unknown-unknown
make playground-check
npm --prefix web exec -- playwright install chromium
make playground-browser-check
make site-serve
```

Open `http://127.0.0.1:8080/playground/`. On Linux, Playwright may additionally
require its documented system dependencies; CI uses `playwright install --with-deps`.

| File | Responsibility |
| --- | --- |
| `app.mjs` | Editor, terminal input, files, sharing, traces and worker recovery. |
| `worker.mjs` | Loads the WASM module and handles requests off the UI thread. |
| `abi.mjs` | Owned-buffer JSON transport across the Rust ABI. |
| `build.mjs` | Bundles local assets and emits a source/hash receipt. |
| `examples.json`, `fixtures.json` | Thirty runnable workflows (script, group, hint, exact expected output and files) and the virtual files they read. |
| `benchmark.mjs` | Checked worker and round-trip latency samples, with timer limits. |
| `test/` | WASM contracts, native-output comparisons, per-demo scenario tests and real browser interactions. |

Run `npm --prefix web run format` to format owned JS, JSON and CSS, or
`npm --prefix web run format:check` to check without modifying files. The locked
formatter skips generated lockfiles and dependencies. Generated site assets live
under ignored `docs/playground/`; change source here or the tracked HTML there.
The Python build helper adds full dependency license texts after bundling.

The browser supports typed, process-free `use value VALUE as NAME` bindings.
Bound results stay internal; explicitly reuse text with `use value NAME` or
encode structured values with `then encode NAME as json` to produce stdout.
Unbound commands stream when no later pipeline consumer needs their bytes.
The complete browser stdout response remains limited to 1 MiB.

The demos also use `json.parse`, `json.stringify`, `json.set`, and `text.format`.
Browser `text.format` supports `escape "plain"`; other escape modes and caching
these four data applets are refused. Set `EXECUTE_TEST_WASM` to test a WASM build
in a lane-specific target directory without replacing generated site assets.

See the [browser contract](../docs/playground.md), [Rust host](../browser/README.md),
and [contributor guide](../CONTRIBUTING.md) for execution limits and the full check matrix.

## Adding a demo

A demo is an entry in `examples.json` plus any new virtual files in `fixtures.json`.
Fixture names are flat and shared by every demo, and the page seeds one program file
per demo, so keep the total (fixtures, programs and files a full run generates) under
the 128-file limit; `test/demos.test.mjs` fails above 120.

| Field | Meaning |
| --- | --- |
| `id`, `file` | Unique id for `#example=ID` links and the `.program` file name. |
| `group` | Heading in the picker. Consecutive demos with the same group share a heading. |
| `title`, `subtitle` | Button text, at most 40 and 70 characters. |
| `hint` | One sentence under the picker naming an edit to try. |
| `source`, `stdin` | The program and optional preloaded standard input. |
| `expected`, `expectedFiles` | Exact stdout and the files the run writes. |
| `previewFile` | A generated dashboard JSON (`title`, `status`, up to eight `metrics`) shown as cards. |

Write the program with the applets the browser host has (see the
[playground guide](../docs/playground.md)), run `npm test`, and add a scenario test that
edits a fixture and checks the changed result. The test suite runs each demo under WASM
and compares its stdout and files with native Execute, so every demo must produce the
same bytes in both. Point `EXECUTE_TEST_BINARY` at a native build, and run the parity
tests where GNU `grep`, `sort`, `uniq`, `wc`, `head` and `printf` are on `PATH`: on
Windows, the `printf` bundled with Git for Windows mis-parses double quotes in
arguments, so printf-built JSON fails there while it passes on Linux and macOS.
