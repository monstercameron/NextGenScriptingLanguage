# Bash and Omarchy production gates

**Status: source candidate, not a production Bash replacement.** The current
work targets equivalent behavior in readable `.program` files. It does not
parse existing Bash syntax or install itself as `/bin/bash` or a login shell.
Published 0.9.0-rc.4 packages and the retained 0.8.0 binary remain unchanged;
the current source contains untagged candidate work.

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
Installers, migrations, sourced shell configuration and QML are outside that
command count and require separate inventories. The ten initial ports cover
small command interfaces with fake backends. None is certified for a live
Omarchy desktop.

## Ordered acceptance gates

| Gate | Required behavior and proof | Current gap |
| --- | --- | --- |
| Command contracts | Exact argv, stdin, stdout/stderr, cwd, native status, missing tools and failure behavior on macOS and Linux | 110-case/46-tool Unix matrix; applet contracts add typed/status/stdio coverage, while output and pipeline failure policy differences remain explicit |
| Shell control | Compact loops, local bindings, command results, early return/status, environment overrides and unset, append/separate/merged redirection | Typed inputs, scalar conditions and bounded repeat/for-each/while loops exist in current source; the remaining control surface needs proof |
| Jobs and terminal ownership | Spawn, await, cancel, `exec` identity or a documented alternative, signal propagation, traps/finally and terminal handoff | Supervised children and bounded services exist; Bash job/session parity is missing |
| Reuse and data | External program modules, typed records and structured streams without generated application helpers | Same-file finite reuse, versioned external modules, scheduled module graph nodes, bounded task groups, finite JSON and typed applet records exist; reusable listener services and general structured streaming remain missing |
| Real Omarchy ports | Port every in-scope command; add success, absent dependency, cancellation, failure and side-effect oracles per command | Pinned inventory plus ten isolated interface ports; no complete command or desktop certification |
| Desktop acceptance | Disposable Arch/Omarchy sessions testing Hyprland, Wayland, D-Bus, terminals, services and clean teardown | No native Omarchy/Arch desktop runner available locally; Ubuntu CI cannot substitute for this |
| Sourced shell integration | Define how environment/cwd changes reach a caller and which Bash startup hooks stay Bash | External programs cannot mutate their parent shell; no sourced Bash emulation |
| Safety and recovery | Inspectable effect boundaries, fault injection, checkpoint uncertainty and cleanup under interruption | Strict applet policy and opt-in macOS/Linux sandboxes exist in current source; no-policy commands remain trusted, and broader adversarial/platform coverage remains open |
| Distribution | Clean install/uninstall, paths with spaces, shebang portability, declared Bash-independent dependencies and immutable versioned packages | Candidate package scripts include `execute-applet`; no new release package or provenance receipt has been published and validated |
| Performance | Identical behavior and workload on both runtimes, paired repeated release measurements, cold/warm startup, CPU, memory, streaming, idle and event latency on macOS and Linux | Existing focused measurements are not comprehensive; source optimizations need their own receipts |
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
