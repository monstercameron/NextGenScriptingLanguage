# Bash and Omarchy production gates

**Status: source candidate, not a production Bash replacement.** The current
work targets equivalent behavior in readable `.program` files. It does not
parse existing Bash syntax or install itself as `/bin/bash` or a login shell.
Published 0.9.0-rc.4 packages and the retained 0.8.0 binary remain unchanged;
the current source (version `0.9.0-rc.5`, untagged and unpublished) contains
candidate work. The project's aim is an AI-first, complete Unix/POSIX scripting
language whose programs humans can read and observe; these gates measure how far
the source is from replacing Bash for real workloads, not whether it can run a
pipeline.

## Changes backed by tests

* Unhandled native failures preserve their actual exit status. Signal deaths
  return `128 + signal`; interpreter errors and timeouts still return 1.
  Captured statuses remain available to `.program` conditions.
* Typed inputs can come from environment variables, positional arguments or
  a complete literal argument list. Named inputs override those sources;
  defaults apply to absent values. Validation precedes command execution.
* `#!/usr/bin/env -S execute --` forwards program arguments, including
  `--help`, without confusing them with interpreter options.
* Static inspection identifies input sources without reading the environment.
  Resolved inputs participate in checkpoint identity. Changes reject resume
  rather than silently replaying a different request.
* A macOS test-harness cleanup race now keeps the launcher PID reserved until
  group termination, and verifies dead group members before accepting macOS
  `EPERM`. This changes failure cleanup, not normal runtime performance.
* Versioned applet contracts add typed parameters/output, explicit outcome
  routing, stream dispositions, and command, in-process and persistent-worker
  backends. Generic operations include comment-preserving JSONC edits,
  single-file compare-and-swap writes and a bounded journaled file-only
  transaction with explicit recovery. A transaction has partial visibility
  while files apply and does not cover external effects. These contracts do not
  impose universal I/O limits.
