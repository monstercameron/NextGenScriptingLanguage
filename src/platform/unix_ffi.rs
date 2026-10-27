//! The fcntl, poll and geteuid calls the runtime makes, written once here. Each wrapper
//! turns the call's -1 result into an io::Error, and each `unsafe` block states the kernel's
//! contract for its arguments. None of these calls reads or writes memory the caller does not
//! pass in: an invalid descriptor is reported as EBADF or POLLNVAL, never as undefined behaviour.
use std::io;
use std::os::fd::{FromRawFd, OwnedFd, RawFd};

/// Mark descriptors inherited by the process as close-on-exec before runtime threads start.
///
/// This is intentionally a startup operation rather than a per-child `pre_exec` hook: changing
/// descriptor flags in the parent immediately before a spawn races with other threads spawning
/// children. Doing the change once, before the runtime starts any threads, keeps later spawns on
/// Rust's normal `posix_spawn` fast path while ensuring inherited descriptors do not leak.
/// If the platform exposes no exact descriptor enumeration API, startup fails closed; RLIMIT
/// values are not a safe scan bound because an already-open descriptor can exceed a lowered limit.
pub(crate) fn mark_inherited_descriptors_close_on_exec() -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        // CLOSE_RANGE_CLOEXEC was added in Linux 5.11. Unlike CLOSE_RANGE_UNSHARE, it changes
        // the current descriptor table in place; at process startup this table is not shared
        // with any other threads yet.
        const CLOSE_RANGE_CLOEXEC: libc::c_uint = 1 << 2;
        // SAFETY: close_range with CLOEXEC changes flags for the inclusive descriptor range and
        // does not dereference either endpoint.
        let result = unsafe {
            libc::syscall(
                libc::SYS_close_range,
                3 as libc::c_uint,
                libc::c_uint::MAX,
                CLOSE_RANGE_CLOEXEC,
            )
        };
        if result == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if !matches!(
            error.raw_os_error(),
            Some(libc::ENOSYS) | Some(libc::EINVAL) | Some(libc::EPERM)
        ) {
            return Err(error);
        }
    }

    mark_open_descriptors_close_on_exec()
}

fn mark_open_descriptors_close_on_exec() -> io::Result<()> {
    #[cfg(target_os = "linux")]
    let paths = ["/proc/self/fd", "/dev/fd"];
    #[cfg(target_os = "macos")]
    let paths = ["/dev/fd"];
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let paths = ["/proc/self/fd", "/dev/fd"];

    let mut enumeration_errors = Vec::new();
    for path in paths {
        match list_open_descriptors(path) {
            Ok(descriptors) => {
                for descriptor in descriptors {
                    set_close_on_exec_preserving_flags(descriptor)?;
                }
                return Ok(());
            }
            Err(error) => enumeration_errors.push(error),
        }
    }
    #[cfg(target_os = "macos")]
    {
        match list_open_descriptors_with_libproc() {
            Ok(descriptors) => {
                for descriptor in descriptors {
                    set_close_on_exec_preserving_flags(descriptor)?;
                }
                return Ok(());
            }
            Err(error) => enumeration_errors.push(error),
        }
    }
    let details = enumeration_errors
        .iter()
        .map(io::Error::to_string)
        .collect::<Vec<_>>()
        .join("; ");
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("cannot safely enumerate inherited descriptors: {details}"),
    ))
}

fn list_open_descriptors(path: &str) -> io::Result<Vec<RawFd>> {
    // On Linux procfs and on macOS /dev/fd enumerate descriptors actually open in this process.
    // Unlike an RLIMIT_NOFILE scan, the work is proportional to the number of open descriptors,
    // even when the configured descriptor limit is very large. The directory's own descriptor
    // may appear in the snapshot; marking it is harmless, and this runs before runtime threads.
    let entries = std::fs::read_dir(path)?;
    let mut descriptors = Vec::new();
    for entry in entries {
        let entry = entry?;
        let Some(descriptor) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<RawFd>().ok())
        else {
            continue;
        };
        if descriptor >= 3 {
            descriptors.push(descriptor);
        }
    }
    Ok(descriptors)
}

