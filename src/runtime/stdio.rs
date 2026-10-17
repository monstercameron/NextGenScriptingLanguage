//! Cooperative descriptor writes for CLI output and runtime trace pipes.
use super::{check_deadline, Signals};
use anyhow::{bail, Context, Result};
use std::io;
use std::os::fd::RawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, TryLockError};
use std::time::{Duration, Instant};

// Flag transitions on a shared descriptor must be serialized with its writes.
// Waiting for another trace writer remains cancellable and deadline bounded.
static WRITES: Mutex<()> = Mutex::new(());

struct NonblockingFd {
    fd: RawFd,
    original_flags: libc::c_int,
}
impl NonblockingFd {
    fn new(fd: RawFd) -> Result<Self> {
        // SAFETY: fd is the caller's open descriptor, which stays open for the whole write (see
        // write_with_deadline). F_GETFL only reads the status flags and writes no memory. A bad
        // fd makes it return -1 with errno set, and the check on the next lines reports that.
        let original_flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if original_flags == -1
            // SAFETY: `||` short-circuits, so F_SETFL runs only when F_GETFL above succeeded and
            // original_flags holds a real flag word. It is the same open descriptor as above.
            || unsafe { libc::fcntl(fd, libc::F_SETFL, original_flags | libc::O_NONBLOCK) } == -1
        {
            return Err(io::Error::last_os_error()).context("setting output nonblocking");
        }
        Ok(Self { fd, original_flags })
    }
}
impl Drop for NonblockingFd {
    fn drop(&mut self) {
        // SAFETY: self.fd is the descriptor this guard was built from. The caller keeps it open
        // until write_with_deadline returns, and this guard drops before that. original_flags came
        // from the successful F_GETFL in new(), so F_SETFL only restores that flag word.
        unsafe {
            libc::fcntl(self.fd, libc::F_SETFL, self.original_flags);
        }
    }
}

/// Write bytes without letting pipe backpressure prevent signal cancellation.
/// The caller owns the descriptor; its original flags are restored on return.
/// Regular-file I/O still follows the filesystem's blocking behavior.
pub fn write_cancellable(fd: RawFd, bytes: &[u8], cancelled: &AtomicBool) -> Result<()> {
    write_with_deadline(fd, bytes, cancelled, None)
}
pub fn write_with_deadline(
    fd: RawFd,
    bytes: &[u8],
    cancelled: &AtomicBool,
    deadline: Option<Instant>,
) -> Result<()> {
    if bytes.is_empty() {
        super::check_deadline(deadline)?;
        if cancelled.load(Ordering::Relaxed) {
            bail!("program cancelled by signal");
        }
        return Ok(());
    }
    let _write_lock = loop {
        super::check_deadline(deadline)?;
        if cancelled.load(Ordering::Relaxed) {
            bail!("program cancelled by signal");
        }
        match WRITES.try_lock() {
            Ok(guard) => break guard,
            Err(TryLockError::Poisoned(error)) => break error.into_inner(),
            Err(TryLockError::WouldBlock) => {
                std::thread::sleep(std::time::Duration::from_millis(1))
            }
        }
    };
    let _guard = NonblockingFd::new(fd)?;
    let mut offset = 0;
    while offset < bytes.len() {
        super::check_deadline(deadline)?;
        if cancelled.load(Ordering::Relaxed) {
            bail!("program cancelled by signal");
        }
        let length = (bytes.len() - offset).min(isize::MAX as usize);
        // SAFETY: offset < bytes.len() from the loop condition, so bytes[offset..] is in bounds.
        // Its pointer is valid for `length` bytes, which is at most the bytes that remain. fd is
        // the caller's open descriptor, already O_NONBLOCK from NonblockingFd::new.
        let count = unsafe { libc::write(fd, bytes[offset..].as_ptr().cast(), length) };
        if count > 0 {
            offset += count as usize;
            continue;
        }
        if count == 0 {
            bail!("writing output made no progress");
        }
        let error = io::Error::last_os_error();
        match error.kind() {
            io::ErrorKind::Interrupted => continue,
            io::ErrorKind::WouldBlock => {
                let mut descriptor = libc::pollfd {
                    fd,
                    events: libc::POLLOUT,
                    revents: 0,
                };
                let timeout =
                    deadline.map_or(20, |deadline| poll_timeout_ms(deadline, Instant::now()));
                // SAFETY: descriptor is a live local pollfd, and nfds is 1 for its one element.
                // timeout is 20 or poll_timeout_ms(), which lies in 0..=20, a valid poll timeout.
                let ready = unsafe { libc::poll(&mut descriptor, 1, timeout) };
                if ready == -1 {
                    let error = io::Error::last_os_error();
                    if error.kind() != io::ErrorKind::Interrupted {
                        return Err(error).context("waiting for output to become writable");
                    }
                }
            }
            _ => return Err(error).context("writing output"),
        }
    }
    Ok(())
}

