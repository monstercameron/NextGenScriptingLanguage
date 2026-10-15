//! Absolute wire-I/O budgets, independent of valid handler wait time.
use super::UnixStream;
use anyhow::Result;
use std::io::{self, BufRead, BufReader, Read, Write};
#[cfg(unix)]
use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

pub(super) const IO_TIMEOUT: Duration = Duration::from_secs(2);
pub(super) const RESPONSE_TIMEOUT: Duration = Duration::from_secs(12);

struct DeadlineIo<'a> {
    stream: &'a mut UnixStream,
    deadline: Instant,
    #[cfg(unix)]
    original_flags: libc::c_int,
}
impl<'a> DeadlineIo<'a> {
    fn new(stream: &'a mut UnixStream, timeout: Duration) -> io::Result<Self> {
        let deadline = Instant::now().checked_add(timeout).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "live I/O timeout is too large")
        })?;
        #[cfg(unix)]
        let original_flags = {
            let fd = stream.as_raw_fd();
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            if flags < 0 {
                return Err(io::Error::last_os_error());
            }
            if flags & libc::O_NONBLOCK == 0
                && unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
            {
                return Err(io::Error::last_os_error());
            }
            flags
        };
        Ok(Self {
            stream,
            deadline,
            #[cfg(unix)]
            original_flags,
        })
    }

    fn remaining(&self) -> io::Result<Duration> {
        self.deadline
            .checked_duration_since(Instant::now())
            .filter(|duration| !duration.is_zero())
            .ok_or_else(Self::expired)
    }
    fn expired() -> io::Error {
        io::Error::new(io::ErrorKind::TimedOut, "live transport deadline exceeded")
    }
    // A blocking socket syscall can keep progressing beyond SO_SNDTIMEO on
    // Unix. These streams are exclusively owned; restore descriptor flags on drop.
    #[cfg(unix)]
    fn unix_io(
        &mut self,
        events: libc::c_short,
        mut transfer: impl FnMut(&mut UnixStream) -> io::Result<usize>,
    ) -> io::Result<usize> {
        loop {
            self.remaining()?;
            let error = match transfer(self.stream) {
                Ok(count) => return self.checked(Ok(count)),
                Err(error) => error,
            };
            match error.kind() {
                io::ErrorKind::Interrupted => continue,
                io::ErrorKind::WouldBlock => {
                    let remaining = self.remaining()?;
                    let milliseconds = remaining.as_nanos().div_ceil(1_000_000);
                    let timeout = milliseconds.min(libc::c_int::MAX as u128) as libc::c_int;
                    let mut descriptor = libc::pollfd {
                        fd: self.stream.as_raw_fd(),
                        events,
                        revents: 0,
                    };
                    let ready = unsafe { libc::poll(&mut descriptor, 1, timeout) };
                    if ready < 0 {
                        let error = io::Error::last_os_error();
                        if error.kind() != io::ErrorKind::Interrupted {
                            return Err(error);
                        }
                    }
                }
                _ => return Err(error),
            }
        }
    }
    fn checked(&self, result: io::Result<usize>) -> io::Result<usize> {
        let count = result.map_err(|error| {
            if matches!(
                error.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
            ) {
                Self::expired()
            } else {
                error
            }
        })?;
        self.remaining()?;
        Ok(count)
    }
}
#[cfg(unix)]
impl Drop for DeadlineIo<'_> {
    fn drop(&mut self) {
        if self.original_flags & libc::O_NONBLOCK == 0 {
            loop {
                let result = unsafe {
                    libc::fcntl(self.stream.as_raw_fd(), libc::F_SETFL, self.original_flags)
                };
                if result >= 0 || io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                    break;
                }
            }
        }
    }
}
impl Read for DeadlineIo<'_> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        #[cfg(unix)]
        {
            self.unix_io(libc::POLLIN, |stream| stream.read(bytes))
        }
        #[cfg(windows)]
        {
            self.stream.set_read_timeout(Some(self.remaining()?))?;
            let result = self.stream.read(bytes);
            self.checked(result)
        }
    }
}
impl Write for DeadlineIo<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        #[cfg(unix)]
        {
            self.unix_io(libc::POLLOUT, |stream| stream.write(bytes))
        }
        #[cfg(windows)]
        {
            self.stream.set_write_timeout(Some(self.remaining()?))?;
            let result = self.stream.write(bytes);
            self.checked(result)
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        self.stream.flush()
    }
}

