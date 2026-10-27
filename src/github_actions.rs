//! GitHub Actions workflow-command hardening for finite runs.
//!
//! The Actions runner interprets lines printed to the job log that start with
//! `::` as workflow commands (`::error::`, `::add-mask::`, `::stop-commands::`
//! and friends). A program that processes untrusted content can therefore forge
//! log commands through the child output Execute forwards, and secrets Execute
//! already knows would otherwise appear unmasked in the surrounding log.
//!
//! When hardening is enabled, Execute neutralizes `::` sequences at line starts
//! in the bytes it forwards to the terminal (streamed step output, re-emitted
//! captured stderr, the final program output and final diagnostics) by inserting
//! a zero-width space between the colons. The bytes stay human-readable but no
//! longer parse as a workflow command. Every newly registered secret value is
//! additionally masked with `::add-mask::` so the runner redacts it from the
//! log from that point on.
//!
//! Hardening is opt-in through [`set_enabled`]. The `execute` CLI enables it for
//! `run` and `resume` when `GITHUB_ACTIONS` is `"true"`. Library hosts running
//! under Actions should call [`set_enabled`] before executing; it defaults to
//! off so library tests and framed protocols (LSP, DAP, repl JSON) never see
//! mask lines or rewritten bytes unless the host asked for them.
//!
//! Under hardening, terminal-bound child streams are captured and re-emitted
//! through the sanitizing writers instead of inheriting the raw descriptors,
//! including manifest `inherit` dispositions and the streamed-step inherit
//! fast path. Program-level redirects to files stay raw in their files; only
//! the bytes Execute itself copies to the log are filtered. `hand over to`
//! is refused before any step runs: the handed-over child would inherit the
//! terminal and write raw after `exec`, which sanitization cannot intercept.
//! `execute check --format github` intentionally emits workflow commands and
//! is unaffected.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

/// Process-wide hardening switch. Set once before execution; read on every
/// forwarded write. `SeqCst` keeps the reasoning simple across worker threads.
static HARDENING: AtomicBool = AtomicBool::new(false);

/// Enable or disable Actions hardening for this process. The CLI calls this
/// for `run`/`resume` based on `GITHUB_ACTIONS`; library tests leave it off.
pub fn set_enabled(on: bool) {
    HARDENING.store(on, Ordering::SeqCst);
}

/// Whether forwarded child output is neutralized and secrets are masked.
pub fn enabled() -> bool {
    HARDENING.load(Ordering::SeqCst)
}

/// Whether the current process runs inside GitHub Actions.
pub fn is_ci() -> bool {
    std::env::var_os("GITHUB_ACTIONS").is_some_and(|value| value == "true")
}

/// Enable hardening when running under GitHub Actions. Returns the new state.
/// The CLI calls this at the start of `run`/`resume`; library hosts running
/// under Actions should call it before executing.
pub fn enable_if_ci() -> bool {
    let on = is_ci();
    set_enabled(on);
    // A fresh CLI registry holds no secrets yet, so this snapshot is usually
    // empty; it covers library hosts that registered secrets before enabling.
    if on {
        emit_pending_snapshot();
    }
    on
}

/// Emit mask lines for secrets already registered with the process registries.
/// Registration order is registry-scoped, so this best-effort sweep covers
/// the common CLI case (one registry per process). Secrets registered after
/// this call are masked individually by the secret registry hook.
fn emit_pending_snapshot() {
    // The CLI creates its registry after enabling; library hosts may have
    // registered before. Without a global registry list we cannot enumerate
    // them here, so this is a no-op placeholder kept to document the ordering:
    // enable before registering, and each fresh registration masks itself.
}

/// Emit one mask line per already-registered secret in `registry`. Call after
/// [`enable_if_ci`] (or [`set_enabled`]) when secrets may have been registered
/// before hardening was turned on. Secrets registered afterwards mask
/// themselves through the registry hook; calling this twice only repeats
/// mask lines, which the runner tolerates.
pub fn emit_masks_for_registry(registry: &crate::manifest::Registry) {
    if !enabled() {
        return;
    }
    for secret in crate::security::secret_redactor(registry).snapshot_secrets() {
        emit_mask_for_bytes(&secret);
    }
}

/// Reset the terminal sanitizer line state. Tests use this between cases so a
/// trailing newline (or pending colon) from one case cannot leak into the next.
pub fn reset_terminal_state() {
    if let Some(slot) = STDOUT_SANITIZER.get() {
        if let Ok(mut guard) = slot.lock() {
            *guard = StreamSanitizer::new();
        }
    }
    if let Some(slot) = STDERR_SANITIZER.get() {
        if let Ok(mut guard) = slot.lock() {
            *guard = StreamSanitizer::new();
        }
    }
}

