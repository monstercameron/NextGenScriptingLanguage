//! Bounded filesystem opens and reads (threat model H7).
//!
//! Command timeouts bound a child's run and captured bytes, but a blocking
//! `open`/`read` on a FIFO, a device, or a stalled network mount hangs the
//! operation with no timeout. The helpers here open read targets nonblocking
//! on Unix, refuse non-regular targets before any byte moves, poll regular
//! files for readiness within a configurable filesystem-operation deadline,
//! and read through a bounded poll/read loop.
//!
//! Errors are plain [`std::io::Error`]s so both caller families can map them
//! onto their own typed errors without a shared error enum:
//! [`std::io::ErrorKind::TimedOut`] means the wait exceeded the deadline and
//! must surface as the caller's typed timeout; any other kind is a refusal
//! or an ordinary I/O failure. A refused or timed-out open never creates,
//! truncates, or partially writes the target: these helpers only ever open
//! existing paths read-only.

use std::fs::File;
use std::io;
use std::path::Path;
use std::time::{Duration, Instant};

/// Default bound for one filesystem open/read operation when no explicit
/// `--fs-timeout-ms` value is configured. Healthy local operations finish in
/// milliseconds; this only bounds pathological waits.
pub(crate) const DEFAULT_FS_TIMEOUT: Duration = Duration::from_secs(2);

/// One poll slice while waiting for readiness. Each slice re-runs the
/// caller's checkpoint, so cancellation and an enclosing run deadline preempt
/// the wait within milliseconds. No helper thread is spawned.
#[cfg(unix)]
const POLL_SLICE_MS: i32 = 20;

/// Read chunk between readiness polls. Matches the applet I/O chunk size so
/// large files stay interruptible at the same granularity.
#[cfg(unix)]
const READ_CHUNK_BYTES: usize = 64 * 1024;

/// Absolute deadline for one filesystem operation: the configured
/// filesystem timeout, never later than an enclosing run deadline so a
/// generous `--fs-timeout-ms` cannot outlive `--command-timeout-ms` or
/// `--total-timeout-ms`.
pub(crate) fn fs_deadline(fs_timeout: Option<Duration>, outer: Option<Instant>) -> Instant {
    let timeout = fs_timeout.unwrap_or(DEFAULT_FS_TIMEOUT);
    let own = Instant::now().checked_add(timeout).unwrap_or_else(|| {
        // Practically unreachable: the CLI rejects timeouts that overflow
        // `Instant`, and the default is seconds. Prefer the enclosing
        // deadline, else a distant one, over panicking.
        outer.unwrap_or_else(|| Instant::now() + Duration::from_secs(86_400))
    });
    match outer {
        Some(deadline) if deadline < own => deadline,
        _ => own,
    }
}

/// Refusal for a target that must never be served as file input.
pub(crate) fn refusal(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("refusing to read a non-regular file: {}", path.display()),
    )
}

/// Timeout for a wait that exceeded the filesystem-operation deadline.
/// Callers map [`io::ErrorKind::TimedOut`] onto their typed timeout error.
pub(crate) fn timed_out(path: &Path, stage: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::TimedOut,
        format!(
            "{stage} {}: filesystem operation timed out within the configured deadline",
            path.display()
        ),
    )
}

/// The checkpoint a wait loop re-runs every poll slice. It reports
/// cancellation (and any caller-side budget the closure captures) as an I/O
/// error so the wait ends at once instead of sleeping through it.
pub(crate) type WaitCheckpoint<'a> = dyn Fn() -> io::Result<()> + 'a;

/// Open `path` for reading without ever blocking in `open(2)`, refuse
/// non-regular targets, wait for a regular file to become readable within
/// `deadline`, and return it in blocking mode so a handle later handed to a
/// native child as stdin keeps blocking semantics.
///
/// A FIFO or socket fails at once with [`io::ErrorKind::TimedOut`]: it can
/// never supply file bytes, so waiting out the deadline for data that must
/// be refused anyway would only burn the budget. A device or any other
/// non-regular target fails with a refusal. Regular files that never become
/// readable (a stalled mount) fail with a timeout when `deadline` passes.
#[cfg(unix)]
pub(crate) fn open_input_file(
    path: &Path,
    deadline: Instant,
    checkpoint: &WaitCheckpoint<'_>,
) -> io::Result<File> {
    checkpoint()?;
    let file = open_nonblocking(path, false)?;
    classify_regular_input(&file, path)?;
    wait_readable(&file, path, deadline, checkpoint)?;
    restore_blocking(&file)?;
    Ok(file)
}

