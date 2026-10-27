# Rust/WASM browser host

This private crate builds the `wasm32-unknown-unknown` library used by the
[playground](../docs/playground.md). It depends on the root `execute` library;
WASM selects `src/browser.rs` instead of the native process host.

Both hosts share the AST, parser, input types, JSON helpers, manifest mapping,
validation, conditions and reusable-program rules. The browser supplies a finite
execution loop, virtual filesystem and bounded Rust applet ports. Native pipes,
services, model calls, TUI rendering and live IPC remain in the native host.
Reusable-program calls can capture a failed applet's status and continue in the
caller. `exit with status` and `fail with message` remain unsupported in browser
program bodies and are rejected before execution.

The ABI uses `allocate`, `execute` and `release` with owned byte buffers. Requests
and responses are JSON. The module has no host imports; the JS bridge must consume
each input allocation and release each returned response exactly once. Read the
safety comments in `src/lib.rs` before changing the ABI.

Build from the repository root:

```sh
rustup target add wasm32-unknown-unknown
make setup
make playground-check
```

`tools/build_playground.py` builds from this crate's directory, so
`.cargo/config.toml` applies the 64 MiB memory cap and stack limit. It then bundles
the web assets and license texts. Build output goes under ignored `browser/target/`.
The standalone lockfile includes the root path dependency; `tools/release.py`
updates that dependency version together with the canonical root version.

The [web client](../web/README.md) owns worker transport and browser acceptance.
The [playground guide](../docs/playground.md) defines supported applets, flags,
resource limits and the difference between Stop and native checkpoints.
