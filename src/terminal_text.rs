//! Terminal-safe rendering of text that can carry untrusted bytes: file names, applet
//! output and error messages. Control characters, including ESC (which starts terminal
//! sequences), CSI, OSC and the C1 range, become visible `\u{..}` escapes. Newlines and
//! tabs stay in rendered messages so intentional layout keeps its shape.

use std::fmt::Write;

/// Escape control characters, except newline and tab, as visible `\u{..}` sequences.
pub fn escape_controls(text: &str) -> String {
    escape(text, true)
}

/// Escape controls and line separators in one untrusted field, including newlines and tabs, so
/// the field cannot inject terminal rows or columns into surrounding output.
pub fn escape_terminal_field(text: &str) -> String {
    escape(text, false)
}

/// Make serialized JSON safe to view in a terminal: terminal-unsafe characters
/// inside JSON strings (C1 controls, bidi overrides/isolates, and the other
/// characters [`escape_controls`] escapes) become JSON `\uXXXX` escapes.
/// The JSON-decoded value is unchanged, so machine readers see the original
/// text while a person viewing the emitted bytes sees no raw controls.
/// Characters outside strings (structural punctuation and pretty-print
/// whitespace) pass through untouched.
pub fn escape_json_for_terminal(serialized: &str) -> String {
    let mut out = String::with_capacity(serialized.len());
    let mut in_string = false;
    let mut escaped = false;
    for character in serialized.chars() {
        if in_string && !escaped && is_unsafe_terminal_character(character) {
            write_json_unicode_escape(&mut out, character);
            continue;
        }
        out.push(character);
        if in_string {
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                in_string = false;
            }
        } else if character == '"' {
            in_string = true;
        }
    }
    out
}

fn write_json_unicode_escape(out: &mut String, character: char) {
    let code = u32::from(character);
    if code <= 0xffff {
        let _ = write!(out, "\\u{code:04x}");
    } else {
        let shifted = code - 0x1_0000;
        let high = 0xd800 + (shifted >> 10);
        let low = 0xdc00 + (shifted & 0x3ff);
        let _ = write!(out, "\\u{high:04x}\\u{low:04x}");
    }
}

/// Whether pipeline output is bound for a terminal: the actual stdout sink,
/// not the content source. Model text becomes an ordinary pipeline value, so
/// the sink cannot tell model bytes apart later; every stdout sink below
/// reads this one decision. Pipes and files keep exact bytes; TTYs get
/// escaped text.
pub fn stdout_is_terminal() -> bool {
    std::io::IsTerminal::is_terminal(&std::io::stdout())
}

/// Encode pipeline bytes for the stdout sink. Pipes and files keep the exact
/// bytes (H17 pipe guarantee); a TTY sink gets [`escape_controls`] over the
/// text so OSC/CSI/C1 escapes and bidi overrides in untrusted model output
/// cannot rewrite the terminal. Invalid UTF-8 renders lossily on a TTY: it
/// cannot display as text there, while pipes still carry the original bytes.
/// The `stdout_is_terminal` flag is a parameter (rather than read here) so the
/// sink decision stays testable without a real TTY.
pub fn encode_pipeline_for_stdout(bytes: &[u8], stdout_is_terminal: bool) -> Vec<u8> {
    if !stdout_is_terminal {
        return bytes.to_vec();
    }
    escape_controls(&String::from_utf8_lossy(bytes)).into_bytes()
}

/// Incremental form of [`encode_pipeline_for_stdout`] for a TTY sink that receives its bytes in
/// chunks as a child writes them. Feeding every chunk through [`push`](Self::push) and then
/// calling [`finish`](Self::finish) yields exactly the bytes the one-shot encoding produces for
/// the concatenated input, wherever the chunk boundaries fall.
///
/// Escaping is per character, so the only state carried between chunks is an incomplete UTF-8
/// sequence at the end of a chunk, at most three bytes. An escape sequence split across chunks
/// needs no state: its ESC or C1 character is escaped on its own and the rest of the sequence
/// passes through as text, the same as in the one-shot path.
#[derive(Debug, Default)]
pub struct TerminalEscaper {
    pending: Vec<u8>,
}

impl TerminalEscaper {
    /// Escape the complete characters in `chunk` into `out`, keeping an incomplete trailing
    /// sequence for the next chunk.
    pub fn push(&mut self, chunk: &[u8], out: &mut Vec<u8>) {
        if self.pending.is_empty() {
            let rest = escape_utf8_prefix(chunk, out);
            self.pending.extend_from_slice(rest);
        } else {
            self.pending.extend_from_slice(chunk);
            let joined = std::mem::take(&mut self.pending);
            let rest = escape_utf8_prefix(&joined, out);
            self.pending.extend_from_slice(rest);
        }
    }

