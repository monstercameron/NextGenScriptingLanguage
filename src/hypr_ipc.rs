//! Hyprland IPC: request encoding, typed replies and event lines (#316).
//!
//! This is pure protocol code over byte slices and generic `Read`, `Write` and
//! `BufRead` values. It never opens a socket; the Unix socket connection belongs
//! with the applets. Every name, size and separator is checked before a request
//! is encoded or a reply or event line is decoded.

use serde::de::DeserializeOwned;
use serde::Deserialize;
use std::fmt;
use std::io::{self, BufRead, Read, Write};
use std::path::PathBuf;

/// Largest encoded request, in bytes, including the `[[BATCH]]` prefix.
pub const MAX_REQUEST_BYTES: usize = 1 << 20;
/// Largest reply read to end of stream, in bytes.
pub const MAX_REPLY_BYTES: usize = 16 << 20;
/// Largest `NAME>>DATA` event line, in bytes, without its newline.
pub const MAX_EVENT_LINE_BYTES: usize = 64 << 10;

const BATCH_PREFIX: &str = "[[BATCH]]";
const MAX_WORD_BYTES: usize = 64;
const MAX_REJECT_TEXT_BYTES: usize = 512;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HyprIpcError {
    BadRuntimeDir(&'static str),
    BadSignature(&'static str),
    BadName(&'static str),
    BadArgument(&'static str),
    EmptyBatch,
    TooLarge { what: &'static str, limit: usize },
    Rejected(String),
    Json(String),
    MalformedEvent(&'static str),
    FieldCount { expected: usize, found: usize },
    EventStreamFailed,
    Io(String),
}

impl fmt::Display for HyprIpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadRuntimeDir(why) => write!(f, "runtime directory {why}"),
            Self::BadSignature(why) => write!(f, "Hyprland instance signature {why}"),
            Self::BadName(what) => write!(f, "{what} is not a plain name"),
            Self::BadArgument(why) => write!(f, "argument {why}"),
            Self::EmptyBatch => write!(f, "batch has no commands"),
            Self::TooLarge { what, limit } => write!(f, "{what} exceeds {limit} bytes"),
            Self::Rejected(text) => write!(f, "Hyprland rejected the request: {text}"),
            Self::Json(text) => write!(f, "invalid Hyprland JSON: {text}"),
            Self::MalformedEvent(why) => write!(f, "malformed Hyprland event: {why}"),
            Self::FieldCount { expected, found } => {
                write!(f, "expected {expected} event fields, found {found}")
            }
            Self::EventStreamFailed => write!(f, "event stream failed earlier and cannot resync"),
            Self::Io(text) => write!(f, "I/O error: {text}"),
        }
    }
}

impl std::error::Error for HyprIpcError {}

fn io_err(e: io::Error) -> HyprIpcError {
    HyprIpcError::Io(e.to_string())
}

/// Which of Hyprland's two per-instance sockets to address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    /// `.socket.sock`: request and reply.
    Request,
    /// `.socket2.sock`: the event stream.
    Events,
}

/// Resolves `<runtime_dir>/hypr/<signature>/<socket file>`.
///
/// The runtime directory must be an absolute path. The signature may only hold
/// ASCII letters, digits, `_`, `-` and `.`, and may not contain `..` or be `.`,
/// so no environment value can point the connection outside that directory.
pub fn socket_path(
    runtime_dir: &str,
    signature: &str,
    channel: Channel,
) -> Result<PathBuf, HyprIpcError> {
    if runtime_dir.is_empty() || !runtime_dir.starts_with('/') || runtime_dir.contains('\0') {
        return Err(HyprIpcError::BadRuntimeDir("must be an absolute path"));
    }
    if signature.is_empty() {
        return Err(HyprIpcError::BadSignature("is empty"));
    }
    if signature == "." || signature.contains("..") {
        return Err(HyprIpcError::BadSignature(
            "contains a relative path component",
        ));
    }
    if !signature
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.')
    {
        return Err(HyprIpcError::BadSignature(
            "has a character outside [A-Za-z0-9_.-]",
        ));
    }
    let file = match channel {
        Channel::Request => ".socket.sock",
        Channel::Events => ".socket2.sock",
    };
    Ok(PathBuf::from(runtime_dir)
        .join("hypr")
        .join(signature)
        .join(file))
}

/// One command for the command socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Request<'a> {
    /// `hyprctl <verb>`, or `j/<verb>` when `json` is set for JSON replies.
    Query { verb: &'a str, json: bool },
    /// `dispatch <dispatcher> <args...>`.
    Dispatch {
        dispatcher: &'a str,
        args: &'a [&'a str],
    },
}

fn check_word(word: &str, what: &'static str) -> Result<(), HyprIpcError> {
    if word.is_empty()
        || word.len() > MAX_WORD_BYTES
        || !word.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        return Err(HyprIpcError::BadName(what));
    }
    Ok(())
}