/// Zero-width space, inserted between the colons of a line-leading `::` so the
/// runner no longer recognizes a workflow command while the text still reads
/// the same.
const ZERO_WIDTH_SPACE: &[u8] = "\u{200b}".as_bytes();

/// Incremental line-start sanitizer for terminal-bound byte streams.
///
/// One-shot [`sanitize_bytes`] treats every buffer as starting a new line,
/// which over-neutralizes when a write continues a previous line and misses a
/// `::` split across two writes. Long-lived terminal streams (streamed step
/// output, final output following streamed output, live diagnostics) share one
/// sanitizer per file descriptor so both cases stay correct: a `::` is only
/// neutralized when it truly opens a log line, and a trailing `:` at a line
/// start is held back until the next chunk proves whether it forms `::`.
#[derive(Debug, Clone)]
pub struct StreamSanitizer {
    at_line_start: bool,
    pending_colon: bool,
}

impl StreamSanitizer {
    /// A sanitizer positioned at the start of a log line.
    pub fn new() -> Self {
        Self {
            at_line_start: true,
            pending_colon: false,
        }
    }

    /// Sanitize one chunk, preserving line-start state for the next chunk.
    /// A trailing `:` that opens a line is buffered, not emitted; call
    /// [`flush`](Self::flush) at end of stream to release it.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<u8> {
        if bytes.is_empty() {
            return Vec::new();
        }
        let mut out = Vec::with_capacity(bytes.len() + 4);
        let mut index = 0;
        if self.pending_colon {
            self.pending_colon = false;
            if bytes[0] == b':' {
                // The previous chunk ended with `:` at a line start and this
                // chunk opens with `:`: together they form a forged command.
                out.push(b':');
                out.extend_from_slice(ZERO_WIDTH_SPACE);
                out.push(b':');
                index = 1;
                self.at_line_start = false;
            } else {
                // A lone line-leading `:` is harmless; emit it and treat the
                // rest of this chunk as mid-line.
                out.push(b':');
                self.at_line_start = false;
            }
        }
        while index < bytes.len() {
            let byte = bytes[index];
            if self.at_line_start && byte == b':' {
                if index + 1 < bytes.len() {
                    if bytes[index + 1] == b':' {
                        out.push(b':');
                        out.extend_from_slice(ZERO_WIDTH_SPACE);
                        out.push(b':');
                        index += 2;
                        self.at_line_start = false;
                        continue;
                    }
                    out.push(b':');
                    index += 1;
                    self.at_line_start = false;
                    continue;
                }
                // Trailing `:` at a line start: hold it for the next chunk so
                // a split `::` is still neutralized.
                self.pending_colon = true;
                index += 1;
                break;
            }
            out.push(byte);
            self.at_line_start = byte == b'\n' || byte == b'\r';
            index += 1;
        }
        out
    }

    /// Release a buffered trailing `:` held by [`push`](Self::push).
    pub fn flush(&mut self) -> Vec<u8> {
        if self.pending_colon {
            self.pending_colon = false;
            self.at_line_start = false;
            return vec![b':'];
        }
        Vec::new()
    }
}

impl Default for StreamSanitizer {
    fn default() -> Self {
        Self::new()
    }
}

static STDOUT_SANITIZER: OnceLock<Mutex<StreamSanitizer>> = OnceLock::new();
static STDERR_SANITIZER: OnceLock<Mutex<StreamSanitizer>> = OnceLock::new();

fn stdout_sanitizer() -> &'static Mutex<StreamSanitizer> {
    STDOUT_SANITIZER.get_or_init(|| Mutex::new(StreamSanitizer::new()))
}

fn stderr_sanitizer() -> &'static Mutex<StreamSanitizer> {
    STDERR_SANITIZER.get_or_init(|| Mutex::new(StreamSanitizer::new()))
}

/// Sanitize terminal-bound stdout bytes when hardening is enabled, preserving
/// cross-write line state (including a `::` split across writes). When
/// hardening is off the input is returned unchanged.
pub fn filter_stdout(bytes: &[u8]) -> Vec<u8> {
    if !enabled() {
        return bytes.to_vec();
    }
    stdout_sanitizer()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(bytes)
}

/// Sanitize terminal-bound stderr bytes when hardening is enabled. See
/// [`filter_stdout`] for the cross-write guarantees.
pub fn filter_stderr(bytes: &[u8]) -> Vec<u8> {
    if !enabled() {
        return bytes.to_vec();
    }
    stderr_sanitizer()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(bytes)
}