    /// End the stream. An incomplete sequence left at end of input renders as one U+FFFD, as
    /// [`String::from_utf8_lossy`] renders it.
    pub fn finish(&mut self, out: &mut Vec<u8>) {
        if !self.pending.is_empty() {
            out.extend_from_slice("\u{fffd}".as_bytes());
            self.pending.clear();
        }
    }
}

/// Escape the complete characters of `bytes` into `out`, replacing invalid sequences with
/// U+FFFD exactly as [`String::from_utf8_lossy`] does. Returns the incomplete sequence at the
/// end of `bytes`, if any, so the caller can prepend it to the next chunk.
fn escape_utf8_prefix<'a>(mut bytes: &'a [u8], out: &mut Vec<u8>) -> &'a [u8] {
    loop {
        match std::str::from_utf8(bytes) {
            Ok(text) => {
                out.extend_from_slice(escape_controls(text).as_bytes());
                return &[];
            }
            Err(error) => {
                let (valid, rest) = bytes.split_at(error.valid_up_to());
                out.extend_from_slice(escape_controls(&String::from_utf8_lossy(valid)).as_bytes());
                match error.error_len() {
                    Some(invalid) => {
                        out.extend_from_slice("\u{fffd}".as_bytes());
                        bytes = rest.get(invalid..).unwrap_or(&[]);
                    }
                    None => return rest,
                }
            }
        }
    }
}

fn escape(text: &str, preserve_layout: bool) -> String {
    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        if (preserve_layout && (character == '\n' || character == '\t'))
            || !is_unsafe_terminal_character(character)
        {
            out.push(character);
        } else {
            let _ = write!(out, "\\u{{{:x}}}", u32::from(character));
        }
    }
    out
}

fn is_unsafe_terminal_character(character: char) -> bool {
    character.is_control()
        || matches!(
            u32::from(character),
            0x00ad
                | 0x034f
                | 0x061c
                | 0x115f..=0x1160
                | 0x17b4..=0x17b5
                | 0x180b..=0x180f
                // Zero-width space and the LRM/RLM marks. ZWNJ and ZWJ (U+200C, U+200D)
                // and the variation selectors stay: Persian, Indic and emoji text need them.
                | 0x200b
                | 0x200e..=0x200f
                | 0x2028..=0x2029
                | 0x202a..=0x202e
                | 0x2060..=0x206f
                | 0x3164
                | 0xfeff
                | 0xffa0
                | 0x1bca0..=0x1bca3
                | 0x1d173..=0x1d17a
                | 0xe0000..=0xe0fff
        )
}

/// What the process's standard streams report about the terminal attached to them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamFacts {
    pub stdin_tty: bool,
    pub stdout_tty: bool,
    pub stderr_tty: bool,
    /// Columns and rows of an attached terminal, when the OS reports them.
    pub size: Option<(u16, u16)>,
}

/// Read the terminal facts of the process's standard streams. The size comes from the
/// first standard stream that reports one (stdout, then stderr, then stdin).
pub fn stream_facts() -> StreamFacts {
    StreamFacts {
        stdin_tty: std::io::IsTerminal::is_terminal(&std::io::stdin()),
        stdout_tty: stdout_is_terminal(),
        stderr_tty: std::io::IsTerminal::is_terminal(&std::io::stderr()),
        size: attached_size(),
    }
}

#[cfg(unix)]
fn attached_size() -> Option<(u16, u16)> {
    use std::os::fd::AsRawFd;
    [
        std::io::stdout().as_raw_fd(),
        std::io::stderr().as_raw_fd(),
        std::io::stdin().as_raw_fd(),
    ]
    .into_iter()
    .find_map(window_size_of_fd)
}

#[cfg(unix)]
fn window_size_of_fd(fd: std::os::fd::RawFd) -> Option<(u16, u16)> {
    let mut size = libc::winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: `fd` is a standard stream descriptor of this process, and TIOCGWINSZ writes at
    // most one `winsize` through the pointer, which refers to the live local `size`.
    let status = unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut size as *mut libc::winsize) };
    if status != 0 || size.ws_col == 0 || size.ws_row == 0 {
        return None;
    }
    Some((size.ws_col, size.ws_row))
}

