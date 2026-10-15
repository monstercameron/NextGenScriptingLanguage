//! Interruptible TCP transport adapter for ureq.
//!
//! ureq's public request API is synchronous, so its default socket transport
//! can remain blocked until the socket timeout expires. This adapter uses
//! nonblocking mio sockets and polls in short slices, checking the workflow's
//! cancellation flag between polls. The adapter is deliberately connector-only:
//! ureq continues to provide HTTP parsing and the configured TLS implementation.

use std::fmt;
use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use mio::net::TcpStream;
use mio::{Events, Interest, Poll, Token};
use ureq::unversioned::resolver::ResolvedSocketAddrs;
use ureq::unversioned::transport::{
    Buffers, ConnectionDetails, Connector, LazyBuffers, NextTimeout, Transport,
};
use ureq::Error;

const SOCKET_TOKEN: Token = Token(0);
pub(super) const CANCEL_POLL_INTERVAL: Duration = Duration::from_millis(20);

#[derive(Clone)]
pub(super) struct InterruptibleTcpConnector {
    cancelled: Arc<AtomicBool>,
}

impl InterruptibleTcpConnector {
    pub(super) fn new(cancelled: Arc<AtomicBool>) -> Self {
        Self { cancelled }
    }
}

impl fmt::Debug for InterruptibleTcpConnector {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("InterruptibleTcpConnector")
            .finish_non_exhaustive()
    }
}

impl<In: Transport> Connector<In> for InterruptibleTcpConnector {
    type Out = EitherTransport<In>;

    fn connect(
        &self,
        details: &ConnectionDetails,
        chained: Option<In>,
    ) -> Result<Option<Self::Out>, Error> {
        if let Some(chained) = chained {
            return Ok(Some(EitherTransport::Chained(chained)));
        }
        if details.uri.scheme_str() != Some("http") && details.uri.scheme_str() != Some("https") {
            return Ok(None);
        }
        let stream =
            connect_nonblocking(&details.addrs, details.timeout, Arc::clone(&self.cancelled))?;
        let buffers = LazyBuffers::new(
            details.config.input_buffer_size(),
            details.config.output_buffer_size(),
        );
        Ok(Some(EitherTransport::Socket(InterruptibleTcpTransport {
            poll: stream.poll,
            events: Events::with_capacity(16),
            stream: stream.stream,
            buffers,
            cancelled: Arc::clone(&self.cancelled),
            open: true,
        })))
    }
}

pub(super) enum EitherTransport<In: Transport> {
    Chained(In),
    Socket(InterruptibleTcpTransport),
}

impl<In: Transport> fmt::Debug for EitherTransport<In> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Chained(_) => formatter.write_str("ChainedTransport"),
            Self::Socket(socket) => fmt::Debug::fmt(socket, formatter),
        }
    }
}

impl<In: Transport> Transport for EitherTransport<In> {
    fn buffers(&mut self) -> &mut dyn Buffers {
        match self {
            Self::Chained(inner) => inner.buffers(),
            Self::Socket(socket) => socket.buffers(),
        }
    }

    fn transmit_output(&mut self, amount: usize, timeout: NextTimeout) -> Result<(), Error> {
        match self {
            Self::Chained(inner) => inner.transmit_output(amount, timeout),
            Self::Socket(socket) => socket.transmit_output(amount, timeout),
        }
    }

    fn await_input(&mut self, timeout: NextTimeout) -> Result<bool, Error> {
        match self {
            Self::Chained(inner) => inner.await_input(timeout),
            Self::Socket(socket) => socket.await_input(timeout),
        }
    }

    fn is_open(&mut self) -> bool {
        match self {
            Self::Chained(inner) => inner.is_open(),
            Self::Socket(socket) => socket.is_open(),
        }
    }

    fn is_tls(&self) -> bool {
        match self {
            Self::Chained(inner) => inner.is_tls(),
            Self::Socket(_) => false,
        }
    }
}

struct ConnectedSocket {
    poll: Poll,
    stream: TcpStream,
}

