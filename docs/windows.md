# Native Windows

The Windows runtime uses native executables and pipes. It does not require WSL.
Windows x64 and ARM64 have separate native CI runners and release artifacts.
The MSVC C runtime is linked statically. Packaging inspects PE imports and rejects
Visual C++ runtime DLL dependencies; no separate VC++ redistributable is required.

The language, typed values, branches, reusable programs, JSON operations, cache,
checkpoint state machine, intervals, listeners, headless widgets, live state
protocol and persistent JSON-lines workers are shared with Unix. Commands must
be installed for the host platform. A Unix command name does not magically
install its Windows equivalent.

```text
run "C:\\tools\\my-tool.exe" with arguments ["--format", "json"]
then parse json as result
then encode result as json
```

Use quoted executable paths or named applet manifests. Arguments are quoted
with Windows CRT rules, including ASCII whitespace such as line breaks, so the
argument boundaries are preserved for MSVC and MSYS-family executables. `.cmd` and
`.bat` are rejected as executable targets because Windows executes them through
an implicit command shell. Invoke a shell explicitly when its semantics are
needed; shell code then has that shell's quoting and security properties.

Children start suspended, enter an owned Job Object, and only then resume.
Timeout, cancellation and normal cleanup terminate owned descendants. Child
stdout connects directly to the next child stdin through OS byte pipes. The
Windows supervisor currently polls readiness at a one millisecond interval;
Unix timing measurements do not describe Windows overhead.

Persistent JSON-lines workers use the same bounded pool, request protocol,
sensitivity history, timeout and cancellation rules as Unix. A worker starts
suspended and enters its owned Job Object before it can run. The parent checks
worker stdout through one owned reader thread and a one-chunk bounded channel;
that reader is cancelled and joined after terminating the Job if the request
deadline or cancellation signal arrives. A bounded request write runs on one
owned thread with the same cancellation and join rule. Workers remain
unavailable to native pipe groups and the applet cache.

`execute live --socket PATH` keeps the same JSON CRUD protocol. On Windows,
PATH identifies a local named pipe namespace, scoped to the current user's SID;
it is not a filesystem socket. Client and server must use the same absolute
path spelling. Remote clients are rejected. A protected DACL admits the owner
and SYSTEM; both peers also check the connected process's user SID.

Checkpoint directories and files receive protected owner/SYSTEM DACLs and an
exclusive `LockFileEx` lock. NTFS reparse points are rejected at the checkpoint
boundary. Journal replacement flushes the file and uses `MoveFileEx` with
write-through. This is not a promise of Unix directory-fsync durability on every
Windows filesystem. Binary identity is rehashed on resume rather than trusting
weaker Windows metadata. Use a local NTFS directory for checkpoints.

Redirected standard I/O preserves bytes. Interactive console output uses UTF-16
console APIs, and console input uses Rust's Unicode-aware standard input.
Ctrl-C requests cooperative cancellation. Native console behavior, antivirus,
filesystem and installed tools can differ from Unix; CI cannot prove arbitrary
third-party commands or every terminal configuration.

`tests/windows_native.rs` and `tests/worker_backend_windows.rs` use an owned Rust
fixture executable to exercise argv, Unicode, binary pipelines, typed inputs,
file replacement, exit statuses, timeouts, descendant cleanup, cache/resume,
intervals, streaming/listeners, named-pipe CRUD, headless widgets and persistent
worker reuse, frame failures, pool limits, cancellation and cleanup. The Unix
utility suites remain Unix-only.

## Features that differ on Windows

Windows release packages and the Windows CI jobs use the `full` feature set. Some
newer features are Unix-only or weaker on Windows:

- The warm daemon (`execute daemon`, `run --daemon`) is Unix only. `daemon serve`
  reports that serving is not available, and `run --daemon` runs in the client.
- `use folder` and `cd` authorize the resolved path, but the device and inode
  identity check against the opened directory is Unix only, and a launched child
  enters the directory by path, not by handle (threat model H12 and H13).
- A secret prompt input refuses to run when the console echo cannot be turned off.
- Journal version 5 checkpoints, per-element logs and keyed input digests are shared
  with Unix. The Windows-specific ACL and reparse-point rules above still apply.
- The Linux-only checks (the Omarchy port comparison, the soak runs, sanitizers, fuzzing
  and the bash comparison) do not run on Windows. The flaky-test workflow does run on
  Windows.
- Shell-script fake tools for `execute test` are Unix-only (see the
  [guide](guide.md#testing-programs)), and the Omarchy and Hyprland applets and ports
  target Linux desktops. The [threat model](threat-model.md) lists the remaining
  Windows gaps.