* An optional strict capability policy checks declared grants and filters the
  child environment. `sandbox_required` separately opts into fail-closed
  macOS/Linux OS confinement for command and worker processes; in-process
  operations cannot use it. See [the security contract](../SECURITY.md#applet-contracts-and-optional-capability-policy).
* An optional [data-flow policy](security-dataflow.md) separately governs
  sensitivity labels at remote and persistence sinks. Source builds can also
  `check`, `inspect`, `explain` and `run` bounded typed applet graphs; these do
  not add a universal streaming, desktop or Bash-compatibility guarantee.
* `execute service generate systemd|launchd` prints a validated foreground
  service definition for review only. It does not save, install, enable, load
  or start the service; TTY handoff and universal detached-descendant cleanup
  are not provided. See [the practical guide](guide.md#generate-a-reviewable-service-definition).

* A warm per-user daemon (`execute daemon`, `run --daemon`) forwards runs over a
  checked Unix socket. It helps only where manifest and policy setup dominates;
  for a one-line program it is level with a cold run. See
  [the daemon section](language.md#warm-daemon---daemon).
* Checkpoints record top-level `for each` loops element by element, match resume
  nodes by step id, and use journal version 5 with a keyed input digest. Retry
  safety follows declared effects, so a pure in-process loop writes no durable
  begin record per element. See [recovery](language.md#recover-after-a-failure).
* `use folder` and `cd` authorize the opened directory and later relative paths
  join onto it. The [threat model](threat-model.md) lists nineteen known holes
  (H1-H19) with severities, and CI runs dependency, secret, fuzz, sanitizer,
  crash-consistency and soak checks; see
  [CONTRIBUTING](../CONTRIBUTING.md#continuous-integration-and-nightly-checks).
* The default build is lean (no terminal UI, model helper, live control, remote
  backend, desktop renderer, DAP adapter or language server); packages ship the
  `full` feature set. The shipped manifests are embedded in an index, so a
  default-path run does not read and validate every manifest file.

See [the input reference](language.md#process-input-sources) and
[the isolated Omarchy ports](../examples/omarchy-ports/README.md).

## Pinned upstream, not a compatibility percentage

The inventory reads the `bin/` directory of
[Omarchy commit c668141](https://github.com/omacom/omarchy/commit/c668141e9c42b13c80c9ca4ea108e11708c5e8a5),
tag v4.0.4: **444 command files, 439 Bash and five Python shebangs**.
[omarchy-inventory.json](omarchy-inventory.json) retains hashes and lexical
feature locations. Every inventory entry remains `unverified`.

The inventory is a source scanner, not a Bash parser. Its lexical hits can
include strings and comments; they are leads for porting, not readiness scores.
`tools/omarchy_port_report.py` is a parser-based report of the Execute features each
command needs. Installers, migrations, sourced shell configuration and QML are
outside that command count and require separate inventories. The ten initial
ports cover small command interfaces with fake backends. Twenty further scripts
are ported under `examples/omarchy-ports/<script>/` with the Bash original, the
port and a `case.json`. `tools/port_compare.py` runs both in sandboxes with logging
fake binaries first on `PATH` and compares exit status, stdout, stderr, the
fake-call log and the final file tree over 75 cases, and CI runs it on Linux. None
is certified for a live Omarchy desktop.

## Ordered acceptance gates

| Gate | Required behavior and proof | Current gap |
| --- | --- | --- |
| Command contracts | Exact argv, stdin, stdout/stderr, cwd, native status, missing tools and failure behavior on macOS and Linux | 110-case/46-tool Unix matrix; applet contracts add typed/status/stdio coverage, while output and pipeline failure policy differences remain explicit |
| Shell control | Compact loops, local bindings, command results, early return/status, environment overrides and unset, append/separate/merged redirection | Typed inputs, scalar conditions and bounded repeat/for-each/while loops exist in current source; the remaining control surface needs proof |
| Jobs and terminal ownership | Spawn, await, cancel, `exec` identity or a documented alternative, signal propagation, traps/finally and terminal handoff | Finite scopes own bounded named native jobs, also under checkpoints. `on exit` and `on interrupt` handlers drain gracefully, `hand over` replaces the process with `execve` on Unix, `launch detached` starts an untracked child and `interactively` hands the terminal to one command. Persistent-lifetime job handles and Bash terminal/session parity remain missing |
| Reuse and data | External program modules, typed records and structured streams without generated application helpers | Same-file finite reuse, versioned external modules, scheduled services and bounded native-command listener module nodes (one decoded event per sequential handler, with typed startup inputs), bounded task groups, finite JSON and typed applet records exist; other listener backends, joins and general structured streaming remain missing |
| Real Omarchy ports | Port every in-scope command; add success, absent dependency, cancellation, failure and side-effect oracles per command | Pinned inventory, ten isolated interface ports and twenty behavior-compared ports (75 cases) out of 444 commands; no complete command or desktop certification |
| Desktop acceptance | Disposable Arch/Omarchy sessions testing Hyprland, Wayland, D-Bus, terminals, services and clean teardown | The disposable Arch VM reached an active seat0/VT1 Wayland session, then Quickshell exited 255 before IPC readiness. Direct renderer and local/remote graph acceptance remain pending; full Omarchy certification remains open |
| Sourced shell integration | Define how environment/cwd changes reach a caller and which Bash startup hooks stay Bash | External programs cannot mutate their parent shell; no sourced Bash emulation |
| Safety and recovery | Inspectable effect boundaries, fault injection, checkpoint uncertainty and cleanup under interruption | Strict applet policy and opt-in macOS/Linux sandboxes exist in current source; no-policy commands remain trusted. Nightly crash-consistency, fuzz, sanitizer and soak workflows exist but are new; see the [CI notes](#ci-evidence-and-its-status). Broader adversarial/platform coverage remains open |
| Distribution | Clean install/uninstall, paths with spaces, shebang portability, declared Bash-independent dependencies and immutable versioned packages | [Source `58b42f6` validation](../benchmarks/package-validation-38836416853/README.md) passed the fresh macOS ARM64 archive/root, catalog, spaced-path shebang and scratch rollback checks, all six native package targets, both Flatpak targets, and the exact twelve-asset hash set. Newer source revisions require their own validation; `0.9.0-rc.5` remains unpublished |
| Performance | Identical behavior and workload on both runtimes, paired repeated release measurements, cold/warm startup, CPU, memory, streaming, idle and event latency on macOS and Linux | Six retained equal-command-plan fixtures put Bash medians below Execute and Execute below Nushell. A Linux comparison run on every `main` commit has Execute behind bash on startup, binary size and memory, and ahead on a builtin loop, a spawn loop and a JSON pointer lookup ([below](#performance-against-bash-and-dash)). A bounded Linux 1,000-actor run passed; cold-start, sustained operation and process-tree resource evidence remain incomplete |
| Readability and generation | Held-out real scripts, fixed prompts, repair counts and blind human review | Example counts and passing tests do not establish this claim |

Do not emulate missing control flow by generating Bash or application Rust.
Add a small primitive only when a real port demonstrates the need, use the
shared process runtime, and test all lifetimes where that primitive is allowed.
Keep the original [thesis](thesis-source.md) and
[replacement proof gates](replacement-gates.json) intact.

## Performance policy

“Faster than Bash in every dimension” is an aspiration to test, not a valid
release claim. Bash builtins can avoid external processes; native tools can
dominate both runtimes; I/O and provider latency can dominate the application.
Measure equivalent semantics and report losses as well as wins. Removing an
unnecessary helper process can improve one workflow without establishing a
general interpreter speed advantage.

Keep raw paired samples, executable/source hashes, exact output oracles,
warmup exclusions and environment details. Do not time builds or stress suites
concurrently. Native Ubuntu and macOS CI verify portability; neither proves
performance on an Omarchy workstation. Production promotion requires the
gates above, not a synthetic 10/10 score or a fixed number of examples.

The [applet boundary harness](../benchmarks/applet_boundaries.py) is prepared to
compare equivalent successful `text.matches` calls across all three native
backends after correctness checks. It is not a result, and it cannot establish
a general runtime or Bash speed advantage. Exact Omarchy success-path timings
are covered by a separate [paired harness](../benchmarks/omarchy_success.py);
they must remain separate from newly covered failures and side-effect
correctness.

## Performance against Bash and dash

Optimisation is ongoing. Today Execute is 2-20% slower than Bash on startup, on a
single external command, on pipelines and on uppercase text, at parity on a 100 MB
pipe, uses more memory at startup and has a larger binary. It is ahead on a loop of
builtin calls, a loop of process spawns and a JSON pointer lookup.

Measured on Linux x86_64 GitHub-hosted runners (Ubuntu 24.04, CI run 39133435292 at commit `7320bb6`),
default lean build, default manifest path, medians in milliseconds:

| Scenario | bash | dash | Execute | Execute / bash |
| --- | ---: | ---: | ---: | ---: |
| Startup, no work | 1.19 | 0.65 | 1.22 | 1.02 |
| One external command | 1.92 | 1.48 | 2.16 | 1.12 |
| 5-stage pipeline | 2.96 | 2.17 | 3.19 | 1.08 |
| 1000-iteration builtin loop | 4.20 | n/a | 2.25 | 0.54 |
| 100-spawn loop | 65.59 | n/a | 54.86 | 0.84 |
| Uppercase 1 MB (`tr` vs `text.upper`) | 2.72 | 2.18 | 3.26 | 1.20 |
| JSON pointer lookup, about 240 KB (`jq` vs `json.get`) | 10.36 | 9.90 | 7.24 | 0.70 |
| 100 MB through a pipe | 25.84 | 25.61 | 26.06 | 1.01 |

Peak resident memory at startup is 3.4 MB for bash and 6.1 MB for Execute (2.8 MB
for the static core-musl-min build). Binary size is 7.48 MB for the default x86_64
build and 3.50 MB for core-musl-min, 1.45 MB for bash and 130 KB for dash. The arm64
runner shows the same pattern. Remaining gaps are tracked in #305.

How it is measured. [`tools/bash_compare.py`](../tools/bash_compare.py) pairs a bash
command line with an Execute program that does the same job and gives both the same
stdin file. Wall time is taken around each process from Python, with stdout discarded.
Each scenario runs 2 warmups and then 20 timed runs (10 for the spawn loop and 5 for
the 100 MB pipe), and the report is the median. Bash runs as
`bash --norc --noprofile -c`; dash runs `dash -c` only for scenarios that are plain
`sh`, which excludes the two loops (they use bash arithmetic loops). Execute runs
`execute run FILE` with no `--manifest-dir`, so it takes the default manifest
path an installed binary uses, including the bundled index. A run that exits nonzero
fails the comparison. Peak RSS comes from GNU `/usr/bin/time -f %M` (the maximum of
three runs), because `wait4` from Python reports the parent's high-water mark. The
[`bash-compare.yml`](../.github/workflows/bash-compare.yml) workflow runs it for every
push to `main`, on `ubuntu-24.04` and `ubuntu-24.04-arm`, and keeps a JSON report. The
table above is the default release build. Since commit `e2c6861` the workflow also builds
and compares an `opt-level = "s"` variant, a static musl build and a musl build with the
size-first `min` profile; those results are in the workflow runs, not in this table.

What it does not show. The pairs use each runtime's natural tool (`tr` and `jq`
against in-process applets), so they compare jobs, not one tool. The runners are shared
machines, files are warm in the page cache, and the 20-run medians have no confidence
intervals, so differences of a few percent are within noise. It does not measure
Omarchy scripts, network or disk-bound work, or Windows and macOS. Numbers change as
optimisation lands; read the latest workflow run, not this table, for the current
state. The measurements that predate this comparison, with their own methods and
boundaries, are indexed in [benchmarks/README.md](../benchmarks/README.md).

## Research and evidence

These pages hold the supporting write-ups. They apply to their recorded versions.

- [Benchmark index and methods](../benchmarks/README.md): source-pinned workloads, paired
  samples and retained losses.
- [Optimization session report](optimization-session-2026-10-14-report.md) and its
  [plan](optimization-session-2026-10-14.md): the bounded October 14 campaign across the
  twelve rubric dimensions.
- [Replacement challenge](replacement-challenge.md): the fifteen proof gates, with the
  [supplied challenge](replacement-challenge-source.md).
- [Thesis ledger](thesis-progress.md): implemented capabilities against the original goals.
- [Unix compatibility review](unix-compatibility-review.md): the supplied review set against
  the implementation, with benchmark gates.
- [Threat model](threat-model.md) and [audit resolution ledger](audit-resolution.md):
  attackers, controls and known holes; the 92 numbered audit findings.
- [AI productivity protocol](../benchmarks/ai_productivity/README.md): the held-out
  evaluation design, with results still pending, and a small public pilot.

## CI evidence and its status

The workflows listed in [CONTRIBUTING](../CONTRIBUTING.md#continuous-integration-and-nightly-checks)
produce evidence, and most of the nightly ones are new. As of 2026-10-17 the fuzz replay,
security scans and bash comparison pass on `main`; the crash-consistency and platform
workflows have passed manual runs; the soak and flaky-test workflows have so far only
failed on manual runs, and the findings are being fixed; and the sanitizer workflow has not
yet run. Treat a nightly workflow as evidence only after it has passed on the commit in
question. Check the [Actions page](https://github.com/monstercameron/NextGenScriptingLanguage/actions).
