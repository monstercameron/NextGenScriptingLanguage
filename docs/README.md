# Execute documentation

Execute is an AI-first, complete Unix/POSIX scripting language that humans can observe,
with a thin grammar and computation in applets. It is not yet a Bash replacement; the
production gates, listed under explanation below, say what is missing.

The latest published release is 0.9.0-rc.4; current source is 0.9.0-rc.5, unpublished.
The retained repository binary is historical 0.8.0. The default source build is lean, and
`--features full` adds the terminal UI, model helper, live control, remote backend, desktop
renderer, debug adapter and language server. The
[browser playground](https://monstercameron.github.io/NextGenScriptingLanguage/playground/)
runs a bounded Rust/WASM host.

This index is grouped by reader need: tutorials for a first run, how-to guides for a
specific task, reference for exact behaviour, and explanation for design and status.
Evidence reports are listed last, outside the main path.

## Tutorials

Start here if Execute is new to you. Each page builds on the one before it.

| Document | Purpose |
| --- | --- |
| [Quickstart](../README.md#quickstart) | Install, validate and run your first native pipeline. |
| [Practical guide](guide.md) | A tour of inputs, composition, scheduling, review-only service generation, terminal apps, AI routing and recovery. |
| [Omarchy quickstart](omarchy-quickstart.md) | A source build on an Omarchy desktop, a Hyprland toggle, a test with a fake `hyprctl` and a user service. |

## How-to guides

Each entry answers one practical question.

| Document | Purpose |
| --- | --- |
| [Migrating from Bash](migrating-from-bash.md) | Common Bash idioms mapped to their Execute form or applet, with a status for each. |
| [Install a release](releases.md) | Platform packages, checksums, provenance and immutable releases. |
| [Run on native Windows](windows.md) | Executable lookup, Job Objects, Unicode, named pipes and platform differences. |
| [Run in the browser](playground.md) | Browser applets, virtual files, memory limits and local builds. |
| [Build and preview the site](site.md) | Building, previewing and deploying the GitHub Pages showcase. |
| [Use the VS Code extension](../editors/vscode/README.md) | Install highlighting, snippets and editor support. |
| Run the desktop and server recipes: [desktop-focus-window](recipes/desktop-focus-window.program), [desktop-notify-message](recipes/desktop-notify-message.program), [desktop-pacman-query](recipes/desktop-pacman-query.program), [desktop-volume-osd](recipes/desktop-volume-osd.program), [server-backup-archive](recipes/server-backup-archive.program), [server-check-package](recipes/server-check-package.program), [server-disk-usage](recipes/server-disk-usage.program), [server-http-health](recipes/server-http-health.program), [server-journal-errors](recipes/server-journal-errors.program), [server-large-files](recipes/server-large-files.program), [server-rotate-logs](recipes/server-rotate-logs.program) | Short, runnable `.program` files for common jobs, each with its run line and the Bash it replaces. The desktop recipes need Omarchy, Hyprland or Arch tools. |
| [CPU monitor](top-cpu.md) | A complete single-file terminal app that uses `ps` and `jq`. |
| [GitHub star tracker](github-stars.md) | A complete single-file terminal app that uses authenticated `gh` and keeps a session history graph. |
| [Utility workflows](../examples/ladder/README.md) | 150 utility workflows with data contracts, local fixtures and exact outputs. |
| [Model workflows](../examples/models/README.md) | 100 model workflows with offline routing cases and explicit live inference. |
| [Live examples](../examples/live/README.md) | Editable runtime and widget state. |
| [Task groups](../examples/tasks/README.md) | Finite `parallel` and `race` operations with cancellation and reaping. |
| [Omarchy ports](../examples/omarchy-ports/README.md) | Thirty Omarchy commands as `.program` files, twenty of them compared with their Bash originals by `tools/port_compare.py`. |

## Reference

Exact behaviour, interfaces and version promises.

| Document | Purpose |
| --- | --- |
| [Language reference](language.md) | Core syntax, language rules, command forms and structured JSON. |
| [Formal grammar](grammar.md) | The EBNF grammar and the modifier and context table. |
| [Runtime reference](runtime-reference.md) | Operational interfaces, environment variables, the warm daemon and recovery details. |
| [Stability and deprecation](stability.md) | Which interfaces carry a compatibility promise and how changes are announced. |
| [Version history](../CHANGELOG.md) | Published releases and unreleased work, kept separate. |
| [VS Code extension changes](../editors/vscode/CHANGELOG.md) | Version history of the editor extension. |
| [Language server and applet discovery](language-server.md) | LSP diagnostics, completion and navigation, safe executable discovery and typed port matching. |
| [Applet contracts](applet-contracts.md) | Manifest discovery, typed boundaries, explicit outcomes, execution lifetimes and security limits. |
| [Applet manifests](../manifests) | Named parameters mapped directly to argv. |
| [CSV applets](csv.md) | Finite UTF-8 CSV parsing and stringifying with typed table contracts. |
| [Typed applet graphs](graph.md) | Validated applet nodes, typed event and record ports, bounded queues, runtime limits and renderer components. |
| [Program modules](program-modules.md) | Versioned external program packages, dependency locks, source hashes, path rules and graph limits. |
| [Remote applets](../examples/remote/README.md) | Typed endpoint descriptors, origin authorization, bounded transport, events and uncertain outcomes. |
| [Data-flow policy](security-dataflow.md) | Sensitivity labels, remote-origin grants, persistence controls, cache-purity checks and exact-value secret redaction. |
| [Persistent streams](streaming.md) | Framing, byte transport, buffering, cancellation and producer lifetimes. |
| [Live-control API](live-control.md) | Runtime and widget CRUD, revisions, events and lifecycle controls. |
| [Model operations](models.md) | Explicit Jev/LLM adapters, offline fixtures, credentials and limits. |
| [Widget catalog](tui-catalog.md) | Implemented terminal widget elements and the wider design inventory. |
| [Checkpoint format](internals/checkpoint-format.md) | The on-disk checkpoint layout used for recovery. |
| [JSON schemas](schemas/) | Machine-readable schemas for errors, explain output, checkpoint status, graph events and the REPL. |
| [Check result schema](check-result.schema.json) | Machine-readable schema for check results. |
| [Manual page](man/execute.1) | The `execute` command-line manual. |
| [Contributing](../CONTRIBUTING.md) | Toolchains, shared check commands, the source map, build features and change discipline. |
| [Tooling map](../tools/README.md) | Packaging, release and documentation helpers. |
| [Browser host](../browser/README.md) | The shared language core, WASM ABI and browser host. |
| [Web client](../web/README.md) | The frontend tooling for the web client. |
| [Benchmark methods](../benchmarks/README.md) | Workloads, raw distributions and comparison boundaries. |

## Explanation

Why the system is shaped as it is, what it promises, and where it stands.

| Document | Purpose |
| --- | --- |
| [Architecture](../DESIGN.md) | Interpreter design, process supervision, state and retained design decisions. |
| [Threat model](threat-model.md) | Assets, trust boundaries, attacker models, controls with their tests, and known holes H1-H19 with severities. |
| [Security policy](../SECURITY.md) | Security boundaries, how to report a vulnerability and the remaining isolation gaps. |
| [Production gates](production-readiness.md) | Remaining Bash and Omarchy acceptance work, the performance policy, the Bash and dash comparison and the CI evidence status. |
| [Replacement challenge](replacement-challenge.md) | Fifteen proof gates for replacing Bash scripts, with the stress scenarios and evaluation protocol. |
| [Thesis ledger](thesis-progress.md) | Original goals, implemented capabilities and open requirements. |

<a id="historical-evidence"></a>

## Evidence

These reports, logs and data files record results for specific source hashes, binaries and
environments. They are retained for traceability. They are not current scores, and they are
not proof for later changes. Check the
[current CI runs](https://github.com/monstercameron/NextGenScriptingLanguage/actions)
for the exact revision being evaluated.

### Reports and reviews

| Document | Purpose |
| --- | --- |
| [0.6.1 full test audit](full-testing.md) | Earlier coverage, adversarial checks and limits. |
| [0.4.5 adversarial refinement](refinement.md) | Independent review and fixes from October 13 and the limits they recorded. |
| [Optimization session plan](optimization-session-2026-10-14.md) | The bounded eight-hour campaign plan. |
| [Optimization checkpoint](optimization-session-2026-10-14-report.md) | The completed bounded campaign and its results. |
| [Audit resolution ledger](audit-resolution.md) | The 92 numbered audit findings with their verification status. |
| [Unix compatibility review](unix-compatibility-review.md) | The supplied review measured against the implementation. |
| [Supplied Unix review](unix-review-source.md) | The user-provided review text, kept verbatim. |
| [Supplied replacement challenge](replacement-challenge-source.md) | The user-provided challenge text, kept verbatim. |
| [Supplied thesis](thesis-source.md) | The user-provided project thesis, kept verbatim. |
| [Language quality rubric](language-quality-rubric.md) | The rubric used for the adversarial refinement. |
| [Model workflow ideas](model-workflow-ideas.md) | A catalog of design proposals for model-assisted workflows. These are not runnable programs. |
| [Parallel result accounting](parallel-result-accounting-follow-up.md) | Bounds and accounting for parallel results. |

### Data and validation records

| Document | Purpose |
| --- | --- |
| [Release receipt 0.9.0-rc.4](release-validation-0.9.0-rc.4.json) | Published package digests and platform validation. |
| [0.9.0-dev validation](production-validation-0.9.0-dev.json) | Unix and production-gate measurements before the Windows port. |
| [0.9.0-dev validation logs](production-validation-0.9.0-dev/) | Logs and CI output behind the 0.9.0-dev validation. |
| [macOS arm64 matrix 0.9.0-dev](unix-matrix-v0.9.0-dev-macos-arm64.json) | Unix compatibility matrix for macOS arm64. |
| [0.8.0 validation](validation-v0.8.0.json) | Validation record for the retained packaged generation. |
| [0.8.0 validation logs](validation-v0.8.0/) | Logs and smoke output behind the 0.8.0 validation. |
| [0.7.0 validation](validation-v0.7.0.json) | Validation record for 0.7.0. |
| [0.7.0 validation logs](validation-v0.7.0/) | Logs and smoke output behind the 0.7.0 validation. |
| [0.6.1 full validation](full-validation-v0.6.1.json) | Full validation record for 0.6.1. |
| [0.6.1 validation logs](validation-v0.6.1/) | Test, clippy and stress logs for 0.6.1. |
| [0.6.1 macOS arm64 coverage](coverage-v0.6.1-macos-arm64.json) | Coverage data for 0.6.1 on macOS arm64. |
| [Model integration validation](model-integration-validation.json) | Model adapter validation record. |
| [Model integration logs](model-integration-validation/) | Model adapter runs, workflow logs and live smoke results. |
| [Live-control validation](live-control-validation.json) | Live-control validation record. |
| [Utility validation](utility-validation.json) | Utility workflow validation record. |
| [VS Code extension validation](vscode-extension-validation.json) | Editor extension validation record. |
| [VS Code extension validation 0.1.1](vscode-extension-validation-v0.1.1.json) | Editor extension validation record for 0.1.1. |
| [VS Code extension validation 0.2.0](vscode-extension-validation-v0.2.0.json) | Editor extension validation record for 0.2.0. |
| [Replacement gates](replacement-gates.json) | Machine-readable replacement proof gates. |
| [Refinement scorecard](refinement-scorecard.json) | Machine-readable scorecard for the adversarial refinement. |
| [Omarchy inventory](omarchy-inventory.json) | Inventory of Omarchy commands used for the ports. |
| [Audit resolution status](audit-resolution-2026-10-14.json) | Machine-readable status of the audit findings. |
| [Retained benchmark artifacts](../benchmarks/optimization-pass/README.md) | Historical optimization benchmark output. |
| [Patch preimages](../work/README.md) | Historical patch preimages for the optimization work. |
| [Applet-boundary benchmark harness](../benchmarks/applet_boundaries.py) | Same-code command, in-process and worker measurements. No results are published until a final release build is authorized. |
| [Omarchy success-path benchmark harness](../benchmarks/omarchy_success.py) | Paired saved-baseline and current-source applet scenarios, checked against the Bash oracle before timing. |
| [Bash comparison tool](../tools/bash_compare.py) | Produces the Linux head-to-head table against bash and dash. |
| [Omarchy port comparison tool](../tools/port_compare.py) | Compares the Omarchy ports with their Bash originals. |

The original project files and the historical reports stay in this directory so that links
from older records still resolve. Current contracts are defined by the reference guides,
the source and the tests.
