# `.program` — The 10/10 Replacement Challenge

To earn a genuine 10/10 as an Omarchy Bash-script replacement and universal applet glue language, `.program` has to prove three things:

1. It can replace real Bash scripts without losing capabilities or reliability.
2. It can compose existing local, remote, and GUI applets more efficiently than writing application code.
3. An LLM can build and maintain useful programs with substantially less effort, fewer tokens, and fewer failures.

I checked Omarchy's current architecture. It makes this a demanding test: Omarchy already exposes machine-readable CLI commands, has a Quickshell plugin architecture, and includes automated performance guards that reject idle shell subprocess spawning. Its installation scripts can also be sourced into another Bash execution context.&#x20;

[image](https://www.google.com/s2/favicons?domain=https://github.com\&sz=32)

GitHub

+2



Those are real constraints your replacement must survive.

## The 15 proof gates

A 10/10 requires passing every critical gate, not merely averaging high scores.

P0 — MUST WORK BEFORE CLAIMING BASH REPLACEMENT

| # | Proof requirement                 | Measurable acceptance test                                                                                                                                                                        |
| - | --------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| 1 | Unix compatibility                | Run 100 representative commands covering stdin/stdout/stderr, binary streams, exit codes, redirection, signals, environment, working directory, and TTY behavior with no unexplained differences. |
| 2 | Real streaming                    | Process a 1 GB stream without loading it into memory. Native backpressure, predictable output, and no subprocess-per-record overhead.                                                             |
| 3 | Process lifecycle                 | Pass the `tail -f` three-event test. Stop producers, reap children, and handle Ctrl-C, timeouts, and crashes correctly over 10,000 repetitions.                                                   |
| 4 | Complete scripting control flow   | Recreate scripts involving conditions, loops, variables, functions, file operations, failures, and parallel execution without escaping into Bash.                                                 |
| 5 | Actual Omarchy script replacement | Port a stratified sample of 150 real Omarchy scripts/workflows. Achieve at least 95% behavioral equivalence across in-scope workloads, and 100% on safety-critical test assertions.               |
| 6 | Failure semantics                 | Prove that pipeline failures cannot silently succeed. Preserve exit status, stderr, cleanup, timeout, retry, and cancellation behavior.                                                           |

P1 — MUST WORK TO JUSTIFY THE APPLET ARCHITECTURE

| #  | Proof requirement          | Measurable acceptance test                                                                                                                                                       |
| -- | -------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| 7  | Universal applet interface | Discover and compose 1,000 registered applets across executable, native, IPC, QML, and remote backends without runtime changes for individual applets.                           |
| 8  | Composable programs        | A `.program` can itself become an applet. Demonstrate five levels of nested composition with typed ports, error propagation, and deterministic lifecycle ownership.              |
| 9  | Structured dataflow        | Support bytes, text, records, lists, JSON, and events. Detect incompatible connections before execution and prohibit silent lossy conversions.                                   |
| 10 | Concurrent correctness     | Run 1,000 concurrent applet operations under randomized delays and failures. No stale UI writes, leaked resources, uncontrolled queues, or unintended duplicate effects.         |
| 11 | Remote reliability         | Compose local and remote applets with explicit authentication, deadlines, reconnection, version compatibility, and offline behavior. Simulate latency, packet loss, and outages. |
| 12 | Desktop integration        | Compose 20 real Quickshell widgets and services with CLI applets, supporting bidirectional events and state synchronization without application-specific QML or Rust.            |

P2 — MUST WORK TO MAKE IT THE PREFERRED CHOICE

| #  | Proof requirement                       | Measurable acceptance test                                                                                                                                                                          |
| -- | --------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| 13 | Runtime performance                     | Target cold starts under 20 ms p95, local event routing under 10 ms p95, bounded memory, and zero idle subprocess spawning. Match or beat Bash on representative end-to-end workloads.              |
| 14 | AI generation superiority               | Across 200 held-out application tasks, achieve at least 95% first-pass functional correctness, 50% fewer generated tokens, and 50% fewer repair iterations than the Bash/QML baseline.              |
| 15 | Production maintainability and security | Permission-scoped applets, protected credentials, versioned contracts, event tracing, replayable tests, and safe upgrades. Demonstrate a month of normal desktop use without a critical regression. |

These thresholds are proposed acceptance targets, not claims about the implementation today.

## The four tests I'd consider decisive

### 1. The 1,000-applet composition test

This is the single biggest proof of your architecture.

Create a registry containing:

# 400

Unix applets

# 200

Native transforms

# 150

GUI widgets

# 150

Remote services

# 100

Other adapters

# 1,000

Total

Then give an LLM a task it hasn't seen:

> Build an Omarchy panel with network usage, GPU temperature, weather, clock, and battery status. Add notifications, controls, and remote synchronization.

The LLM should discover suitable applets, inspect their contracts, connect them, and deliver a working application.

Pass criteria: No new application-specific Rust, QML, JavaScript, or Python. No hidden Bash implementation of the actual application logic.

A missing capability may require creating a reusable applet, but the system should not need one-off adapters for each composition.

### 2. The adversarial Unix test

Your earlier example:

```
listen to tail with flags ["-n", "+1", "-f"] and path "events.txt"
whenever a line arrives as record
  then tr with from "[:lower:]" and to "[:upper:]"
```

Now make the input adversarial.

Feed it millions of lines, invalid UTF-8, embedded null bytes, paths containing spaces, abrupt producer termination, and a consumer that pauses indefinitely.

Then stop the pipeline after exactly three records.

The runtime must preserve data integrity, prevent uncontrolled buffering, stop producers, and report errors accurately.

A program that looks beautiful but occasionally hangs, leaks a child process, or loses data is not a Bash replacement.

### 3. The Omarchy regression test

This is where the project has to prove genuine ecosystem compatibility.

Take real examples from Omarchy's command categories: package management, hardware detection, theme management, application launching, updates, and configuration. Its own contributor instructions document these categories and their expectations.&#x20;

[image](https://www.google.com/s2/favicons?domain=https://github.com\&sz=32)

GitHub



Run both versions in disposable environments:

Existing Bash

Reference behavior

`.program`

Candidate replacement

Differential test harness

Outputs · Exit codes · Filesystem changes · UI state · Side effects · Latency

Equivalent results, no new regressions

Also test the hard edge cases: privilege escalation, noninteractive execution, partially failed updates, missing commands, and sourced installation scripts.

I'd defer bootstrapping and recovery scripts until the interpreter is reliably installed and available in recovery environments. Those scripts cannot depend on desktop services.

### 4. The AI productivity test

This is the proof that `.program` is worth adopting even where Bash already works.

Give the same local LLM the same 200 unseen tasks using:

- Bash plus standard Unix tools.
- Nushell plus external commands.
- Bash plus QML where a GUI is required.
- `.program` with a discoverable applet registry.

Record total generated tokens, tool calls, execution attempts, repair iterations, wall-clock completion time, and acceptance-test success.

The key metric shouldn't be source-code length alone. It should be cost per successfully completed application.

Target improvement against the strongest baseline

Generated tokens

50%

At least 50% reduction

Repair iterations

50%

At least 50% reduction

First-pass correctness

95%

At least 95% success

Compare equivalent capabilities, including the cost of discovering applets and the cost of creating any missing reusable applets.

## Three architectural dealbreakers

Even passing many of the benchmarks wouldn't justify 10/10 if these fail.

1\. The applet adapter tax becomes too high. If every new integration requires writing bespoke Rust bindings and QML glue, you've simply moved application complexity into a growing adapter ecosystem. The interface needs to be standardized enough that new implementations plug in with minimal ceremony.

2\. Natural syntax fails under complexity. If conditions, transformations, data references, or error handling become ambiguous, an LLM will produce programs that read correctly but behave incorrectly. The grammar must remain small, formal, and predictable.

3\. Security depends on trusting every applet. A registry containing remote services and third-party local executables becomes a security boundary. Authorization must be enforced at execution and communication boundaries, not merely declared in a manifest. Omarchy's documentation explicitly notes that third-party QML plugins share the shell process and are not fully sandboxed.&#x20;

[image](https://www.google.com/s2/favicons?domain=https://plugins.omarchy.org\&sz=32)

Omarchy Plugins

+1



## The order I'd attack these

01

Prove Unix parity

Gates 1–6

Commands, pipelines, errors, streams, cancellation, and shell-level execution semantics.

02

Prove generic applet composition

Gates 7–9

A registry with typed interfaces, composable programs, and minimal adapter overhead.

03

Prove reliable distributed and reactive execution

Gates 10–12

Concurrency, event ordering, remote failures, and Quickshell integration.

04

Prove measurable superiority

Gates 13–15

Latency, actual LLM productivity, security, and sustained reliability.

## The actual 10/10 bar

My acceptance standard would be:

`.program` is a 10/10 Omarchy scripting replacement when a capable LLM consistently prefers it over Bash or Bash+QML on unseen real-world tasks—and the resulting programs are at least as correct, safe, reliable, and maintainable.

The most important comparison is not a 10-line `.program` against a 100-line Bash script. It's whether `.program` can assemble a 10-line solution from reusable applets without hiding new complexity, excessive dependencies, or failure modes elsewhere.

I would keep an explicit supported-scope statement: routine Omarchy scripting and application composition can reach 10/10 before `.program` is suitable to replace every Bash-dependent bootstrap or recovery mechanism.

My highest-priority proof: gates 1, 3, 7, and 14. They test Unix fidelity, lifecycle correctness, the scalable applet abstraction, and measurable AI advantage. If those four fail, the rest of the vision won't compensate.