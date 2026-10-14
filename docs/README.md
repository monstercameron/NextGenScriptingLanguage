# Execute documentation

Use a [release download](releases.md) or build current `main` for the native runtime.
The [browser playground](https://monstercameron.github.io/NextGenScriptingLanguage/playground/)
runs a bounded Rust/WASM host. The retained repository binary is historical 0.8.0.
[Version history](../CHANGELOG.md) separates published releases from unreleased work.

## Learn and use Execute

| Guide | Purpose |
| --- | --- |
| [Quickstart](../README.md#quickstart) | Install, validate and run your first native pipeline. |
| [Practical guide](guide.md) | Inputs, composition, services, terminal apps, AI routing and recovery. |
| [Language reference](language.md) | Exact syntax, type rules, stdin, status, JSON, scopes, cache and checkpoints. |
| [Persistent streams](streaming.md) | Framing, byte transport, buffering, cancellation and producer lifetimes. |
| [Live-control API](live-control.md) | Runtime and widget CRUD, revisions, events and lifecycle controls. |
| [Model operations](models.md) | Explicit Jev/LLM adapters, offline fixtures, credentials and limits. |
| [Browser playground](playground.md) | Browser applets, virtual files, memory limits and local builds. |
| [Releases and installation](releases.md) | Platform packages, checksums, provenance and immutable releases. |
| [Native Windows](windows.md) | Executable lookup, Job Objects, Unicode, named pipes and platform differences. |
| [VS Code extension](../editors/vscode/README.md) | Install highlighting, snippets and editor support. |

## Find a useful program

- [150 utility workflows](../examples/ladder/README.md): data contracts, local fixtures and exact outputs.
- [100 model workflows](../examples/models/README.md): offline routing cases and explicit live inference.
- [Live examples](../examples/live/README.md): editable runtime and widget state.
- [CPU monitor](top-cpu.md) and [GitHub star tracker](github-stars.md): complete single-file terminal apps.
- [Widget catalog](tui-catalog.md): implemented elements and the wider design inventory.
- [Applet manifests](../manifests): named parameters mapped directly to argv.

## Develop and evaluate

| Reference | Purpose |
| --- | --- |
| [Contributing](../CONTRIBUTING.md) | Toolchains, shared check commands, source map and change discipline. |
| [Architecture](../DESIGN.md) | Interpreter design, process supervision, state and retained design decisions. |
| [Tooling map](../tools/README.md) | Packaging, release and documentation helpers. |
| [Browser host](../browser/README.md) / [web client](../web/README.md) | Shared language core, WASM ABI and frontend tooling. |
| [Pages site](site.md) | Build, preview, deployment and source-backed showcase data. |
| [Benchmark methods](../benchmarks/README.md) | Workloads, raw distributions, comparison boundaries and historical reports. |
| [Production gates](production-readiness.md) | Remaining Bash/Omarchy acceptance work. |
| [Replacement challenge](replacement-challenge.md) | Proof gates, stress scenarios and evaluation protocol. |
| [Thesis ledger](thesis-progress.md) | Original goals, implemented capabilities and open requirements. |

## Historical evidence

Reports apply to their recorded source hashes, binaries and environments. They are
retained evidence, rather than current scores or proof for later changes.

- [0.9.0-rc.4 release receipt](release-validation-0.9.0-rc.4.json): published package digests and platform validation.
- [0.9.0-dev validation](production-validation-0.9.0-dev.json): Unix and production-gate measurements before the Windows port.
- [0.8.0 validation](validation-v0.8.0.json): the retained packaged generation.
- [0.6.1 audit](full-testing.md) and [0.4.5 refinement](refinement.md): earlier coverage, adversarial checks and limits.
- [Optimization checkpoint](optimization-session-2026-10-14-report.md): the completed bounded campaign.
- [Retained benchmark artifacts](../benchmarks/optimization-pass/README.md) and [patch preimages](../work/README.md): historical optimization evidence.

Original user-provided reviews and thesis files remain in this directory for
traceability. They express requirements or historical opinions; current contracts
are defined by the reference guides, source and tests. Check the
[current CI runs](https://github.com/monstercameron/NextGenScriptingLanguage/actions)
for the exact revision being evaluated.