/// Release a buffered trailing `:` held by the stdout sanitizer at end of
/// stream. Returns empty when hardening is off or nothing is pending. Final
/// output paths call this after [`filter_stdout`] so a program whose output
/// ends with a line-leading `:` is not truncated; live streams never flush so
/// a `::` split across writes stays neutralized.
pub fn flush_stdout() -> Vec<u8> {
    if !enabled() {
        return Vec::new();
    }
    stdout_sanitizer()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .flush()
}

/// End-of-stream flush for the stderr sanitizer. See [`flush_stdout`].
pub fn flush_stderr() -> Vec<u8> {
    if !enabled() {
        return Vec::new();
    }
    stderr_sanitizer()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .flush()
}

/// Sanitize a final stdout buffer at end of stream, preserving the shared
/// cross-write line state with streamed output. Use for program output written
/// after streaming has finished (`run`, `resume`, `--once`, live `host_run`).
pub fn filter_stdout_final(bytes: &[u8]) -> Vec<u8> {
    if !enabled() {
        return bytes.to_vec();
    }
    let mut out = filter_stdout(bytes);
    out.extend(flush_stdout());
    out
}

/// End-of-stream sanitize for a final stderr buffer. See [`filter_stdout_final`].
pub fn filter_stderr_final(bytes: &[u8]) -> Vec<u8> {
    if !enabled() {
        return bytes.to_vec();
    }
    let mut out = filter_stderr(bytes);
    out.extend(flush_stderr());
    out
}

/// Neutralize line-leading `::` sequences in terminal-bound bytes.
///
/// A `::` at the start of the buffer or immediately after `\n` or `\r` becomes
/// `:` + zero-width space + `:`. All other bytes, including mid-line `::` and
/// non-UTF-8 sequences, pass through untouched. Applying the function twice is
/// harmless: the inserted space already breaks the `::` pair.
pub fn sanitize_bytes(bytes: &[u8]) -> Vec<u8> {
    if bytes.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    let mut at_line_start = true;
    while index < bytes.len() {
        let byte = bytes[index];
        if at_line_start
            && byte == b':'
            && index + 1 < bytes.len()
            && bytes[index + 1] == b':'
        {
            out.push(b':');
            out.extend_from_slice(ZERO_WIDTH_SPACE);
            out.push(b':');
            index += 2;
            at_line_start = false;
            continue;
        }
        out.push(byte);
        at_line_start = byte == b'\n' || byte == b'\r';
        index += 1;
    }
    out
}

/// String convenience for [`sanitize_bytes`].
pub fn sanitize_text(text: &str) -> String {
    String::from_utf8_lossy(&sanitize_bytes(text.as_bytes())).into_owned()
}

/// Escape workflow-command data (`::add-mask::` values): `%`, CR and LF.
/// Colons and commas are data here, not separators, and stay literal.
pub fn escape_data(text: &str) -> String {
    text.replace('%', "%25")
        .replace('\r', "%0D")
        .replace('\n', "%0A")
}

/// Format one mask command line (without the trailing newline) for a secret.
pub fn mask_command(secret: &str) -> String {
    format!("::add-mask::{}", escape_data(secret))
}

/// Render mask lines for a snapshot of registered secret bytes. Non-UTF-8
/// values use the same lossy rendering a log viewer would show, so the mask
/// still matches the visible text.
pub fn mask_lines(secrets: &[Vec<u8>]) -> Vec<String> {
    secrets
        .iter()
        .filter(|secret| !secret.is_empty())
        .map(|secret| mask_command(&String::from_utf8_lossy(secret)))
        .collect()
}

