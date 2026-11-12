//! Request and reply framing for stream sockets, with typed outcomes (#312).
//!
//! This module is platform-neutral. It runs one request/reply exchange over any
//! `Read + Write` stream. Connecting, the socket's own read and write timeouts and
//! the socket path authorization belong to the applet layer, which hands a connected
//! stream to [`exchange`].
//!
//! Retry rule: once any request byte has been written, no outcome may be treated as
//! "nothing happened". Only [`Outcome::NotConnected`] answers
//! [`Outcome::may_retry_safely`] with `true`.

use std::fmt;
use std::io::{self, ErrorKind, Read, Write};

/// Default reply size limit (64 MiB). Every framing carries its own limit.
pub const DEFAULT_MAX_REPLY_BYTES: usize = 64 * 1024 * 1024;

/// Largest single read or write request made to the stream.
const CHUNK: usize = 8 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
enum Kind {
    UntilClose,
    Delimiter(Vec<u8>),
    Fixed(usize),
    LengthPrefix4Be,
}

/// How the end of a reply is recognised. Constructed only through the validated
/// constructors, so an empty delimiter or an impossible fixed length cannot exist.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Framing {
    kind: Kind,
    max_reply_bytes: usize,
}

/// A framing that cannot be used, reported before any byte is sent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FramingError {
    /// A delimiter must contain at least one byte.
    EmptyDelimiter,
    /// A fixed-length reply cannot be larger than the reply size limit.
    FixedLenExceedsMax { len: usize, max: usize },
}

impl fmt::Display for FramingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FramingError::EmptyDelimiter => write!(f, "reply delimiter must not be empty"),
            FramingError::FixedLenExceedsMax { len, max } => {
                write!(
                    f,
                    "fixed reply length {len} exceeds the reply size limit {max}"
                )
            }
        }
    }
}

impl std::error::Error for FramingError {}

impl Framing {
    /// The reply ends when the peer closes its side (end of stream).
    pub fn until_close(max_reply_bytes: usize) -> Self {
        Framing {
            kind: Kind::UntilClose,
            max_reply_bytes,
        }
    }

    /// The reply ends at the first occurrence of `delimiter`, which is not part of the
    /// reply. Bytes after the delimiter are an error.
    pub fn delimiter(delimiter: &[u8], max_reply_bytes: usize) -> Result<Self, FramingError> {
        if delimiter.is_empty() {
            return Err(FramingError::EmptyDelimiter);
        }
        Ok(Framing {
            kind: Kind::Delimiter(delimiter.to_vec()),
            max_reply_bytes,
        })
    }

    /// The reply is exactly `len` bytes. Reads never go past those bytes.
    pub fn fixed_len(len: usize, max_reply_bytes: usize) -> Result<Self, FramingError> {
        if len > max_reply_bytes {
            return Err(FramingError::FixedLenExceedsMax {
                len,
                max: max_reply_bytes,
            });
        }
        Ok(Framing {
            kind: Kind::Fixed(len),
            max_reply_bytes,
        })
    }

    /// The reply starts with a 4-byte big-endian length, followed by that many bytes.
    /// Reads never go past the frame.
    pub fn length_prefix(max_reply_bytes: usize) -> Self {
        Framing {
            kind: Kind::LengthPrefix4Be,
            max_reply_bytes,
        }
    }

    /// The reply size limit for this framing.
    pub fn max_reply_bytes(&self) -> usize {
        self.max_reply_bytes
    }
}

/// The typed result of one exchange.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The caller could not connect. Nothing was sent. The exchange never produces this
    /// variant; the applet layer does, and it shares this type.
    NotConnected,
    /// The request was (at least partly) written, then the stream ended, or a write
    /// failed, before any reply byte arrived.
    SentNoReply,
    /// The reply is complete. `reply` excludes any delimiter or length prefix.
    ReplyComplete { reply: Vec<u8> },
    /// The stream ended or the timeout passed before the reply was complete. `partial`
    /// holds the bytes received so far, including any length prefix.
    ReplyTruncated { partial: Vec<u8> },
    /// The request was sent and the timeout passed with no reply byte received.
    TimeoutAfterSend,
    /// The reply exceeded `limit`. This is never reported as a success.
    ReplyTooLarge { limit: usize },
    /// The delimiter arrived, followed by `trailing` more bytes. Never reported as a
    /// success.
    TrailingBytes { reply: Vec<u8>, trailing: usize },
}

