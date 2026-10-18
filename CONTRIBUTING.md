# Contributing to Execute

Keep application control flow in `.program`. Rust supplies generic parsing,
native process supervision, data transport and widgets. Preserve literal argv,
bounded capture, cancellation, local scopes and explicit uncertain-effect retry.
The [language reference](docs/language.md) describes these contracts; the
[thesis ledger](docs/thesis-progress.md) separates implemented capabilities from
the roadmap and outstanding proof gates.

## Set up and check

Use current stable Rust, Python 3.9+, Node.js 24, npm, `make`, `jq`, and standard
Unix tools on macOS or Linux. Tests use private temporary fixtures and fake
external services; GitHub authentication and model-provider credentials are not
required. Install the locked editor and playground dependencies once:

```sh
make setup
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

## Build features

`cargo build --locked --release` produces the **lean** build: no terminal UI, no
model helper, no live control plane, no remote backend, no Quickshell renderer, no
DAP adapter and no language server. `--features full` adds all of them (`tui`,
`model`, `desktop`, `dap`, `remote`, `live`, `lsp`); `tui` implies `live`. Release
packages, `make check` and most CI jobs build with `full`, and a Linux job keeps the
lean build compiling and testing (`--no-default-features`) against a size budget in
`tools/linux-minimal-binary-budget.json`. A lean build refuses a gated feature with
a message that says to use the full build, and manifests declare `build_feature` so
the two applets that need `remote` and the Quickshell applet that needs `desktop`
report `requires_full_build`. The release profile uses fat LTO and one codegen
unit; the `dist` profile inherits it for packaged artifacts, and an opt-in `min` profile
(`opt-level = "z"`, `panic = "abort"`, for example with a musl target) builds a much
smaller binary but does not unwind panics, so releases do not use it. Two more features exist
for tests only: `failpoints` (fault injection for the crash-consistency tests) and
`fuzzing` (the fuzz entry points). Neither is part of `full`.

Use `make rust-check` when working only on Rust, `make build` for a release build,
and `make proof-smoke` for a small streaming/lifecycle check. The smoke report
goes into ignored `target/`. `CARGO`, `PYTHON` and `NPM` can override executable
names. Existing toolchain environment variables continue to work normally.

## Browser, documentation and formatting

The native check does not require a WASM target or browser installation. For the
playground, install those once, then use the shared targets:

```sh
rustup target add wasm32-unknown-unknown
npm --prefix web exec -- playwright install chromium
make playground-check
make playground-browser-check
make site-serve
```

The preview serves `http://127.0.0.1:8080/`; use `PORT=9000` to change the port.
The [browser contract](docs/playground.md), [Rust host](browser/README.md) and
[web source map](web/README.md) describe the separate native/browser boundaries.
On Linux, Chromium may need additional OS dependencies; the CI job installs them
with Playwright's `--with-deps` option.

`make format` formats owned Rust and playground source. `make format-check`
checks without editing. Rustfmt explicitly includes `src/runtime/shared.rs`, which
is shared through `include!`; Prettier is pinned in the web lockfile and checks JS,
JSON and CSS. Generated assets, vendored code, lockfiles and historical reports
are not reformatted. `.editorconfig` and `.gitattributes` define editing and
cross-platform line-ending conventions.

`make docs-check` verifies local files and fragments in maintained Markdown.
Historical reports and user-provided reviews are retained verbatim. Add new
maintained entry points to `tools/check_docs.py`; the [documentation index](docs/README.md)
keeps guides separate from evidence. The [changelog](CHANGELOG.md) distinguishes
unreleased features from immutable published versions.

## Continuous integration and nightly checks

Every workflow is in `.github/workflows/`, and every action is pinned by commit SHA.

