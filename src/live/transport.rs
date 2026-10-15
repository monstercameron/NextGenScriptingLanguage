//! Absolute wire-I/O budgets, independent of valid handler wait time.
use super::UnixStream;
use anyhow::Result;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::time::{Duration, Instant};

pub(super) const IO_TIMEOUT: Duration = Duration::from_secs(2);
pub(super) const RESPONSE_TIMEOUT: Duration = Duration::from_secs(12);

struct DeadlineIo<'a> {
    stream: &'a mut UnixStream,
    deadline: Instant,
}
impl<'a> DeadlineIo<'a> {
    fn new(stream: &'a mut UnixStream, timeout: Duration) -> io::Result<Self> {
        Ok(Self {
            stream,
            deadline: Instant::now().checked_add(timeout).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "live I/O timeout is too large")
            })?,
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
impl Read for DeadlineIo<'_> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        self.stream.set_read_timeout(Some(self.remaining()?))?;
        let result = self.stream.read(bytes);
        self.checked(result)
    }
}
impl Write for DeadlineIo<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        self.stream.set_write_timeout(Some(self.remaining()?))?;
        let result = self.stream.write(bytes);
        self.checked(result)
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
}
