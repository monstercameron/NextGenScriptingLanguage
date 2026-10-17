//! The fcntl, poll and geteuid calls the runtime makes, written once here. Each wrapper
//! turns the call's -1 result into an io::Error, and each `unsafe` block states the kernel's
//! contract for its arguments. None of these calls reads or writes memory the caller does not
//! pass in: an invalid descriptor is reported as EBADF or POLLNVAL, never as undefined behaviour.
use std::io;
use std::os::fd::{FromRawFd, OwnedFd, RawFd};

/// The file-status flags of `fd` (F_GETFL).
pub(crate) fn status_flags(fd: RawFd) -> io::Result<libc::c_int> {
    // SAFETY: F_GETFL takes no argument and reads no memory. An invalid `fd` makes the call
    // return -1 with EBADF, which is reported below.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(flags)
}

/// Replace the file-status flags of `fd` (F_SETFL), for example to restore saved flags.
pub(crate) fn set_status_flags(fd: RawFd, flags: libc::c_int) -> io::Result<()> {
    // SAFETY: F_SETFL takes an integer flag set and reads no memory. An invalid `fd` makes the
    // call return -1, which is reported below.
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Add O_NONBLOCK to `fd` and return the flags it had, so the caller can restore them with
/// `set_status_flags`.
pub(crate) fn set_nonblocking(fd: RawFd) -> io::Result<libc::c_int> {
    let original = status_flags(fd)?;
    set_status_flags(fd, original | libc::O_NONBLOCK)?;
    Ok(original)
}

/// Mark `fd` close-on-exec (F_SETFD with FD_CLOEXEC).
pub(crate) fn set_close_on_exec(fd: RawFd) -> io::Result<()> {
    // SAFETY: F_SETFD takes an integer and reads no memory. An invalid `fd` makes the call
    // return -1, which is reported below.
    if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// A new close-on-exec descriptor, at least `minimum`, that refers to the same file as `fd`
/// (F_DUPFD_CLOEXEC). The returned OwnedFd is the only owner of the new descriptor.
pub(crate) fn duplicate_cloexec(fd: RawFd, minimum: libc::c_int) -> io::Result<OwnedFd> {
    // SAFETY: F_DUPFD_CLOEXEC takes an integer and reads no memory. On success it returns a
    // new descriptor that nothing else owns, and on failure it returns -1, which is checked.
    let copy = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, minimum) };
    if copy < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `copy` is the fresh descriptor the call returned above, and no other owner
    // wraps it, so OwnedFd closes it exactly once.
    Ok(unsafe { OwnedFd::from_raw_fd(copy) })
}

/// Poll `fd` for `events` for at most `timeout_ms` milliseconds (-1 waits without a limit).
/// Returns the ready count and the revents the kernel set for the entry.
pub(crate) fn poll_one(
    fd: RawFd,
    events: libc::c_short,
    timeout_ms: libc::c_int,
) -> io::Result<(libc::c_int, libc::c_short)> {
    let mut descriptor = libc::pollfd {
        fd,
        events,
        revents: 0,
    };
    // SAFETY: `descriptor` is one live pollfd and the count is 1, so poll reads and writes only
    // that entry. Its `fd` is validated by the kernel, and an invalid one reports POLLNVAL in
    // revents rather than undefined behaviour.
    let ready = unsafe { libc::poll(&mut descriptor, 1, timeout_ms) };
    if ready < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((ready, descriptor.revents))
}

/// The effective user id of this process (geteuid).
pub(crate) fn effective_uid() -> libc::uid_t {
    // SAFETY: geteuid takes no arguments, reads no memory, and cannot fail.
    unsafe { libc::geteuid() }
}
