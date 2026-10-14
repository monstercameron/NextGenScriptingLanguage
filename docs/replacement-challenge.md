# Replacement challenge: evidence before scores

The [supplied challenge](replacement-challenge-source.md) defines fifteen proof
gates for a Bash replacement and universal applet platform. **Execute 0.6.0 does
not meet that bar.** No overall score is assigned. Passing one local probe does
not close a gate whose platform, workload, or reliability requirements remain open.

This work retains the project's small Unix-glue scope. Remote protocols, QML
hosting, and a third-party sandbox would be separate architecture work. Their
absence still counts against the supplied *full-platform* bar; it is not removed
from the ledger to improve a score.

## Current gate ledger

The [machine-readable ledger](replacement-gates.json) carries the same gate IDs,
evidence, and remaining acceptance work. `partial` means narrower evidence exists;
`not implemented` and `not measured` are not passes.

| Gate | Requirement | Status | Evidence and remaining work |
| --- | --- | --- | --- |
| 1 · P0 | Unix compatibility | Partial | Native pipes, binary files, cwd, signals, and PTY tests exist. No stratified 100-command differential matrix; generic CLI exit 1, finite buffering, and literal arguments differ from Bash. |
| 2 · P0 | 1 GB real streaming | Partial | New local 1,000,000,000-byte probe verifies exact output hash, binary bytes, and two native launches. Compare a small input's high-water usage; Linux execution and broader slow-consumer/memory observations remain open. |
| 3 · P0 | Lifecycle, 10,000 repetitions | Partial | New seven-case repeatable suite checks record cutoff, signals, timeout, blocked output, failures, and reaping. Latest local run: 1,000 cycles / 7,000 probes. The 10,000-cycle target, crash/descendant cases, and Linux run remain open. |
| 4 · P0 | Complete scripting control flow | Not implemented | Named results, sequences, events, and schedules exist. General conditions, loops, functions, CLI input declarations, and parallel jobs are missing. |
| 5 · P0 | 150 real Omarchy ports | Not measured | The 150 local utilities are **not** Omarchy ports. No pinned upstream sample or disposable-desktop differential run exists. |
| 6 · P0 | Failure semantics | Partial | Pipeline failure, timeout, cancellation, uncertain checkpoint retry, and cleanup tests exist. Numeric statuses and stderr are not structured script-visible results; recovery branches are missing. |
| 7 · P1 | 1,000 applets across backends | Not implemented | There are 28 shipped manifest entries, covering executables and builtin cwd. No native/IPC/QML/remote backend abstraction or agent-facing registry discovery API. Aliases are not evidence of distinct integrations. |
| 8 · P1 | Five levels of nested programs | Not implemented | Same-file terminal components have text ports. A `.program` cannot become an imported, typed, lifecycle-owned applet. |
| 9 · P1 | Structured typed dataflow | Partial | Finite byte payloads, UTF-8 bindings, binary stream records, and text UI ports exist. JSON projection is available in widgets; schema-checked records/lists/events and connection typing are missing. |
| 10 · P1 | 1,000 concurrent operations | Not implemented | Bounded UI workers and stale-result rejection are covered at current scale. There is no general 1,000-operation concurrency primitive or randomized acceptance run. |
| 11 · P1 | Remote reliability | Not implemented | A CLI such as `gh` can perform a request. Runtime-owned auth, reconnection, offline/version semantics, and fault-injected transports are absent. |
| 12 · P1 | 20 Quickshell integrations | Not implemented | Execute renders terminal widgets. No Quickshell bridge or twenty-widget desktop acceptance suite exists. |
| 13 · P2 | Performance and idle behavior | Partial | Warm-cache 0.6.0 local control/render measurements and historical matched Bash workloads exist. True cold-cache p95, idle-spawn guards, Linux parity, and representative end-to-end wins are unproven. |
| 14 · P2 | AI advantage on 200 tasks | Not measured | No held-out comparative generation study, token ledger, or repair-cost dataset. Source length and this project's successful examples do not establish productivity gains. |
| 15 · P2 | Production/security/month of use | Partial | Private local control sockets, revision checks, tracing, bounds, and recovery tests exist. Applets retain user privileges; scoped permissions, credential policy, safe contract upgrades, and a month-long deployment study are absent. |

The [0.6.1 full audit](full-testing.md) extends the local evidence with CLI panic
fixes, source coverage, and 1,000 lifecycle cycles. The broad replacement verdict
remains unchanged.

## New reproducible proofs

From the project directory, using the packaged macOS binary or your own release
build:

```sh
python3 tests/replacement_proofs.py
python3 benchmarks/replacement_proofs.py --binary bin/execute \
  --lifecycle-repetitions 100 \
  --report benchmarks/replacement-proofs-local.json
```

The runner processes an 8 MiB fixture and a **decimal 1 GB** fixture by default.
Its `.program` runs `cat` into `cat`, with one process per stage and the final
stream sink declared in the script. Fixtures contain every byte value, including
NUL and invalid UTF-8, with explicit newline framing and paths containing spaces.
Output is hashed incrementally with a 64 KiB read buffer; no 1 GB Python output
capture or output file is created. Fixture generation is outside the measured run.

Seven probes run per lifecycle cycle:

1. Real `tail -f` → `cat`, exactly three binary records, then producer cleanup.
2. Ctrl-C while a file-following stream remains active.
3. A command deadline while the producer remains active.
4. A consumer that withholds every stdout read until cancellation.
5. A fixture producer that emits a partial result, writes stderr, and exits 7.
6. A native middle stage that fails while accepting a pipe.
7. Rejection of an oversized final record.