/// Renders one command. `batch` also refuses `;`, the batch separator.
fn render(request: &Request<'_>, batch: bool) -> Result<String, HyprIpcError> {
    match *request {
        Request::Query { verb, json } => {
            check_word(verb, "verb")?;
            Ok(if json {
                format!("j/{verb}")
            } else {
                verb.to_string()
            })
        }
        Request::Dispatch { dispatcher, args } => {
            check_word(dispatcher, "dispatcher")?;
            let mut out = format!("dispatch {dispatcher}");
            for arg in args {
                if arg.bytes().any(|b| b == b'\n' || b == b'\r' || b == 0) {
                    return Err(HyprIpcError::BadArgument("contains a newline or NUL"));
                }
                if batch && arg.contains(';') {
                    return Err(HyprIpcError::BadArgument("contains ';' inside a batch"));
                }
                out.push(' ');
                out.push_str(arg);
            }
            Ok(out)
        }
    }
}

fn sized(bytes: Vec<u8>) -> Result<Vec<u8>, HyprIpcError> {
    if bytes.len() > MAX_REQUEST_BYTES {
        return Err(HyprIpcError::TooLarge {
            what: "request",
            limit: MAX_REQUEST_BYTES,
        });
    }
    Ok(bytes)
}

/// Encodes one command. No terminator is added; the caller writes these bytes
/// and then reads the reply.
pub fn encode(request: &Request<'_>) -> Result<Vec<u8>, HyprIpcError> {
    sized(render(request, false)?.into_bytes())
}

/// Encodes `[[BATCH]]cmd1;cmd2;...` as one request.
pub fn encode_batch(requests: &[Request<'_>]) -> Result<Vec<u8>, HyprIpcError> {
    if requests.is_empty() {
        return Err(HyprIpcError::EmptyBatch);
    }
    let mut out = String::from(BATCH_PREFIX);
    for (index, request) in requests.iter().enumerate() {
        if index > 0 {
            out.push(';');
        }
        out.push_str(&render(request, true)?);
    }
    sized(out.into_bytes())
}

/// Writes a complete encoded request.
pub fn write_request<W: Write>(writer: &mut W, request: &[u8]) -> Result<(), HyprIpcError> {
    writer.write_all(request).map_err(io_err)?;
    writer.flush().map_err(io_err)
}

/// Reads the reply to end of stream. Fails if it exceeds `MAX_REPLY_BYTES`.
pub fn read_reply<R: Read>(reader: &mut R) -> Result<Vec<u8>, HyprIpcError> {
    let mut buf = Vec::new();
    reader
        .by_ref()
        .take(MAX_REPLY_BYTES as u64 + 1)
        .read_to_end(&mut buf)
        .map_err(io_err)?;
    if buf.len() > MAX_REPLY_BYTES {
        return Err(HyprIpcError::TooLarge {
            what: "reply",
            limit: MAX_REPLY_BYTES,
        });
    }
    Ok(buf)
}

/// Accepts the plain `ok` reply (with an optional trailing newline) that a
/// dispatch or keyword command returns. Anything else is a rejection, and its
/// text is kept, bounded, in the error.
pub fn parse_ok_reply(reply: &[u8]) -> Result<(), HyprIpcError> {
    let trimmed = reply.strip_suffix(b"\n").unwrap_or(reply);
    if trimmed == b"ok" {
        return Ok(());
    }
    let shown = reply.get(..MAX_REJECT_TEXT_BYTES).unwrap_or(reply);
    Err(HyprIpcError::Rejected(
        String::from_utf8_lossy(shown).trim().to_string(),
    ))
}

fn parse_json<T: DeserializeOwned>(reply: &[u8]) -> Result<T, HyprIpcError> {
    serde_json::from_slice(reply).map_err(|e| HyprIpcError::Json(e.to_string()))
}

/// A workspace reference inside monitor and client records.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct WorkspaceRef {
    pub id: i64,
    pub name: String,
}