#[cfg(target_os = "macos")]
fn list_open_descriptors_with_libproc() -> io::Result<Vec<RawFd>> {
    const PROC_PIDLISTFDS: libc::c_int = 1;
    const PROC_FDINFO_SIZE: usize = 8;
    const EXTRA_FD_SLOTS: usize = 20;

    // SAFETY: getpid takes no arguments and cannot fail.
    let pid = unsafe { libc::getpid() };
    // SAFETY: proc_pidinfo with a null buffer asks libproc for a required buffer size. It does
    // not read or write through the null pointer for this sizing call.
    let required = unsafe { proc_pidinfo(pid, PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0) };
    if required <= 0 {
        return Err(io::Error::last_os_error());
    }
    let mut capacity = (required as usize)
        .checked_add(EXTRA_FD_SLOTS * PROC_FDINFO_SIZE)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "fd list size overflow"))?;
    for _ in 0..3 {
        let mut bytes = vec![0u8; capacity];
        // SAFETY: `bytes` is writable for `capacity` bytes, which fits in c_int; proc_pidinfo
        // writes only the returned fd-info records into the supplied buffer.
        let written = unsafe {
            proc_pidinfo(
                pid,
                PROC_PIDLISTFDS,
                0,
                bytes.as_mut_ptr().cast(),
                capacity as libc::c_int,
            )
        };
        if written < 0 {
            return Err(io::Error::last_os_error());
        }
        let written = written as usize;
        if written < capacity {
            if written % PROC_FDINFO_SIZE != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "libproc returned a partial fd-info record",
                ));
            }
            return Ok(bytes[..written]
                .chunks_exact(PROC_FDINFO_SIZE)
                .map(|record| i32::from_ne_bytes([record[0], record[1], record[2], record[3]]))
                .filter(|descriptor| *descriptor >= 3)
                .collect());
        }
        capacity = capacity
            .checked_mul(2)
            .filter(|capacity| *capacity <= libc::c_int::MAX as usize)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "fd list size exceeds libproc limit",
                )
            })?;
    }
    Err(io::Error::new(
        io::ErrorKind::WouldBlock,
        "descriptor table changed while libproc fd lists were being read",
    ))
}

#[cfg(target_os = "macos")]
#[link(name = "proc")]
// SAFETY: `proc_pidinfo` is provided by libproc and accepts process metadata output buffers.
extern "C" {
    fn proc_pidinfo(
        pid: libc::c_int,
        flavor: libc::c_int,
        arg: u64,
        buffer: *mut libc::c_void,
        buffersize: libc::c_int,
    ) -> libc::c_int;
}

fn set_close_on_exec_preserving_flags(descriptor: RawFd) -> io::Result<()> {
    // SAFETY: F_GETFD reads only this descriptor's flags.
    let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFD) };
    if flags == -1 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::EBADF) {
            return Ok(());
        }
        return Err(error);
    }
    if flags & libc::FD_CLOEXEC == 0
        // SAFETY: F_SETFD reads no pointer and preserves existing descriptor flags.
        && unsafe { libc::fcntl(descriptor, libc::F_SETFD, flags | libc::FD_CLOEXEC) } == -1
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

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

#[cfg(test)]
mod descriptor_enumeration_tests {
    use super::{list_open_descriptors, mark_inherited_descriptors_close_on_exec};
    use std::fs::File;
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::process::CommandExt;
    use std::process::Command;

