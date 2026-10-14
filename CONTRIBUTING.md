# Contributing to Execute

Keep application control flow in `.program`. Rust supplies generic parsing,
native process supervision, data transport and widgets. Preserve literal argv,
bounded capture, cancellation, local scopes and explicit uncertain-effect retry.
The [language reference](docs/language.md) describes these contracts; the
[thesis ledger](docs/thesis-progress.md) separates implemented capabilities from
the roadmap and outstanding proof gates.

## Set up and check

Use current stable Rust, Python 3.9+, Node.js 22+, npm, `make`, `jq`, and standard
Unix tools on macOS or Linux. Tests use private temporary fixtures and fake
external services; GitHub authentication and model-provider credentials are not
required. Install the editor's locked development dependencies once:

```sh
npm --prefix editors/vscode ci --ignore-scripts --no-audit --no-fund
make check
```

`make check` runs formatting, warnings-denied Clippy, the release Rust suite,
Python harness acceptance checks and editor tests against the freshly built
interpreter. Rust tests include the utility collection and live CLI/socket/TUI
scenarios. It also runs all 100 model workflows against local fixtures; no provider
keys or paid requests are needed. The real calendar-minute test may wait up to 75 seconds. Ignored
profiling and timing probes are opt-in; passing this command does not run them.
CI uses the same command on macOS and Ubuntu. Native Windows runs portable
library/model tests and the PE fixture suite on x64 and ARM64:

```powershell
$target = "x86_64-pc-windows-msvc" # ARM64: aarch64-pc-windows-msvc
cargo build --locked --target $target --release --bins --example native-fixture
cargo test --locked --target $target --release --test windows_native --test models --test opt_manifest_regressions
cargo test --locked --target $target --release --lib
```

See [Windows contracts](docs/windows.md) and the [release workflow](docs/releases.md).

Use `make rust-check` when working only on Rust, `make build` for a release build,
and `make proof-smoke` for a small streaming/lifecycle check. The smoke report
goes into ignored `target/`. `CARGO`, `PYTHON` and `NPM` can override executable
names. Existing toolchain environment variables continue to work normally.

## Find the right code

| Area | Files |
| --- | --- |
| CLI parsing and discovery | `src/main.rs`, `src/inputs.rs` |
| Syntax and same-file definitions | `src/parser.rs`, `src/parser/reusable.rs`, `src/ast.rs` |
| Native execution and structured values | `src/runtime.rs`, `src/runtime/`, `src/structured.rs` |
| Live state and control socket | `src/live/` |
| Platform jobs, handles, ACLs and signals | `src/platform.rs`, `src/platform/`, `src/runtime/*_process.rs` |
| SemVer, archives and installers | `tools/release.py`, `tools/package.py`, `.github/workflows/` |
| Terminal UI | `src/ui/` |
| Model transport and provider validation | `src/models.rs`, `src/bin/execute-model.rs`, `tests/model_control.rs` |
| Model workflows and exact offline oracles | `examples/models/`, `tests/model_catalog.py` |
| Applet argv contracts | `manifests/` |
| Runtime regressions and owned fake tools | `tests/`, `tests/fixtures/` |
| Useful programs and their fixture runner | `examples/ladder/` |
| Editor grammar, snippets and tokenizer tests | `editors/vscode/` |
| Measurements and historical evidence | `benchmarks/`, `docs/validation-*/` |

Add focused regressions for behavior changes, especially validation before
effects, byte preservation, failure cleanup and checkpoint/live boundaries.
Use owned temporary paths/processes. Do not make tests depend on the user's
repositories, credentials, network, desktop configuration or installed release.

## Measure and retain evidence

Run timing comparisons separately from builds and tests. Preserve the baseline
and candidate release hashes, identical workloads and native argv, independent
output/status checks, warmups, interleaved samples and raw distributions. Keep
intentional Bash differences explicit. See [benchmark methods](benchmarks/README.md).
The Unix matrix needs all of its listed system tools and is an explicit broader
check, rather than a dependency of `make check`.

Historical reports are evidence for their recorded versions. Retain them and
label new results with their actual source/binary/environment. Do not overwrite
an installed release or existing release receipts to make them describe pending
source changes. A source build can support more syntax than packaged 0.8.0.

Keep build output in `target/` and local caches/state in ignored directories.
Never commit `.env` files, credentials, OS metadata or `node_modules`. External
campaign scratch belongs outside the repository; the retained `work/` patches
are historical evidence. Run `git status --short` before preparing a change so
unrelated edits remain intact. No benchmark or test result establishes a full
Bash or desktop replacement by itself.