fn connect_nonblocking(
    addresses: &ResolvedSocketAddrs,
    timeout: NextTimeout,
    cancelled: Arc<AtomicBool>,
) -> Result<ConnectedSocket, Error> {
    if addresses.is_empty() {
        return Err(Error::HostNotFound);
    }
    let mut last_error = None;
    let started = Instant::now();

    for address in addresses.iter().copied() {
        check_cancel(&cancelled)?;
        let mut poll = Poll::new().map_err(Error::Io)?;
        let mut stream = match TcpStream::connect(address) {
            Ok(stream) => stream,
            Err(error) => {
                last_error = Some(error);
                continue;
            }
        };
        poll.registry()
            .register(&mut stream, SOCKET_TOKEN, Interest::WRITABLE)
            .map_err(Error::Io)?;
        let mut events = Events::with_capacity(16);
        wait_ready(&mut poll, &mut events, timeout, started, &cancelled)?;
        if let Some(error) = stream.take_error().map_err(Error::Io)? {
            last_error = Some(error);
            continue;
        }
        poll.registry()
            .reregister(
                &mut stream,
                SOCKET_TOKEN,
                Interest::READABLE.add(Interest::WRITABLE),
            )
            .map_err(Error::Io)?;
        return Ok(ConnectedSocket { poll, stream });
    }

    Err(Error::Io(last_error.unwrap_or_else(|| {
        io::Error::new(io::ErrorKind::ConnectionRefused, "connection refused")
    })))
}

fn check_cancel(cancelled: &AtomicBool) -> Result<(), Error> {
    if cancelled.load(Ordering::Relaxed) {
        Err(Error::Io(io::Error::new(
            io::ErrorKind::Interrupted,
            "request cancelled",
        )))
    } else {
        Ok(())
    }
}

fn wait_ready(
    poll: &mut Poll,
    events: &mut Events,
    timeout: NextTimeout,
    started: Instant,
    cancelled: &AtomicBool,
) -> Result<(), Error> {
    loop {
        check_cancel(cancelled)?;
        let wait = match timeout.not_zero() {
            Some(limit) => {
                let elapsed = started.elapsed();
                if elapsed >= *limit {
                    return Err(Error::Timeout(timeout.reason));
                }
                (*limit - elapsed).min(CANCEL_POLL_INTERVAL)
            }
            None => CANCEL_POLL_INTERVAL,
        };
        poll.poll(events, Some(wait)).map_err(Error::Io)?;
        if !events.is_empty() {
            return Ok(());
        }
    }
}

pub(super) struct InterruptibleTcpTransport {
    poll: Poll,
    events: Events,
    stream: TcpStream,
    buffers: LazyBuffers,
    cancelled: Arc<AtomicBool>,
    open: bool,
}

impl fmt::Debug for InterruptibleTcpTransport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("InterruptibleTcpTransport")
            .field("open", &self.open)
            .finish_non_exhaustive()
    }
}

impl Transport for InterruptibleTcpTransport {
    fn buffers(&mut self) -> &mut dyn Buffers {
        &mut self.buffers
    }

    fn transmit_output(&mut self, amount: usize, timeout: NextTimeout) -> Result<(), Error> {
        let started = Instant::now();
        let mut offset = 0;
        while offset < amount {
            check_cancel(&self.cancelled)?;
            let output = &self.buffers.output()[offset..amount];
            match self.stream.write(output) {
                Ok(0) => {
                    self.open = false;
                    return Err(Error::Io(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "socket accepted no request bytes",
                    )));
                }
                Ok(written) => offset += written,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => wait_ready(
                    &mut self.poll,
                    &mut self.events,
                    timeout,
                    started,
                    &self.cancelled,
                )?,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {
                    check_cancel(&self.cancelled)?;
                }
                Err(error) => {
                    self.open = false;
                    return Err(Error::Io(error));
                }
            }
        }
        Ok(())
    }

    fn await_input(&mut self, timeout: NextTimeout) -> Result<bool, Error> {
        let started = Instant::now();
        loop {
            check_cancel(&self.cancelled)?;
            let input = self.buffers.input_append_buf();
            match self.stream.read(input) {
                Ok(0) => {
                    self.open = false;
                    self.buffers.input_appended(0);
                    return Ok(false);
                }
                Ok(read) => {
                    self.buffers.input_appended(read);
                    return Ok(true);
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => wait_ready(
                    &mut self.poll,
                    &mut self.events,
                    timeout,
                    started,
                    &self.cancelled,
                )?,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {
                    check_cancel(&self.cancelled)?;
                }
                Err(error) => {
                    self.open = false;
                    return Err(Error::Io(error));
                }
            }
        }
    }

    fn is_open(&mut self) -> bool {
        self.open && !self.cancelled.load(Ordering::Relaxed)
    }
}
