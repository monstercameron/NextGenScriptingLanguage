# Native Windows

The Windows runtime uses native executables and pipes. It does not require WSL.
Windows x64 and ARM64 have separate native CI runners and release artifacts.

The language, typed values, branches, reusable programs, JSON operations, cache,
checkpoint state machine, intervals, listeners, headless widgets and live state
protocol are shared with Unix. Commands must be installed for the host platform.
A Unix command name does not magically install its Windows equivalent.

```text
run "C:\\tools\\my-tool.exe" with arguments ["--format", "json"]
then parse json as result
then encode result as json
```

Use quoted executable paths or named applet manifests. Arguments are passed
literally to the executable using Rust's native Windows quoting. `.cmd` and
`.bat` are rejected as executable targets because Windows executes them through
an implicit command shell. Invoke a shell explicitly when its semantics are
needed; shell code then has that shell's quoting and security properties.

Children start suspended, enter an owned Job Object, and only then resume.
Timeout, cancellation and normal cleanup terminate owned descendants. Child
stdout connects directly to the next child stdin through OS byte pipes. The
Windows supervisor currently polls readiness at a one millisecond interval;
Unix timing measurements do not describe Windows overhead.

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

`tests/windows_native.rs` uses an owned Rust fixture executable to exercise argv,
Unicode, binary pipelines, typed inputs, file replacement, exit statuses,
timeouts, descendant cleanup, cache/resume, intervals, streaming/listeners,
named-pipe CRUD and headless widgets. The Unix utility suites remain Unix-only.
