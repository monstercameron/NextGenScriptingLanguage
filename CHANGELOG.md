# Changelog

Published versions are immutable. This file highlights user-facing changes;
[release receipts](docs/releases.md) and [CI](https://github.com/monstercameron/NextGenScriptingLanguage/actions)
record exact source revisions and validation. Historical reports retain their
original version labels.

## Unreleased

- Native and browser `repeat`, `for each` and `while` blocks, with typed outer
  updates, iteration-local names, bounded execution and whole-loop recovery.
  Includes three native utilities and a browser batch workflow; source builds only.
- VS Code source package 0.2.2 highlights loop headers and adds three checked
  loop snippets.

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
