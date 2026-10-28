# Security boundaries

Without a capability policy, Execute runs trusted local Unix-style programs
with the caller's permissions. Policies default native children to OS-confined
untrusted mode as described below.
Literal argument passing prevents implicit shell expansion; it does not make an
arbitrary command safe. An explicit shell invocation can still interpret shell
code. Scripts, applet manifests, selected executables and their dependencies
remain trusted inputs. Manifest directories, manifest files, module roots and
sources, and the bundled helper binaries pass an ownership and writability
gate before they decide what runs: on Unix each leaf must be owned by the
effective user or root and must not be group/world-writable (file leaves
additionally refuse setuid/setgid bits and extra hardlinks), and every ancestor
directory up to the filesystem root passes the same ownership and
writability check (sticky root-owned directories such as `/tmp` are accepted
because the sticky bit blocks cross-user renames). Symlinked manifest or
module files embedded in directory scans (and Windows reparse points) are
refused; an explicitly named single manifest file or catalog may be a symlink
alias, which is resolved and verified at its canonical target before reading.
On Linux a leaf or ancestor with a POSIX `system.posix_acl_access` ACL fails
closed because mode bits cannot attest ACL-granted writes; NFSv4, RichACL
and macOS ACLs use different attributes and are not probed, so an
ACL-granted write through those mechanisms would pass the mode check
undetected. Other Unix systems do not expose ACLs to this gate. The opened
descriptor is re-checked on the descriptor itself (Linux `fgetxattr`, Windows
handle ACL), so an ACL or owner attached between the path check and the open
fails closed. On Windows each leaf and ancestor must be owned by the
current user, SYSTEM or Administrators and must grant write, delete, and
extended-attribute-write access to no other principal (data/attribute writes,
delete, child add/delete, DACL/owner takeover, and the generic umbrellas);
read grants to others stay allowed so shared installs keep working, and an
unreadable ACL fails closed. Manifest-selected executables pass the same
leaf-plus-ancestor, hardlink/setuid and ACL gate as manifests: a trusted
manifest naming a writable binary is refused at resolution time. Install or
extract release archives only into locations writable solely by their owner
or Administrators (default per-user and Program Files locations qualify;
shared world-writable trees such as a bare `C:\` subfolder do not), and keep
those permissions after extraction. Process groups and Windows jobs supervise
children; they do not sandbox filesystem, network or detached-process access.

## Cache and checkpoint state

Cache reads require regular files. Cache directory/file symlinks and Windows
reparse points are rejected. Unix cache opens use no-follow, nonblocking flags
and check the opened file again, so a FIFO or device cannot become a cache
stream. Cache directories and entries on Unix must belong to the effective
user and their mode bits must not grant group/other writes. New Unix cache
directories use mode 0700; new result files use mode 0600. Cache key derivation
and result payload encoding remain unchanged. Unsafe state fails closed rather
than silently executing the cached applet again. Cache entries are authenticated with
HMAC-SHA-256 under the per-user key at the what-if key location; the tag binds
the cache key and exact result bytes. Missing, malformed or mismatched tags abort
reuse, and consumers read the exact byte snapshot whose tag was verified. On
Unix the validated cache directory is held open and cache entries, tags, locks, and
publication are opened relative to that descriptor, so replacing a parent after
it is pinned cannot redirect those operations. Cache publication retains a
private temporary file, file sync and atomic rename. The HMAC detects rewritten
entry bytes when the tag is unchanged; a process running as the same user can
read the per-user key and can therefore forge a replacement tag. Existing
entries without a valid tag, including entries written before authenticated
cache support, are rejected and must be removed explicitly before recomputation.

Existing symlinked cache paths must be replaced with their real directory path.
An existing group/world-writable cache needs an owner-reviewed private location
or permissions repair before reuse. Execute does not chmod rejected paths.
On Unix, checkpoint journals, logs, blobs and record files are opened or
published relative to pinned directory descriptors after the configured leaf
directory is validated. This prevents a parent replacement after pinning from
redirecting those operations. Checkpoint state also has no-follow/regular-file
checks, private permissions, checksummed state and an exclusive writer lock.
Replay parses the program bytes captured from the pinned record descriptor and
uses the canonical source identity captured while opening that directory; it
does not re-resolve a replaced record pathname or gain access to called files.

Security-sensitive policy, key, answers, and live-token files on Windows must
be owned by the current user and have an explicit, non-inherited DACL with no
allow entries for other principals. SYSTEM and local Administrators are trusted
exceptions. An unreadable or unverifiable ACL fails closed and names the file.

On Unix the canonical target path is walked from the filesystem root with
no-follow directory opens, and each opened ancestor anchors the next lookup.
Missing cache, checkpoint, and record directory components are created with
`mkdirat` relative to a pinned existing ancestor; they are not recursively
created by pathname and accepted from a later first stat.
Same-user processes that can read the per-user key can forge cache tags, and
checksummed checkpoint/record state is not authenticated against such processes.
Windows continues to use path-based state operations. Unix extended ACLs are not
inspected. Windows cache reparse checks are implemented; cache UID/mode checks
have no Windows equivalent. Ordinary explicitly requested file reads, saves and
executable aliases retain their normal path semantics.

## Live control and model applets

Live control is opt-in. Unix sockets and Windows named pipes check the peer's
user identity before accepting control data; requests, documents and queues are
bounded. Access grants powerful state and source replacement, including native
execution as that user. This is a same-user control plane, not an authorization
system separating applications or agents of that user.

Established live connections have absolute wire-I/O budgets: two seconds for
server request reads and response writes, and CLI request writes; twelve seconds
for CLI response reads. Unix temporarily makes each exclusively owned connection
nonblocking and polls only for the remaining budget; this also bounds a single
write while the peer continues draining bytes. Original descriptor flags are
restored when the wire operation ends. Windows retains its overlapped I/O with
a shrinking timeout. Byte progress does not renew the budget. Expired incomplete
requests are rejected before state mutation; error replies are best effort when
the peer keeps writing. Handler `wait` time remains separate, up to ten seconds.
These are transport budgets, not connect, mutex/CPU, filesystem or native-command
deadlines. The [live transport evidence](benchmarks/security-2026-10-15-live-transport.json)
retains the owned partial-request/shutdown reproduction and regressions.
The [CLI deadline proof](benchmarks/security-2026-10-15-live-client.json)
additionally tests a slowly responding peer and a slowly draining peer against
the actual CLI. The latter exposed a macOS blocking-write gap in the initial
transport fix; the nonblocking path closes it.

Model operations require an explicit invocation. Transport uses HTTPS, with HTTP
allowed only for loopback endpoints; redirects and environment proxies are
disabled. Endpoint choice and data disclosure remain the script author's
responsibility. Failure diagnostics omit credentials, request/response bodies
and endpoint details. Local fixture tests do not establish provider safety,
model accuracy or resistance to prompt injection.

Model file inputs (`fixture`, `env_file`) are declared as `path` parameters
so a capability policy authorizes them against the applet's own filesystem
grant after bindings resolve, including background jobs. The child pins only
the final path component at open time; a concurrent ancestor rename between
the parent check and the child open can redirect the file and is not stopped
on any platform. Fixture files have no ownership check by design (replay
data); only `env_file` requires an owner-private file. Non-Unix platforms
without an atomic no-follow open retain a final-component check-to-open
window. These paths are documented as advisory, not proven secure.

## Applet contracts and optional capability policy

Current source adds version-1 applet contracts and a separate opt-in policy.
These changes are not in published 0.9.0-rc.4 or the retained 0.8.0 binary.
Without a policy, execution keeps the existing trusted-process behavior.

When a policy is present, its strict JSON shape is:

```json
{
  "version": 1,
  "host": {
    "capabilities": ["filesystem.read", "filesystem.write"],
    "filesystem": { "read": ["/etc/example.conf"], "write": ["/tmp/results"] }
  },
  "applets": {
    "file.snapshot": {
      "capabilities": ["filesystem.read"],
      "environment": ["LANG"],
      "filesystem": { "read": ["/etc/example.conf"], "write": [] },
      "network": false,
      "network_origins": [],
      "sandbox_required": true
    }
  }
}
```

Unknown fields and versions fail validation. Every policy entry must name a
canonical applet with a v1 contract, every invoked applet must have an explicit
grant, and direct quoted commands are denied. Capability and effect identifiers
are extensible, exact-match namespaced strings. Declared effects must also be
declared capabilities, and the grant must permit every capability in the
applet's contract. Separately, a grant with `network: true` must include the
`network.connect` capability, and nonempty filesystem read/write grants must
include `filesystem.read`/`filesystem.write`, respectively. The optional
`host` grant follows the same rule for host-language file actions. These grant
checks do not require a remote applet's logical contract to declare
`network.connect`.
`binary_sha256` is an optional executable-content check. For each applet launch,
the environment is cleared and only names in that grant's `environment` list
are forwarded. Contract-level `set` overrides must use allowlisted names;
`unset` may remove inherited names. Host `UseFolder`, `Read` and `Save` paths are
checked against the matching read/write paths in the explicit `host` grant
before effects; applet grants never authorize these host actions. Without a
host grant, these actions are denied under a policy. Dynamic paths that cannot
be checked in preflight are rejected. These host checks do not confine a native
child process. Configured cache paths are also checked
against each cacheable applet's own read and write grants. When the program
changes working directory, a relative or default cache path is rejected because
preflight cannot resolve the eventual cache directory safely.

The policy's optional `host_environment` list controls which process environment
variables a program may read through `accept ... from environment`; it is empty
by default. `deny_privileged` classifies interpreters and text processors such
as Python, Perl, Node, awk, sed, git, make and tar as wrappers that require
`allow_wrappers`. The explain/effects gate marks launchers such as find, xargs,
ssh, parallel, bwrap, tmux, container-host escape (`flatpak-spawn`, `host-spawn`,
`distrobox-host-exec`, `toolbox`, `docker`, `podman`, `kubectl`), schedulers
(`at`, `batch`, `crontab`), D-Bus launchers (`dbus-send`, `gdbus`, `busctl`), and
desktop launchers (`xdg-open`, `gio`) as privileged, whatever their arguments. An
absolute path or alias that keeps the listed file name, and a symlink whose
canonical target keeps the listed file name, are classified the same way; a
renamed copy or hardlink with an unlisted file name is not recognised because
classification is by name, not content. Versioned
executable names such as `python3.12` use the same classification as `python`.
Hyphenated same-family variants that are not listed (for example
`docker-compose`, `dbus-run-session`, `atq`) are also not recognised. The list
is a review aid, not a sandbox or a complete boundary: unlisted or renamed
tools are not recognised, and enforcement stays with the strict capability
policy and OS confinement in untrusted mode. An alias that fails to resolve at
review time falls back to its written name, so runtime enforcement remains
authoritative.

`--record` redacts secret and environment input values. The review fingerprint
covers module sources and packages. Replay refuses detached launches and handovers.

Optional `network_origins` lists canonical HTTP(S) origins, without a path,
query, fragment, credentials or wildcard. The list requires `network: true`
and the `network.connect` grant, and is supported only for host-managed remote
applets. An absent or empty list allows broad network access for metadata-policy
checks. Required OS sandboxing rejects a nonempty origin list because the
available native sandbox backends can enforce only all-or-none network access.
Remote endpoint authorization runs before discovery or invocation; redirects
and environment proxies are disabled. A remote applet's bearer-file path is
checked against that applet's own filesystem read grant.

SSRF protection is fail-closed at two layers for host-managed remote applets
and the model helper. Before any resolution or socket use, a non-public
endpoint host (loopback, RFC1918, CGNAT including 100.100.100.200, link-local
including 169.254.169.254, ULA, multicast, and IPv4-mapped IPv6 forms) is
refused by name; at connect time the resolver pins the whole resolved address
set, so a hostname that first resolves to a public address cannot rebind to a
private one. Only an origin explicitly listed in `network_origins` preserves a
private destination, and only for remote applets; a broad empty-list grant
does not. The model helper accepts no origin grants (dependency #375 keeps
model endpoints outside `network_origins`, which `Command`-backend applets
cannot declare): its endpoints stay restricted to HTTPS, or HTTP to loopback,
with the same guard. A script-controlled `--endpoint` therefore cannot reach
metadata or private destinations, and live loopback endpoints additionally
require the explicit `EXECUTE_MODEL_ALLOW_LOOPBACK` grant in the helper's
environment — without it the request fails as configuration before any
credential read or connection, so an unauthenticated local service is never
reachable by default. Hermetic `--fixture` runs never open a socket and do
not need the grant. The self-grant prevention holds only when the ambient
environment is constrained: under a capability policy, or a host
`--clear-env`/`--env-allow` filter, a program cannot grant the variable to
itself via `use environment` or `and environment` unless the applet's
`environment` grant lists it. In default trusted mode with full ambient
inheritance, a program can set the variable itself.

`--execution-mode trusted|untrusted` selects native-child confinement. With no
policy the default is `trusted`; a capability policy defaults to `untrusted`.
Agent runners should always supply a policy and select `untrusted` explicitly.
Untrusted mode without a policy is refused. In trusted mode,
`sandbox_required: false` enforces policy declarations, environment filtering
and host path checks, while native children retain the caller's filesystem and
network authority. `sandbox_required: true` still requires confinement even in
trusted mode. In untrusted mode every command and persistent worker requires a
separate OS sandbox, regardless of that per-applet boolean. Filesystem and
network grants are intersected with the contract's declared effects. An unknown
native effect or native origin restriction is refused with `SandboxRefused`.

| Mode | Linux | macOS | Windows / other platforms |
| --- | --- | --- | --- |
| Trusted, no required sandbox | Native children have the caller's authority. | Native children have the caller's authority. | Native children have the caller's authority. |
| Untrusted, or required sandbox | Trusted system bubblewrap, isolated namespaces, read-only runtime and executable mounts, explicit filesystem mounts; network namespace unless declared and broadly granted. Missing facility or failed setup refuses execution. | Deny-default sandbox profile, runtime reads and explicit path grants; network denied unless declared and broadly granted. Missing facility or failed setup refuses execution. | Native children are refused. Job objects control lifetime, not filesystem or origin access; no AppContainer backend is implemented. |

Linux currently uses the existing bubblewrap backend; it does not install
Landlock or seccomp filters. Read access to system runtime files and execution
of tools within that runtime remain available. This is filesystem/network
confinement, not a hostile-kernel-code or resource-exhaustion boundary.
`explain` and `--what-if` report `execution_mode` and each step's `enforcement`.
`os_sandbox_required` is prospective: a real run must still pass OS preflight;
`refused` means the current platform cannot provide the required confinement.
`declared_only` means no OS sandbox is required; `host_managed` identifies
interpreter operations rather than confined native children. Review
fingerprints include these fields, so changing modes changes the fingerprint.
`--control` refuses an explicit mode override until the live host protocol can
carry it; its capability policies still default to untrusted mode.

macOS uses a generated deny-default
`sandbox-exec` profile with system runtime reads and explicit path grants;
network is denied unless granted, and child execution is restricted unless
`process.exec` is granted. Linux uses bubblewrap namespaces, an isolated network
namespace unless network is granted, read-only runtime binds and explicit
read/write mounts. Linux bubblewrap limits filesystem and network access but
does not limit which executables the sandboxed applet can start. Linux write
grants must name an existing file or directory; grant the parent directory to
create a new file. In-process operations are rejected when OS isolation is
required. With any policy present, in-process operations are limited to the
reviewed pure-operation allowlist; effectful or unreviewed in-process
operations are denied, and they cannot be pinned by `binary_sha256`. The
`program_module` backend is currently rejected under a capability policy
because it cannot be fully isolated or preflighted. Missing sandbox tools,
failed probes and unsupported platforms fail closed before workflow effects;
Windows does not currently support required applet sandboxes.
The host probes the applet-specific generated profile or mount arguments with
a fixed system no-op executable and a bounded timeout; preflight never runs
the untrusted applet.

## Panics and long-lived hosts

Release builds use `panic=abort`. A panic is an interpreter bug and terminates
the containing process; it cannot become an ordinary recoverable task error.
The CLI installs an `execute.panic/1` JSON stderr hook at live, daemon, graph,
screen, stream, REPL, LSP, DAP, triggered-service/listener, and checkpoint host
boundaries. It emits one
diagnostic with host kind, thread, available step/line, Rust location and the
action (`abort` or `unwind`). The panic payload is withheld to avoid disclosing
secrets. A missing step/line is represented as `null`, never guessed. The hook
does not call the default hook or write into a possibly locked record/journal.
The supervisor must capture stderr. Library embedders must install their own
hook or explicitly call `security::install_panic_hook` before starting workers.

| Host | Decision under panic=abort |
| --- | --- |
| `live serve`, `run --control`, interactive screens | Terminate the host process; deploy behind an external supervisor and retain stderr. In-flight requests are interrupted. |
| Listener, interval/calendar service | Terminate the service process. Generated systemd services specify `Restart=on-failure`; generated launchd jobs use `KeepAlive` with `SuccessfulExit=false`. The service manager supplies restart supervision. |
| Graph workers and bounded/parallel task workers | A worker panic terminates its containing process. `catch_unwind` cannot contain an abort; no per-thread recovery guarantee is made. |
| Daemon forwarded run | The run is already a forked child on Unix; aborting it preserves the daemon parent. The client receives a failed run. |
| Daemon parent | Terminate; an external supervisor must restart the daemon. |
| Stream, REPL, LSP and DAP | Terminate the host process; its launcher or an external supervisor must restart it. |
| Checkpointed run | Preserve committed progress. Resume treats the unfinished invocation as interrupted/uncertain; retry requires the normal retry-safety decision or explicit `--retry-uncertain`. |

`src/tasks.rs` has one debug-only `catch_unwind`; `src/runtime/tasks.rs`
has two task/element wrappers. They are recovery mechanisms only in builds
that unwind. Debug containment does not establish release containment.
Hook installation and task-local step/line propagation for standalone finite
task hosts still require integration in the task runtime; graph/task threads
without a finite-step scope report null step/line. Actual supervisor restart
and daemon-parent survival after an aborted forwarded run require Unix CI.

The executable hash check narrows replacement risk but does not pin an inode
through `exec`; a replacement race can remain between hashing and launch. The
macOS profile uses Apple's undocumented, deprecated `sandbox-exec` interface.
The sandbox applies to the launched applet and descendants that inherit it; a
descendant that deliberately detaches may outlive its parent, so parent
supervision alone is not a cleanup guarantee. The host and unrelated same-user
processes remain outside it. Linux bubblewrap does not restrict `execve`
targets, and namespace/process lifetime details do not provide a universal
detached-process cleanup guarantee. Host-language `UseFolder`, `Read` and `Save` actions receive
path-policy checks, but do not themselves run inside the OS sandbox. The
`fs.transaction` applet offers bounded journaled recovery for file-only batches,
but files become visible one at a time and external effects are outside that
mechanism. Review the exact manifests,
path grants and executable identities for each deployment. The [applet contract guide](docs/applet-contracts.md)
describes typed inputs, outputs, outcomes, stdio and worker lifecycle semantics.
The separate [data-flow policy guide](docs/security-dataflow.md) covers
sensitivity labels, remote-origin grants, persistence and cache-purity checks;
capability policy does not supply those data-flow grants.

## Verification and remaining work

The cache security regressions cover ordinary cache reuse, exact binary bytes,
output limits, symlinked files/directories including trailing separators,
FIFOs, sockets and Unix write permissions. The manifest-trust regressions
cover the same shape of attack on the inputs that pick the executable:
group/world-writable manifest directories, manifest files, module roots and
helper binaries are refused with the offending path named, as are
group/world-writable ancestors, extra hardlinks, setuid/setgid bits, Unix
sockets, symlinked and FIFO manifest entries (which fail closed without
hanging), and an owned directory still runs. Explicit single-file and catalog
symlink aliases resolve to a verified target; embedded directory-scan links
stay refused. Directory aliases resolve to a verified canonical target and
list that target; single-file and catalog aliases do the same, while an alias
to an untrusted target is refused with the real path named. Manifest-selected
executables resolve the same way: the canonical target is verified and the
selected spelling is kept for launch, so a writable binary or writable binary
parent named by a trusted manifest is refused before any native launch.
Residual limits: the gate stats each path without following a
trailing link, verifies before reading, and re-checks the opened descriptor
(including descriptor-side ACL/owner on Linux via `fgetxattr` and on Windows
via the open handle),
but a replacement between the final check and use remains a race — the gate
narrows check-then-use races without eliminating them, and a replacement
between helper or executable verification and `exec` in particular cannot be
pinned from userspace. A same-user process can rewrite what it owns (as with
the cache key). POSIX `system.posix_acl_access` ACLs fail closed on Linux;
NFSv4, RichACL and macOS ACLs are not probed and would pass the mode check
undetected, and other Unix systems do not expose ACLs to this gate at all.
Windows enforces owner plus no-foreign-write/delete/EA-ACE on leaves
and ancestors with fail-closed unreadable ACLs, re-checked on the open
handle, but a replacement after that handle check still races. Real CLI tests require no output,
no later save and no native launch when an invalid cache is encountered. Unit
regressions also cover same-user cache-byte tampering, malformed integrity tags,
and a parent replacement after the Unix cache directory descriptor is pinned.
[The security evidence](benchmarks/security-2026-10-14-cache.json) retains
baseline reproductions and the fixed results using owned synthetic fixtures.

Same-user isolation of the persisted cache key and broad platform fault
injection remain open. Host `read` opens/reads and in-process filesystem-applet
opens are bounded by `--fs-timeout-ms` (default two seconds, never later than
the run deadline): FIFOs and sockets fail with the typed timeout, and other
non-regular targets are refused before any byte moves. A single
operating-system call that blocks inside the kernel on an already-open regular
file (a stalled mount mid-read or mid-write) stays unbounded; only the slices
between calls observe the deadline. Windows state operations remain
path-based.
Capability policy and
OS sandboxing are source-candidate features; tests do not establish resistance
to every same-user attack or certify every platform/package combination.
Command timeouts bound a native child's run, not filesystem operations the
child performs itself; host `read` and applet file opens carry their own
`--fs-timeout-ms` budget instead.
Do not interpret passing tests as a production-completeness or arbitrary-Bash-
compatibility claim.

Report defects with the source version, platform and a minimal synthetic
reproduction. Keep real credentials and private command output out of reports.