/// Non-Unix opens keep the historical blocking behavior: there is no FIFO
/// open hazard through this path on Windows (redirect targets additionally
/// reject reserved device names), and native Windows semantics stay
/// CI-verified there.
#[cfg(not(unix))]
pub(crate) fn open_input_file(
    path: &Path,
    deadline: Instant,
    checkpoint: &WaitCheckpoint<'_>,
) -> io::Result<File> {
    let _ = deadline;
    checkpoint()?;
    let file = File::open(path)?;
    if !file.metadata()?.is_file() {
        return Err(refusal(path));
    }
    Ok(file)
}

/// Open `path` read-only with `O_NONBLOCK|O_CLOEXEC` so a FIFO with no
/// writer cannot hang the open. With `no_follow_final`, refuse a symlink in
/// the final component instead of following it.
#[cfg(unix)]
pub(crate) fn open_nonblocking(path: &Path, no_follow_final: bool) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    let mut flags = libc::O_NONBLOCK | libc::O_CLOEXEC;
    if no_follow_final {
        flags |= libc::O_NOFOLLOW;
    }
    options.custom_flags(flags);
    // A signal arriving mid-open surfaces as `Interrupted`; retry instead of
    // leaking it, so only the caller's checkpoint reports cancellation (as
    // `Interrupted`) and timeouts (as `TimedOut`).
    loop {
        match options.open(path) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            result => return result,
        }
    }
}

/// Non-Unix opens keep the historical blocking behavior: no FIFO open
/// hazard exists through this path on Windows, whose device and reparse
/// semantics stay CI-verified there. The regular-file check below still
/// applies.
#[cfg(not(unix))]
pub(crate) fn open_nonblocking(path: &Path, _no_follow_final: bool) -> io::Result<File> {
    std::fs::OpenOptions::new().read(true).open(path)
}

/// Check that an already-opened handle refers to a regular file. FIFOs and
/// sockets fail with [`io::ErrorKind::TimedOut`] (see [`open_input_file`]
/// for why the timeout kind applies); devices and anything else fail with a
/// refusal. Regular files pass, leaving readiness and content to the caller.
pub(crate) fn classify_regular_input(file: &File, path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt;
        let file_type = file.metadata()?.file_type();
        if file_type.is_file() {
            return Ok(());
        }
        if file_type.is_fifo() || file_type.is_socket() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "refusing to read a FIFO or socket (would block beyond the filesystem deadline): {}",
                    path.display()
                ),
            ));
        }
        Err(refusal(path))
    }
    #[cfg(not(unix))]
    {
        if file.metadata()?.is_file() {
            Ok(())
        } else {
            Err(refusal(path))
        }
    }
}

/// Poll a nonblocking handle until it is readable, `deadline` passes, or the
/// checkpoint fails. Regular files on a healthy filesystem report readiness
/// at once; only pathological waits burn budget.
#[cfg(unix)]
pub(crate) fn wait_readable(
    file: &File,
    path: &Path,
    deadline: Instant,
    checkpoint: &WaitCheckpoint<'_>,
) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    let fd = file.as_raw_fd();
    loop {
        checkpoint()?;
        let now = Instant::now();
        if now >= deadline {
            return Err(timed_out(path, "waiting for readability of"));
        }
        let remaining_ms = deadline
            .saturating_duration_since(now)
            .as_millis()
            .min(POLL_SLICE_MS as u128) as i32;
        match crate::platform::unix_ffi::poll_one(fd, libc::POLLIN, remaining_ms) {
            Ok((_, revents)) if revents & libc::POLLNVAL != 0 => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("file is not open for reading: {}", path.display()),
                ));
            }
            Ok((_, revents)) if revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 => {
                return Ok(());
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => {
                return Err(io::Error::new(
                    error.kind(),
                    format!("waiting for readability of {}: {error}", path.display()),
                ));
            }
        }
    }
}

/// Checkpoint-only wait on platforms without a pollable readiness model.
#[cfg(not(unix))]
pub(crate) fn wait_readable(
    _file: &File,
    _path: &Path,
    _deadline: Instant,
    checkpoint: &WaitCheckpoint<'_>,
) -> io::Result<()> {
    checkpoint()
}