fn poll_timeout_ms(deadline: Instant, now: Instant) -> libc::c_int {
    const MAX_WAIT_MS: u128 = 20;
    let remaining = deadline.saturating_duration_since(now).as_nanos();
    let milliseconds = remaining / 1_000_000;
    milliseconds.min(MAX_WAIT_MS) as libc::c_int
}

/// Final diagnostics, written as the process exits. The whole message is written: partial
/// writes are retried, and a full or nonblocking descriptor is waited on rather than
/// abandoned, so a slow reader cannot cut the message short or drop it. The write does not
/// change the descriptor's flags, which another worker may rely on. The exit status is
/// already non-zero, so a failed write leaves nothing else to report.
pub fn write_diagnostic(bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    let _write_lock = WRITES.lock().unwrap_or_else(|error| error.into_inner());
    let mut offset = 0;
    while offset < bytes.len() {
        // SAFETY: offset < bytes.len(), so the pointer is in bounds, and the length passed is
        // exactly the bytes left from offset. STDERR_FILENO is this process's fd 2. If it is
        // closed or invalid, write fails with an errno that the branches below handle.
        let count = unsafe {
            libc::write(
                libc::STDERR_FILENO,
                bytes[offset..].as_ptr().cast(),
                bytes.len() - offset,
            )
        };
        if count < 0 {
            match io::Error::last_os_error().kind() {
                io::ErrorKind::Interrupted => {}
                io::ErrorKind::WouldBlock => wait_until_writable(libc::STDERR_FILENO),
                _ => return,
            }
        } else if count == 0 {
            return;
        } else {
            offset += count as usize;
        }
    }
}

/// Blocks until `fd` can accept output.
fn wait_until_writable(fd: RawFd) {
    let mut descriptor = libc::pollfd {
        fd,
        events: libc::POLLOUT,
        revents: 0,
    };
    // SAFETY: descriptor is a live local pollfd, and nfds is 1 for its one element. A -1 timeout
    // blocks until fd is ready or poll fails. poll writes only revents into that pollfd. The
    // return value is ignored, and the caller's write loop retries the write afterwards.
    unsafe {
        libc::poll(&mut descriptor, 1, -1);
    }
}