/// Keep the original LF/EOF framing and one extra byte for the caller's
/// existing size diagnostic. Progress never renews the absolute budget.
pub(super) fn read_message(
    stream: &mut UnixStream,
    limit: usize,
    timeout: Duration,
) -> Result<Vec<u8>> {
    let mut budget = DeadlineIo::new(stream, timeout)?;
    let mut bytes = Vec::new();
    BufReader::new(&mut budget)
        .take((limit as u64).saturating_add(1))
        .read_until(b'\n', &mut bytes)?;
    budget.remaining()?;
    Ok(bytes)
}
pub(super) fn write_message(
    stream: &mut UnixStream,
    bytes: &[u8],
    timeout: Duration,
) -> Result<()> {
    DeadlineIo::new(stream, timeout)?.write_all(bytes)?;
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::net::Shutdown;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };

    #[test]
    fn framing_preserves_lf_eof_and_extra_byte_limit_detection() {
        for (wire, limit, expected) in [
            (b"{}\nignored".as_slice(), 16, b"{}\n".as_slice()),
            (b"{}".as_slice(), 16, b"{}".as_slice()),
            (b"oversized\n".as_slice(), 2, b"ove".as_slice()),
            (b"".as_slice(), 2, b"".as_slice()),
        ] {
            let (mut client, mut server) = UnixStream::pair().unwrap();
            client.write_all(wire).unwrap();
            client.shutdown(Shutdown::Write).unwrap();
            assert_eq!(
                read_message(&mut server, limit, IO_TIMEOUT).unwrap(),
                expected
            );
        }
    }

    #[test]
    fn progressing_reads_do_not_renew_the_wire_deadline() {
        let (mut client, mut server) = UnixStream::pair().unwrap();
        client
            .set_write_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let quit = stop.clone();
        let sender = std::thread::spawn(move || {
            while !quit.load(Ordering::Relaxed) {
                if client.write_all(b" ").is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        });
        let start = Instant::now();
        let result = read_message(&mut server, 1024, Duration::from_millis(80));
        stop.store(true, Ordering::Relaxed);
        drop(server);
        sender.join().unwrap();
        assert_eq!(
            result
                .unwrap_err()
                .downcast_ref::<io::Error>()
                .unwrap()
                .kind(),
            io::ErrorKind::TimedOut
        );
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn backpressured_write_has_an_absolute_budget() {
        let (mut client, _unread_peer) = UnixStream::pair().unwrap();
        let start = Instant::now();
        let result = write_message(
            &mut client,
            &vec![b'x'; 4 * 1024 * 1024],
            Duration::from_millis(80),
        );
        assert_eq!(
            result
                .unwrap_err()
                .downcast_ref::<io::Error>()
                .unwrap()
                .kind(),
            io::ErrorKind::TimedOut
        );
        assert!(start.elapsed() < Duration::from_secs(2));
    }
    #[test]
    fn progressing_writes_do_not_renew_the_wire_deadline_or_change_flags() {
        let (mut client, mut peer) = UnixStream::pair().unwrap();
        let fd = client.as_raw_fd();
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        assert!(flags >= 0);
        let quit = Arc::new(AtomicBool::new(false));
        let stop = quit.clone();
        let receiver = std::thread::spawn(move || {
            peer.set_read_timeout(Some(Duration::from_millis(100)))
                .unwrap();
            let mut progress = 0;
            while !stop.load(Ordering::Relaxed) {
                let mut bytes = [0; 1024];
                match peer.read(&mut bytes) {
                    Ok(0) => break,
                    Ok(count) => progress += count,
                    Err(error)
                        if matches!(
                            error.kind(),
                            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                        ) => {}
                    Err(_) => break,
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            progress
        });
        let started = Instant::now();
        let result = write_message(
            &mut client,
            &vec![b'x'; 4 * 1024 * 1024],
            Duration::from_millis(150),
        );
        quit.store(true, Ordering::Relaxed);
        let elapsed = started.elapsed();
        let after = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        drop(client);
        let progress = receiver.join().unwrap();
        assert_eq!(
            result
                .unwrap_err()
                .downcast_ref::<io::Error>()
                .unwrap()
                .kind(),
            io::ErrorKind::TimedOut
        );
        assert_eq!(flags, after);
        assert!(progress > 0);
        assert!(elapsed < Duration::from_secs(2));
    }
}
