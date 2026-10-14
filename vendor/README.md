# Vendored timezone lookup

`iana-time-zone/` is the locked upstream 0.1.65 crate, retaining its original
licenses, metadata and platform backends. This project patches only macOS linkage:
CoreFoundation is loaded with `dlopen`/`dlsym` on the first timezone lookup instead
of during every interpreter startup. The original cache-reset/query/string-copy
sequence, error path and CF ownership remain intact. Other Apple targets retain
the original linked backend; Linux uses the original Linux backend.

The resolved API table and complete framework handle intentionally live for the
process lifetime. A partial symbol-resolution failure closes its handle. Future
upgrades must review the exact C ABI signatures and borrowed/owned CF lifetimes.

The absolute system framework path was tested on this macOS arm64 host; older
macOS releases were not exercised. Exact timezone output matched the unmodified
crate for eight TZ environments at six winter/summer/DST instants, including a
32-thread simultaneous first-call check. Native Linux runtime tests remain separate.

CoreFoundation may add `__CF_USER_TEXT_ENCODING` to the process environment when
loaded. Deferring loading therefore can change an inherited applet environment.
Cache keys continue hashing the full effective environment: old entries can miss
when that environment differs. The cross-version compatibility harness supplies
an identical explicit value on macOS to verify cache/checkpoint format compatibility
without weakening runtime identity checks.