/// Read inherited stdin without changing its shared open-file-description flags.
/// Poll before each read so a quiet pipe remains cancellable and deadline-bound.
pub(super) fn capture_stdin(
    limit: usize,
    signals: &Signals,
    timeout: Option<Duration>,
) -> Result<Vec<u8>> {
    let deadline = timeout
        .map(|duration| {
            Instant::now()
                .checked_add(duration)
                .context("stdin timeout is too large")
        })
        .transpose()?;
    let mut output = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        if signals.cancelled.load(Ordering::Relaxed) {
            bail!("workflow cancelled by signal");
        }
        check_deadline(deadline)?;
        let mut descriptor = libc::pollfd {
            fd: libc::STDIN_FILENO,
            events: libc::POLLIN,
            revents: 0,
        };
        let wait = deadline.map_or(20, |end| poll_timeout_ms(end, Instant::now()));
        // SAFETY: descriptor is a live local pollfd, and nfds is 1 for its one element. wait is
        // 20 or poll_timeout_ms(), which lies in 0..=20, so it is a valid bounded timeout.
        let ready = unsafe { libc::poll(&mut descriptor, 1, wait) };
        if ready < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error).context("waiting for stdin");
        }
        if ready == 0 {
            continue;
        }
        if signals.cancelled.load(Ordering::Relaxed) {
            bail!("workflow cancelled by signal");
        }
        check_deadline(deadline)?;
        if descriptor.revents & libc::POLLNVAL != 0 {
            bail!("stdin is not an open file descriptor");
        }
        let count =
            // SAFETY: chunk is a live local array, and chunk.len() is the length passed, so the
            // kernel writes at most that many bytes. POLLNVAL was rejected above, and poll just
            // reported readiness, so the read should return without waiting.
            unsafe { libc::read(libc::STDIN_FILENO, chunk.as_mut_ptr().cast(), chunk.len()) };
        if count < 0 {
            let error = io::Error::last_os_error();
            if matches!(
                error.kind(),
                io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
            ) {
                continue;
            }
            return Err(error).context("reading stdin");
        }
        if count == 0 {
            return Ok(output);
        }
        let count = count as usize;
        if count > limit.saturating_sub(output.len()) {
            bail!("stdin exceeds maximum of {limit} bytes");
        }
        output.extend_from_slice(&chunk[..count]);
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    #[test]
    fn poll_wait_is_bounded_by_the_command_deadline() {
        let now = Instant::now();
        assert_eq!(poll_timeout_ms(now + Duration::from_millis(3), now), 3);
        assert_eq!(poll_timeout_ms(now + Duration::from_micros(1), now), 0);
        assert_eq!(poll_timeout_ms(now, now), 0);
        assert_eq!(poll_timeout_ms(now + Duration::from_secs(1), now), 20);
    }

    #[test]
    fn empty_write_still_observes_cancellation_and_deadline() {
        assert!(write_with_deadline(-1, &[], &AtomicBool::new(true), None).is_err());
        let error = write_with_deadline(-1, &[], &AtomicBool::new(false), Some(Instant::now()))
            .unwrap_err();
        assert!(error.is::<super::super::CommandTimedOut>());
        write_with_deadline(-1, &[], &AtomicBool::new(false), None).unwrap();
    }

    #[test]
    fn concurrent_writes_keep_deadlines_and_restore_blocking_mode() {
        let mut descriptors = [-1; 2];
        // SAFETY: descriptors is a 2-element c_int array, which is the size pipe() writes. The
        // assert on this line checks the result before either descriptor is used.
        assert_eq!(unsafe { libc::pipe(descriptors.as_mut_ptr()) }, 0);
        // SAFETY: descriptors[0] is the read end from that successful pipe() call. This OwnedFd
        // is its only owner, so the descriptor is closed exactly once, when _reader drops.
        let _reader = unsafe { OwnedFd::from_raw_fd(descriptors[0]) };
        // SAFETY: descriptors[1] is the write end from the same pipe() call, and no other owner
        // wraps it, so OwnedFd closes it exactly once.
        let writer = unsafe { OwnedFd::from_raw_fd(descriptors[1]) };
        let fd = writer.as_raw_fd();
        // SAFETY: fd is writer's raw descriptor. writer lives until the end of this test, so
        // F_GETFL reads an open descriptor and only reads flags.
        let initial = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        std::thread::scope(|scope| {
            let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
            let jobs = (0..2)
                .map(|_| {
                    let barrier = barrier.clone();
                    scope.spawn(move || {
                        barrier.wait();
                        write_with_deadline(
                            fd,
                            &vec![0; 4 * 1024 * 1024],
                            &AtomicBool::new(false),
                            Some(Instant::now() + Duration::from_millis(100)),
                        )
                    })
                })
                .collect::<Vec<_>>();
            barrier.wait();
            for job in jobs {
                assert!(job
                    .join()
                    .unwrap()
                    .unwrap_err()
                    .is::<super::super::CommandTimedOut>());
            }
        });
        assert_eq!(
            // SAFETY: fd is still writer's open descriptor, which lives until the end of the
            // test. F_GETFL only reads flags.
            unsafe { libc::fcntl(fd, libc::F_GETFL) } & libc::O_NONBLOCK,
            initial & libc::O_NONBLOCK
        );
    }
    #[test]
    fn full_descriptor_write_times_out_and_restores_flags() {
        let mut descriptors = [-1; 2];
        // SAFETY: descriptors is a 2-element c_int array, which is the size pipe() writes. The
        // assert on this line checks the result before either descriptor is used.
        assert_eq!(unsafe { libc::pipe(descriptors.as_mut_ptr()) }, 0);
        // SAFETY: descriptors[0] is the read end from that successful pipe() call. This OwnedFd
        // is its only owner, so the descriptor is closed exactly once, when _unread drops.
        let _unread = unsafe { OwnedFd::from_raw_fd(descriptors[0]) };
        // SAFETY: descriptors[1] is the write end from the same pipe() call, and no other owner
        // wraps it, so OwnedFd closes it exactly once.
        let writer = unsafe { OwnedFd::from_raw_fd(descriptors[1]) };
        let fd = writer.as_raw_fd();
        // SAFETY: fd is writer's raw descriptor. writer lives until the end of this test, so
        // F_GETFL reads an open descriptor and only reads flags.
        let before = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        let error = write_with_deadline(
            fd,
            &vec![0; 4 * 1024 * 1024],
            &AtomicBool::new(false),
            Some(Instant::now() + Duration::from_millis(20)),
        )
        .unwrap_err();
        assert!(error.is::<super::super::CommandTimedOut>());
        // Darwin reports an additional internal bit after a full pipe write.
        // The guard promises to restore the descriptor's blocking mode.
        assert_eq!(
            // SAFETY: fd is still writer's open descriptor, which lives until the end of the
            // test. F_GETFL only reads flags.
            unsafe { libc::fcntl(fd, libc::F_GETFL) } & libc::O_NONBLOCK,
            before & libc::O_NONBLOCK
        );
    }
}
