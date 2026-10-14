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
| `examples.json`, `fixtures.json` | Runnable scripts, inputs and exact expected output. |
| `benchmark.mjs` | Checked worker and round-trip latency samples, with timer limits. |
| `test/` | WASM contracts, native-output comparisons and real browser interactions. |

Run `npm --prefix web run format` to format owned JS, JSON and CSS, or
`npm --prefix web run format:check` to check without modifying files. The locked
formatter skips generated lockfiles and dependencies. Generated site assets live
under ignored `docs/playground/`; change source here or the tracked HTML there.
The Python build helper adds full dependency license texts after bundling.

See the [browser contract](../docs/playground.md), [Rust host](../browser/README.md),
and [contributor guide](../CONTRIBUTING.md) for execution limits and the full check matrix.