#[cfg(windows)]
fn attached_size() -> Option<(u16, u16)> {
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::System::Console::{
        GetConsoleScreenBufferInfo, GetStdHandle, CONSOLE_SCREEN_BUFFER_INFO, STD_ERROR_HANDLE,
        STD_OUTPUT_HANDLE,
    };
    [STD_OUTPUT_HANDLE, STD_ERROR_HANDLE]
        .into_iter()
        .find_map(|kind| {
            // SAFETY: `kind` is a standard-handle constant. GetStdHandle takes no pointers and
            // returns a handle this process does not own, which is only read here, never closed.
            let handle = unsafe { GetStdHandle(kind) };
            if handle.is_null() || handle == INVALID_HANDLE_VALUE {
                return None;
            }
            // SAFETY: CONSOLE_SCREEN_BUFFER_INFO is plain old data, so the all-zero bit pattern is
            // a valid value that the call below overwrites when it succeeds.
            let mut info: CONSOLE_SCREEN_BUFFER_INFO = unsafe { std::mem::zeroed() };
            // SAFETY: `handle` came from GetStdHandle above, and `info` is a live out-parameter
            // of the exact type the call writes.
            if unsafe { GetConsoleScreenBufferInfo(handle, &mut info) } == 0 {
                return None;
            }
            let window = info.srWindow;
            let columns = i32::from(window.Right) - i32::from(window.Left) + 1;
            let rows = i32::from(window.Bottom) - i32::from(window.Top) + 1;
            Some((u16::try_from(columns).ok()?, u16::try_from(rows).ok()?))
        })
}

#[cfg(not(any(unix, windows)))]
fn attached_size() -> Option<(u16, u16)> {
    None
}

#[cfg(test)]
mod tests {
    use super::escape_controls;

    #[test]
    fn escape_sequences_become_visible_and_layout_survives() {
        assert_eq!(escape_controls("a\u{1b}[2Jb"), r"a\u{1b}[2Jb");
        assert_eq!(escape_controls("x\u{9b}31m"), r"x\u{9b}31m");
        assert_eq!(escape_controls("line\nnext\tcol"), "line\nnext\tcol");
        assert_eq!(escape_controls("caf\u{e9}"), "caf\u{e9}");
    }

    #[test]
    fn escapes_bidi_and_invisible_format_characters() {
        let input = [
            'a',
            char::from_u32(0x202e).unwrap(),
            'b',
            char::from_u32(0x2066).unwrap(),
            'c',
        ]
        .iter()
        .collect::<String>();
        assert_eq!(escape_controls(&input), r"a\u{202e}b\u{2066}c");
        assert_eq!(
            escape_controls(&char::from_u32(0x9b).unwrap().to_string()),
            r"\u{9b}"
        );
    }

    #[test]
    fn pipeline_sink_escapes_model_escapes_on_a_tty() {
        use super::encode_pipeline_for_stdout;
        // OSC title-set, CSI erase, C1 CSI and a bidi override: the model
        // payload from #522 must not reach a terminal writer raw.
        let hostile = "go\u{1b}]0;pwned\u{7}now\u{1b}[2Jthen\u{9b}31mswap\u{202e}end\nnext\tcol";
        let rendered = encode_pipeline_for_stdout(hostile.as_bytes(), true);
        let rendered = String::from_utf8(rendered).expect("escaped output is UTF-8");
        assert!(
            !rendered.contains('\u{1b}'),
            "raw ESC reached the TTY encoding: {rendered:?}"
        );
        assert!(
            !rendered.contains('\u{9b}'),
            "raw C1 CSI reached the TTY encoding: {rendered:?}"
        );
        assert!(
            !rendered.contains('\u{202e}'),
            "raw bidi override reached the TTY encoding: {rendered:?}"
        );
        assert_eq!(
            rendered,
            "go\\u{1b}]0;pwned\\u{7}now\\u{1b}[2Jthen\\u{9b}31mswap\\u{202e}end\nnext\tcol"
        );
    }

    #[test]
    fn pipeline_sink_keeps_pipe_bytes_exact() {
        use super::encode_pipeline_for_stdout;
        let hostile = "go\u{1b}]0;pwned\u{7}now\u{1b}[2Jthen\u{9b}31mswap\u{202e}end\ncaf\u{e9}\n";
        assert_eq!(
            encode_pipeline_for_stdout(hostile.as_bytes(), false),
            hostile.as_bytes()
        );
    }

    #[test]
    fn pipeline_sink_preserves_valid_text_and_shows_invalid_lossily_on_a_tty() {
        use super::encode_pipeline_for_stdout;
        // Valid UTF-8 (including non-ASCII) survives TTY escaping unchanged.
        assert_eq!(
            encode_pipeline_for_stdout("caf\u{e9} \u{6f22}\u{5b57}\n".as_bytes(), true),
            "caf\u{e9} \u{6f22}\u{5b57}\n".as_bytes()
        );
        // Invalid bytes cannot render as terminal text; they become a visible
        // placeholder instead of passing through raw or vanishing.
        let rendered = encode_pipeline_for_stdout(b"a\xffb", true);
        assert_eq!(rendered, "a\u{fffd}b".as_bytes());
        assert_eq!(
            encode_pipeline_for_stdout(b"a\xffb", false),
            b"a\xffb".to_vec(),
            "pipes keep even invalid bytes exact"
        );
    }

