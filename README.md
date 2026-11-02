# Execute

**Readable programs. Native tools. Explicit control flow.**

[![Native checks](https://github.com/monstercameron/NextGenScriptingLanguage/actions/workflows/ci.yml/badge.svg)](https://github.com/monstercameron/NextGenScriptingLanguage/actions/workflows/ci.yml)
[![Playground and Pages](https://github.com/monstercameron/NextGenScriptingLanguage/actions/workflows/pages.yml/badge.svg)](https://github.com/monstercameron/NextGenScriptingLanguage/actions/workflows/pages.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

Execute is a small Rust interpreter for `.program` files that connect existing
command-line tools. It handles pipelines, typed inputs, supervision, caching and
terminal widgets. **Application control flow belongs in the `.program` file.**

**Where it is going.** Execute aims to be an AI-first, complete Unix/POSIX scripting
language whose programs a human can read and observe. *Complete* means the work Bash
is used for: processes, pipes, files, redirection, environment, signals, jobs,
scheduling and exit statuses, without being limited to what Bash can express. *AI-first*
means a model can write, check, review and repair a program through machine-readable
interfaces (`check --json`, `explain --json`, stable diagnostic codes, `--what-if`
plans, run records), and *observable* means a person can read the same program and see
what it will do before it runs. The grammar stays thin glue; computation goes to
applets (the manifests in [`manifests/`](manifests) and the in-process applets in
`src/applets/`) and to the native tools you already have. It is not yet a Bash
replacement; [Scope and current limits](#scope-and-current-limits) says what is missing.

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

Requires stable Rust to build (minimum 1.89.0). Programs require the command-line
tools they invoke. `cargo build --locked --release` builds the lean default: no
terminal UI, model helper, live control, remote backend, Quickshell renderer, debug
adapter or language server. Add `--features full` for those; release packages are
built with `full`. A lean build says so when you use a gated feature. The lean default
still includes graphs, screens, records, tooling, module tools and agent advisory
features; `--no-default-features` builds the smaller scripting core without them.
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
without manifests; named applets use the shipped catalog or an explicit
`--manifest-dir PATH`/legacy JSON file. The shipped catalog is also embedded in the
binary as an index, so a default-path run parses a manifest only when a program uses
it (see [CLI and manifests](docs/language.md#cli-and-manifests)). A packed catalog can
be selected with `--manifest-catalog FILE`; this is opt-in and mutually exclusive with
`--manifest-dir`. Keep runtime executables and the selected catalog together
when extracting a release. The working directory is never searched for manifests.

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
| `spawn as job` / `inspect job as state` / `await job` / `cancel job` | Own a bounded background native pipeline in finite runs, handlers, or service setup. |

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

## Check, review and replay, before and after a run

A program can be inspected without running it, which is what lets a person or a model
review it:

```sh
execute check report.program --json        # static validation, stable diagnostic codes
execute explain report.program --json      # the static effect plan: writes, network, process, cwd
execute run report.program --what-if       # plan plus a fingerprint; runs nothing
execute run report.program --confirm FINGERPRINT   # runs only if the plan is unchanged
execute help report.program --json         # inputs and flags from `accept ... described as`
```

Diagnostics carry stable codes and, where one exists, a machine-applicable fix.
`--json-errors` makes runtime errors structured, `--record` keeps a run for replay
without side effects, and `execute test` and `execute run-parts` run program tests and
directories of programs. See [machine-readable checks](docs/language.md#machine-readable-checks),
[review a plan](docs/language.md#review-a-plan-before-it-runs) and the
[record and replay guide](docs/guide.md#record-a-run-show-what-failed-and-replay-it).

`execute compile --to sh FILE` writes POSIX `sh` for the subset of programs that have a
faithful `sh` form and refuses the rest by name and line; see
[compile to POSIX sh](docs/language.md#compile-to-posix-sh).

Native applet processes and the bundled model helper inherit the host environment by default. Set
`EXECUTE_CHILD_ENV_ALLOW=PATH,HOME,LANG,TERM` to pass only those named variables;
an empty value passes none. The allowlist also applies to daemon-forwarded runs
and intersects with capability-policy grants. You can also use `--env-allow PATH,HOME` on a
single invocation or `--clear-env` to pass none; these flags work with `run`, `run-parts`,
service generation, and `model`. See the [environment reference](docs/runtime-reference.md#warm-daemon---daemon).

## A warm daemon for repeated runs

`execute daemon serve` provides per-user status and management on Unix. Run forwarding is
disabled by default because the daemon cannot preserve the client's OS confinement. `--daemon` and `EXECUTE_DAEMON=1` execute locally with a notice.
Set `EXECUTE_DAEMON_ACCEPT_CONTEXT=1` to explicitly accept the daemon context and
restore forwarding. The daemon bounds concurrent forwarded runs; set
`EXECUTE_DAEMON_MAX_RUNS` when starting it to configure the limit. At capacity,
additional clients fall back to local execution. Avoid `EXECUTE_DAEMON=1` in
sandboxed environments with older versions that forward without this opt-in.
See the [daemon section](docs/runtime-reference.md#warm-daemon---daemon).

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
implemented elements from design ideas. Screens need the terminal UI, so build with
`--features full` (a lean build says so).

## Inspect and edit a running app

The opt-in [live-control CLI](docs/live-control.md) exposes runtime and TUI widget
state through local IPC, with JSON responses, revisions, atomic patches and
lifecycle controls. Transport is a Unix socket or a native Windows named pipe.
[Runnable examples](examples/live/README.md) include an operator console. Live control
needs the `live` feature (part of `full`; `tui` implies it).

## Cache work. Repair failures. Resume progress.

Explicit caches avoid repeating deterministic transformations. Checkpoints record
completed nodes so a repaired script can resume its remaining work:

From the repository root, copy the intentionally failing example into a temporary
working directory and run it there:

```sh
repair_dir=$(mktemp -d)
cp examples/repair.program "$repair_dir/repair.program"
cd "$repair_dir"
execute repair.program --checkpoint .execute-state/repair --trace
# Inspect any external effects, then replace `then false` with `then cat`.
execute resume repair.program --checkpoint .execute-state/repair --retry-uncertain --trace
```

Use a fresh working directory and checkpoint for each attempt. The first command
fails intentionally. Its failed node can have partial effects, so even after editing
it, resume requires explicit `--retry-uncertain` before repeating it.

A top-level `for each` or `parallel for each` is recorded element by element, so a
resume continues after the committed elements instead of rerunning the loop. Resume
matches completed nodes by step id, so comments and edits below the last completed
node do not force a rerun. Retry safety follows declared effects: an in-process applet
that only reads, or declares nothing, can run again without the override, while a
failed or interrupted native command can have uncertain effects. The journal is
version 5 (finite graphs checkpoint too), input digests are keyed, and `execute checkpoint status DIR` reports
what a resume would do.

Checkpointing does not make external effects transactional. Uncertain effects
require explicit retry. See the [recovery contract](docs/language.md#recover-after-a-failure),
[per-element checkpoints](docs/language.md#per-element-checkpoints) and
[retry safety](docs/language.md#retry-safety).

<a id="bring-another-cli"></a>
<a id="150-useful-programs-with-inputs-and-outputs"></a>
<a id="explicit-ai-decisions-ordinary-program-control-flow"></a>

## Examples and integrations

| Collection | What to explore |
| --- | --- |
| [150 utilities](examples/ladder/README.md) | Useful file, log, data, CI and release workflows with inputs, outputs and exact fixture oracles. |
| [100 model workflows](examples/models/README.md) | Explicit Jev/LLM calls, typed results and script-owned routing; offline fixtures by default. |
| [AI gauntlet](examples/ai-gauntlet/README.md) | 24 everyday scripts where a bounded Jev/LLM step beats bash: triage, extraction to typed records, fuzzy device choice, risk and danger gates; typed verdicts, thresholds, deterministic fallbacks, offline fixtures and a test. |
| [Omarchy ports](examples/omarchy-ports/README.md) | Thirty commands from the pinned Omarchy v4.0.4 `bin/`: ten interface ports and twenty compared against their Bash originals case by case (`tools/port_compare.py`, 75 cases, fake backends). Not a certified desktop replacement. |
| [Native applet catalog](manifests) and [applet contracts](docs/applet-contracts.md) | Readable named parameters mapped directly to argv, with optional typed values, outcomes and execution backends. Direct quoted calls need no adapter. |
| [Typed applet graphs](docs/graph.md) | Source-build graph files with preflighted typed ports, bounded event queues and `check`/`inspect`/`explain`/`run` commands. |
| [Security and data-flow policies](SECURITY.md) and [data-flow guide](docs/security-dataflow.md) | Optional applet capability/sandbox policy and separate sensitivity, remote-origin, persistence and cache-purity policy. |
| [External program modules](docs/program-modules.md) | Source-build, versioned local program packages with explicit dependency locks and bounded resolution. |
| [Model operations](docs/models.md) | TypeSafe Jev classification, judgment and scoring; OpenAI generation and JSON extraction. |
| [VS Code extension](editors/vscode/README.md) | Syntax highlighting, snippets and grammar tests. |
| [Language server and discovery](docs/language-server.md) | Source-build stdio LSP diagnostics/completion, local reusable-program navigation and rename, safe applet discovery and typed port matching. |
| [Browser playground](docs/playground.md) | Rust/WASM language core, terminal, virtual files and thirty runnable workflows, twenty of them sysadmin and day-to-day audits, with checked outputs and generated reports. |

The browser uses bounded Rust applet ports and buffered pipelines. Installed
executables, native pipes, services, model calls and TUI screens use native Execute.

## Real OS scripts, a little beyond one-liners

These are checked programs from the repository, the size of scripts a desktop or a
server actually needs. Each shows something a plain shell script does not give you
for free: a reviewable plan, typed data, safe file updates, resume after a failure,
or a bounded model decision.

### Switch a desktop theme, and resume if the reload fails

[`examples/demo/theme-refresh.program`](examples/demo/theme-refresh.program) renders
theme templates, swaps the current-theme link and reloads the compositor. The header
declares the effects it may have; `explain --json` shows every step before it runs. Its default `execute.explain/2` output includes argument templates; existing v1 consumers can use `--explain-version 1`. What-if reviews use v2 by default; select the same version when confirming.

```
# execute-policy: allow reads writes process undeclared
accept theme_name as text described as "Directory name for the theme, for example dark"
accept themes_root as text described as "Directory that holds the theme directories"
accept current_link as text described as "Path of the current-theme symbolic link"
accept templates as text described as "Glob of templates to render"
accept values_json as text described as "JSON object of template values"

run path.join with parts [themes_root, theme_name] as theme_dir
run fs.ensure_dir with path theme_dir as made_dir
run json.parse with text values_json as values
run fs.glob with pattern templates as template_files
for each template in template_files
  run path.basename with path template as file_name
  run text.strip_suffix with text file_name and suffix ".tpl" as output_name
  run path.join with parts [theme_dir, output_name] as output_path
  run text.render with template_path template and values values as rendered
  run fs.ensure_file with path output_path and content rendered as written
run fs.ensure_symlink with path current_link and target theme_dir as swap
run "hyprctl" with arguments ["reload"]
run "printf" with arguments ["theme %s active\n", theme_name]
```

```sh
# Run from the repository root. The same inputs must be used for review,
# confirmation and resume so the fingerprint and checkpoint match.
demo_root=$(mktemp -d)
mkdir -p "$demo_root/themes" "$demo_root/current"
# Use the fake compositor from the demo, and keep its log with the scratch files.
export PATH="$PWD/examples/demo/fixtures/bin:$PATH"
DEMO_LOG="$demo_root/calls.log"
export DEMO_LOG
program="$demo_root/theme-refresh.program"
cp examples/demo/theme-refresh.program "$program"
templates="$PWD/examples/demo/templates/*.tpl"
values_json='{"background":"#1e1e2e","foreground":"#cdd6f4"}'

with_theme_inputs() {
  "$@" \
    --input theme_name=dark \
    --input "themes_root=$demo_root/themes" \
    --input "current_link=$demo_root/current/theme" \
    --input "templates=$templates" \
    --input "values_json=$values_json"
}

with_theme_inputs execute check "$program"
with_theme_inputs execute run "$program" --what-if # plan plus a fingerprint
with_theme_inputs execute run "$program" --confirm FINGERPRINT --checkpoint "$demo_root/ck"
# the reload fails: repair that one step as a structured edit, then continue
execute repair "$demo_root/ck" --op 'replace-command 23 run "hyprctl" with arguments ["reload", "--recover"]'
with_theme_inputs execute resume "$program" --checkpoint "$demo_root/ck" --retry-uncertain
```

The rendered files and the link swap are not redone on resume; the failed reload is
retried only because `--retry-uncertain` says so. The full walk-through is
[`examples/demo/walkthrough.sh`](examples/demo/walkthrough.sh).

### Toggle compositor state without racing another script

[`examples/omarchy-rewrites/07-workspace-layout-toggle.program`](examples/omarchy-rewrites/07-workspace-layout-toggle.program)
reads Hyprland's JSON, decides the next layout and records it. The state file is
written with an expected hash, so a concurrent change is refused instead of
overwritten, and a failure is reported with its status:

```
define a program named report_file_failure
  accept path as text
  accept operation as text
  accept status as integer
  run "printf" with arguments ["Warning: %s failed for %s (status %s)\n", operation, path, status]

define a program named save_workspace_state
  accept path as text
  accept content as text
  accept expected_hash as text
  run fs.write_atomic with path path and expected_content_hash expected_hash and symlink_policy "preserve" and permissions_policy "preserve" and content content as write_result and keep status as write_status
  if write_status is not 0
    run program report_file_failure with path path and operation "workspace layout save" and status write_status

run "hyprctl" with arguments ["activeworkspace", "-j"] and keep status as workspace_status
then "jq" with arguments ["-sc", "{id: (map(.id | tostring) | join(\"\\n\")) }"]
then parse json as workspace
take text at "/id" from workspace as workspace_id
```

Nine more ports of real Omarchy commands, each compared against its Bash original,
are in [`examples/omarchy-rewrites`](examples/omarchy-rewrites).

### React to desktop events and run as a service

[`examples/desktop/hyprland-events.program`](examples/desktop/hyprland-events.program)
is a long-running listener on the compositor's event socket; every event is the
handler's input:

```
accept socket_path as text

listen to hypr.socket2 with socket_path socket_path
whenever a line arrives as event
  then "cat"
```

`execute service generate systemd PROGRAM --name NAME --working-dir DIR` prints a
validated systemd user unit (or a launchd job) for a listener or schedule without
installing anything, so the review happens before the system changes.

### Let a model decide only what it is good at

[`examples/ai-gauntlet/005-config-risk.program`](examples/ai-gauntlet/005-config-risk.program)
asks a bounded judge whether a configuration change is low risk. The answer is a
typed verdict with a confidence threshold; anything unsure, refused or failed falls
back to "needs human review", and model text never becomes a command. It runs
offline from fixtures unless `live` is set:

```
accept source_path as text with default "fixtures/005/source.txt"
accept fixture as text with default "fixtures/005/confident.json"
accept live as boolean with default false
try
  read source_path
  if live is true
    then jev.judge with question "Is this change limited to reversible editor formatting, with no permissions, network or deletion changes?" and minimum 80 as raw
  otherwise
    then jev.judge with question "Is this change limited to reversible editor formatting, with no permissions, network or deletion changes?" and minimum 80 and fixture fixture as raw
  then parse json as verdict
  take boolean at "/accepted" from verdict as accepted
  take boolean at "/decision" from verdict as decision
  if accepted is true
    if decision is true
      run json.set with document {"status":"candidate-for-review","result":null} and pointer "/result" and value "formatting-only change"
    otherwise
      run json.set with document {"status":"needs-human-review","result":null} and pointer "/result" and value null
  otherwise
    run json.set with document {"status":"needs-human-review","result":null} and pointer "/result" and value null
catch failure
  run json.set with document {"status":"needs-human-review","result":null} and pointer "/result" and value null
then save to "out/005.json"
```

[`examples/ai-gauntlet`](examples/ai-gauntlet) has 24 of these everyday tasks (log
triage, receipt extraction, Wi-Fi and audio device choice, command danger scoring,
commit subjects, notification triage), each with its offline fixtures, its
advantage over Bash recorded in `catalog.json`, and a test that runs them all.

## Performance, with receipts

The numeric table below is an earlier eight-case Linux snapshot. In that run, Execute
was not faster than Bash at the basics. At
startup, per external command, per pipeline, per megabyte of text or JSON handled by
its in-process applets (except JSON lookup), in memory and in binary size, Bash is
ahead, by 2-20% in time. Execute is ahead on a loop of builtin calls, a loop of process
spawns and a JSON pointer lookup.

Median wall time in milliseconds on Linux x86_64 GitHub-hosted runners (Ubuntu 24.04,
CI run 39133435292 at commit `7320bb6`), default lean build, default manifest path:

| Scenario | bash | dash | Execute |
| --- | ---: | ---: | ---: |
| Startup, no work | 1.19 | 0.65 | 1.22 |
| One external command | 1.92 | 1.48 | 2.16 |
| 5-stage pipeline | 2.96 | 2.17 | 3.19 |
| 1000-iteration builtin loop | 4.20 | n/a | **2.25** |
| 100-spawn loop | 65.59 | n/a | **54.86** |
| Uppercase 1 MB (`tr` vs `text.upper`) | 2.72 | 2.18 | 3.26 |
| JSON pointer lookup, about 240 KB (`jq` vs `json.get`) | 10.36 | 9.90 | **7.24** |
| 100 MB through a pipe | 25.84 | 25.61 | 26.06 |

Peak resident memory at startup: bash 3.4 MB, Execute 6.1 MB (2.8 MB for the static
core-musl-min build). Binary size: Execute 7.48 MB default and 3.50 MB core-musl-min
(x86_64), bash 1.45 MB, dash 130 KB. An arm64 runner shows the same pattern. Work to
close the remaining gaps is tracked in #305.

**Method.** The [benchmark runner](tools/bash_compare.py) pairs 22 workloads grouped
as startup, processes, text, files and data. Every shell/runtime arm must pass an exact
stdout oracle before timing; the small-file scenario also verifies its output tree.
All 22 workloads have dash counterparts; the here-string case uses an explicit pipeline
equivalent for its dash arm. The Bash/dash contract test covers the inventory, pairs, and
shell output oracles.

The default is 2 warmups and 20 measured runs (10 for spawn, 10,000-line and
command-substitution cases; 5 for the 100 MB pipe and 1 MB capture; 4 for file tests).
GNU time reports peak
RSS, and the runner enforces a 285-second end-to-end limit with a 300-second hard cap.
Each scenario uses temporary input fixtures and fresh output/checkpoint state. CI checks
the contract and compares every release/profile build on x86_64 and arm64 Linux, saving
grouped JSON reports. The [expanded macOS arm64 report](benchmarks/bash-compare-macos-arm64-20261027.json)
contains current 22-scenario medians and raw samples; the table above remains the earlier
Linux release snapshot. Use the latest grouped workflow report for current Linux values.
The compared operations use
natural tools and applets, so these jobs do not isolate interpreter overhead.
**What it means.** Execute pays a fixed start-up cost (about 0.6 ms over bash here)
that dominates short programs. It wins where it replaces a process with an in-process
call. For a program that starts many processes, the work is in those processes, and the
two runtimes are close. Daemon forwarding requires explicit acceptance of the daemon context. Earlier measurements,
with their own methods, are indexed in [benchmarks/README.md](benchmarks/README.md).
Browser virtual-applet timings do not measure native processes. No result establishes a
universal speed advantage or full Bash parity.

**Research and write-ups.**

- [Benchmark index and methods](benchmarks/README.md): source-pinned, paired workloads and retained losses.
- [Optimization session report](docs/optimization-session-2026-10-14-report.md): the bounded October 14 campaign.
- [Production gates](docs/production-readiness.md): acceptance gates, the performance policy and the full comparison notes.
- [Replacement challenge](docs/replacement-challenge.md): fifteen proof gates, none closed.
- [Thesis ledger](docs/thesis-progress.md): implemented capabilities against the original goals.
- [Unix compatibility review](docs/unix-compatibility-review.md): the supplied review against the implementation.
- [Threat model](docs/threat-model.md): attackers, controls and known holes.

<a id="release-and-source-status"></a>

## Scope and current limits

Execute targets Unix glue programs on macOS and Linux, with a native Windows port on
x64 and ARM64. Current `main` is version `0.9.0-rc.5`. The latest published release is
the multi-platform candidate [`0.9.0-rc.4`](https://github.com/monstercameron/NextGenScriptingLanguage/releases/tag/v0.9.0-rc.4)
(tag `v0.9.0-rc.4`); `0.9.0-rc.5` has no tag and no release yet, so everything added
since rc.4 requires a source build. The retained `bin/execute` is historical 0.8.0.
[Version history](CHANGELOG.md) separates published releases from unreleased work,
and [release validation](docs/releases.md) records what each package set passed.

**Not a Bash replacement yet.** The project does not parse Bash and does not install
itself as a login shell or `/bin/sh`. Missing or partial, as tracked in the
[production gates](docs/production-readiness.md): job handles that outlive their
script (`spawn` jobs end with it, and a `launch detached` child is not tracked),
terminal ownership beyond handing the terminal to one `interactively` command,
sourced shell configuration, a certified Omarchy desktop run, and general structured
streaming. [`execute compile --to sh`](docs/language.md#compile-to-posix-sh) covers only
a subset. Measured performance trails Bash on the basics (see above).

**Security model.** With no policy, Execute has Bash's trust model: native commands and
applets run with the caller's permissions, explicit paths follow links and `..`, and
nothing is sandboxed. An opt-in capability policy checks declared applet grants and
filters environment variables, and `sandbox_required` can require an OS sandbox for
supported command and worker applets on macOS and Linux. A separate
[data-flow policy](docs/security-dataflow.md) governs remote egress and persistence of
labeled values. Secret prompt answers are kept out of traces, records and checkpoints,
and redaction is by exact value. `use folder` and `cd` authorize the opened directory,
and the keyed checkpoint digest, daemon socket checks and fixes from a security audit
are described in the [threat model](docs/threat-model.md), which lists nineteen known
holes with severities (H1-H19). Neither policy proves arbitrary native code safe. See
[SECURITY.md](SECURITY.md) for boundaries and disclosure. CI runs
dependency, secret, fuzz, sanitizer, crash-consistency and soak checks; the nightly ones
are new, and [their status is recorded](docs/production-readiness.md#ci-evidence-and-its-status).

**Features by build.** The lean default omits the terminal UI, model helper, live control
and `attach`, remote applets, the Quickshell renderer, the debug adapter and the language
server; `--features full` has them. Also in source, and in release packages only after
the next release: typed applet [contracts](docs/applet-contracts.md), a bounded
journaled `fs.transaction`, [external program modules](docs/program-modules.md),
[typed applet graphs](docs/graph.md), the
[language server](docs/language-server.md), and
[loops](docs/language.md#loops-current-source), `parallel for each`, named background
jobs and temporary scopes. The Quickshell adapter starts one process per renderer node,
and real Omarchy desktop acceptance is open. Platform contracts are in
[windows.md](docs/windows.md).

## Build and verify

```sh
make help           # Available contributor commands
make setup          # Locked editor and playground npm dependencies
make check          # Native Rust, Python harness and editor checks
make playground-check  # WASM build, formatter and runtime contracts
make playground-browser-check  # Real Chromium acceptance checks
```

See [Contributing](CONTRIBUTING.md) for prerequisites, browser installation,
platform-specific tests, the feature sets and the
[CI and nightly workflows](CONTRIBUTING.md#continuous-integration-and-nightly-checks).
Tests use local fixtures; provider credentials are not needed. `make check` builds with
`--features full`.

## Documentation

Start at the **[documentation index](docs/README.md)** for current guides,
architecture, contribution paths and a separate map of historical evidence.

Licensed under the [MIT License](LICENSE).

Security boundaries and remaining isolation gaps are documented in [SECURITY.md](SECURITY.md).