    #[test]
    fn platform_enumerator_finds_open_descriptors_above_standard_streams() {
        let file = File::open("/dev/null").expect("opening descriptor enumeration fixture");
        // SAFETY: F_DUPFD_CLOEXEC reads the live file descriptor and returns a fresh one.
        let descriptor = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 64) };
        assert!(
            descriptor >= 64,
            "allocating high fd: {}",
            std::io::Error::last_os_error()
        );
        // SAFETY: the fcntl call above allocated this descriptor and no other value owns it.
        let descriptor = unsafe { OwnedFd::from_raw_fd(descriptor) };
        #[cfg(target_os = "linux")]
        let paths = ["/proc/self/fd", "/dev/fd"];
        #[cfg(target_os = "macos")]
        let paths = ["/dev/fd"];
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        let paths = ["/proc/self/fd", "/dev/fd"];

        let available_enumerators = paths
            .iter()
            .filter_map(|path| list_open_descriptors(path).ok())
            .collect::<Vec<_>>();
        let found_in_directory = available_enumerators
            .iter()
            .any(|descriptors| descriptors.contains(&descriptor.as_raw_fd()));
        #[cfg(target_os = "macos")]
        {
            let descriptors = super::list_open_descriptors_with_libproc()
                .expect("enumerating open descriptors with libproc");
            assert!(
                found_in_directory || descriptors.contains(&descriptor.as_raw_fd()),
                "descriptor {} was missing from /dev/fd and libproc",
                descriptor.as_raw_fd()
            );
        }
        #[cfg(not(target_os = "macos"))]
        if available_enumerators.is_empty() {
            eprintln!("skipping fd-directory check: neither procfs nor /dev/fd is available");
            return;
        }
        #[cfg(not(target_os = "macos"))]
        assert!(
            found_in_directory,
            "descriptor {} was missing from every available descriptor directory",
            descriptor.as_raw_fd()
        );
    }

    #[test]
    fn missing_descriptor_directory_returns_without_a_limit_scan() {
        let error = list_open_descriptors("/definitely-not-a-descriptor-directory")
            .expect_err("missing descriptor directory must be reported");
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
    }

    #[test]
    fn startup_marking_covers_an_inherited_fd_above_the_reduced_soft_limit() {
        const HELPER_ENV: &str = "EXECUTE_TEST_FD_MARKING_HELPER";
        const TEST_NAME: &str = concat!(
            "platform::unix_ffi::descriptor_enumeration_tests::",
            "startup_marking_covers_an_inherited_fd_above_the_reduced_soft_limit"
        );

        if let Ok(helper_mode) = std::env::var(HELPER_ENV) {
            let descriptor: libc::c_int = helper_mode
                .split_once(':')
                .and_then(|(_, fd)| fd.parse().ok())
                .expect("helper descriptor");
            if helper_mode.starts_with("scan:") {
                let lowered_limit: libc::rlim_t = std::env::var("EXECUTE_TEST_FD_LOWERED_LIMIT")
                    .expect("lowered limit")
                    .parse()
                    .expect("numeric lowered limit");
                let mut limits = std::mem::MaybeUninit::<libc::rlimit>::uninit();
                let resource = libc::RLIMIT_NOFILE;
                // SAFETY: getrlimit initializes the supplied rlimit on success.
                let get_limits = unsafe { libc::getrlimit(resource, limits.as_mut_ptr()) };
                assert_eq!(get_limits, 0);
                // SAFETY: getrlimit succeeded and initialized `limits`.
                let mut limits = unsafe { limits.assume_init() };
                assert!(lowered_limit < descriptor as libc::rlim_t);
                limits.rlim_cur = lowered_limit;
                // SAFETY: setrlimit reads the initialized rlimit structure and only lowers the
                // soft descriptor limit in this isolated helper process.
                let set_limits = unsafe { libc::setrlimit(resource, &limits) };
                assert_eq!(set_limits, 0);
                mark_inherited_descriptors_close_on_exec().expect("marking inherited descriptors");
                // SAFETY: F_GETFD reads only the flags for the still-open inherited descriptor.
                let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFD) };
                assert!(flags >= 0, "descriptor {descriptor} was closed");
                assert_ne!(
                    flags & libc::FD_CLOEXEC,
                    0,
                    "descriptor {descriptor} not marked"
                );

                let output =
                    Command::new(std::env::current_exe().expect("current test executable"))
                        .args(["--exact", TEST_NAME, "--nocapture"])
                        .env(HELPER_ENV, format!("exec:{descriptor}"))
                        .output()
                        .expect("starting fd-close verification child");
                assert!(
                    output.status.success(),
                    "exec child retained descriptor {descriptor}:\n{}",
                    String::from_utf8_lossy(&output.stderr)
                );
                return;
            }
            assert!(helper_mode.starts_with("exec:"));
            // SAFETY: F_GETFD reads only the descriptor flags at the inherited fd number.
            assert_eq!(unsafe { libc::fcntl(descriptor, libc::F_GETFD) }, -1);
            assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::EBADF));
            return;
        }

        let mut limits = std::mem::MaybeUninit::<libc::rlimit>::uninit();
        let resource = libc::RLIMIT_NOFILE;
        // SAFETY: getrlimit initializes the supplied rlimit on success.
        let get_limits = unsafe { libc::getrlimit(resource, limits.as_mut_ptr()) };
        assert_eq!(get_limits, 0);
        // SAFETY: getrlimit succeeded and initialized `limits`.
        let limits = unsafe { limits.assume_init() };
        let upper = limits.rlim_cur.min(512 as libc::rlim_t);
        if upper <= 64 {
            eprintln!("skipping lowered-limit descriptor test: soft limit is too small");
            return;
        }
        let descriptor = upper as libc::c_int - 1;
        let lowered_limit = descriptor as libc::rlim_t / 2;

        let mut pipe = [-1; 2];
        // SAFETY: `pipe` points to two writable integers for the kernel to initialize.
        assert_eq!(unsafe { libc::pipe(pipe.as_mut_ptr()) }, 0);
        let (reader, writer) = {
            // SAFETY: pipe initialized both entries as fresh, uniquely owned descriptors.
            unsafe { (OwnedFd::from_raw_fd(pipe[0]), OwnedFd::from_raw_fd(pipe[1])) }
        };
        // SAFETY: F_DUPFD_CLOEXEC reads the live pipe writer and returns a fresh descriptor.
        let inherited = unsafe { libc::fcntl(writer.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
        assert!(
            inherited >= 0,
            "duplicating pipe writer: {}",
            io::Error::last_os_error()
        );
        // SAFETY: fcntl above created this descriptor and no other value owns it.
        let inherited = unsafe { OwnedFd::from_raw_fd(inherited) };
        let inherited_fd = inherited.as_raw_fd();
        let mut helper = Command::new(std::env::current_exe().expect("current test executable"));
        helper
            .args(["--exact", TEST_NAME, "--nocapture"])
            .env(HELPER_ENV, format!("scan:{descriptor}"))
            .env("EXECUTE_TEST_FD_LOWERED_LIMIT", lowered_limit.to_string());
        // SAFETY: `inherited_fd` remains live until spawn returns. dup2 onto the selected target
        // clears FD_CLOEXEC, simulating a high descriptor inherited from the invoking process.
        unsafe {
            helper.pre_exec(move || {
                if libc::dup2(inherited_fd, descriptor) == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = helper
            .spawn()
            .expect("starting isolated fd enumerator helper");
        drop(writer);
        drop(inherited);
        let output = child
            .wait_with_output()
            .expect("waiting for fd enumerator helper");
        assert!(
            output.status.success(),
            "fd enumerator helper failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let mut byte = 0u8;
        let bytes_read = {
            // SAFETY: `reader` is the live read end; EOF confirms no process kept the writer open.
            unsafe { libc::read(reader.as_raw_fd(), (&mut byte as *mut u8).cast(), 1) }
        };
        assert_eq!(bytes_read, 0);
    }
}
