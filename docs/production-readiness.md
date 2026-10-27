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

* Daemon run forwarding defaults to local execution because the client's OS confinement cannot be preserved. `EXECUTE_DAEMON_ACCEPT_CONTEXT=1` explicitly
  accepts the daemon context and enables forwarding.
  See [the daemon section](runtime-reference.md#warm-daemon---daemon).
* Checkpoints record top-level `for each` loops element by element, match resume
  nodes by step id, and use journal version 5 with a keyed input digest. Retry
  safety follows declared effects, so a pure in-process loop writes no durable
  begin record per element. See [recovery](runtime-reference.md#recover-after-a-failure).
* `use folder` and `cd` authorize the opened directory and later relative paths
  join onto it. The [threat model](threat-model.md) lists nineteen known holes
  (H1-H19) with severities, and CI runs dependency, secret, fuzz, sanitizer,
  crash-consistency and soak checks; see
  [CONTRIBUTING](../CONTRIBUTING.md#continuous-integration-and-nightly-checks).
* The default build is lean (no terminal UI, model helper, live control, remote
  backend, desktop renderer, DAP adapter or language server); packages ship the
  `full` feature set. The shipped manifests are embedded in an index, so a
  default-path run does not read and validate every manifest file.

See [the input reference](runtime-reference.md#process-input-sources) and
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

The expanded suite has a local diagnostic run on macOS arm64. It completed all 22
scenarios in 111.161 seconds. Execute was faster on three scenarios (uppercase 1 MB,
10,000-line processing and 1 MB output capture) and slower on the other 19.
These are job-level comparisons, not a general interpreter-speed claim.

Measured 2026-10-27 on macOS 26.6.2 arm64, using the default release executable built
from Rust source commit `d86b7ca4`, explicit `manifests`, Python 3.9.6, two warmups and
20 samples by default (scenario-specific reductions are in the runner). The executable
SHA-256 is `54c1874744ab78b2998984d868d2abee5e6f13e6b51d3a3d8268d7e17d949929`. GNU
time was unavailable on this host, so this run contains no peak-RSS data. The
[raw per-run report](../benchmarks/bash-compare-macos-arm64-20261027.json) retains
individual timings, host details, and executable/runner hashes.

| Group | Scenario | bash ms | dash ms | Execute ms | Execute / bash |
| --- | --- | ---: | ---: | ---: | ---: |
| startup | startup (no work) | 2.52 | 2.18 | 10.90 | 4.32x |
| startup | checkpoint creation for a 100-step loop | 3.42 | 2.54 | 169.22 | 49.45x |
| processes | one external command | 3.97 | 3.53 | 12.31 | 3.10x |
| processes | 5-stage pipeline | 6.20 | 5.56 | 13.78 | 2.22x |
| processes | 1000-iteration builtin loop | 7.13 | 4.97 | 11.93 | 1.67x |
| processes | 100-spawn loop | 50.24 | 46.35 | 129.21 | 2.57x |
| processes | uppercase 1 MB (tr vs text.upper) | 45.35 | 45.19 | 11.68 | 0.26x |
| processes | JSON pointer lookup (jq vs json.get) | 9.67 | 9.00 | 13.81 | 1.43x |
| processes | 100 MB through a pipe | 26.54 | 25.82 | 34.21 | 1.29x |
| processes | command substitution in a 1,000-iteration loop | 466.55 | 404.01 | 1335.61 | 2.86x |
| processes | capture 1 MB command output | 33.66 | 8.58 | 13.49 | 0.40x |
| text | while read 10,000 lines | 166.15 | 83.58 | 11.63 | 0.07x |
| text | grep/cut/sort/uniq chain | 6.51 | 5.89 | 14.27 | 2.19x |
| text | parameter suffix removal | 2.70 | 2.26 | 11.07 | 4.09x |
| text | here-string scalar input | 3.00 | 2.93 | 11.02 | 3.67x |
| text | string split and rejoin | 2.72 | 2.29 | 11.00 | 4.04x |
| files | glob count over 1,000 files and one matching directory | 5.61 | 3.82 | 11.94 | 2.13x |
| files | file-test every match in a 1,000-file glob | 9.64 | 6.11 | 16.69 | 1.73x |
| files | find piped to xargs | 411.12 | 412.24 | 420.65 | 1.02x |
| files | write 100 small files | 69.88 | 60.89 | 1216.77 | 17.41x |
| data | parse a 1,000-row CSV file | 6.41 | 5.94 | 11.28 | 1.76x |
| data | environment variable selection | 5.20 | 4.99 | 13.80 | 2.65x |

Historical eight-scenario snapshot from Linux x86_64 GitHub-hosted runners follows; it is
kept for its original platform and source context, and is separate from the expanded local
measurement above.

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

How it is measured. [tools/bash_compare.py](../tools/bash_compare.py) pairs 22
workloads in the startup, processes, text, files and data groups. Each scenario has an
independent exact-stdout oracle; the 100-file writer also checks the complete output
tree, including the applet's lock files. Bash and Execute receive the same named stdin
fixture where applicable. The file fixture contains 1,000 regular `.dat` files and a
matching directory: glob-count returns 1,001 entries, while file-test counts 1,000
regular files. The glob-count Execute arm encodes and reparses `fs.glob`'s
`list<text>` result because `list.count` currently accepts only generic JSON lists; that
adapter work is included in its timing. A preflight runs every shell/runtime arm and rejects
status or output mismatches before timing. The Python contract test checks the inventory,
all 22 Bash/dash pairs and all shell output oracles. The here-string case uses an explicit
pipeline equivalent for the dash arm.

Each timed scenario has 2 warmups and 20 samples (10 for the spawn, 10,000-line,
command-substitution cases; 5 for the 100 MB pipe and 1 MB capture; 4 for the 1,000-file
file-test case), reported as medians.
Bash uses --norc --noprofile; dash runs for all 22 scenarios. Execute runs
execute run FILE with the workflow's explicit manifest directory. Peak RSS uses GNU
/usr/bin/time -f %M, at most three runs per arm, with the same temporary working
directory and fresh output/checkpoint fixtures. Every child has a 30-second timeout and
the harness stops at a 285-second end-to-end budget (hard maximum 300 seconds).
[bash-compare.yml](../.github/workflows/bash-compare.yml) runs the workload-contract
test and comparison for each release/profile variant on ubuntu-24.04 and
ubuntu-24.04-arm, retaining a grouped JSON report. The local macOS expanded table above
is diagnostic; the Linux eight-scenario table below is a historical snapshot. The
latest Linux workflow artifacts remain the source for per-profile CI results.
What it does not show. The pairs use each runtime's natural operation, so they compare
jobs rather than isolating interpreter overhead. The runners are shared machines, files
are warm in the page cache, and the 20-run medians have no confidence intervals; small
differences are within noise. The 100 MB case checks the byte count through a pipe; the
1 MB case compares command substitution with a captured Execute binding. The checkpoint
case compares checkpoint creation for a finite Execute loop with shell recomputation.
These workloads do not measure Omarchy scripts, network or disk-bound work, Windows or
macOS. Read the latest workflow run for current results; other retained measurements and
their boundaries are indexed in [benchmarks/README.md](../benchmarks/README.md).
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
