# Checkpoint format internals

This document records the on-disk representation used by checkpoint recovery. It is implementation reference material; program authors should use the [runtime recovery guide](../runtime-reference.md#recover-after-a-failure).

### Journal versions

The checkpoint journal is versioned, and `execute checkpoint status DIR [--json]`
reads it without taking the lock or running anything. Version 1 is the legacy
format; version 2 kept completed nodes inline in `journal.json`; version 3 moved them to the
append-only `completed.jsonl` log, so a persist no longer rewrites every earlier
node; version 4 added `elements.jsonl`; version 5 added the `graph` mode for
[finite graphs](../graph.md#checkpoints), with its own `graph.jsonl`; **version 6**
is the current version and marks the in-flight node's element kind (`for_each`
or `parallel_for_each`) so `resolve` and resume scope element decisions without
the program. A build reads program journals 1 through 6 (graph journals 5 and 6,
whose bodies are identical); a missing marker is a pre-marking journal and
fails closed where the kind matters. A journal written by a newer build is
refused by an older one. Program and graph journals refuse each other.

Input digests recorded for resume are a keyed HMAC under a per-user key,
so a low-entropy input cannot be guessed from a journal. A journal written before the
keyed digest matches its unkeyed digest once and is then rewritten with the keyed
one; a keyed journal never matches the unkeyed digest. See
[threat model H11](../threat-model.md).

Checkpoint directories contain private outputs and named values (0700 directories,
0600 files), checksummed state, and an exclusive writer lock. They are local trusted
state, not a sandbox or an encrypted secrets store. Keep them until recovery is
complete; choose a separate directory for each execution.

### Resolutions log

`execute checkpoint resolve` appends one JSON object per line to the append-only
`resolutions.jsonl` beside the journal (version 1 records; unknown versions are
refused). A record carries the node step id, an optional 0-based element index, the
`committed` or `retry` decision, the reason, the actor identity, Unix time, and, for
`committed` only, the hex-encoded output, the bindings, and the element fingerprint
captured from the element log's begin record. The actor is `uid:<euid>` with the
claimed login name as display-only text on Unix, so a spoofed `USER` cannot
impersonate another user; older bare-name records still read. Resolve holds the
checkpoint's exclusive lock across validation and append, so concurrent resolves
for one item fail closed (one records, the other is refused as locked or
duplicate) instead of both appending. Reads are line-bounded; a torn final line
refuses every resume and status until the operator removes it, and a duplicate
node/element key does the same. Element nodes take only per-element records; a
node-level record authorizes nothing there. Parallel (`parallel for each`)
elements resolve as `retry` only; `committed` is refused at resolve and at resume
because the record cannot reproduce their value, stderr and outcome. Files are
created 0600 and the log is fsynced on append. Status reports only the
who/when/why view; output, bindings and fingerprints stay in the checkpoint as
resume evidence. Run records (`show-run`) carry no checkpoint pointer and never
reference this log; there is no revoke tool, so refused records require manual
line removal.

The runtime rejects symlinks at the requested checkpoint
and blob directory leaves, including a trailing `/` or `/.` on the requested
checkpoint path. Lock, journal and blob files must be regular files; opens use
`O_NOFOLLOW` and descriptor checks, with nonblocking opens to reject substituted
FIFOs without waiting for a peer. These changes prevent following those symlink
leaves or changing their targets' permissions. Parent aliases remain supported
(for example macOS temporary-directory aliases), so use an owner-controlled
parent directory. This does not pin every ancestor with directory descriptors,
exclude hard links, enforce capabilities or make concurrent hostile filesystem
replacement safe.