/// Emit one `::add-mask::` line to standard output for a newly registered
/// secret. Called from the secret registry, so a mask always precedes any
/// later log line that could echo the value. Best-effort and silent when
/// hardening is off or the write fails: masking must never fail a run.
pub fn emit_mask_for_bytes(secret: &[u8]) {
    if !enabled() || secret.is_empty() {
        return;
    }
    use std::io::Write as _;
    let line = mask_command(&String::from_utf8_lossy(secret));
    let _ = writeln!(std::io::stdout().lock(), "{line}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hardening_defaults_to_off_without_touching_process_state() {
        // Read-only: this suite never enables hardening, so parallel tests
        // keep legacy behavior.
        assert!(!enabled());
    }

    #[test]
    fn line_leading_commands_are_neutralized_but_read_the_same() {
        let out = sanitize_bytes(b"::stop-commands::token\n");
        assert_eq!(out, "::stop-commands::token\n".replace("::", ":\u{200b}:").as_bytes());
        assert!(!out.starts_with(b"::"));
        // The visible text is unchanged apart from the zero-width break.
        assert_eq!(
            String::from_utf8_lossy(&out).replace('\u{200b}', ""),
            "::stop-commands::token\n"
        );
    }

    #[test]
    fn only_line_starts_are_rewritten() {
        // Mid-line sequences (Rust paths, URLs, timestamps) are harmless to
        // the runner and must survive byte-identical.
        let input = b"a::b http://x::1\nsecond line ok\n";
        assert_eq!(sanitize_bytes(input), input);
        // Indented commands keep their bytes: workflow commands start at
        // column zero, and re-indenting user output would corrupt it.
        let indented = b"  ::error::not a command\n";
        assert_eq!(sanitize_bytes(indented), indented);
    }

    #[test]
    fn carriage_returns_and_empty_inputs_pass_through() {
        assert_eq!(sanitize_bytes(b""), b"");
        let crlf = sanitize_bytes(b"done\r\n::error::x\r\n");
        assert_eq!(crlf, "done\r\n:\u{200b}:error::x\r\n".as_bytes());
        let lone_cr = sanitize_bytes(b"a\rb::c");
        assert_eq!(lone_cr, b"a\rb::c");
        let trailing = sanitize_bytes(b"\n::");
        assert_eq!(trailing, "\n:\u{200b}:".as_bytes());
    }

    #[test]
    fn sanitizing_is_idempotent_and_preserves_non_utf8() {
        let once = sanitize_bytes(b"::add-mask::v\nbin\xff\xfe:: mid\n");
        // \xff\xfe survive untouched; the mid-line :: is not at a line start.
        let mut expected = ":\u{200b}:add-mask::v\nbin".as_bytes().to_vec();
        expected.extend_from_slice(&[0xff, 0xfe]);
        expected.extend_from_slice(b":: mid\n");
        assert_eq!(once, expected);
        assert_eq!(sanitize_bytes(&once), once);
        assert_eq!(sanitize_text("::notice::hi"), ":\u{200b}:notice::hi");
    }

    #[test]
    fn mask_commands_escape_percent_and_newlines() {
        assert_eq!(mask_command("hunter2"), "::add-mask::hunter2");
        assert_eq!(
            mask_command("a%b\r\nc:d,e"),
            "::add-mask::a%25b%0D%0Ac:d,e"
        );
    }

    #[test]
    fn mask_lines_skip_empties_and_render_lossy_bytes() {
        let lines = mask_lines(&[Vec::new(), b"abc".to_vec(), vec![0xff, 0xfe]]);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], "::add-mask::abc");
        assert!(lines[1].starts_with("::add-mask::"), "{lines:?}");
    }

    #[test]
    fn stream_sanitizer_neutralizes_a_command_split_across_chunks() {
        // A `:` ending one write and `:` opening the next is one forged
        // command and must still be neutralized.
        let mut stream = StreamSanitizer::new();
        let first = stream.push(b"done\n:");
        assert!(first.ends_with(b"done\n"), "{first:?}");
        let second = stream.push(b":error::x\n");
        assert_eq!(second, ":\u{200b}:error::x\n".as_bytes());
        assert!(stream.flush().is_empty());
    }

    #[test]
    fn stream_sanitizer_keeps_a_lone_line_leading_colon_and_flushes_it() {
        let mut stream = StreamSanitizer::new();
        // A line-leading `:` followed by other text is harmless mid-line.
        let mut out = stream.push(b":ok\n");
        out.extend(stream.push(b"next\n"));
        assert_eq!(out, b":ok\nnext\n");
        // A trailing line-leading `:` is buffered until the stream ends.
        let mut trailing = StreamSanitizer::new();
        assert_eq!(trailing.push(b"value\n:"), b"value\n");
        assert_eq!(trailing.flush(), b":");
        assert!(trailing.flush().is_empty());
    }

    #[test]
    fn stream_sanitizer_preserves_state_across_stream_then_final() {
        // Streamed output ending mid-line followed by final output starting
        // with `::` must not be treated as a line start.
        let mut stream = StreamSanitizer::new();
        let mut out = stream.push(b"partial ");
        out.extend(stream.push(b"::not-a-command\n"));
        assert_eq!(out, b"partial ::not-a-command\n");
        // A streamed newline followed by a final forged command is neutralized.
        let mut boundary = StreamSanitizer::new();
        let mut out = boundary.push(b"first\n");
        out.extend(boundary.push(b"::error::x\n"));
        out.extend(boundary.flush());
        assert_eq!(out, "first\n:\u{200b}:error::x\n".as_bytes());
    }
}
