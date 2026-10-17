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
| Terminal escapes | Diagnostics and streamed output escape control bytes (to be confirmed by `tests/security_terminal.rs`). | `tests/security_terminal.rs` (planned) |
| Secrets in traces, journals, records, explain and what-if | Secret prompts are read with echo off and are not recorded; checkpoints refuse secret prompts. | `tests/security_redaction.rs`, `tests/security_secrets.rs` (planned) |
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

Audit findings are listed here with an ID, a severity and the test that shows them,
once confirmed. Until then, no audit finding is claimed.
