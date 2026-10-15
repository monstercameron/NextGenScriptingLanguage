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

## Verification and remaining work

The cache security regressions cover ordinary cache reuse, exact binary bytes,
output limits, symlinked files/directories including trailing separators,
FIFOs, sockets and Unix write permissions. Real CLI tests require no output,
no later save and no native launch when an invalid cache is encountered.
[The security evidence](benchmarks/security-2026-10-14-cache.json) retains
baseline reproductions and the fixed results using owned synthetic fixtures.

Enforced command capabilities, a command sandbox, complete ancestor-race
protection, cache authentication, filesystem-operation deadlines and broad
platform fault injection remain open. Supported native child deadlines do not
bound arbitrary filesystem operations. Do not interpret passing tests as a
production-completeness or arbitrary-Bash-compatibility claim.

Report defects with the source version, platform and a minimal synthetic
reproduction. Keep real credentials and private command output out of reports.
