# Threat model

Status: first version. It states the assets, trust boundaries, attacker models and
controls, then lists the known holes with severities. Holes marked *from SECURITY.md*
are documented limitations that predate this model. Holes marked *audit* are added
only when a test under `tests/security_*.rs` or the audit map confirms them. Keep
this file current: a change that adds a path, process, terminal, secret or resource
limit answers the vector it affects (see the [security checklist](../CONTRIBUTING.md#security-checklist)).

The policy for disclosure and the list of supported boundaries live in
[SECURITY.md](../SECURITY.md). This document explains how those boundaries map to
attackers and tests.

## The default trust model: no policy means Bash's trust model

With no capability policy, Execute runs as the Bash-style trust model: applets,
native commands and scripts run as the caller, with the caller's file, network and
process permissions. Explicit paths keep their normal semantics, including `..` and
symbolic links, because the author asked for them. No sandbox confines them, and no
environment or argument filtering applies beyond what the program itself says.
Literal argument passing prevents implicit shell expansion, but an explicit shell
invocation still interprets shell code. Only the opt-in capability policy and OS
sandbox add the checks described in SECURITY.md; they are not on by default.

## Assets

| Asset | Why it matters |
| --- | --- |
| User files reached by `Read`, `Save`, `UseFolder` and applets | Integrity and confidentiality of the caller's data. |
| Checkpoint journals and blobs | A tampered journal can make a resumed run repeat or skip effects. |
| Cache entries | A tampered entry can supply a stale or attacker-chosen result. |
| Records and replay inputs | They can hold inputs and outputs, so they can leak data. |
| Secret prompt answers and credentials (API keys, bearer files) | Must not reach traces, journals, records, explain or what-if output. |
| The caller's terminal | Escape sequences in untrusted text can rewrite the screen or set titles. |
| Host resources (memory, file descriptors, processes, disk) | Exhaustion stops other work on the same machine. |

## Trust boundaries

Trusted, by design: the program author, applet manifests, the scripts and the
selected executables and their dependencies (*from SECURITY.md*: "remain trusted
inputs"). A program that runs untrusted code is outside this model.

Untrusted data, by design: values that flow into a run from outside the script:

- applet and command output, and streamed output;
- environment variables and program input values;
- file contents and file names inside directories the program reads;
- remote model and HTTP responses;
- request bodies and documents sent to the live control endpoint.

Other users of the machine: refused. Live control checks the peer's user identity
before it accepts data. Other processes of the same user are not isolated from the
caller's state, see the holes below.

## Attacker models

- **A1, hostile data source.** Crafts output, input values, environment values or file
  names that try to escape a directory, inject an argument or command, inject terminal
  escapes, or inflate output.
- **A2, hostile remote endpoint.** Returns crafted model or HTTP responses, or tries to
  redirect a request.
- **A3, racing same-user process.** Swaps a path component for a symbolic link between a
  check and an open, or replaces an executable between a hash and a launch.
- **A4, tampering with state.** Edits cache, checkpoint or record files that the same
  user owns.
- **A5, resource exhaustion.** Supplies huge inputs, deep nesting, endless output or
  blocking files (FIFOs, devices) to stall the run or the host.

A1 and A5 are in scope for the adversarial suite. A3 and A4 are partly in scope: the
cache and checkpoint code rejects symbolic links and unsafe permissions, but races on
ancestor directories remain open (below).

## Controls and where they are tested

| Vector | Controls | Evidence |
| --- | --- | --- |
| Traversal and symbolic links (cache, checkpoint) | Cache reads require regular files; no-follow opens; owner and mode checks; checkpoint no-follow and private permissions. *From SECURITY.md.* | `tests/cache_security.rs`, `tests/checkpoint_*.rs` |
| Traversal and symbolic links (explicit paths) | None by design: explicit paths keep normal semantics. A policy adds path checks. | Policy tests in `tests/capability_policy.rs`. |
| Check-then-open | Opened file descriptor is checked again on Unix cache reads. | `tests/cache_security.rs` |
| Argument and environment injection | Literal argument vectors for ordinary commands; policy environment allowlist; explicit shells only on request. | `tests/direct_commands.rs`, `tests/capability_policy.rs` |
| Terminal escapes | Error text and diagnostics escape control bytes (ESC, CSI, OSC, C1) before they reach a terminal (`terminal_text::escape_controls`). Streamed stdout stays raw so pipes keep their bytes (H17). | Unit tests in `src/terminal_text.rs`. |
| Secrets in traces, journals, records, explain and what-if | Secret prompts are read with echo off. Their values are withheld from inspect, records (replay refuses them), live reads and snapshots, and checkpoint signatures (a keyed HMAC). A checkpoint refuses a run with a secret prompt input. Redaction is by exact value (H14). | `tests/security_redaction.rs`, `tests/prompt_inputs.rs`, `tests/what_if_confirm.rs` |
| Checkpoint state tampering and guessing | Journals are checksummed and chained; input digests are an HMAC under a per-user key (H11); element and graph logs are anchored in the journal, and a shorter or re-chained log refuses the resume. | `src/runtime/checkpoint.rs` unit tests, `tests/checkpoint_*.rs`, `tests/crash_consistency.rs` |
| Working directory | `use folder` and `cd` open the folder, authorize the resolved path, check identity and record the resolved path; children enter the opened directory (H12). | `tests/security_fixes.rs` (Unix) |
| Daemon socket | A private per-user socket directory, owner checks on the socket and the directory, a peer user-id check after connect, and refusal of a second daemon. Forwarded runs inherit the client's directory, environment, umask and limits. | `tests/daemon_cli.rs` (Unix) |
| Parsers and decoders fed hostile bytes | Eleven libFuzzer targets (program parser, manifests, env files, input values, DAP and live frames, checkpoint, graph, record, element and completed logs), a replay of seeds and saved crashes on every push, and property tests that journals mutate into typed errors. | [`fuzz.yml`](../.github/workflows/fuzz.yml), `tests/fuzz_regressions.rs`, `tests/crash_properties.rs` |
| Live control | Peer identity check; two-second wire budgets; explicit grants; same-user plane. | `benchmarks/security-2026-10-15-live-transport.json` |
| Model transport | HTTPS, with HTTP only for loopback; redirects and environment proxies disabled. | `tests/models.rs` |
| Resource limits | Output limits, record size limits, command timeouts, parser nesting depth, live budgets. | `tests/cache_security.rs`, `tests/conditions.rs`, `tests/adversarial.rs` |
| Policy (opt-in) | Strict policy schema; per-applet grants; environment cleared and allowlisted; path grants checked before effects; OS sandbox when required. | `tests/capability_policy.rs`, `tests/security_dataflow_runtime.rs` |

## Known holes

Severity scale: **High** breaks a documented guarantee for a supported configuration.
**Medium** breaks a guarantee under a realistic same-user or policy-enabled attack.
**Low** is a documented limit with a small blast radius, or a fail-closed gap.
**Informational** is a design boundary that users must understand.

| ID | Hole | Severity | Source |
| --- | --- | --- | --- |
| H1 | Ancestor directories of cache and checkpoint paths are not pinned against concurrent replacement. | Medium | *From SECURITY.md* |
| H2 | Cache entries are not authenticated against another process of the same user; there is no integrity envelope. | Medium | *From SECURITY.md* |
| H3 | Executable hash checks do not pin the inode through `exec`; a replacement can happen between hashing and launch. | Medium (policy with `binary_sha256`) | *From SECURITY.md* |
| H4 | Linux bubblewrap limits filesystem and network access but not which executables the sandboxed applet can start. | Medium (policy with `sandbox_required`) | *From SECURITY.md* |
| H5 | Detached descendants can outlive their parent; no universal cleanup guarantee. | Low | *From SECURITY.md* |
| H6 | Windows does not support required applet sandboxes. Policies that require them fail closed. | Low (fail-closed availability gap) | *From SECURITY.md* |
| H7 | Arbitrary filesystem operations have no deadlines; a FIFO or a stalled network mount can block a run. | Medium (resource exhaustion) | *From SECURITY.md* |
| H8 | The macOS sandbox uses the deprecated, undocumented `sandbox-exec` interface. | Low | *From SECURITY.md* |
| H9 | Live control is a same-user plane that grants native execution as that user. | Informational | *From SECURITY.md* |
| H10 | Explicit paths follow symbolic links and `..` by design under Bash's trust model. | Informational | *From SECURITY.md* |
| H11 | Checkpoint input digests were unkeyed SHA-256, so low-entropy inputs could be guessed from a journal. **Fixed:** new checkpoints record an HMAC under the per-user key. Journals written before the switch match the legacy digest once, and the resume rewrites them with the keyed digest. A marked journal never matches the legacy digest. Journals from before the switch also lack the keyed marker, so the rule is version-gated by that marker. A newer build's journals are refused by older builds. | Medium, fixed | *audit* |
| H12 | The working directory set by `use folder` (and `cd`) was re-resolved by path. **Narrowed:** the folder is opened first and the path it resolves to is authorized. The handle then records that resolved path, so later relative paths join onto the authorized directory. Call frames carry a duplicate of the handle, and background jobs and handoffs use the run's handle, so a link swapped later cannot move them. Launched children enter the opened directory with `fchdir` before `exec`. Still path-based: sandboxed children (the sandbox sets its own directory), and the runtime's own relative file operations, which are checked on their resolved targets when they open. Test: `a_relative_read_after_a_swapped_link_reads_the_authorized_directory` (Unix, `tests/security_fixes.rs`). | Low, narrowed from Medium | *audit* |
| H13 | Read and use-folder identity checks use device and inode on Unix only. Windows has no equivalent check. | Low | *audit* |
| H14 | Redaction matches exact secret values only; a derived value (a substring, an encoding, a case change) is not redacted. | Low | *audit* |
| H15 | Relative `PATH` entries resolve against the working directory, as in Bash. A program that chooses both the directory and `PATH` from data can run an executable from that directory. | Low, Bash-model | *audit* |
| H16 | With no policy, a child inherits the caller's full environment, except that the dynamic loader's variables (`LD_*`, `DYLD_*`) cannot be set from a program or manifest. | Informational, Bash-model | *audit* |
| H17 | Applet stdout is passed through raw so Unix pipes keep their bytes. Terminal control bytes in error text and diagnostics are escaped; streamed stdout is not. | Informational | *audit* |
| H18 | Listeners and persistent workers launch with a path-based working directory, not a handle. A link swapped in after a `use folder` that set their directory can move them. Background jobs and handoffs already carry the run's handle. | Low, follow-up | *audit* |
| H19 | The daemon reads each request on its single accepting thread (it must stay single-threaded because it forks), with a 2 second timeout. A same-user client that connects and sends nothing delays other clients for up to 2 seconds. Other users cannot connect. | Low, same-user delay | *Documented in `src/agent/daemon.rs`* |

Fixed in the audit, with the test that shows each fix:

- Error text reaching stderr escapes control bytes (`terminal_text::escape_controls`, unit test).
- `and environment` refuses `LD_*` and `DYLD_*` without a policy (`tests/security_fixes.rs`).
- A one-of input error does not echo a sensitive value (`tests/security_fixes.rs`).
- `Read` and `use folder` authorize the resolved path and check identity against the opened file (`tests/security_fixes.rs`, Unix links module).
- Record blob names must be the writer's `blobs/<sha256>` form, and record files are read without following links (unit tests in `src/record.rs`).
- A journal written before the keyed digest matches its legacy digest once and is rewritten with the keyed digest; a keyed journal never matches the legacy digest (unit tests `a_legacy_journal_matches_its_unkeyed_digest_once_and_migrates_to_the_keyed_form` and `a_legacy_digest_is_refused_for_a_keyed_journal_and_for_changed_inputs` in `src/runtime/checkpoint.rs`, Unix-only).
- A secret prompt on Windows refuses to run if echo cannot be turned off.
- `ensure` rechecks the link type just before it renames a replacement link (narrow window remains).

The checks in `tests/security_fixes.rs` marked Unix run on Linux and macOS in CI.

## Continuous security checks

Scheduled and per-push workflows exercise these controls; see
[CONTRIBUTING](../CONTRIBUTING.md#continuous-integration-and-nightly-checks) for the full list.

- [`security.yml`](../.github/workflows/security.yml): `cargo audit` and `cargo deny`
  (licenses, sources, duplicate versions), a gitleaks scan of the history, a refusal of
  tracked env files, CodeQL for Rust (advisory until a run confirms it) and dependency
  review on pull requests.
- [`fuzz.yml`](../.github/workflows/fuzz.yml): seed and crash replay on every push; the
  eleven fuzz targets for 20 minutes each, nightly.
- [`sanitizers.yml`](../.github/workflows/sanitizers.yml): nightly Miri on the pure
  modules, AddressSanitizer and ThreadSanitizer on the library unit tests.
- `tools/check_unsafe.py` in the native checks: every `unsafe` block and impl has a
  `// SAFETY:` comment, and clippy denies undocumented unsafe blocks.
- [`crash-consistency.yml`](../.github/workflows/crash-consistency.yml) and
  [`soak.yml`](../.github/workflows/soak.yml): nightly kill and write-failure runs of
  checkpoints, and one-hour resource soaks.