    #[test]
    fn json_escaping_removes_raw_bidi_and_c1_but_preserves_decoded_values() {
        let message = "a\u{202e}b\u{2066}c\u{009b}d\u{0080}e\u{009f}f\"g\\h\ni";
        let document = serde_json::json!({
            "message": message,
            "nested": [{ "stderr_tail": message, "key\u{202e}": "v" }],
        });
        for serialized in [
            document.to_string(),
            serde_json::to_string_pretty(&document).unwrap(),
        ] {
            let rendered = super::escape_json_for_terminal(&serialized);
            for raw in ['\u{202e}', '\u{2066}', '\u{009b}', '\u{0080}', '\u{009f}'] {
                assert!(!rendered.contains(raw), "raw {raw:?} in JSON: {rendered:?}");
            }
            assert!(rendered.contains(r"\u202e"), "{rendered:?}");
            assert!(rendered.contains(r"\u009b"), "{rendered:?}");
            let parsed: serde_json::Value = serde_json::from_str(&rendered).unwrap();
            assert_eq!(parsed, document, "decoded JSON changed: {rendered:?}");
        }
    }

    #[test]
    fn field_escaping_turns_layout_controls_and_unicode_separators_visible() {
        assert_eq!(
            super::escape_terminal_field("line\nnext\tcol\u{2028}row\u{2029}paragraph"),
            r"line\u{a}next\u{9}col\u{2028}row\u{2029}paragraph"
        );
        assert_eq!(
            escape_controls("line\u{2028}next\u{2029}paragraph"),
            r"line\u{2028}next\u{2029}paragraph"
        );
    }

    fn streamed(chunks: &[&[u8]]) -> Vec<u8> {
        let mut escaper = super::TerminalEscaper::default();
        let mut out = Vec::new();
        for chunk in chunks {
            escaper.push(chunk, &mut out);
        }
        escaper.finish(&mut out);
        out
    }

    #[test]
    fn streamed_escaping_matches_one_shot_at_every_split_position() {
        let inputs: [&[u8]; 5] = [
            b"\x1b[31mred\x1b[0m",
            "x\u{9b}31my".as_bytes(),
            "a\u{1f600}b\u{202e}".as_bytes(),
            b"ok\xf0\x9f\x98\x80\xff\xf0\x9f!z\xe2\x82",
            "line\nnext\tcol \u{6f22}\u{5b57} \u{fffd}".as_bytes(),
        ];
        for input in inputs {
            let expected = super::encode_pipeline_for_stdout(input, true);
            for split in 0..=input.len() {
                let got = streamed(&[&input[..split], &input[split..]]);
                assert_eq!(got, expected, "split at {split} of {input:?}");
            }
            let one_byte: Vec<&[u8]> = input.chunks(1).collect();
            assert_eq!(
                streamed(&one_byte),
                expected,
                "one byte per chunk of {input:?}"
            );
        }
    }

    #[test]
    fn streamed_escape_sequence_split_across_chunks_is_escaped_in_full() {
        let mut escaper = super::TerminalEscaper::default();
        let mut out = Vec::new();
        escaper.push(b"\x1b[", &mut out);
        escaper.push(b"3", &mut out);
        escaper.push(b"1m", &mut out);
        escaper.finish(&mut out);
        assert_eq!(out, br"\u{1b}[31m");
    }

    #[test]
    fn streamed_four_byte_character_waits_for_its_last_byte() {
        let mut escaper = super::TerminalEscaper::default();
        let mut out = Vec::new();
        let character = "\u{1f600}".as_bytes();
        for byte in &character[..3] {
            escaper.push(std::slice::from_ref(byte), &mut out);
            assert!(out.is_empty(), "emitted before the character completed");
            assert!(escaper.pending.len() <= 3, "pending grew past one sequence");
        }
        escaper.push(&character[3..], &mut out);
        escaper.finish(&mut out);
        assert_eq!(out, character);
    }

    #[test]
    fn streamed_incomplete_sequence_at_end_renders_one_replacement() {
        let mut escaper = super::TerminalEscaper::default();
        let mut out = Vec::new();
        escaper.push(b"a\xf0\x9f", &mut out);
        escaper.finish(&mut out);
        assert_eq!(out, "a\u{fffd}".as_bytes());
    }
}