/// One entry of `j/monitors`. Unknown fields are kept in `rest`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Monitor {
    pub id: i64,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub x: i64,
    pub y: i64,
    pub scale: f64,
    pub refresh_rate: f64,
    pub focused: bool,
    pub active_workspace: WorkspaceRef,
    #[serde(flatten)]
    pub rest: serde_json::Map<String, serde_json::Value>,
}

/// One entry of `j/workspaces`. Unknown fields are kept in `rest`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Workspace {
    pub id: i64,
    pub name: String,
    pub monitor: String,
    #[serde(rename = "monitorID")]
    pub monitor_id: Option<i64>,
    pub windows: u32,
    #[serde(flatten)]
    pub rest: serde_json::Map<String, serde_json::Value>,
}

/// One window from `j/clients`, or the object `j/activewindow` returns.
/// Unknown fields are kept in `rest`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Client {
    pub address: String,
    pub class: String,
    pub initial_class: String,
    pub title: String,
    #[serde(rename = "focusHistoryID")]
    pub focus_history_id: i64,
    pub workspace: WorkspaceRef,
    pub monitor: Option<i64>,
    pub pid: Option<i64>,
    pub mapped: Option<bool>,
    pub hidden: Option<bool>,
    pub floating: Option<bool>,
    pub focused: Option<bool>,
    pub at: Option<(i64, i64)>,
    pub size: Option<(i64, i64)>,
    #[serde(flatten)]
    pub rest: serde_json::Map<String, serde_json::Value>,
}

/// Parses the reply to `j/monitors`.
pub fn parse_monitors(reply: &[u8]) -> Result<Vec<Monitor>, HyprIpcError> {
    parse_json(reply)
}

/// Parses the reply to `j/workspaces`.
pub fn parse_workspaces(reply: &[u8]) -> Result<Vec<Workspace>, HyprIpcError> {
    parse_json(reply)
}

/// Parses the reply to `j/clients`.
pub fn parse_clients(reply: &[u8]) -> Result<Vec<Client>, HyprIpcError> {
    parse_json(reply)
}

/// Parses the reply to `j/activewindow`. Hyprland answers `{}` when no window
/// has focus, which decodes to `None`.
pub fn parse_activewindow(reply: &[u8]) -> Result<Option<Client>, HyprIpcError> {
    let value: serde_json::Value = parse_json(reply)?;
    let is_empty_object = matches!(&value, serde_json::Value::Object(map) if map.is_empty());
    if is_empty_object {
        return Ok(None);
    }
    serde_json::from_value(value)
        .map(Some)
        .map_err(|e| HyprIpcError::Json(e.to_string()))
}

/// One `NAME>>DATA` line from socket2. `data` is the raw text after the first
/// `>>`, so it may itself contain `>>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    pub name: String,
    pub data: String,
}

impl Event {
    /// Splits `data` into exactly `count` fields on `,`. The last field keeps
    /// any further commas, so a window title stays intact.
    pub fn fields(&self, count: usize) -> Result<Vec<&str>, HyprIpcError> {
        split_fields(&self.data, count)
    }
}

/// Splits `data` into exactly `count` fields on `,`, the last one unsplit.
pub fn split_fields(data: &str, count: usize) -> Result<Vec<&str>, HyprIpcError> {
    if count == 0 {
        return Err(HyprIpcError::BadArgument("field count must be at least 1"));
    }
    let parts: Vec<&str> = data.splitn(count, ',').collect();
    if parts.len() != count {
        return Err(HyprIpcError::FieldCount {
            expected: count,
            found: parts.len(),
        });
    }
    Ok(parts)
}