| Workflow | Runs | What it does |
| --- | --- | --- |
| `ci.yml` (Native checks) | Every push to `main` and every pull request | Release-contract and workflow lint, `check_unsafe.py`, `make check` on Ubuntu and macOS, native Windows x64 and ARM64, Linux binary-size budgets (full and lean), the Omarchy port comparison, the latency and hot-path budgets. Each `main` commit has its own concurrency group and runs to completion. |
| `bash-compare.yml` | Every push to `main` | Builds the default lean binary (and, as variants, an `opt-level = "s"` build, a static musl build and a musl `min`-profile build) on x86_64 and arm64 Linux and compares each with bash and dash; see [benchmark methods](benchmarks/README.md#execute-against-bash-and-dash). Reports only; it does not fail on a loss. |
| `security.yml` | Push, pull request, weekly | `cargo audit` and `cargo deny`, gitleaks over the history, a refusal of tracked env files, CodeQL (advisory), dependency review. |
| `fuzz.yml` | Push and pull request (replay); nightly (fuzzing) | Replays seeds and saved crashes; nightly runs each of eleven libFuzzer targets for 20 minutes. |
| `sanitizers.yml` | Nightly | Miri on the pure modules, AddressSanitizer and ThreadSanitizer on library unit tests. |
| `crash-consistency.yml` | Nightly | Random kills and injected checkpoint write failures, then resume, plus journal and parser property tests. |
| `soak.yml` | Nightly | One-hour service, live-controlled service and graph runs that assert flat memory, descriptors and threads (`tools/soak.py`). |
| `flaky.yml` | Nightly | Runs the full suite 20 times per OS with `cargo nextest` and no retries, then lists each test that failed as `flaky` (some runs) or `failing` (every run). |
| `platforms.yml` | Weekly | Minimum supported Rust (1.89.0), Linux arm64 library tests, and a static musl build on Alpine. |
| `pages.yml` | Pushes and pull requests that touch the playground inputs | WASM build, playground checks and GitHub Pages deployment. |
| `packages.yml`, `release.yml` | Manual dispatch, `v*` tags | Package validation and the SemVer release; see [releases](docs/releases.md). |

The remaining workflows (`desktop-live-acceptance`, the `*-hosted-replication` and
`graph-recv-any-hosted-paired` benchmarks, `mixed-runtime-probe` and
`real-bash-100-runner-contract`) run by hand and produce retained evidence, not a
pass or fail gate for changes.

The nightly workflows have scheduled triggers plus `workflow_dispatch`, so any of
them can be run on a branch before relying on it. Reproduce a CI failure locally with
the command in the failing step; `make check` covers the `ci.yml` suite, and
`python3 tools/port_compare.py --binary target/release/execute` the port
comparison.

## Find the right code

| Area | Files |
| --- | --- |
| CLI parsing and discovery | `src/main.rs`, `src/cli/`, `src/inputs.rs` |
| Daemon, checkpoint status, gate, explain, dap | `src/agent/` |
| Checkpoint journal, element log, resume | `src/runtime/checkpoint.rs`, `src/runtime/checkpoint/`, `src/runtime/resume.rs` |
| Bundled manifest index | `src/bundled_manifests.rs`, `src/bundled_manifests/` (regenerate with `EXECUTE_UPDATE_BUNDLED_INDEX=1 cargo test`) |
| Fuzz targets and saved crashes | `fuzz/fuzz_targets/`, `fuzz/regressions/`, `src/fuzz_entry.rs` |
| Omarchy ports and their Bash comparison | `examples/omarchy-ports/`, `tools/port_compare.py` |
| Syntax and same-file definitions | `src/parser.rs`, `src/parser/reusable.rs`, `src/ast.rs` |
| Native execution and structured values | `src/runtime.rs`, `src/runtime/`, `src/structured.rs` |
| Live state and control socket | `src/live/` |
| Platform jobs, handles, ACLs and signals | `src/platform.rs`, `src/platform/`, `src/runtime/*_process.rs` |
| SemVer, archives and installers | `tools/release.py`, `tools/package.py`, `.github/workflows/` |
| Terminal UI | `src/ui/` |
| Model transport and provider validation | `src/models.rs`, `src/bin/execute-model.rs`, `tests/model_control.rs` |
| Model workflows and exact offline oracles | `examples/models/`, `tests/model_catalog.py` |
| Shared host validation and argv rules | `src/runtime/shared.rs`, `src/runtime/control.rs`, `src/runtime/calls.rs` |
| Rust/WASM host and ABI | `src/browser.rs`, `src/browser/`, `browser/` |
| Browser UI, worker, fixtures and acceptance | `web/`, `docs/playground/` |
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

## Prepare a contribution

Keep changes focused and describe the observable problem, resulting behavior and
checks actually run. Include a small `.program` and owned fixtures when changing
syntax or runtime contracts. Update the current guides and `CHANGELOG.md` for
user-visible changes; preserve historical source hashes and reports. Issue forms
and the pull-request template prompt for reproduction and platform details.

Run `git diff --check` and inspect the staged file list before committing. Do not
use broad cleanup commands against unknown local files, installed releases or
ignored directories that may contain a contributor's data.

## Security checklist

Every change that touches a path, a process, the terminal, secret values or a
resource limit answers the [threat model](docs/threat-model.md) for its vector.
Before opening a pull request, check the items that apply:

- Data that comes from applet output, environment, inputs, file contents or file
  names is untrusted. Do not build a path, an argument list or a shell command from it
  without the existing checks.
- Path operations do not follow symlinks on the opened file, and checks use the file
  descriptor that is opened, not a separate earlier lookup. Add a race test when the
  change adds a check followed by an open.
- Native commands get literal argument vectors. Do not introduce a shell for an
  ordinary command, and do not forward environment variables that are not allowlisted.
- Diagnostics and streamed output that can contain untrusted text escape control
  bytes (ESC, CSI, OSC and C1) before they reach a terminal.
- Secret prompt values stay out of traces, checkpoints, records, explain and
  what-if output, and error text. Add the value to the leak test for the surface.
- New reads, buffers, queues and recursion have a bound, and the bound is tested
  with input at and just over the limit.
- Adversarial tests live in `tests/security_*.rs`. A change that makes one fail
  is a security regression, not a flaky test: fix the change or document the hole in
  the threat model with its severity before merging.

## Rust quality policy and staged checks

Keep syntax, typed values and validation safe and independent of platform effects.
Those pure modules forbid unsafe Rust. Put OS operations in the platform/runtime
boundary, document the ownership and lifetime invariants at each unsafe boundary,
and keep unsafe operations explicit even inside unsafe functions. Public unsafe
APIs require `# Safety` documentation. Both Cargo packages deny debug macros;
intentional fixture panics and profiling assertions remain valid. Prefer borrowed
values and fallible typed conversion to repeated shape checks plus `unwrap`.
Preserve bounded transport and existing diagnostic/serialization contracts when
refactoring. Do not rewrite vendored dependencies or generated playground assets.

Enable the tracked pre-commit hook after installing the prerequisites:

```sh
rustup component add rustfmt clippy
rustup target add wasm32-unknown-unknown
cargo fetch --locked
cargo fetch --manifest-path browser/Cargo.toml --locked
python3 tools/install_hooks.py
```

Installation sets repository-local `core.hooksPath=.githooks`, which also applies
to its Git worktrees. The hook checks a temporary snapshot of **staged content**;
partial staging is preserved, and an unstaged fix cannot hide a staged error.
It runs native/browser Rustfmt, the shared `include!` file's Rustfmt check and
native/browser warnings-denied Clippy, using only fixed tool commands. Clippy runs
offline and reuses ignored `target/pre-commit` output. Missing tools, targets or
locked dependencies fail clearly; the hook never installs tools, fetches network
dependencies, formats files or changes the index. `CARGO`, `RUSTFMT`, `PYTHON`,
`CARGO_HOME`, `RUSTUP_HOME` and `CARGO_TARGET_DIR` overrides are honored.

Only changes to owned Rust sources, Cargo manifests/locks or Cargo/Rustfmt/Clippy configuration
trigger these checks. Vendored/generated-only changes are excluded. CI remains
the authoritative platform matrix; the local hook checks its host and WASM.
Run `python3 tests/staged_rust.py` to verify staging isolation and hook behavior.

## Minimum supported Rust version

The minimum supported Rust version is **1.89.0** for the full feature set
(`cargo check --locked --features full --lib --bins --tests`). The platform workflow
(`.github/workflows/platforms.yml`, job `msrv`) checks it on every scheduled run. Raising
it needs a changelog entry and a reason, such as a dependency that requires a newer
compiler. Continuous integration uses the current stable compiler and the pinned
release-job compiler (1.99.1) for reproducible binaries.