/// Clear `O_NONBLOCK` again so the returned handle behaves like an ordinary
/// blocking open for later reads and for native children inheriting it.
#[cfg(unix)]
pub(crate) fn restore_blocking(file: &File) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    let fd = file.as_raw_fd();
    let flags = crate::platform::unix_ffi::status_flags(fd)?;
    crate::platform::unix_ffi::set_status_flags(fd, flags & !libc::O_NONBLOCK)
}

/// Non-Unix handles never gain `O_NONBLOCK` here, so there is nothing to clear.
#[cfg(not(unix))]
pub(crate) fn restore_blocking(_file: &File) -> io::Result<()> {
    Ok(())
}

/// Read `file` to end, capped at `limit` bytes, without blocking past
/// `deadline`. The handle is left in blocking mode on return, including on
/// error paths, so callers can still hand it to a native child.
///
/// Over-limit input fails with [`io::ErrorKind::QuotaExceeded`] carrying the
/// same `output exceeds maximum of {limit} bytes` message the unbounded
/// reader reported, so callers can preserve its exact rendering.
#[cfg(unix)]
pub(crate) fn read_input_file(
    file: &File,
    limit: usize,
    deadline: Instant,
    checkpoint: &WaitCheckpoint<'_>,
) -> io::Result<Vec<u8>> {
    use std::os::fd::AsRawFd;
    let fd = file.as_raw_fd();
    let original = crate::platform::unix_ffi::set_nonblocking(fd)?;
    let result = read_nonblocking(file, limit, deadline, checkpoint);
    let restore = crate::platform::unix_ffi::set_status_flags(fd, original);
    let output = result?;
    restore?;
    Ok(output)
}

#[cfg(unix)]
fn read_nonblocking(
    mut file: &File,
    limit: usize,
    deadline: Instant,
    checkpoint: &WaitCheckpoint<'_>,
) -> io::Result<Vec<u8>> {
    use std::io::Read;
    use std::os::fd::AsRawFd;
    let fd = file.as_raw_fd();
    let mut output = Vec::new();
    let mut chunk = vec![0u8; READ_CHUNK_BYTES];
    loop {
        checkpoint()?;
        if Instant::now() >= deadline {
            return Err(timed_out(Path::new("<open file>"), "reading"));
        }
        let remaining_ms = deadline
            .saturating_duration_since(Instant::now())
            .as_millis()
            .min(POLL_SLICE_MS as u128) as i32;
        match crate::platform::unix_ffi::poll_one(fd, libc::POLLIN, remaining_ms) {
            Ok((_, revents)) if revents & libc::POLLNVAL != 0 => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "file is not open for reading",
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => {
                return Err(io::Error::new(
                    error.kind(),
                    format!("waiting to read file: {error}"),
                ));
            }
        }
        match file.read(&mut chunk) {
            Ok(0) => return Ok(output),
            Ok(count) => {
                if count > limit.saturating_sub(output.len()) {
                    return Err(io::Error::new(
                        io::ErrorKind::QuotaExceeded,
                        format!("output exceeds maximum of {limit} bytes"),
                    ));
                }
                output.extend_from_slice(&chunk[..count]);
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
                ) => {}
            Err(error) => return Err(error),
        }
    }
}

/// Bounded-limit blocking read on platforms without a pollable model.
#[cfg(not(unix))]
pub(crate) fn read_input_file(
    mut file: &File,
    limit: usize,
    deadline: Instant,
    checkpoint: &WaitCheckpoint<'_>,
) -> io::Result<Vec<u8>> {
    use std::io::Read;
    let mut output = Vec::new();
    let mut chunk = vec![0u8; 64 * 1024];
    loop {
        checkpoint()?;
        // Only the slices between completed reads are interruptible; a
        // single blocked operating-system call still waits inside the
        // kernel, as documented for the Unix poll loop.
        if Instant::now() >= deadline {
            return Err(timed_out(Path::new("<open file>"), "reading"));
        }
        match file.read(&mut chunk) {
            Ok(0) => return Ok(output),
            Ok(count) => {
                if count > limit.saturating_sub(output.len()) {
                    return Err(io::Error::new(
                        io::ErrorKind::QuotaExceeded,
                        format!("output exceeds maximum of {limit} bytes"),
                    ));
                }
                output.extend_from_slice(&chunk[..count]);
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
}
