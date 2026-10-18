# Security boundaries

Execute runs trusted local Unix-style programs with the caller's permissions.
Literal argument passing prevents implicit shell expansion; it does not make an
arbitrary command safe. An explicit shell invocation can still interpret shell
code. Scripts, applet manifests, selected executables and their dependencies
remain trusted inputs. Process groups and Windows jobs supervise children;
they do not sandbox filesystem, network or detached-process access.

## Cache and checkpoint state

Cache reads require regular files. Cache directory/file symlinks and Windows
reparse points are rejected. Unix cache opens use no-follow, nonblocking flags
and check the opened file again, so a FIFO or device cannot become a cache
stream. Cache directories and entries on Unix must belong to the effective
user and their mode bits must not grant group/other writes. New Unix cache
directories use mode 0700; new result files use mode 0600. Existing valid cache
keys and raw result bytes remain compatible. Unsafe state fails closed rather than silently
executing the cached applet again. Cache publication retains its private
temporary file, file sync and atomic rename.

Existing symlinked cache paths must be replaced with their real directory path.
An existing group/world-writable cache needs an owner-reviewed private location
or permissions repair before reuse. Execute does not chmod rejected paths.
Checkpoint state has separate no-follow/regular-file checks, private permissions,
checksummed state and an exclusive writer lock.

Cache/checkpoint parent directories must be trusted. These checks do not pin
every ancestor against concurrent replacement, authenticate data against another
process of the same user, provide cache integrity envelopes, or enforce Windows
cache ownership/ACL policy. Unix extended ACLs are not inspected. Windows cache
reparse checks are implemented; Unix UID/mode checks have no claimed Windows equivalent. Ordinary explicitly requested
file reads, saves and executable aliases retain their normal path semantics.

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

## Applet contracts and optional capability policy

Current source adds version-1 applet contracts and a separate opt-in policy.
These changes are not in published 0.9.0-rc.4 or the retained 0.8.0 binary.
Without a policy, execution keeps the existing trusted-process behavior.

When a policy is present, its strict JSON shape is:

```json
{
  "version": 1,
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
include `filesystem.read`/`filesystem.write`, respectively. These grant checks
do not require a remote applet's logical contract to declare `network.connect`.
`binary_sha256` is an optional executable-content check. For each applet launch,
the environment is cleared and only names in that grant's `environment` list
are forwarded. Contract-level `set` overrides must use allowlisted names;
`unset` may remove inherited names. Host `UseFolder`, `Read` and `Save` paths are
checked against the union of policy filesystem grants before effects; dynamic
paths that cannot be checked in preflight are rejected. These host checks do
not confine a native child process. Configured cache paths are also checked
against each cacheable applet's own read and write grants. When the program
changes working directory, a relative or default cache path is rejected because
preflight cannot resolve the eventual cache directory safely.

The policy's optional `host_environment` list controls which process environment
variables a program may read through `accept ... from environment`; it is empty
by default. `deny_privileged` classifies interpreters and text processors such
as Python, Perl, Node, awk, sed, git, make and tar as wrappers that require
`allow_wrappers`. The explain/effects gate marks launchers such as find, xargs,
ssh, parallel, bwrap and tmux as privileged. Versioned executable names such as
`python3.12` use the same classification as `python`.

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

`sandbox_required: false` enforces policy declarations, environment filtering
and host path checks, but does not isolate applet processes from the caller's
filesystem or network. `sandbox_required: true` requests a separate OS sandbox
for command and worker backends. macOS uses a generated deny-default
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
FIFOs, sockets and Unix write permissions. Real CLI tests require no output,
no later save and no native launch when an invalid cache is encountered.
[The security evidence](benchmarks/security-2026-10-14-cache.json) retains
baseline reproductions and the fixed results using owned synthetic fixtures.

Complete ancestor-race protection, cache authentication, filesystem-operation
deadlines and broad platform fault injection remain open. Capability policy and
OS sandboxing are source-candidate features; tests do not establish resistance
to every same-user attack or certify every platform/package combination.
Supported native child deadlines do not bound arbitrary filesystem operations.
Do not interpret passing tests as a production-completeness or arbitrary-Bash-
compatibility claim.

Report defects with the source version, platform and a minimal synthetic
reproduction. Keep real credentials and private command output out of reports.
