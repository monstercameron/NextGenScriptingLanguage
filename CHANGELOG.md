# Changelog

Published versions are immutable. This file highlights user-facing changes;
[release receipts](docs/releases.md) and [CI](https://github.com/monstercameron/NextGenScriptingLanguage/actions)
record exact source revisions and validation. Historical reports retain their
original version labels.

## Unreleased

- Graphs accept ordinary typed `accept` declarations, named CLI inputs and
  positional arguments. Static checks and inspection describe unresolved
  inputs; runtime inputs resolve before graph effects. See [typed graphs](docs/graph.md).
- Typed applet discovery suggests bounded multi-stage converter plans and
  retains only the requested top-ranked endpoint pairs. Plans describe declared
  compatibility, not conversion costs or losslessness. See [discovery](docs/language-server.md).
- Bounded finite `csv.parse` and `csv.stringify` applets preserve UTF-8 fields
  as text and expose a typed `{headers, rows}` contract. Malformed quoting,
  duplicate headers and ragged records fail. See [CSV applets](docs/csv.md).
- Repair Windows process/listener API mismatches and target-specific compiler
  diagnostics, and update the Linux graph stress fixture for enum schemas.
- In-process applets now cooperatively check cancellation, the per-call command
  timeout and the parent total deadline during bounded JSON transforms, text
  transforms and JSONC scanning. Synchronous OS calls and parser/serializer
  library calls remain non-interruptible; started applet interruption reports
  uncertain effects. See [applet contracts](docs/applet-contracts.md).
- Versioned applet contracts add typed inputs and outputs, explicit outcome
  routing, result envelopes, named typed ports, per-applet environment changes,
  and `capture`/`discard`/`inherit` stdio policies.
  Command, in-process and persistent JSON-lines worker backends share the same
  call syntax; see [applet contracts](docs/applet-contracts.md).
  Envelopes include classified effect certainty so callers can route uncertain
  outcomes without guessing from status codes.
- New `execute-applet` helper and bundled generic path, text, numeric, color,
  filesystem and JSONC operations. Single-file writes use content-hash compare
  and swap, locking and atomic replacement. A separate bounded `fs.transaction`
  applet retains a journal for file-only recovery; it applies files one by one,
  so visibility is partial during the batch and external effects are not
  covered. The helper is included by current source packaging scripts.
- Optional strict v1 capability policy checks canonical applet grants, declared
  capabilities and environment allowlists. `sandbox_required` separately opts
  into fail-closed macOS/Linux command sandboxing; this is not implied by
  metadata, and in-process operations cannot use the OS sandbox. See
  [security boundaries](SECURITY.md#applet-contracts-and-optional-capability-policy).
- Native and browser `repeat`, `for each` and `while` blocks, with typed outer
  updates, iteration-local names, bounded execution and whole-loop recovery.
  Includes three native utilities and a browser batch workflow; source builds only.
- VS Code source package 0.2.2 highlights loop headers and adds three checked
  loop snippets.
- Source builds provide a stdio LSP server for diagnostics, completion, local
  reusable-program navigation/rename, applet discovery and typed-port matching;
  see [language-server guide](docs/language-server.md).
- Source builds accept typed applet graph files with preflighted ports, bounded
  event queues and inspectable `check`/`inspect`/`explain`/`run` surfaces; see
  [graph guide](docs/graph.md). The graph limits do not bound arbitrary applet
  I/O or memory.
- Optional `--data-flow-policy` controls labeled remote egress and plaintext
  persistence separately from applet capabilities. Review-only
  `execute service generate systemd|launchd` output is also available in source;
  it never installs or starts a service. See the [data-flow guide](docs/security-dataflow.md)
  and [service generation guide](docs/guide.md#generate-a-reviewable-service-definition).

- Browser terminal playground on GitHub Pages, with the Rust/WASM language core,
  virtual files, seven runnable workflows, typed inputs, branches and cache traces.
- Bounded browser applet ports and a worker lifecycle separate from native OS
  processes, streaming and services; see [browser contracts](docs/playground.md).
- Automated WASM, native-output comparison and real-browser acceptance checks,
  build receipts, dependency notices and Pages deployment.
- VS Code source package 0.2.1 adds repository metadata and validates snippets
  with the current source runtime; package filenames derive from its version.
- Clearer documentation entry points, contributor commands, formatting checks
  and GitHub contribution templates.

## 0.9.0-rc.4 — 2026-10-14

First published multi-platform release candidate:

- Native Windows x64 and ARM64 execution, byte pipes, Job Object supervision,
  Unicode paths, checkpoints, services, widgets and named-pipe live control.
- Six native archives, DEB/RPM packages and sandboxed Flatpak bundles, with
  extracted-install checks, checksums and GitHub build provenance.
- Finite stdin, typed environment and positional inputs, native status bindings,
  scalar conditions, structured JSON and same-file reusable finite programs.
- Explicit Jev and LLM adapters with typed routing and offline fixture workflows.

[Release](https://github.com/monstercameron/NextGenScriptingLanguage/releases/tag/v0.9.0-rc.4) ·
[Validation receipt](docs/release-validation-0.9.0-rc.4.json) ·
[Platform contracts](docs/windows.md)

Earlier 0.9.0 candidate tags retain publication and packaging fixes. They were
not published releases and must not be treated as equivalent downloads.

## Earlier versions

The retained `bin/execute` is **0.8.0**. Its
[validation receipt](docs/validation-v0.8.0.json), the
[0.6.1 audit](docs/full-testing.md), and
[historical benchmark records](benchmarks/README.md) preserve the older evidence.
See [Git history](https://github.com/monstercameron/NextGenScriptingLanguage/commits/main/)
for changes preceding this changelog.
