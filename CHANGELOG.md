# Changelog

Published versions are immutable. This file highlights user-facing changes;
[release receipts](docs/releases.md) and [CI](https://github.com/monstercameron/NextGenScriptingLanguage/actions)
record exact source revisions and validation. Historical reports retain their
original version labels.

## Unreleased

- Windows file publishes now ask the OS to persist the rename: both the
  POSIX-semantics rename path and the `MoveFileEx` write-through fallback end
  with a flush of the published parent directory, and the checkpoint, `then
  save to`, transaction-journal, and applet file/directory syncs run real
  directory flushes instead of no-ops. The parent directory is pinned open
  before the rename so the flush addresses that handle rather than a
  re-resolved path (a publish whose parent cannot be pinned fails before any
  rename); the pin-to-rename window itself is still path-based and only
  journal publishes exclude it via the checkpoint lock. Flushes open the
  parent without following a trailing reparse point (a junction or symlink
  parent is refused), but intermediate junctions in the parent path are still
  followed. A flush failure after a successful rename is reported as
  published-but-unsynced (the new data may be live; resume treats the effect
  as uncertain) instead of as a failed publish. These flushes only ask the OS
  to persist the rename: success means the flush call succeeded, volumes
  without flush support fail closed with an explicit error asking for a local
  NTFS directory, and no hard power-loss survival is claimed until the
  50-trial hard-reset lab passes.

- `execute run GRAPH --events jsonl` streams one `execute.graph-event/1` JSON
  line per terminal event as it arrives, flushing after every event and holding
  no collected outputs. Graphs with persistent nodes (timers, listeners, native
  streams, program services, or renderers) stream by default so a timer graph
  prints each tick within milliseconds and runs indefinitely; closing the
  reader (`| head -n 3`) cancels the session and exits quietly through the
  SIGPIPE convention (141). Finite
  graphs keep the closing `execute.graph-run/1` document unless `--events jsonl`
  is passed. The event schema is `docs/schemas/graph-event.v1.schema.json`.
  Follow-up hardening: queued events are now delivered before a worker failure
  surfaces instead of a clean EOF, each streamed line is bounded by a per-event
  limit derived from the message byte limit, graph runs refuse `--record` and
  `--answers` instead of ignoring them, and the daemon declines event streams
  (explicit `--events jsonl` and unflagged persistent graphs alike) so the
  client streams them directly. A closed reader that lands after a worker
  failure reports the failure instead of a quiet pipe close.

- Uncertain nodes and `for each` elements can now be resolved per item.
  `execute checkpoint resolve DIR --node STEP_ID [--element N] --as committed|retry`
  records one operator decision with who (`uid:<euid>` on Unix), when and why;
  `committed` replays the supplied output and bindings at resume instead of
  rerunning the effect, `retry` authorizes one item to run again without the
  bulk `--retry-uncertain` flag. Element nodes take only per-element records;
  parallel (`parallel for each`) elements resolve as `retry` only. Resolve holds
  the checkpoint lock across validation and append, so concurrent resolves fail
  closed. Journals are version 6 (in-flight element kind); versions 1-6 read,
  graph journals 5-6 read. `checkpoint status --json` reports decisions under
  `resolutions` and still-undecided items under `unresolved_uncertain`, using
  the same step ids and 0-based element indices. Run records and `show-run`
  have no checkpoint pointer and do not show resolutions.

- A repeat interrupt (Ctrl-C) during a `finally`, `on exit` or `on interrupt`
  body now aborts the cleanup at once and kills its running command, exiting
  with status 128 plus the interrupting signal (130 for SIGINT) and a
  `cleanup interrupted` diagnostic instead of waiting out the 5 s cleanup
  budget. A single interrupt still runs cleanup to completion.

- Run records now carry a durable policy-decision audit. `execute run
  --record` writes the allow/deny decisions into `run.json` with the execution
  mode and which policies were configured (never policy contents), including a
  baseline on runs with no policy so a record never silently implies
  confinement that was not there. `execute show-run` prints the audit in text
  and JSON output. The audit keeps the most recent 256 decisions with an
  explicit total and truncation flag.

- The run-record policy audit is hash-chained and verified. Each kept decision
  links into the record's `audit_head`, and `execute show-run` re-verifies the
  links, the sequence order and the truncation accounting on every read: an
  edited, deleted or reordered decision, or a truncation flag that disagrees
  with the total, is shown labeled untrusted with an `audit_error` rather than
  hidden. Records from the first bounded
  audit stay readable but are labeled unverified. The chain evidences naive
  edits and corruption, not a same-user forger who recomputes it;
  whole-record tamper evidence remains separate record-integrity work.

- The run-record policy audit is sealed and warn-open. `execute run --record`
  seals the chain head plus the truncation accounting with `audit_mac`, an
  HMAC under the per-user key, and names each configured policy by version
  and a one-way content digest (never policy contents). `execute show-run`
  always presents the audit instead of hiding it on tamper: a failed chain or
  seal check, or a record with no usable seal, is shown labeled unsealed or
  untrusted, with an `audit_error` in JSON output. The seal raises the bar to
  key holders -- a same-user editor with key access can still rewrite the
  audit and reseal it, so whole-record tamper evidence remains separate
  record-integrity work.

- `env.load` can mark loaded values secret with `secret true` or
  `secret ["NAME", ...]`; marked values are redacted from records, traces and
  diagnostics and refuse `--checkpoint` before effects. Checkpoint
  documentation now states the refusal instead of HMAC storage.

- On Windows, `hand over to` now runs the replacement program in a job object
  that ends it if the interpreter process is killed, and the wait observes
  cancellation and the workflow deadline instead of blocking unconditionally.
  The exit code is still propagated unchanged.

- Applet records can now validate arbitrary property names with per-value
  schemas and property-count bounds. Object-only applet inputs reject known
  non-object values during checking while retaining dynamic object keys.

- JSON parameters can declare recursive list and closed-record schemas with
  mutually exclusive `one_of` variants, finite string enums, and list length
  bounds. The bundled `fs.transaction` and `json.filter` manifests now statically
  check their operation and predicate variants while deferring unresolved
  references.
- Applet manifests can constrain string arguments to octal modes or six-digit
  hexadecimal colors with an optional `#`. Invalid literals fail static checking;
  referenced values are checked after resolution.

- `execute check` now rejects literal `csv.stringify` tables with duplicate headers,
  empty rows or row widths that disagree with the headers (or first row when headers
  are absent). Tables supplied through references keep their runtime validation.

- Program inputs now support `list of integer` and `list of json`; integer list
  values are statically typed when iterated and invalid items fail before execution.
- `number.min` and `number.max` declare a required, nonempty list of numbers, so
  invalid literal shapes fail static checking while dynamic JSON remains checked at runtime.

- Measured comparison with bash and dash. `tools/bash_compare.py` pairs a bash
  command line with an Execute program for eight scenarios and reports median wall
  time, peak RSS and binary size; the `Execute vs bash` workflow runs it on x86_64 and
  arm64 Linux for every push to `main`. On the x86_64 runner (default lean build,
  commit `234c1fc`) Execute is slower than bash on startup (1.76 against 1.20 ms), one
  external command, a 5-stage pipeline, 1 MB of uppercase text, a JSON lookup and a
  100 MB pipe, and uses more memory (6.2 against 3.4 MB) and a larger binary (7.16
  against 1.45 MB). It is faster on a 1,000-iteration builtin loop (2.80 against
  4.37 ms) and marginally on a 100-spawn loop (63.86 against 67.07 ms). The workflow also
  compares `opt-level = "s"`, static musl and musl `min`-profile builds. Optimisation is
  ongoing. See [the numbers and method](docs/production-readiness.md#performance-against-bash-and-dash).
- Operational instruction gate on shared runners. `tools/bash_compare.py` reports a deterministic
  bootstrap 95% confidence interval for every median wall time; the wall-time
  comparison runs counter-free so counting never perturbs its medians or budget.
  The `Execute vs bash` workflow installs valgrind, builds two distinct binaries
  on the same runner (the baseline at the base revision and the candidate at the
  head revision), and measures the eight gated
  CPU-bound scenarios once per build, back-to-back, pinned to cachegrind
  `I refs` (no PMU required), both with the head-revision tool so a base
  predating newer flags still gates and both reports share one tool hash
  (`--source-revision` stamps each report with its built revision); each
  focused report records the backend plus counter diagnostics
  (`perf_event_paranoid`, probe outcomes), the measurement-tool hash, and the
  `valgrind --version` provider string, and CI prints real
  per-scenario counts with coverage. `tools/check_instruction_regression.py`
  defines the +2% gate for CPU-bound rows: CI runs it fail-closed with
  `--fail-closed --require-provenance` on the baseline-versus-candidate pair, then runs a canary
  proving the wired gate fails a synthetic +3% regression on the measured candidate.
  The canary proves the wiring is live but is not itself a regression gate.
  Revisions come from `tools/resolve_gate_revisions.py` (pull-request base/head;
  push before/sha with a HEAD~1 fallback; dispatch inputs or HEAD~1 versus HEAD).
  An inconclusive canary or incomplete pair fails the job; a missing backend
  against a recorded one is rejected as incomparable units, as are a differing
  tool hash or valgrind version. Measurement budgets
  match the former same-binary pair; cachegrind overhead against the shared
  10-minute step timeout, static musl behavior, and the doubled per-variant
  build cost are still unverified on Linux CI.
- Twenty more Omarchy v4.0.4 scripts are ported under `examples/omarchy-ports/<script>/`
  with the Bash original, the port and a `case.json`. `tools/port_compare.py` runs both in
  sandboxes with logging fake binaries and compares status, stdout, stderr, the
  fake-call log and the final file tree over 75 cases; CI runs it on Linux.
  `tools/omarchy_port_report.py` reports which Execute features each pinned Omarchy
  command needs. The ports follow Bash's streaming rule. See
  [the ports](examples/omarchy-ports/README.md).
- Thirty typed manifests wrap Omarchy desktop tools (notifications, menus, systemd user
  units, `pacman`/`yay`, Hyprland), and `examples/desktop/theme-switch.program` swaps a
  theme with rollback. `examples/stdlib/` shows the in-process applets below.

- Warm daemon. `execute daemon serve|status|stop` keeps a per-user process behind a
  checked Unix socket, and `run FILE --daemon` (or `EXECUTE_DAEMON=1`) forwards a run to
  it, falling back to a normal run when none answers. The forked child takes the client's
  directory, environment, umask, limits and standard streams; programs that may take the
  terminal are declined. It caches parsed user manifest registries (up to 8, per request
  isolated) and wakes on child exit instead of polling. It helps only when manifest and
  policy setup dominates: a one-line program is level with a cold run, and a
  530-manifest directory with two policies ran 14 percent faster. Unix only. See the
  [daemon section](docs/language.md#warm-daemon---daemon).
- Live control endpoints are confined to private directories. `execute live serve`
  refuses a Unix socket whose parent directory is not owned by you and private
  (mode 0700), and `run --control` hosts its endpoint under a private per-user
  directory with a randomized name instead of a predictable `$TMPDIR` path. A
  reserved lane keeps `control stop` and `read` working while all 16 client slots
  hold long polls; other requests past the slot limit fail fast with `busy`. A
  burst-tolerant flood budget admits benign bursts (2048 back-to-back
  non-priority requests, refilling at 64 per second) while sustained floods shed
  with `busy`; reserve-eligible priority never spends the budget, and partial
  senders wait in a bounded 64-connection queue holding no thread or slot. When
  `--agent-policy` is configured, only requests already authorized for `read` or
  `control stop` may claim a reserve slot; other fake-priority frames are demoted
  to the general lane, as are no-policy claims carrying `auth` and
  authorized-but-invalid `read`/`stop` frames the handler would reject. Shed
  `busy` replies use non-blocking writes that may truncate under saturation
  (callers should retry `busy`, EOF, and truncated responses like `busy`; the
  CLI itself performs a single attempt and does not auto-retry). See the
  [live control contract](docs/live-control.md#resource-and-compatibility-contract).
- Bundled manifest index. The shipped manifests are embedded as one deflate stream with a
  generated index and parsed on first use, so registry load fell from 5.41 to 0.32 ms.
  The default directory uses the index when it matches by name and size; a directory
  given with `--manifest-dir` or `--manifest-catalog` keeps full validation. A test fails
  when the checked-in index is stale. Parsed manifests are shared behind `Arc`.
- Lean default build. `default = []` omits the terminal UI, model helper, live control
  (and `attach`, `run --control`), remote backend, Quickshell renderer, debug adapter and
  language server; `--features full` includes them, and release packages, `make check`
  and CI use `full`. A lean build refuses a gated feature with a message that says to use
  the full build, and manifests declare `build_feature` (`requires_full_build` in their
  contract). Release builds use fat LTO and one codegen unit; a `dist` profile builds
  packaged artifacts, and an opt-in `min` profile builds a size-first binary that aborts on
  panic. See [build features](CONTRIBUTING.md#build-features).
- Linux supervision waits on child exit with a pidfd instead of timed polling, the Windows
  worker waits on events, and the Windows launcher finds the suspended primary thread
  without a system snapshot.
- Packaging: Arch PKGBUILDs for a release tarball and a source build (not submitted to the
  AUR), bash/zsh/fish completions and a man page in the `.deb` and `.rpm`, a deterministic
  CycloneDX SBOM, cosign keyless signing of `SHA256SUMS` with an SBOM attestation, and a
  reproducibility check that builds the musl binaries from two checkouts. Every workflow
  action is pinned by SHA. The minimum supported Rust version is 1.89.0, checked weekly,
  with Linux arm64 and static Alpine builds. See [releases](docs/releases.md).

- Checkpoint journal version 5. It adds `graph` mode, so finite typed graphs checkpoint,
  resume and report through `execute checkpoint status DIR [--json]`; program and graph
  journals refuse each other, and older builds refuse version 5. Version 3 moved completed
  nodes to an append-only `completed.jsonl` log, and a persist serializes the journal once.
- Per-element checkpoints. A top-level `for each` or `parallel for each` records each
  element in `elements.jsonl` (journal version 4) and resumes after the committed ones.
  A changed committed element refuses the resume, a begin record without a commit is
  uncertain for that element only, and the log is anchored in the journal. A parallel
  resume bug that reran every element is fixed. See
  [per-element checkpoints](docs/language.md#per-element-checkpoints).
- Effect-based retry safety. A stage is retry-safe when it is builtin, cacheable, or an
  in-process applet whose declared effects are empty or reads. A checkpointed loop over a
  pure in-process applet no longer writes a durable begin record per element, and on
  resume such a node runs again without `--retry-uncertain`. A checkpointed 10,000-element
  loop over a pure in-process applet (`text.upper`) fell from 28.6 s to 149 ms (median,
  one developer machine). Native commands, writes,
  locks, network and undeclared effects stay uncertain. See
  [retry safety](docs/language.md#retry-safety).
- Resume matches completed program nodes by step id, so comments, line moves and edits
  below the last completed node resume without a rerun, and an insertion or removal above
  a completed node is refused with the node and both lines named. Background jobs
  checkpoint (a `spawn` and its `await` or `cancel` form one node).
- Checkpoint input digests are a keyed HMAC under a per-user key. A journal written
  before the change matches its unkeyed digest once and is rewritten; a keyed journal never
  matches the unkeyed digest (a bug that kept legacy journals unmigrated is fixed).

- `use folder` and `cd` open the folder, authorize the resolved path, check its identity
  and record the resolved path, and launched children enter the opened directory with
  `fchdir` (Unix). A relative read, save, append or redirect after a directory change is
  authorized when its step runs, so a capability policy no longer refuses that pattern in
  preflight. See [working directory](docs/language.md#working-directory-and-use-folder).
- Security work from an adversarial review, with [threat model](docs/threat-model.md)
  holes H1-H19 and severities: diagnostics escape control bytes before reaching a
  terminal; `and environment` and manifest overlays refuse `LD_*` and `DYLD_*` without a
  policy; `read` and redirect targets are authorized and opened before the command runs
  and are no-follow hardened; records read only `blobs/<sha256>` names; secret prompt
  answers stay out of inspect, records, live reads and checkpoint signatures; redaction
  runs before truncation; a privileged-launch check sits at the single spawn point;
  manifests are never loaded from the working directory; model credentials are scoped to
  provider hosts and `api_key_env` must start with `EXECUTE_MODEL_KEY_`; and the daemon
  checks its socket directory, socket and peer user. Windows grant matching is exact.
  Injection through `socat` addresses and `hyprctl dispatch` is closed, and TOML and
  YAML parsing is bounded in depth, value count and work.
- Unsafe audit: all unsafe blocks carry a `// SAFETY:` comment, clippy denies undocumented
  unsafe blocks, and process groups are signalled before the child is reaped so a recycled
  pid cannot be hit. `tools/check_unsafe.py` runs in CI.
- New CI workflows. `security.yml` (cargo audit and deny, gitleaks, tracked-env refusal,
  CodeQL, dependency review), `fuzz.yml` (eleven libFuzzer targets nightly, seed and crash
  replay on every push), `sanitizers.yml` (nightly Miri, AddressSanitizer and
  ThreadSanitizer), `crash-consistency.yml` (nightly random kills and injected
  checkpoint write failures, plus journal and parser property tests), `soak.yml`
  (one-hour service, live and graph soaks asserting flat memory, descriptors and
  threads), `flaky.yml` (nightly 20-run suite on three operating systems with no
  retries), and `bash-compare.yml`. Linux size, latency and hot-path budgets run in the
  native checks, and each `main` commit now has its own concurrency group so it runs to
  completion. Some of the new nightly workflows have not yet produced a passing run; see
  [CI status](docs/production-readiness.md#ci-evidence-and-its-status) and
  [CONTRIBUTING](CONTRIBUTING.md#continuous-integration-and-nightly-checks).

- Programs can state their language version (`execute 1`), label steps (`fetch:`) and
  rely on stable step ids that ignore line numbers; ids appear in `explain --json`,
  `--trace`, records and execution records.
- Output and status: an uncaptured top-level `run` streams its stdout as it is produced,
  like Bash (behavior change: `run printf "a"` then `run printf "b"` prints both), but
  never when a later stage captures; a closed reader ends the run quietly with status 141;
  a native failure keeps its streamed output; an interrupted run exits 128+N after it
  cleans up, and SIGHUP now cancels; `exit with status N` and `fail with message` choose a
  finite program's exit code.
- Control flow: `otherwise if` chains, `if run ... succeeds|fails` and the matching
  `while`, `on exit` and `on interrupt` handlers with a bounded graceful drain
  (`run --drain-timeout-ms N`), and `and retry up to N times` with effect-aware refusal.
  Comparing values of different kinds is now an error, not `false`.
- Process handling: redirection suffixes (`and stderr to`, `appended to`, `merged`,
  `and stdout to stderr`) and `then append to`; per-command `with environment`
  overlays; `interactively` hands the terminal to one command; `hand over to` replaces the
  process (`execve` on Unix); `launch detached` starts an unowned child; a leading `~/` in
  paths and path-typed manifest parameters expands; `run program file` calls another
  `.program` file under the host's read policy; scoped `with temporary file|directory`;
  and every static executable is resolved, with all missing ones reported together, before
  the first effect. `execute run --init FILE` runs a program as PID 1 in a container (Unix). See
  [language reference](docs/language.md).
- Inputs: `accept ... one of [...]`, `described as`, `from program directory|path`, flags and
  options (`from flag "--name"`, `from option`), and `from prompt` / `from secret prompt`
  answered from `--input`, `--answers FILE` or the terminal. `use value` and `use bytes`
  feed text or bytes to the next stage, bracketed continuation lines and raw `"""` blocks
  are accepted, and `execute help FILE [--json]` and `run FILE -- --help` print generated help.
- Applets: in-process applets for file tests, globbing, text and path builders, `{{ }}`
  templates, YAML/TOML/INI parse and stringify (strict subsets), idempotent `fs.ensure_*`,
  and `env.load`. An in-process text result now crosses pipes, saves and the terminal as raw
  UTF-8 rather than JSON. Model applets gain local and compatible endpoints, `effort` and
  `temperature` controls, Jev `criteria`, `jev.ask` and typed result records.
- Agent interfaces: `check --json` and `explain --json` (`execute.explain/1` effect plan,
  with `--allow` approval gate, `--policy-header`, privileged-step flags), stable diagnostic
  codes with did-you-mean fixes, `--json-errors`, `run --what-if` with `--confirm
  FINGERPRINT`, `--single-instance`, run records (`--record`, `show-run`, side-effect-free
  `replay`, `--as-test`), `execute repl`, `execute repair` for a checkpointed program,
  `execute test`, `execute run-parts`, `check --format github` annotations, published JSON
  Schemas for machine output, `execute attach SOCKET`, `run --control`, `execute dap` with
  breakpoints and stepping over the live endpoint, and `execute compile --to sh`. See
  [machine-readable checks](docs/language.md#machine-readable-checks).
- `execute service generate systemd-timer` prints a review-only timer and oneshot service
  for a cron program, and `run --once` runs a cron body once and exits.
- Plain `check` runs the same static analysis as `check --json`. A streamed step's
  execution record reports `captured: false`. A formal [grammar](docs/grammar.md) is
  tested against the real parser.
- VS Code source package grammar covers the newer syntax above; see its changelog.
- Internal refactors (argument and context structs in place of long parameter lists, CLI
  parsing and the runtime step loop moved into modules) change no behavior.

- `parallel for each <name> in <list> at most N as <result>` runs one named
  applet or quoted executable per list element with bounded concurrency and
  returns one record per element plus success/failure counts. Element failures
  do not stop siblings; cancellation and deadlines cancel and reap running
  elements. See [parallel for each](docs/language.md#parallel-for-each-current-source).
- Finite programs and calls can own named background native pipelines with
  `spawn`, `inspect`, idempotent `await`, and isolated `cancel`. Handles and
  captured output share run-wide budgets; unresolved jobs are canceled and
  reaped on unwind. Persistent lifetimes and detached jobs remain unsupported.
- JSON-lines applet workers support native Windows Job and pipe supervision.
  Native Windows execution of this candidate remains subject to its CI results.
- Process accounting retains job and native-stream launches when a nested call
  fails or a graph stream is canceled, without counting observer reports twice.
- Current source includes a narrow Quickshell graph adapter for text and button
  components. It starts one renderer process per node; real desktop acceptance
  and shared renderer sessions remain open.
- Interface-2 `.program` modules can export an `every` or cron service as a
  typed graph node, with single startup inputs and one decoded event per
  handler. Graph cancellation owns its worker pool and cleanup; finite exports
  retain interface 1. Listener exports remain unsupported. See
  [program modules](docs/program-modules.md).
- Process counts retain launches from caught module failures and supervised
  resolver helpers. Cleanup handlers preserve module and worker context.
- Graphs accept ordinary typed `accept` declarations, named CLI inputs and
  positional arguments. Static checks and inspection describe unresolved
  inputs; runtime inputs resolve before graph effects. See [typed graphs](docs/graph.md).
- Typed applet discovery suggests bounded multi-stage converter plans and
  retains only the requested top-ranked endpoint pairs. Plans describe declared
  compatibility, not conversion costs or losslessness. See [discovery](docs/language-server.md).
- Bounded finite `csv.parse` and `csv.stringify` applets preserve UTF-8 fields
  as text and expose a typed `{headers, rows}` contract. Malformed quoting,
  duplicate headers and ragged records fail. See [CSV applets](docs/csv.md).
- Repair Windows process/listener API mismatches and target-specific compiler
  diagnostics, and update the Linux graph stress fixture for enum schemas.
- In-process applets now cooperatively check cancellation, the per-call command
  timeout and the parent total deadline during bounded JSON transforms, text
  transforms and JSONC scanning. Synchronous OS calls and parser/serializer
  library calls remain non-interruptible; started applet interruption reports
  uncertain effects. See [applet contracts](docs/applet-contracts.md).
- Versioned applet contracts add typed inputs and outputs, explicit outcome
  routing, result envelopes, named typed ports, per-applet environment changes,
  and `capture`/`discard`/`inherit` stdio policies.
  Command, in-process and persistent JSON-lines worker backends share the same
  call syntax; see [applet contracts](docs/applet-contracts.md).
  Envelopes include classified effect certainty so callers can route uncertain
  outcomes without guessing from status codes.
- New `execute-applet` helper and bundled generic path, text, numeric, color,
  filesystem and JSONC operations. Single-file writes use content-hash compare
  and swap, locking and atomic replacement. A separate bounded `fs.transaction`
  applet retains a journal for file-only recovery; it applies files one by one,
  so visibility is partial during the batch and external effects are not
  covered. The helper is included by current source packaging scripts.
- Optional strict v1 capability policy checks canonical applet grants, declared
  capabilities and environment allowlists. `sandbox_required` separately opts
  into fail-closed macOS/Linux command sandboxing; this is not implied by
  metadata, and in-process operations cannot use the OS sandbox. See
  [security boundaries](SECURITY.md#applet-contracts-and-optional-capability-policy).
- Native and browser `repeat`, `for each` and `while` blocks, with typed outer
  updates, iteration-local names, bounded execution and whole-loop recovery.
  Includes three native utilities and a browser batch workflow; source builds only.
- VS Code source package 0.2.2 highlights loop headers and adds three checked
  loop snippets.
- Source builds provide a stdio LSP server for diagnostics, completion, local
  reusable-program navigation/rename, applet discovery and typed-port matching;
  see [language-server guide](docs/language-server.md).
- Source builds accept typed applet graph files with preflighted ports, bounded
  event queues and inspectable `check`/`inspect`/`explain`/`run` surfaces; see
  [graph guide](docs/graph.md). The graph limits do not bound arbitrary applet
  I/O or memory.
- Optional `--data-flow-policy` controls labeled remote egress and plaintext
  persistence separately from applet capabilities. Review-only
  `execute service generate systemd|launchd` output is also available in source;
  it never installs or starts a service. See the [data-flow guide](docs/security-dataflow.md)
  and [service generation guide](docs/guide.md#generate-a-reviewable-service-definition).

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