/// Parses one line without its newline. A malformed line is recoverable: the
/// caller may skip it and keep reading.
pub fn parse_event_line(line: &str) -> Result<Event, HyprIpcError> {
    let (name, data) = line
        .split_once(">>")
        .ok_or(HyprIpcError::MalformedEvent("no '>>' separator"))?;
    if name.is_empty()
        || name.len() > MAX_WORD_BYTES
        || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        return Err(HyprIpcError::MalformedEvent(
            "event name is not a plain word",
        ));
    }
    Ok(Event {
        name: name.to_string(),
        data: data.to_string(),
    })
}

enum LineEnd {
    Newline,
    Eof,
}

/// Appends one line to `line` without its newline, reading across as many
/// buffer fills as needed. Fails once the line would exceed `limit`, and
/// stores nothing past that point.
fn read_line_bounded<R: BufRead>(
    reader: &mut R,
    line: &mut Vec<u8>,
    limit: usize,
) -> Result<LineEnd, HyprIpcError> {
    loop {
        let (consumed, end) = {
            let available = match reader.fill_buf() {
                Ok(bytes) => bytes,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(io_err(e)),
            };
            if available.is_empty() {
                return Ok(LineEnd::Eof);
            }
            let (take, newline) = match available.iter().position(|&b| b == b'\n') {
                Some(index) => (index, true),
                None => (available.len(), false),
            };
            let chunk = available.get(..take).unwrap_or(available);
            if line.len() + chunk.len() > limit {
                return Err(HyprIpcError::TooLarge {
                    what: "event line",
                    limit,
                });
            }
            line.extend_from_slice(chunk);
            (take + usize::from(newline), newline)
        };
        reader.consume(consumed);
        if end {
            return Ok(LineEnd::Newline);
        }
    }
}

/// Reads socket2 events from a buffered reader.
///
/// A malformed line is returned as an error and the reader stays usable. An
/// over-long line or a read error is fatal: the reader cannot resync, so later
/// calls return `EventStreamFailed`.
pub struct EventReader<R: BufRead> {
    inner: R,
    limit: usize,
    failed: bool,
}

impl<R: BufRead> EventReader<R> {
    pub fn new(inner: R) -> Self {
        Self::with_limit(inner, MAX_EVENT_LINE_BYTES)
    }

    pub fn with_limit(inner: R, limit: usize) -> Self {
        Self {
            inner,
            limit,
            failed: false,
        }
    }

