# Execute

**Readable programs. Native tools. Explicit control flow.**

[![Native checks](https://github.com/monstercameron/NextGenScriptingLanguage/actions/workflows/ci.yml/badge.svg)](https://github.com/monstercameron/NextGenScriptingLanguage/actions/workflows/ci.yml)
[![Playground and Pages](https://github.com/monstercameron/NextGenScriptingLanguage/actions/workflows/pages.yml/badge.svg)](https://github.com/monstercameron/NextGenScriptingLanguage/actions/workflows/pages.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

Execute is a small Rust interpreter for `.program` files that connect existing
command-line tools. It handles pipelines, typed inputs, supervision, caching and
terminal widgets. **Application control flow belongs in the `.program` file.**

```text
run "cat" with arguments ["contacts.txt"]
pipe to "tr" with arguments ["[:upper:]", "[:lower:]"]
pipe to "sort"
pipe to "uniq"
then save to "contacts-clean.txt"
```

These are real native processes connected by OS pipes. Arguments are literal;
running a program requires no shell evaluation, LLM or API key.

**[Try the playground](https://monstercameron.github.io/NextGenScriptingLanguage/playground/)** ·
[Download](https://github.com/monstercameron/NextGenScriptingLanguage/releases) ·
[Documentation](docs/README.md) · [Contribute](CONTRIBUTING.md)

## Quickstart

Download a matching archive from [Releases](https://github.com/monstercameron/NextGenScriptingLanguage/releases)
and follow the [installation guide](docs/releases.md), or build from source:

```sh
git clone https://github.com/monstercameron/NextGenScriptingLanguage.git
cd NextGenScriptingLanguage
cargo build --locked --release
export PATH="$PWD/target/release:$PATH"
execute --version
```

Requires stable Rust to build. Programs require the command-line tools they invoke.
The example above uses `cat`, `tr`, `sort` and `uniq` on macOS or Linux. Create
`contacts.txt`, save the script as `clean-contacts.program`, then:

```sh
printf '%s\n' 'SAM@example.com' 'lee@example.com' 'sam@example.com' > contacts.txt
execute check clean-contacts.program
execute explain clean-contacts.program
execute clean-contacts.program
```

Both stdout and `contacts-clean.txt` contain:

```text
lee@example.com
sam@example.com
```

`check` validates without running commands. `explain` also prints the plan and argv.
Relative paths resolve from the calling directory. Quoted executable calls work
without manifests; named applets use the shipped catalog or `--manifest-dir PATH`.
Keep both runtime executables and the catalog together when extracting a release.

## The language in one minute

```text
accept path as text
accept lines as integer with default 20
read path
pipe to "head" with arguments ["-n", lines]
```

Save as `preview.program` and run:

```sh
execute preview.program --input 'path=server log.txt' --input lines=10
```

| Form | Contract |
| --- | --- |
| `run "tool" with arguments [...]` | Start a native command with literal argv and closed stdin. |
| `then tool with parameter value` | Wait for success, then pass complete output to a named applet. |
| `pipe to "tool"` | Connect adjacent native stages concurrently with backpressure. |
| `read "file"` / `read from stdin` | Supply bytes to the pipeline. |
| `as result` | Name finite UTF-8 output, preserving trailing newlines. |
| `and keep status as code` | Capture an exit status for explicit branching. |
| `and keep result as execution` | Bind an execution record with outcome, pipeline stage metadata and captured raw bytes. |
| `if code is 0` / `otherwise` | Choose an indented body using typed scalar equality. |
| `then parse json as data` | Parse JSON for typed field selection and encoding. |
| `and cache` | Cache an explicitly cacheable named transformation. |
| `then save to "output.txt"` | Atomically replace a file with the current output. |

Use one statement per line, double-quoted JSON strings, `#` comments and consistent
space indentation. Computation stays in tools such as `jq`, `awk` and `sort`.
Wildcards and shell substitutions do not expand automatically.
See the [practical guide](docs/guide.md) and [language reference](docs/language.md).

## One file, several lifetimes

Finite scripts, persistent native streams, per-record listeners and scheduled
services share the same command vocabulary. The script declares its lifetime:

```text
every 30 seconds
  run pwd
```

Listeners keep their producer alive and backpressure it while handlers run.
Intervals default to skipping missed ticks; cron uses the local calendar.
Current source can print review-only systemd/launchd service definitions for
triggered workflows. [Explore streams and services](docs/guide.md#one-file-several-lifetimes).

## Terminal apps with control flow you can read

Single-file screens define component layouts, ports, widgets and event handlers.
Rust renders the interface; `.program` handlers decide when native commands run.

```sh
execute examples/top-cpu.program
execute examples/github-stars.program
```

The [CPU monitor](docs/top-cpu.md) uses `ps` and `jq`; the
[GitHub star tracker](docs/github-stars.md) uses authenticated `gh` and includes
a session history graph. The [widget catalog](docs/tui-catalog.md) distinguishes
implemented elements from design ideas.

## Inspect and edit a running app

The opt-in [live-control CLI](docs/live-control.md) exposes runtime and TUI widget
state through local IPC, with JSON responses, revisions, atomic patches and
lifecycle controls. Transport is a Unix socket or a native Windows named pipe.
[Runnable examples](examples/live/README.md) include an operator console.

## Cache work. Repair failures. Resume progress.

Explicit caches avoid repeating deterministic transformations. Checkpoints record
completed nodes so a repaired script can resume its remaining work:

```sh
execute repair.program --checkpoint .execute-state/repair --trace
execute resume repair.program --checkpoint .execute-state/repair --trace
```

Checkpointing does not make external effects transactional. Uncertain effects
require explicit retry. See the [recovery contract](docs/language.md#recover-after-a-failure).

<a id="bring-another-cli"></a>
<a id="150-useful-programs-with-inputs-and-outputs"></a>
<a id="explicit-ai-decisions-ordinary-program-control-flow"></a>

## Examples and integrations

| Collection | What to explore |
| --- | --- |
| [150 utilities](examples/ladder/README.md) | Useful file, log, data, CI and release workflows with inputs, outputs and exact fixture oracles. |
| [100 model workflows](examples/models/README.md) | Explicit Jev/LLM calls, typed results and script-owned routing; offline fixtures by default. |
| [Native applet catalog](manifests) and [applet contracts](docs/applet-contracts.md) | Readable named parameters mapped directly to argv, with optional typed values, outcomes and execution backends. Direct quoted calls need no adapter. |
| [Typed applet graphs](docs/graph.md) | Source-build graph files with preflighted typed ports, bounded event queues and `check`/`inspect`/`explain`/`run` commands. |
| [Security and data-flow policies](SECURITY.md) and [data-flow guide](docs/security-dataflow.md) | Optional applet capability/sandbox policy and separate sensitivity, remote-origin, persistence and cache-purity policy. |
| [External program modules](docs/program-modules.md) | Source-build, versioned local program packages with explicit dependency locks and bounded resolution. |
| [Model operations](docs/models.md) | TypeSafe Jev classification, judgment and scoring; OpenAI generation and JSON extraction. |
| [VS Code extension](editors/vscode/README.md) | Syntax highlighting, snippets and grammar tests. |
| [Language server and discovery](docs/language-server.md) | Source-build stdio LSP diagnostics/completion, local reusable-program navigation and rename, safe applet discovery and typed port matching. |
| [Browser playground](docs/playground.md) | Rust/WASM language core, terminal, virtual files and ten runnable workflows with checked outputs and generated reports. |

The browser uses bounded Rust applet ports and buffered pipelines. Installed
executables, native pipes, services, model calls and TUI screens use native Execute.

## Performance, with receipts

Execute parses once and reuses programs for services. Native pipe payloads flow
directly between tools. Process launch cost, buffering and each applet still
matter; Bash remains faster in some recorded workloads.

[Benchmark methods and raw results](benchmarks/README.md) document workloads,
boundaries, source hashes and known differences. Historical measurements describe
their recorded versions. Browser virtual-applet timings do not measure native
processes. The [paired Omarchy success harness](benchmarks/omarchy_success.py)
is prepared separately from correctness coverage and has no result yet. No result
establishes a universal speed advantage or full Bash parity.
The [five-hour session summary](benchmarks/session-2026-10-14-summary.json)
indexes the latest measured changes, security costs and remaining gaps.

<a id="release-and-source-status"></a>

## Scope and current limits

The project targets small, readable Unix glue programs on macOS and Linux, with
a native Windows port on x64 and ARM64. `0.9.0-rc.4` is the published multi-platform
release candidate. Current `main` also includes the browser playground and
[repeat, for each and while loops](docs/language.md#loops-current-source).

The current source candidate also adds [versioned applet contracts](docs/applet-contracts.md),
generic bundled operations through `execute-applet`, and optional capability
policy with a separate, opt-in OS sandbox for supported command and worker
applets. A bounded journaled `fs.transaction`
supports recovery for file-only batches, with partial visibility while updates
apply. These features are source-only until a new release is published; the
retained 0.8.0 binary and published 0.9.0-rc.4 packages remain unchanged.

Current source also includes [versioned external program modules](docs/program-modules.md),
scheduled module graph nodes, finite task and graph APIs, and native desktop
protocol groundwork. These are
source-candidate interfaces and do not complete the remaining jobs, richer
redirection, or Omarchy desktop acceptance gates.

The [data-flow policy](docs/security-dataflow.md) is a separate opt-in boundary
for remote egress, cache/checkpoint/save persistence and reviewed cache purity.
It complements capability policy; neither interface infers sensitivity or
proves arbitrary native code safe.

The source also provides a stdio [Language Server Protocol server](docs/language-server.md)
for diagnostics, completion, local reusable-program navigation and rename, plus
manifest-based applet discovery and typed-port matching. These editor and
discovery interfaces do not infer contracts from executable help output.

Execute is still evolving toward the [Bash and Omarchy proof gates](docs/production-readiness.md).
Native commands run with the caller's permissions;
they are not automatically sandboxed. A configured capability policy checks
declared applet grants, filters environment variables and can require an OS
sandbox for supported command and worker backends. In-process operations do not
run inside that sandbox. Flatpak has a separate application-wide sandbox contract; see
[security boundaries](SECURITY.md) for details.

The retained `bin/execute` is historical **0.8.0**. Published **0.9.0-rc.4**
packages do not include the source-candidate features above; build the current
source to try them. Published releases and historical evidence retain their
original contents. See [version history](CHANGELOG.md),
[platform contracts](docs/windows.md) and [release validation](docs/releases.md).

## Build and verify

```sh
make help           # Available contributor commands
make setup          # Locked editor and playground npm dependencies
make check          # Native Rust, Python harness and editor checks
make playground-check  # WASM build, formatter and runtime contracts
make playground-browser-check  # Real Chromium acceptance checks
```

See [Contributing](CONTRIBUTING.md) for prerequisites, browser installation,
platform-specific tests and measured performance work. Tests use local fixtures;
provider credentials are not needed.

## Documentation

Start at the **[documentation index](docs/README.md)** for current guides,
architecture, contribution paths and a separate map of historical evidence.

Licensed under the [MIT License](LICENSE).

Security boundaries and remaining isolation gaps are documented in [SECURITY.md](SECURITY.md).