Every probe checks the expected runtime exit, exact native launch count, and
absence of traced native child PIDs after the runner exits. Where applicable it
also checks bytes, hashes, and diagnostics. The failure fixtures are test sources,
not application implementations or additional production integrations. The
harness's nonzero exit is itself a failure; a parse error cannot masquerade as
successful lifecycle cleanup.

For the challenge's repetition count:

```sh
python3 benchmarks/replacement_proofs.py --binary target/release/execute \
  --lifecycle-repetitions 10000 --report /tmp/execute-lifecycle-10000.json
```

This means 10,000 cycles of all seven cases, or **70,000 probes**. That run has
not been claimed here. For CI or harness development, use `--stream-bytes 1048576
--lifecycle-repetitions 1`. The full runner uses owned temporary files, up to the
requested fixture size, and removes them afterward. It has finite deadlines and
records partial progress on failure.

The [recorded macOS results](../benchmarks/replacement-proofs-v0.6.1-macos-arm64.json)
include binary/catalog/harness hashes, byte counts, timing observations, raw
high-water usage, launch counts, and every lifecycle result. `wait4` high-water
usage is **not** summed simultaneous process-tree RSS. Timings include tracing,
output hashing, and observation overhead; they are correctness/stress observations,
not matched throughput or tail-latency comparisons with Bash.

Raw stream mode preserves invalid UTF-8. A text listener instead rejects invalid
UTF-8 with an error. Those are distinct contracts; silently dropping or repairing
bytes is unacceptable in either mode. SIGKILL of the interpreter and deliberately
escaped process groups are not contained by the current supervisor.

## Next implementation order within the small language

1. **Results and failures:** expose numeric exit status, stderr, timeout, and
   cancellation as explicit result metadata. Specify when a failure aborts and
   how a handler can recover. Preserve argv boundaries and raw bytes.
2. **Minimal control flow:** settle one unambiguous form for conditions, bounded
   iteration, declared CLI inputs, reusable program calls, and bounded parallel
   jobs. Do not encode these as hidden Bash snippets or one-off Rust app logic.
3. **Unix differential matrix:** preregister 100 meaningful cases across streams,
   files, stderr/status, env/cwd, redirection, signals, and TTY behavior. Record exact
   bytes/status/filesystem effects and explicitly justified semantic differences.
   Unsupported cases remain in the denominator.
4. **Real Omarchy pilot:** pin an upstream commit and select a small stratified
   sample before translating. Grow to 150 with category quotas, 95% behavioral
   equivalence, and 100% safety assertions. Run mutations and graphical workflows
   in disposable Linux desktops; never substitute mocks for measured desktop
   equivalence. Bootstrap/recovery and Bash-sourced leaves need a separate contract.
5. **Discovery and composition:** add machine-readable catalog discovery and
   program-as-applet contracts before scaling integration counts. Count genuinely
   distinct capabilities; report adapter-authoring cost separately from script size.
6. **Comparative AI evaluation:** use the preregistered protocol below before any
   superiority claim. Start with a pilot whose tasks cannot enter the held-out set.

The remaining full-platform gates need decisions about backend protocols, typed
ports, remote ownership, desktop integration, and enforced permissions. They are
not solved by enlarging the English grammar or adding more example files.

## AI evaluation protocol

Freeze the runtime, registry, tool versions, baseline environments, task selection,
and acceptance oracles before generation. Reserve 200 unseen tasks and stratify
by finite processing, files, services, failures, concurrency, and UI. Do not expose
hidden acceptance tests or publish those tasks as examples first.

Use the same model/version, inference settings, task text, execution budgets, and
available native capabilities across Bash, Nushell, Bash/QML where needed, and
`.program`. Randomize run order and keep each run's work/context isolated. Charge
registry discovery, missing adapter creation, generated helper code, repairs,
and failed attempts to that arm. An unsupported task is a failure, not an exclusion.

Record generated tokens, tool calls, executions, repair turns, elapsed time,
first-pass correctness, final correctness, and total cost per successful task.
Define a repair turn and first-pass boundary in advance. Publish paired results,
raw transcripts with secrets removed, confidence intervals, and all exclusions.

The target is at least 95% first-pass correctness and at least 50% reductions in
both tokens and repair iterations against the **strongest comparable baseline**.
These are acceptance thresholds, not current results. Passing them does not
compensate for a failed critical correctness or security gate.

## Checked upstream constraints

Omarchy's [command metadata guide](https://github.com/omacom/omarchy/blob/quattro/agents/skills/command-metadata.md)
documents discoverable command descriptions and argument metadata. A replacement
should preserve useful CLI discovery rather than requiring agents to infer it.

Its [installation guide](https://github.com/omacom/omarchy/blob/quattro/agents/skills/install-scripts.md)
documents Bash-sourced setup leaves. Executing a child `.program` does not mutate
a parent Bash context; that compatibility gap must be explicit.

The [plugin guide](https://plugins.omarchy.org/develop.html) describes shared-process,
unsandboxed Quickshell plugins. The [testing guide](https://github.com/omacom/omarchy/blob/quattro/docs/testing.md)
separates desktop acceptance from non-graphical tests and places desktop work in
VMs. Terminal-widget tests on this macOS host cannot establish either GUI parity
or a stronger sandbox. These are current-branch references, not a pinned regression
corpus; the actual port study must record an immutable upstream commit.