    /// Returns the next event, or `Ok(None)` at end of stream.
    pub fn next_event(&mut self) -> Result<Option<Event>, HyprIpcError> {
        if self.failed {
            return Err(HyprIpcError::EventStreamFailed);
        }
        let mut line = Vec::new();
        let end = match read_line_bounded(&mut self.inner, &mut line, self.limit) {
            Ok(end) => end,
            Err(e) => {
                self.failed = true;
                return Err(e);
            }
        };
        if matches!(end, LineEnd::Eof) && line.is_empty() {
            return Ok(None);
        }
        let text = String::from_utf8_lossy(&line).into_owned();
        let text = text.strip_suffix('\r').unwrap_or(text.as_str());
        parse_event_line(text).map(Some)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    const MONITORS: &str = r#"[{"id":0,"name":"DP-1","description":"Dell U2720Q","make":"Dell",
        "width":3840,"height":2160,"refreshRate":59.997,"x":0,"y":0,
        "activeWorkspace":{"id":1,"name":"1"},"specialWorkspace":{"id":0,"name":""},
        "reserved":[0,0,0,0],"scale":1.5,"transform":0,"focused":true,"dpmsStatus":true,
        "vrr":false,"futureField":{"nested":[1,2,{"x":null}]}}]"#;

    const CLIENTS: &str = r#"[{"address":"0x55d5c0a1b2c0","mapped":true,"hidden":false,
        "at":[10,20],"size":[800,600],"workspace":{"id":2,"name":"2"},"floating":false,
        "monitor":0,"class":"firefox","title":"Mail, Calendar, and more - Mozilla Firefox",
        "initialClass":"firefox","initialTitle":"Mozilla Firefox","pid":4242,
        "xwayland":false,"pinned":false,"fullscreen":0,"focusHistoryID":0,
        "inhibitingIdle":false,"tags":[],"grouped":[],"newUnknown":"kept"}]"#;

    const WORKSPACES: &str = r#"[{"id":1,"name":"1","monitor":"DP-1","monitorID":0,
        "windows":2,"hasfullscreen":false,"lastwindow":"0x55d5c0a1b2c0",
        "lastwindowtitle":"Mail","ispersistent":false,"tiledLayout":"dwindle"}]"#;

    fn events(input: &[u8]) -> EventReader<Cursor<&[u8]>> {
        EventReader::new(Cursor::new(input))
    }

    // Socket paths.

    #[test]
    fn socket_paths_resolve_under_the_instance_directory() {
        let request = socket_path("/run/user/1000", "sig_1", Channel::Request).unwrap();
        let events = socket_path("/run/user/1000", "sig_1", Channel::Events).unwrap();
        assert_eq!(
            request,
            PathBuf::from("/run/user/1000")
                .join("hypr")
                .join("sig_1")
                .join(".socket.sock")
        );
        assert_eq!(
            events,
            PathBuf::from("/run/user/1000")
                .join("hypr")
                .join("sig_1")
                .join(".socket2.sock")
        );
    }

    #[test]
    fn socket_path_refuses_hostile_signatures() {
        for bad in [
            "", ".", "..", "a/../b", "../etc", "x/y", "a\\b", "a\0b", "a b", "a;b", "sig\n",
        ] {
            assert!(
                socket_path("/run/user/1000", bad, Channel::Request).is_err(),
                "accepted signature {bad:?}"
            );
        }
    }

    #[test]
    fn socket_path_refuses_relative_or_empty_runtime_dir() {
        for bad in ["", "run/user/1000", "relative", "/run\0/x"] {
            assert!(
                socket_path(bad, "sig", Channel::Request).is_err(),
                "accepted runtime dir {bad:?}"
            );
        }
    }

    // Request encoding.

    #[test]
    fn json_query_uses_the_j_prefix() {
        let bytes = encode(&Request::Query {
            verb: "monitors",
            json: true,
        })
        .unwrap();
        assert_eq!(bytes, b"j/monitors");
        let plain = encode(&Request::Query {
            verb: "configerrors",
            json: false,
        })
        .unwrap();
        assert_eq!(plain, b"configerrors");
    }

    #[test]
    fn query_verb_must_be_a_plain_word() {
        for bad in ["", "monitors\ndispatch exec x", "a;b", "a b", "../x", "a\0"] {
            assert!(
                encode(&Request::Query {
                    verb: bad,
                    json: true
                })
                .is_err(),
                "accepted verb {bad:?}"
            );
        }
    }

    #[test]
    fn dispatch_encodes_name_and_arguments() {
        let bytes = encode(&Request::Dispatch {
            dispatcher: "workspace",
            args: &["2"],
        })
        .unwrap();
        assert_eq!(bytes, b"dispatch workspace 2");
        let bare = encode(&Request::Dispatch {
            dispatcher: "togglefloating",
            args: &[],
        })
        .unwrap();
        assert_eq!(bare, b"dispatch togglefloating");
    }

    #[test]
    fn dispatch_refuses_a_hostile_dispatcher_name() {
        for bad in ["", "exec;reboot", "focus window", "a\nb", "x,y", "../x"] {
            assert!(
                encode(&Request::Dispatch {
                    dispatcher: bad,
                    args: &[]
                })
                .is_err(),
                "accepted dispatcher {bad:?}"
            );
        }
    }

    #[test]
    fn dispatch_refuses_newline_and_nul_in_arguments() {
        for bad in ["2\nkill", "2\rx", "2\0x"] {
            assert!(
                encode(&Request::Dispatch {
                    dispatcher: "exec",
                    args: &[bad]
                })
                .is_err(),
                "accepted argument {bad:?}"
            );
        }
    }

    #[test]
    fn single_dispatch_allows_semicolon_but_batch_does_not() {
        let single = encode(&Request::Dispatch {
            dispatcher: "exec",
            args: &["echo a;echo b"],
        });
        assert!(single.is_ok());
        let batch = encode_batch(&[Request::Dispatch {
            dispatcher: "exec",
            args: &["echo a;dispatch exit"],
        }]);
        assert_eq!(
            batch,
            Err(HyprIpcError::BadArgument("contains ';' inside a batch"))
        );
    }

    #[test]
    fn batch_joins_commands_with_semicolons() {
        let bytes = encode_batch(&[
            Request::Query {
                verb: "clients",
                json: true,
            },
            Request::Dispatch {
                dispatcher: "focuswindow",
                args: &["address:0x55d5c0a1b2c0"],
            },
        ])
        .unwrap();
        assert_eq!(
            bytes,
            b"[[BATCH]]j/clients;dispatch focuswindow address:0x55d5c0a1b2c0".to_vec()
        );
    }

    #[test]
    fn empty_batch_is_refused() {
        assert_eq!(encode_batch(&[]), Err(HyprIpcError::EmptyBatch));
    }

    #[test]
    fn oversized_request_is_refused() {
        let big = "x".repeat(MAX_REQUEST_BYTES);
        let result = encode(&Request::Dispatch {
            dispatcher: "exec",
            args: &[big.as_str()],
        });
        assert_eq!(
            result,
            Err(HyprIpcError::TooLarge {
                what: "request",
                limit: MAX_REQUEST_BYTES
            })
        );
    }

    #[test]
    fn write_request_writes_the_bytes_unchanged() {
        let mut out = Vec::new();
        write_request(&mut out, b"j/monitors").unwrap();
        assert_eq!(out, b"j/monitors");
    }

    // Reply handling.

    #[test]
    fn read_reply_reads_to_end_of_stream() {
        let mut stream = Cursor::new(b"[]".to_vec());
        assert_eq!(read_reply(&mut stream).unwrap(), b"[]");
    }

    #[test]
    fn read_reply_stops_at_the_size_bound() {
        let mut endless = io::repeat(b'a');
        assert_eq!(
            read_reply(&mut endless),
            Err(HyprIpcError::TooLarge {
                what: "reply",
                limit: MAX_REPLY_BYTES
            })
        );
    }

    #[test]
    fn ok_reply_accepts_plain_ok_only() {
        assert_eq!(parse_ok_reply(b"ok"), Ok(()));
        assert_eq!(parse_ok_reply(b"ok\n"), Ok(()));
        let cases: [&[u8]; 5] = [b"", b"okay", b"ok\nextra", b"ok ", b"Invalid dispatcher"];
        for bad in cases {
            assert!(
                matches!(parse_ok_reply(bad), Err(HyprIpcError::Rejected(_))),
                "accepted reply {bad:?}"
            );
        }
    }

    #[test]
    fn rejection_text_is_bounded_and_lossy() {
        let long = vec![b'z'; MAX_REJECT_TEXT_BYTES * 4];
        match parse_ok_reply(&long) {
            Err(HyprIpcError::Rejected(text)) => assert_eq!(text.len(), MAX_REJECT_TEXT_BYTES),
            other => panic!("unexpected {other:?}"),
        }
        match parse_ok_reply(&[0xff, b'x']) {
            Err(HyprIpcError::Rejected(text)) => assert!(text.contains('x')),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn monitors_decode_and_keep_unknown_fields() {
        let monitors = parse_monitors(MONITORS.as_bytes()).unwrap();
        assert_eq!(monitors.len(), 1);
        let m = &monitors[0];
        assert_eq!(m.name, "DP-1");
        assert_eq!((m.width, m.height), (3840, 2160));
        assert_eq!(m.scale, 1.5);
        assert_eq!(m.refresh_rate, 59.997);
        assert!(m.focused);
        assert_eq!(m.active_workspace.id, 1);
        assert!(m.rest.contains_key("futureField"));
        assert!(m.rest.contains_key("description"));
        assert!(!m.rest.contains_key("name"));
    }

    #[test]
    fn clients_decode_and_keep_titles_with_commas() {
        let clients = parse_clients(CLIENTS.as_bytes()).unwrap();
        let c = &clients[0];
        assert_eq!(c.address, "0x55d5c0a1b2c0");
        assert_eq!(c.class, "firefox");
        assert_eq!(c.initial_class, "firefox");
        assert_eq!(c.title, "Mail, Calendar, and more - Mozilla Firefox");
        assert_eq!(c.focus_history_id, 0);
        assert_eq!(c.workspace.name, "2");
        assert_eq!(c.at, Some((10, 20)));
        assert_eq!(c.size, Some((800, 600)));
        assert_eq!(
            c.rest.get("newUnknown").and_then(|v| v.as_str()),
            Some("kept")
        );
    }

    #[test]
    fn workspaces_decode_with_monitor_id_rename() {
        let workspaces = parse_workspaces(WORKSPACES.as_bytes()).unwrap();
        let w = &workspaces[0];
        assert_eq!(w.id, 1);
        assert_eq!(w.monitor, "DP-1");
        assert_eq!(w.monitor_id, Some(0));
        assert_eq!(w.windows, 2);
        assert!(w.rest.contains_key("tiledLayout"));
    }

    #[test]
    fn activewindow_empty_object_means_no_focus() {
        assert_eq!(parse_activewindow(b"{}"), Ok(None));
        assert_eq!(parse_activewindow(b" {} \n"), Ok(None));
    }

    #[test]
    fn activewindow_decodes_one_client_object() {
        let object = r#"{"address":"0x1","class":"kitty","initialClass":"kitty","title":"a, b","focusHistoryID":0,"workspace":{"id":1,"name":"1"},"fullscreen":0}"#;
        let window = parse_activewindow(object.as_bytes()).unwrap().unwrap();
        assert_eq!(window.class, "kitty");
        assert_eq!(window.title, "a, b");
    }

    #[test]
    fn missing_or_mistyped_fields_are_errors_not_panics() {
        let missing_name = r#"[{"id":0,"width":1,"height":1,"x":0,"y":0,"scale":1.0,"refreshRate":60.0,"focused":true,"activeWorkspace":{"id":1,"name":"1"}}]"#;
        assert!(matches!(
            parse_monitors(missing_name.as_bytes()),
            Err(HyprIpcError::Json(_))
        ));
        let string_id = r#"[{"id":"x","name":"1","monitor":"DP-1","windows":0}]"#;
        assert!(matches!(
            parse_workspaces(string_id.as_bytes()),
            Err(HyprIpcError::Json(_))
        ));
        assert!(parse_clients(b"{}").is_err());
        assert!(parse_monitors(b"null").is_err());
        assert!(parse_activewindow(b"[]").is_err());
    }

    #[test]
    fn truncated_and_deeply_nested_json_are_errors() {
        assert!(parse_clients(b"[{\"address\":").is_err());
        let deep = "[".repeat(10_000);
        assert!(matches!(
            parse_clients(deep.as_bytes()),
            Err(HyprIpcError::Json(_))
        ));
        assert!(parse_monitors(b"").is_err());
        assert!(parse_monitors(&[0xff, 0xfe, 0x00]).is_err());
    }

    // Event lines.

    #[test]
    fn events_arrive_as_name_and_raw_data() {
        let mut reader = events(b"monitoradded>>DP-1\nworkspace>>3\nconfigreloaded>>\n");
        let first = reader.next_event().unwrap().unwrap();
        assert_eq!(
            first,
            Event {
                name: "monitoradded".into(),
                data: "DP-1".into()
            }
        );
        let second = reader.next_event().unwrap().unwrap();
        assert_eq!(second.data, "3");
        let third = reader.next_event().unwrap().unwrap();
        assert_eq!(
            third,
            Event {
                name: "configreloaded".into(),
                data: String::new()
            }
        );
        assert_eq!(reader.next_event().unwrap(), None);
    }

    #[test]
    fn data_keeps_later_separators_and_commas() {
        let event = parse_event_line("openwindow>>0x1,1,kitty,a>>b, c").unwrap();
        assert_eq!(event.name, "openwindow");
        assert_eq!(event.data, "0x1,1,kitty,a>>b, c");
    }

    #[test]
    fn a_line_split_across_reads_is_reassembled() {
        let bytes = b"activewindow>>kitty,a, title, here\nworkspace>>2\n";
        let slow = io::BufReader::with_capacity(1, Cursor::new(&bytes[..]));
        let mut reader = EventReader::new(slow);
        let first = reader.next_event().unwrap().unwrap();
        assert_eq!(first.data, "kitty,a, title, here");
        assert_eq!(reader.next_event().unwrap().unwrap().data, "2");
        assert_eq!(reader.next_event().unwrap(), None);
    }

    #[test]
    fn crlf_and_missing_final_newline_are_handled() {
        let mut reader = events(b"workspace>>1\r\nworkspacev2>>2,name");
        assert_eq!(reader.next_event().unwrap().unwrap().data, "1");
        assert_eq!(reader.next_event().unwrap().unwrap().data, "2,name");
        assert_eq!(reader.next_event().unwrap(), None);
    }

    #[test]
    fn malformed_lines_are_recoverable() {
        let mut reader = events(b"nodelimiter\n>>empty\nbad name>>x\n\nworkspace>>7\n");
        for _ in 0..4 {
            assert!(matches!(
                reader.next_event(),
                Err(HyprIpcError::MalformedEvent(_))
            ));
        }
        assert_eq!(reader.next_event().unwrap().unwrap().data, "7");
    }

    #[test]
    fn invalid_utf8_in_a_name_is_malformed_not_a_panic() {
        let mut reader = events(b"\xff\xfe>>x\nworkspace>>1\n");
        assert!(matches!(
            reader.next_event(),
            Err(HyprIpcError::MalformedEvent(_))
        ));
        assert_eq!(reader.next_event().unwrap().unwrap().data, "1");
    }

    #[test]
    fn overlong_line_is_fatal_and_bounded() {
        let mut reader = EventReader::with_limit(io::BufReader::new(io::repeat(b'a')), 1024);
        assert_eq!(
            reader.next_event(),
            Err(HyprIpcError::TooLarge {
                what: "event line",
                limit: 1024
            })
        );
        assert_eq!(reader.next_event(), Err(HyprIpcError::EventStreamFailed));
    }

    #[test]
    fn line_exactly_at_the_limit_is_accepted() {
        let mut line = b"activewindow>>".to_vec();
        line.extend(vec![b'b'; 16]);
        line.push(b'\n');
        let mut reader = EventReader::with_limit(Cursor::new(line), 14 + 16);
        assert_eq!(reader.next_event().unwrap().unwrap().data.len(), 16);
    }

    #[test]
    fn default_limit_matches_the_constant() {
        let mut huge = b"activewindow>>".to_vec();
        huge.extend(vec![b'c'; MAX_EVENT_LINE_BYTES]);
        huge.push(b'\n');
        let mut reader = EventReader::new(Cursor::new(huge));
        assert!(matches!(
            reader.next_event(),
            Err(HyprIpcError::TooLarge { .. })
        ));
    }

    #[test]
    fn split_fields_keeps_the_last_field_intact() {
        let event = parse_event_line("activewindow>>Firefox,Mail, Calendar, x").unwrap();
        assert_eq!(
            event.fields(2).unwrap(),
            vec!["Firefox", "Mail, Calendar, x"]
        );
        assert_eq!(event.fields(1).unwrap(), vec!["Firefox,Mail, Calendar, x"]);
    }

    #[test]
    fn split_fields_refuses_wrong_counts() {
        let event = parse_event_line("activewindow>>only").unwrap();
        assert_eq!(
            event.fields(2),
            Err(HyprIpcError::FieldCount {
                expected: 2,
                found: 1
            })
        );
        assert_eq!(
            split_fields("a,b", 0),
            Err(HyprIpcError::BadArgument("field count must be at least 1"))
        );
        assert_eq!(split_fields(",", 2).unwrap(), vec!["", ""]);
    }
}