impl Outcome {
    /// True only for `NotConnected`: no request byte can have reached the peer.
    pub fn may_retry_safely(&self) -> bool {
        matches!(self, Outcome::NotConnected)
    }

    /// Stable name for the applet's output.
    pub fn name(&self) -> &'static str {
        match self {
            Outcome::NotConnected => "not_connected",
            Outcome::SentNoReply => "sent_no_reply",
            Outcome::ReplyComplete { .. } => "reply_complete",
            Outcome::ReplyTruncated { .. } => "reply_truncated",
            Outcome::TimeoutAfterSend => "timeout_after_send",
            Outcome::ReplyTooLarge { .. } => "reply_too_large",
            Outcome::TrailingBytes { .. } => "reply_trailing_bytes",
        }
    }
}

/// Closes the write half of the stream after the request is sent, when the peer needs
/// end of input to reply.
pub type ShutdownWrite<'a, S> = &'a mut dyn FnMut(&mut S) -> io::Result<()>;

/// Runs one request/reply exchange on an already connected stream.
///
/// The whole `request` is written, handling short writes and `Interrupted`. Then
/// `shutdown_write`, if given, closes the write half. Then the reply is read according
/// to `framing`.
///
/// `timeout_ms` is measured from the start of the call on the caller's monotonic clock
/// `now_ms`. The stream's own read and write timeouts (`WouldBlock` or `TimedOut`
/// errors) count as the deadline passing.
pub fn exchange<S: Read + Write>(
    stream: &mut S,
    request: &[u8],
    framing: &Framing,
    timeout_ms: u64,
    now_ms: &mut dyn FnMut() -> u64,
    shutdown_write: Option<ShutdownWrite<'_, S>>,
) -> Outcome {
    let start = now_ms();
    let mut deadline = Deadline {
        now_ms,
        start,
        timeout_ms,
    };

    let mut written = 0;
    while written < request.len() {
        if deadline.passed() {
            return Outcome::TimeoutAfterSend;
        }
        match stream.write(&request[written..]) {
            Ok(0) => return Outcome::SentNoReply,
            Ok(n) if n <= request.len() - written => written += n,
            Ok(_) => return Outcome::SentNoReply,
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) if is_timeout(e.kind()) => return Outcome::TimeoutAfterSend,
            Err(_) => return Outcome::SentNoReply,
        }
    }

    if let Some(shutdown) = shutdown_write {
        if shutdown(stream).is_err() {
            return Outcome::SentNoReply;
        }
    }

    read_reply(stream, framing, &mut deadline)
}

struct Deadline<'a> {
    now_ms: &'a mut dyn FnMut() -> u64,
    start: u64,
    timeout_ms: u64,
}

impl Deadline<'_> {
    fn passed(&mut self) -> bool {
        (self.now_ms)().saturating_sub(self.start) >= self.timeout_ms
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stop {
    /// The peer closed the stream.
    Eof,
    /// The deadline passed, or the stream's own timeout fired.
    TimedOut,
    /// Any other read error.
    Failed,
}

fn is_timeout(kind: ErrorKind) -> bool {
    matches!(kind, ErrorKind::TimedOut | ErrorKind::WouldBlock)
}

fn incomplete(partial: Vec<u8>, stop: Stop) -> Outcome {
    match (partial.is_empty(), stop) {
        (true, Stop::TimedOut) => Outcome::TimeoutAfterSend,
        (true, _) => Outcome::SentNoReply,
        (false, _) => Outcome::ReplyTruncated { partial },
    }
}

/// Reads at most `want` bytes (and at most one chunk) and appends them to `buf`.
/// Retries `Interrupted`. Never appends more than `want` bytes.
fn read_some<S: Read>(
    stream: &mut S,
    buf: &mut Vec<u8>,
    want: usize,
    deadline: &mut Deadline<'_>,
) -> Result<(), Stop> {
    let want = want.min(CHUNK);
    let mut chunk = [0u8; CHUNK];
    loop {
        if deadline.passed() {
            return Err(Stop::TimedOut);
        }
        match stream.read(&mut chunk[..want]) {
            Ok(0) => return Err(Stop::Eof),
            Ok(n) if n <= want => {
                buf.extend_from_slice(&chunk[..n]);
                return Ok(());
            }
            Ok(_) => return Err(Stop::Failed),
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) if is_timeout(e.kind()) => return Err(Stop::TimedOut),
            Err(_) => return Err(Stop::Failed),
        }
    }
}

/// Reads until `buf` holds exactly `need` bytes. Never reads past `need`.
fn fill<S: Read>(
    stream: &mut S,
    buf: &mut Vec<u8>,
    need: usize,
    deadline: &mut Deadline<'_>,
) -> Result<(), Stop> {
    while buf.len() < need {
        let want = need - buf.len();
        read_some(stream, buf, want, deadline)?;
    }
    Ok(())
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn read_reply<S: Read>(stream: &mut S, framing: &Framing, deadline: &mut Deadline<'_>) -> Outcome {
    let max = framing.max_reply_bytes;
    let mut buf: Vec<u8> = Vec::new();

    match &framing.kind {
        Kind::UntilClose => loop {
            match read_some(stream, &mut buf, CHUNK, deadline) {
                Ok(()) => {
                    if buf.len() > max {
                        return Outcome::ReplyTooLarge { limit: max };
                    }
                }
                // End of stream with nothing received: a peer that closed without
                // answering, not an empty reply.
                Err(Stop::Eof) if buf.is_empty() => return Outcome::SentNoReply,
                Err(Stop::Eof) => return Outcome::ReplyComplete { reply: buf },
                Err(stop) => return incomplete(buf, stop),
            }
        },
        Kind::Delimiter(delimiter) => {
            // Bytes that cannot start a delimiter that is still incomplete at the end
            // of the buffer. Searching resumes there, so a long reply is scanned once.
            let tail = delimiter.len().saturating_sub(1);
            let mut from = 0;
            loop {
                if let Some(rel) = find(&buf[from..], delimiter) {
                    let at = from + rel;
                    if at > max {
                        return Outcome::ReplyTooLarge { limit: max };
                    }
                    let trailing = buf.len().saturating_sub(at + delimiter.len());
                    buf.truncate(at);
                    return if trailing == 0 {
                        Outcome::ReplyComplete { reply: buf }
                    } else {
                        Outcome::TrailingBytes {
                            reply: buf,
                            trailing,
                        }
                    };
                }
                // Whatever arrives later, the reply body is at least this long.
                from = buf.len().saturating_sub(tail);
                if from > max {
                    return Outcome::ReplyTooLarge { limit: max };
                }
                if let Err(stop) = read_some(stream, &mut buf, CHUNK, deadline) {
                    return incomplete(buf, stop);
                }
            }
        }
        Kind::Fixed(len) => match fill(stream, &mut buf, *len, deadline) {
            Ok(()) => Outcome::ReplyComplete { reply: buf },
            Err(stop) => incomplete(buf, stop),
        },
        Kind::LengthPrefix4Be => {
            if let Err(stop) = fill(stream, &mut buf, 4, deadline) {
                return incomplete(buf, stop);
            }
            let header = match buf.get(..4).and_then(|h| <[u8; 4]>::try_from(h).ok()) {
                Some(h) => h,
                None => return incomplete(buf, Stop::Failed),
            };
            let body_len = u32::from_be_bytes(header) as usize;
            if body_len > max {
                return Outcome::ReplyTooLarge { limit: max };
            }
            let need = match body_len.checked_add(4) {
                Some(n) => n,
                None => return Outcome::ReplyTooLarge { limit: max },
            };
            match fill(stream, &mut buf, need, deadline) {
                Ok(()) => Outcome::ReplyComplete {
                    reply: buf.split_off(4),
                },
                Err(stop) => incomplete(buf, stop),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::collections::VecDeque;

    enum Step {
        Data(Vec<u8>),
        Fail(ErrorKind),
    }

    /// In-memory stream. Reads follow `reads` in order, then report end of stream.
    /// Writes accept at most `write_chunk` bytes per call, fail with BrokenPipe once
    /// `write_fail_after` bytes are accepted, and return Interrupted `write_interrupts`
    /// times first.
    struct Fake {
        reads: VecDeque<Step>,
        written: Vec<u8>,
        write_chunk: usize,
        write_fail_after: Option<usize>,
        write_interrupts: usize,
    }

    impl Fake {
        fn new(reads: Vec<Step>) -> Self {
            Fake {
                reads: reads.into(),
                written: Vec::new(),
                write_chunk: usize::MAX,
                write_fail_after: None,
                write_interrupts: 0,
            }
        }
    }

    impl Read for Fake {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            match self.reads.pop_front() {
                None => Ok(0),
                Some(Step::Fail(kind)) => Err(io::Error::from(kind)),
                Some(Step::Data(mut data)) => {
                    let n = data.len().min(out.len());
                    out[..n].copy_from_slice(&data[..n]);
                    if n < data.len() {
                        let rest = data.split_off(n);
                        self.reads.push_front(Step::Data(rest));
                    }
                    Ok(n)
                }
            }
        }
    }

    impl Write for Fake {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if self.write_interrupts > 0 {
                self.write_interrupts -= 1;
                return Err(io::Error::from(ErrorKind::Interrupted));
            }
            let mut room = self.write_chunk.min(buf.len());
            if let Some(limit) = self.write_fail_after {
                let left = limit.saturating_sub(self.written.len());
                if left == 0 {
                    return Err(io::Error::from(ErrorKind::BrokenPipe));
                }
                room = room.min(left);
            }
            self.written.extend_from_slice(&buf[..room]);
            Ok(room)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn data(bytes: &[u8]) -> Step {
        Step::Data(bytes.to_vec())
    }

    /// A clock that never expires.
    fn frozen_clock() -> impl FnMut() -> u64 {
        || 0
    }

    /// A clock that reads 0 for the first `ok_calls` calls, then reads far past any
    /// test timeout.
    fn expire_after(ok_calls: u64) -> impl FnMut() -> u64 {
        let mut calls = 0;
        move || {
            calls += 1;
            if calls <= ok_calls {
                0
            } else {
                1_000
            }
        }
    }

    fn delim(max: usize) -> Framing {
        Framing::delimiter(b"^^", max).expect("non-empty delimiter")
    }

    #[test]
    fn delimiter_split_across_reads_inside_the_delimiter() {
        let mut fake = Fake::new(vec![data(b"ok^"), data(b"^")]);
        let mut clock = frozen_clock();
        let out = exchange(&mut fake, b"GET\n", &delim(1024), 500, &mut clock, None);
        assert_eq!(
            out,
            Outcome::ReplyComplete {
                reply: b"ok".to_vec()
            }
        );
        assert_eq!(fake.written, b"GET\n");
    }

    #[test]
    fn delimiter_one_byte_at_a_time() {
        let steps = b"hello^^".iter().map(|b| data(&[*b])).collect();
        let mut fake = Fake::new(steps);
        let mut clock = frozen_clock();
        let out = exchange(&mut fake, b"x", &delim(1024), 500, &mut clock, None);
        assert_eq!(
            out,
            Outcome::ReplyComplete {
                reply: b"hello".to_vec()
            }
        );
    }

    #[test]
    fn delimiter_only_is_a_complete_empty_reply() {
        let mut fake = Fake::new(vec![data(b"^^")]);
        let mut clock = frozen_clock();
        let out = exchange(&mut fake, b"x", &delim(1024), 500, &mut clock, None);
        assert_eq!(out, Outcome::ReplyComplete { reply: Vec::new() });
    }

    #[test]
    fn reply_cut_before_delimiter_is_truncated_never_complete() {
        let mut fake = Fake::new(vec![data(b"partial")]);
        let mut clock = frozen_clock();
        let out = exchange(&mut fake, b"x", &delim(1024), 500, &mut clock, None);
        assert_eq!(
            out,
            Outcome::ReplyTruncated {
                partial: b"partial".to_vec()
            }
        );
        assert!(!matches!(out, Outcome::ReplyComplete { .. }));
        assert!(!out.may_retry_safely());
    }

    #[test]
    fn eof_with_no_reply_bytes_is_sent_no_reply() {
        let mut fake = Fake::new(vec![]);
        let mut clock = frozen_clock();
        let out = exchange(&mut fake, b"x", &delim(1024), 500, &mut clock, None);
        assert_eq!(out, Outcome::SentNoReply);
        assert!(!out.may_retry_safely());
    }

    #[test]
    fn socket_timeout_with_partial_reply_is_truncated() {
        let mut fake = Fake::new(vec![data(b"par"), Step::Fail(ErrorKind::TimedOut)]);
        let mut clock = frozen_clock();
        let out = exchange(&mut fake, b"x", &delim(1024), 500, &mut clock, None);
        assert_eq!(
            out,
            Outcome::ReplyTruncated {
                partial: b"par".to_vec()
            }
        );
    }

    #[test]
    fn socket_timeout_before_any_reply_is_timeout_after_send() {
        let mut fake = Fake::new(vec![Step::Fail(ErrorKind::WouldBlock)]);
        let mut clock = frozen_clock();
        let out = exchange(&mut fake, b"x", &delim(1024), 500, &mut clock, None);
        assert_eq!(out, Outcome::TimeoutAfterSend);
        assert!(!out.may_retry_safely());
    }

    #[test]
    fn caller_clock_deadline_during_read_is_truncated() {
        // Calls: start (ok), write check (ok), first read check (ok), second read
        // check (expired). The first read delivers "par", then the deadline stops it.
        let mut fake = Fake::new(vec![data(b"par"), data(b"tial^^")]);
        let mut clock = expire_after(3);
        let out = exchange(&mut fake, b"x", &delim(1024), 500, &mut clock, None);
        assert_eq!(
            out,
            Outcome::ReplyTruncated {
                partial: b"par".to_vec()
            }
        );
    }

    #[test]
    fn delimiter_reply_exactly_at_limit_is_complete() {
        let mut fake = Fake::new(vec![data(b"abcde^^")]);
        let mut clock = frozen_clock();
        let out = exchange(&mut fake, b"x", &delim(5), 500, &mut clock, None);
        assert_eq!(
            out,
            Outcome::ReplyComplete {
                reply: b"abcde".to_vec()
            }
        );
    }

    #[test]
    fn delimiter_reply_one_byte_over_limit_is_too_large() {
        let mut fake = Fake::new(vec![data(b"abcdef^^")]);
        let mut clock = frozen_clock();
        let out = exchange(&mut fake, b"x", &delim(5), 500, &mut clock, None);
        assert_eq!(out, Outcome::ReplyTooLarge { limit: 5 });
        assert!(!out.may_retry_safely());
    }

    #[test]
    fn delimiter_reply_over_limit_split_across_reads_is_too_large() {
        let mut fake = Fake::new(vec![data(b"abcdef"), data(b"^^")]);
        let mut clock = frozen_clock();
        let out = exchange(&mut fake, b"x", &delim(5), 500, &mut clock, None);
        assert_eq!(out, Outcome::ReplyTooLarge { limit: 5 });
    }

    #[test]
    fn delimiter_reply_with_no_delimiter_over_limit_is_too_large() {
        // 7 bytes with no delimiter: the body is at least 7 - 1 = 6 bytes, over 5.
        let mut fake = Fake::new(vec![data(b"abcdefg")]);
        let mut clock = frozen_clock();
        let out = exchange(&mut fake, b"x", &delim(5), 500, &mut clock, None);
        assert_eq!(out, Outcome::ReplyTooLarge { limit: 5 });
    }

    #[test]
    fn delimiter_reply_at_limit_split_across_reads_is_complete() {
        let mut fake = Fake::new(vec![data(b"abcde"), data(b"^^")]);
        let mut clock = frozen_clock();
        let out = exchange(&mut fake, b"x", &delim(5), 500, &mut clock, None);
        assert_eq!(
            out,
            Outcome::ReplyComplete {
                reply: b"abcde".to_vec()
            }
        );
    }

    #[test]
    fn trailing_bytes_after_delimiter_are_an_error() {
        let mut fake = Fake::new(vec![data(b"ok^^extra")]);
        let mut clock = frozen_clock();
        let out = exchange(&mut fake, b"x", &delim(1024), 500, &mut clock, None);
        assert_eq!(
            out,
            Outcome::TrailingBytes {
                reply: b"ok".to_vec(),
                trailing: 5
            }
        );
        assert!(!out.may_retry_safely());
    }

    #[test]
    fn write_fails_halfway_is_sent_no_reply_and_not_retry_safe() {
        let mut fake = Fake::new(vec![data(b"^^")]);
        fake.write_chunk = 2;
        fake.write_fail_after = Some(3);
        let mut clock = frozen_clock();
        let out = exchange(&mut fake, b"hello", &delim(1024), 500, &mut clock, None);
        assert_eq!(out, Outcome::SentNoReply);
        assert!(!out.may_retry_safely());
        assert_eq!(fake.written, b"hel");
    }

    #[test]
    fn interrupted_writes_and_reads_are_retried() {
        let mut fake = Fake::new(vec![
            Step::Fail(ErrorKind::Interrupted),
            data(b"o"),
            Step::Fail(ErrorKind::Interrupted),
            data(b"k^^"),
        ]);
        fake.write_chunk = 1;
        fake.write_interrupts = 2;
        let mut clock = frozen_clock();
        let out = exchange(&mut fake, b"req", &delim(1024), 500, &mut clock, None);
        assert_eq!(
            out,
            Outcome::ReplyComplete {
                reply: b"ok".to_vec()
            }
        );
        assert_eq!(fake.written, b"req");
    }

    #[test]
    fn until_close_reads_to_end_of_stream() {
        let mut fake = Fake::new(vec![data(b"ab"), data(b"c")]);
        let mut clock = frozen_clock();
        let out = exchange(
            &mut fake,
            b"x",
            &Framing::until_close(16),
            500,
            &mut clock,
            None,
        );
        assert_eq!(
            out,
            Outcome::ReplyComplete {
                reply: b"abc".to_vec()
            }
        );
    }

    #[test]
    fn until_close_with_no_bytes_is_sent_no_reply() {
        let mut fake = Fake::new(vec![]);
        let mut clock = frozen_clock();
        let out = exchange(
            &mut fake,
            b"x",
            &Framing::until_close(16),
            500,
            &mut clock,
            None,
        );
        assert_eq!(out, Outcome::SentNoReply);
    }

    #[test]
    fn until_close_size_limit_is_exact() {
        let mut exact = Fake::new(vec![data(b"abcd")]);
        let mut clock = frozen_clock();
        let out = exchange(
            &mut exact,
            b"x",
            &Framing::until_close(4),
            500,
            &mut clock,
            None,
        );
        assert_eq!(
            out,
            Outcome::ReplyComplete {
                reply: b"abcd".to_vec()
            }
        );

        let mut over = Fake::new(vec![data(b"abcde")]);
        let out = exchange(
            &mut over,
            b"x",
            &Framing::until_close(4),
            500,
            &mut clock,
            None,
        );
        assert_eq!(out, Outcome::ReplyTooLarge { limit: 4 });
    }

    #[test]
    fn until_close_deadline_before_end_is_truncated() {
        let mut fake = Fake::new(vec![data(b"ab"), Step::Fail(ErrorKind::TimedOut)]);
        let mut clock = frozen_clock();
        let out = exchange(
            &mut fake,
            b"x",
            &Framing::until_close(16),
            500,
            &mut clock,
            None,
        );
        assert_eq!(
            out,
            Outcome::ReplyTruncated {
                partial: b"ab".to_vec()
            }
        );
    }

    #[test]
    fn fixed_len_reads_exact_count_and_never_over_reads() {
        let framing = Framing::fixed_len(4, 16).expect("len within limit");
        let mut fake = Fake::new(vec![data(b"abcdef")]);
        let mut clock = frozen_clock();
        let out = exchange(&mut fake, b"x", &framing, 500, &mut clock, None);
        assert_eq!(
            out,
            Outcome::ReplyComplete {
                reply: b"abcd".to_vec()
            }
        );
        match fake.reads.front() {
            Some(Step::Data(rest)) => assert_eq!(rest, b"ef"),
            _ => panic!("the bytes after the frame must stay unread"),
        }
    }

    #[test]
    fn fixed_len_short_reply_is_truncated() {
        let framing = Framing::fixed_len(4, 16).expect("len within limit");
        let mut fake = Fake::new(vec![data(b"ab")]);
        let mut clock = frozen_clock();
        let out = exchange(&mut fake, b"x", &framing, 500, &mut clock, None);
        assert_eq!(
            out,
            Outcome::ReplyTruncated {
                partial: b"ab".to_vec()
            }
        );
    }

    #[test]
    fn length_prefix_frames_the_body_across_reads() {
        let mut fake = Fake::new(vec![data(&[0, 0]), data(&[0, 3, b'a']), data(b"bc")]);
        let mut clock = frozen_clock();
        let out = exchange(
            &mut fake,
            b"x",
            &Framing::length_prefix(16),
            500,
            &mut clock,
            None,
        );
        assert_eq!(
            out,
            Outcome::ReplyComplete {
                reply: b"abc".to_vec()
            }
        );
    }

    #[test]
    fn length_prefix_at_limit_is_complete_and_over_limit_is_too_large() {
        let mut at = Fake::new(vec![data(&[0, 0, 0, 3, b'a', b'b', b'c'])]);
        let mut clock = frozen_clock();
        let out = exchange(
            &mut at,
            b"x",
            &Framing::length_prefix(3),
            500,
            &mut clock,
            None,
        );
        assert_eq!(
            out,
            Outcome::ReplyComplete {
                reply: b"abc".to_vec()
            }
        );

        let mut over = Fake::new(vec![data(&[0, 0, 0, 4]), data(b"abcd")]);
        let out = exchange(
            &mut over,
            b"x",
            &Framing::length_prefix(3),
            500,
            &mut clock,
            None,
        );
        assert_eq!(out, Outcome::ReplyTooLarge { limit: 3 });
        assert_eq!(over.reads.len(), 1, "an oversized body must not be read");
    }

    #[test]
    fn length_prefix_cut_inside_header_is_truncated() {
        let mut fake = Fake::new(vec![data(&[0, 0])]);
        let mut clock = frozen_clock();
        let out = exchange(
            &mut fake,
            b"x",
            &Framing::length_prefix(16),
            500,
            &mut clock,
            None,
        );
        assert_eq!(
            out,
            Outcome::ReplyTruncated {
                partial: vec![0, 0]
            }
        );
    }

    #[test]
    fn shutdown_runs_after_the_whole_request_is_written() {
        let mut fake = Fake::new(vec![data(b"ok^^")]);
        fake.write_chunk = 1;
        let seen = Cell::new(0usize);
        let mut shut = |s: &mut Fake| -> io::Result<()> {
            seen.set(s.written.len());
            Ok(())
        };
        let shutdown: &mut dyn FnMut(&mut Fake) -> io::Result<()> = &mut shut;
        let mut clock = frozen_clock();
        let out = exchange(
            &mut fake,
            b"abcd",
            &delim(1024),
            500,
            &mut clock,
            Some(shutdown),
        );
        assert_eq!(seen.get(), 4);
        assert_eq!(
            out,
            Outcome::ReplyComplete {
                reply: b"ok".to_vec()
            }
        );
    }

    #[test]
    fn failed_shutdown_is_sent_no_reply() {
        let mut fake = Fake::new(vec![data(b"ok^^")]);
        let mut shut =
            |_: &mut Fake| -> io::Result<()> { Err(io::Error::from(ErrorKind::NotConnected)) };
        let shutdown: &mut dyn FnMut(&mut Fake) -> io::Result<()> = &mut shut;
        let mut clock = frozen_clock();
        let out = exchange(
            &mut fake,
            b"x",
            &delim(1024),
            500,
            &mut clock,
            Some(shutdown),
        );
        assert_eq!(out, Outcome::SentNoReply);
        assert!(!out.may_retry_safely());
    }

    #[test]
    fn only_not_connected_may_be_retried() {
        let all = [
            Outcome::NotConnected,
            Outcome::SentNoReply,
            Outcome::ReplyComplete { reply: Vec::new() },
            Outcome::ReplyTruncated {
                partial: Vec::new(),
            },
            Outcome::TimeoutAfterSend,
            Outcome::ReplyTooLarge { limit: 1 },
            Outcome::TrailingBytes {
                reply: Vec::new(),
                trailing: 1,
            },
        ];
        for outcome in &all {
            assert_eq!(
                outcome.may_retry_safely(),
                *outcome == Outcome::NotConnected,
                "{}",
                outcome.name()
            );
        }
    }

    #[test]
    fn invalid_framings_are_rejected_before_any_byte_is_sent() {
        assert_eq!(
            Framing::delimiter(b"", 10),
            Err(FramingError::EmptyDelimiter)
        );
        assert_eq!(
            Framing::fixed_len(11, 10),
            Err(FramingError::FixedLenExceedsMax { len: 11, max: 10 })
        );
    }
}
