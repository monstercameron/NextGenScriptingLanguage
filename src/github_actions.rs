//! GitHub Actions workflow-command hardening for finite runs.
//!
//! The Actions runner interprets `::` workflow commands (`::error::`,
//! `::add-mask::`, `::stop-commands::` and friends) after trimming leading
//! whitespace, and legacy `##[` commands (for example `##[error]`) anywhere in
//! a line. A program that processes untrusted content can therefore forge log
//! commands through the child output Execute forwards, and secrets Execute
//! already knows would otherwise appear unmasked in the surrounding log.
//!
//! When hardening is enabled, Execute neutralizes those prefixes in the bytes
//! it forwards to the terminal (streamed step output, re-emitted captured
//! stderr, the final program output and final diagnostics) by inserting a
//! zero-width space into the prefix: between the colons of a line-leading
//! `::` (leading whitespace included) and between the hashes of any `##[`
//! sequence. The bytes stay human-readable but no longer parse as a workflow
//! command. Every newly registered secret value is additionally masked with
//! `::add-mask::` so the runner redacts it from the log from that point on.
//! Each mask is synchronized onto its own log line (a line break is written
//! first when the log is mid-line) because the runner only recognizes `::`
//! commands at a line start; the pre-registered-secret sweep at run start
//! runs before the terminal state reset for the same reason.
//!
//! Hardening is opt-in through [`set_enabled`]. The `execute` CLI enables it for
//! `run` and `resume` when `GITHUB_ACTIONS` is `"true"`. Library hosts running
//! under Actions should call [`set_enabled`] before executing; it defaults to
//! off so library tests and framed protocols (LSP, DAP, repl JSON) never see
//! mask lines or rewritten bytes unless the host asked for them. A host that
//! registered secrets before enabling must call [`emit_masks_for_registry`]
//! (each finite run also sweeps its own registry at start); secrets registered
//! afterwards mask themselves through the registry hook. Values Execute has
//! not yet learned — for example dynamic command output before the producing
//! step completes — cannot be masked before discovery; sensitive-marked steps
//! are therefore captured and only copied to the log after registration, so
//! the mask precedes the value. Already-echoed bytes are never retroactively
//! redacted by the runner.
//!
//! Under hardening, terminal-bound child streams are captured and re-emitted
//! through the sanitizing writers instead of inheriting the raw descriptors,
//! including manifest `inherit` dispositions and the streamed-step inherit
//! fast path. Program-level redirects to files stay raw in their files; only
//! the bytes Execute itself copies to the log are filtered. `hand over to`
//! is refused before any step runs: the handed-over program replaces the
//! process and its terminal writes after `exec` cannot be intercepted.
//! Detached launches, observers and record/replay captures intentionally see
//! pre-filter bytes (they own their own sinks, not the job log) and stay raw;
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
/// under Actions should call it before executing. Secrets registered before
/// this call are not covered here — there is no global registry list to
/// sweep — so hosts with pre-registered secrets must call
/// [`emit_masks_for_registry`]; every finite run sweeps its own registry at
/// start as a backstop.
pub fn enable_if_ci() -> bool {
    let on = is_ci();
    set_enabled(on);
    on
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

/// Reset the terminal sanitizer line state. Each outermost finite run calls
/// this after sweeping pre-registered secrets (see the depth-zero block in
/// `run_finite_state`) so a trailing partial line (or a held prefix) from a
/// previous run in a long-lived host cannot misclassify the next run's first
/// chunk. The sweep runs first on purpose: the inherited state still tells
/// whether the log is mid-line, so the first mask starts its own line; every
/// mask ends with a newline, so resetting afterwards lands on a true line
/// start. Tests use this between cases for the same reason. Callers must not
/// reset while another run is concurrently streaming to the terminal: the
/// sanitizers are process-global by design.
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

/// Zero-width space, inserted into a command prefix so the runner no longer
/// recognizes it while the text still reads the same: between the colons of a
/// line-leading `::`, and between the hashes of any `##[` legacy command.
const ZERO_WIDTH_SPACE: &[u8] = "\u{200b}".as_bytes();

/// ASCII whitespace the runner's `TrimStart` skips before a `::` command.
/// `\r` and `\n` are line breaks handled separately; the rest keeps the
/// sanitizer in the line-leading state.
fn is_ascii_leading_ws(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | 0x0B | 0x0C)
}

/// Length of the trailing incomplete UTF-8 sequence in `buf`, if any. ASCII
/// bytes never start such a tail, so a multi-byte whitespace character split
/// across writes is held back until the next chunk completes it.
fn incomplete_tail_len(buf: &[u8]) -> usize {
    if buf.is_empty() {
        return 0;
    }
    let mut trailing_cont = 0usize;
    let mut cursor = buf.len();
    while cursor > 0 && trailing_cont < 3 {
        if buf[cursor - 1] >> 6 == 0b10 {
            trailing_cont += 1;
            cursor -= 1;
        } else {
            break;
        }
    }
    if trailing_cont == 0 {
        let last = buf[buf.len() - 1];
        if last < 0x80 {
            return 0;
        }
        let expected = if last >> 5 == 0b110 {
            2
        } else if last >> 4 == 0b1110 {
            3
        } else if last >> 3 == 0b11110 {
            4
        } else {
            return 0;
        };
        if expected > 1 {
            return 1;
        }
        return 0;
    }
    if cursor == 0 {
        // Only continuation bytes at the end with no leader in this buffer;
        // treat them as invalid bytes that pass through rather than a split.
        return 0;
    }
    let lead = buf[cursor - 1];
    let expected = if lead >> 5 == 0b110 {
        2
    } else if lead >> 4 == 0b1110 {
        3
    } else if lead >> 3 == 0b11110 {
        4
    } else {
        return 0;
    };
    let have = trailing_cont + 1;
    if have < expected { have } else { 0 }
}

/// Decode the non-ASCII character starting at `index`, if it is a valid UTF-8
/// sequence. Returns the character and its byte length; invalid bytes return
/// `None` so the caller can pass the single byte through.
fn decode_non_ascii(bytes: &[u8], index: usize) -> Option<(char, usize)> {
    let first = bytes[index];
    if first < 0x80 {
        return None;
    }
    let len = if first >> 5 == 0b110 {
        2
    } else if first >> 4 == 0b1110 {
        3
    } else if first >> 3 == 0b11110 {
        4
    } else {
        return None;
    };
    if index + len > bytes.len() {
        return None;
    }
    let slice = &bytes[index..index + len];
    if slice[1..].iter().any(|byte| byte >> 6 != 0b10) {
        return None;
    }
    std::str::from_utf8(slice).ok()?.chars().next().map(|c| (c, len))
}

/// Emit one neutralized modern prefix: `:` + zero-width space + `:`.
fn push_neutralized_colons(out: &mut Vec<u8>) {
    out.push(b':');
    out.extend_from_slice(ZERO_WIDTH_SPACE);
    out.push(b':');
}

/// Emit one neutralized legacy prefix: `#` + zero-width space + `#[`.
fn push_neutralized_hashes(out: &mut Vec<u8>) {
    out.push(b'#');
    out.extend_from_slice(ZERO_WIDTH_SPACE);
    out.push(b'#');
    out.push(b'[');
}

/// Incremental sanitizer for terminal-bound byte streams.
///
/// One-shot [`sanitize_bytes`] treats every buffer as starting a new line,
/// which over-neutralizes when a write continues a previous line and misses a
/// prefix split across two writes. Long-lived terminal streams (streamed step
/// output, final output following streamed output, live diagnostics) share one
/// sanitizer per file descriptor (behind a `Mutex`, so worker threads stay
/// safe) so both cases stay correct. A `::` is neutralized only when nothing
/// but whitespace has appeared since the last `\n`/`\r`, matching the runner's
/// `TrimStart` check; a `##[` legacy command is neutralized wherever it
/// appears, matching the runner's anywhere-in-line search. A line-leading
/// trailing `:` and trailing `#`s that could open `##[` are held back until
/// the next chunk proves what they form, as is a trailing incomplete UTF-8
/// sequence; call [`flush`](Self::flush) at end of stream to release them.
#[derive(Debug, Clone)]
pub struct StreamSanitizer {
    leading: bool,
    pending_colon: bool,
    pending_hashes: u8,
    pending_tail: Vec<u8>,
}

impl StreamSanitizer {
    /// A sanitizer positioned at the start of a log line.
    pub fn new() -> Self {
        Self {
            leading: true,
            pending_colon: false,
            pending_hashes: 0,
            pending_tail: Vec::new(),
        }
    }

    /// Sanitize one chunk, preserving line and prefix state for the next
    /// chunk. Held-back bytes (a line-leading `:`, trailing `#`s, an
    /// incomplete UTF-8 tail) are not emitted until the next chunk or
    /// [`flush`](Self::flush) resolves them.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<u8> {
        let mut buf = std::mem::take(&mut self.pending_tail);
        buf.extend_from_slice(bytes);
        let tail = incomplete_tail_len(&buf);
        let mut held_tail = Vec::new();
        if tail > 0 {
            held_tail = buf[buf.len() - tail..].to_vec();
            buf.truncate(buf.len() - tail);
        }
        self.pending_tail = held_tail;
        if self.pending_hashes > 0 {
            let count = usize::from(self.pending_hashes);
            self.pending_hashes = 0;
            let mut prefixed = vec![b'#'; count];
            prefixed.append(&mut buf);
            buf = prefixed;
        }
        if self.pending_colon {
            self.pending_colon = false;
            let mut prefixed = vec![b':'];
            prefixed.append(&mut buf);
            buf = prefixed;
        }
        if buf.is_empty() {
            return Vec::new();
        }
        let mut out = Vec::with_capacity(buf.len() + 4);
        let mut index = 0;
        while index < buf.len() {
            let byte = buf[index];
            if byte == b'\r' || byte == b'\n' {
                out.push(byte);
                self.leading = true;
                index += 1;
                continue;
            }
            if self.leading && is_ascii_leading_ws(byte) {
                out.push(byte);
                index += 1;
                continue;
            }
            if byte < 0x80 {
                if self.leading && byte == b':' {
                    if index + 1 >= buf.len() {
                        // Trailing line-leading `:`: hold it so a `::` split
                        // across chunks is still neutralized.
                        self.pending_colon = true;
                        break;
                    }
                    if buf[index + 1] == b':' {
                        push_neutralized_colons(&mut out);
                        index += 2;
                        self.leading = false;
                        continue;
                    }
                    out.push(b':');
                    index += 1;
                    self.leading = false;
                    continue;
                }
                if byte == b'#' {
                    let remaining = buf.len() - index;
                    if remaining == 1 {
                        self.pending_hashes = 1;
                        break;
                    }
                    if remaining == 2 && buf[index + 1] == b'#' {
                        self.pending_hashes = 2;
                        break;
                    }
                    if remaining >= 3 && buf[index + 1] == b'#' && buf[index + 2] == b'[' {
                        push_neutralized_hashes(&mut out);
                        index += 3;
                        self.leading = false;
                        continue;
                    }
                    out.push(b'#');
                    index += 1;
                    self.leading = false;
                    continue;
                }
                out.push(byte);
                self.leading = false;
                index += 1;
                continue;
            }
            match decode_non_ascii(&buf, index) {
                Some((ch, len)) => {
                    out.extend_from_slice(&buf[index..index + len]);
                    if self.leading {
                        self.leading = ch.is_whitespace();
                    }
                    index += len;
                }
                None => {
                    out.push(byte);
                    self.leading = false;
                    index += 1;
                }
            }
        }
        out
    }

    /// Account for one raw log line written out-of-band, past this sanitizer.
    /// Mask lines must reach the runner literally, so they bypass [`push`](Self::push);
    /// without this call the sanitizer would still believe it is mid-line and
    /// a following line-leading `::` would pass through raw. Held-back bytes
    /// (a split prefix or a partial UTF-8 sequence) are kept: they rejoin the
    /// next chunk, which is now correctly classified as starting a new line.
    pub fn note_external_line(&mut self) {
        self.leading = true;
    }

    /// Whether the log is positioned at the start of a line: every byte the
    /// sanitizer released has been followed by `\n`/`\r`, or nothing was
    /// released yet. Mask emission reads this to decide whether the mask
    /// needs a line break first so the runner recognizes it as its own line.
    pub fn at_line_start(&self) -> bool {
        self.leading
    }

    /// Release bytes held by [`push`](Self::push): a line-leading `:`,
    /// trailing `#`s and an incomplete UTF-8 tail. Returns empty when nothing
    /// is pending.
    pub fn flush(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        if self.pending_colon {
            self.pending_colon = false;
            out.push(b':');
            self.leading = false;
        }
        if self.pending_hashes > 0 {
            for _ in 0..self.pending_hashes {
                out.push(b'#');
            }
            self.pending_hashes = 0;
            self.leading = false;
        }
        if !self.pending_tail.is_empty() {
            out.extend_from_slice(&self.pending_tail);
            self.pending_tail.clear();
            self.leading = false;
        }
        out
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
/// cross-write line state (including prefixes split across writes). When
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

/// Release bytes held by the stdout sanitizer at end of stream (a
/// line-leading `:`, trailing `#`s, an incomplete UTF-8 tail). Returns empty
/// when hardening is off or nothing is pending. Final output paths call this
/// after [`filter_stdout`] so a program whose output ends with a held prefix
/// is not truncated; live streams never flush so a prefix split across writes
/// stays neutralized.
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

/// Neutralize workflow-command prefixes in terminal-bound bytes.
///
/// A `::` becomes `:` + zero-width space + `:` when nothing but whitespace
/// has appeared since the buffer start or the last `\n`/`\r`, matching the
/// runner's `TrimStart` check; a `##[` legacy command becomes `#` +
/// zero-width space + `#[` wherever it appears, matching the runner's
/// anywhere-in-line search. Only the opening prefix is broken: the `::`
/// separating a command name from its data stays literal, as do mid-line
/// `::` pairs (Rust paths, URLs, timestamps) and non-UTF-8 sequences, which
/// pass through untouched. Applying the function twice is harmless: the
/// inserted space already breaks the prefix.
pub fn sanitize_bytes(bytes: &[u8]) -> Vec<u8> {
    if bytes.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    let mut leading = true;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte == b'\r' || byte == b'\n' {
            out.push(byte);
            leading = true;
            index += 1;
            continue;
        }
        if leading && is_ascii_leading_ws(byte) {
            out.push(byte);
            index += 1;
            continue;
        }
        if byte < 0x80 {
            if leading && byte == b':' {
                if index + 1 < bytes.len() && bytes[index + 1] == b':' {
                    push_neutralized_colons(&mut out);
                    index += 2;
                    leading = false;
                    continue;
                }
                out.push(b':');
                index += 1;
                leading = false;
                continue;
            }
            if byte == b'#'
                && index + 2 < bytes.len()
                && bytes[index + 1] == b'#'
                && bytes[index + 2] == b'['
            {
                push_neutralized_hashes(&mut out);
                index += 3;
                leading = false;
                continue;
            }
            out.push(byte);
            leading = false;
            index += 1;
            continue;
        }
        match decode_non_ascii(bytes, index) {
            Some((ch, len)) => {
                out.extend_from_slice(&bytes[index..index + len]);
                if leading {
                    leading = ch.is_whitespace();
                }
                index += len;
            }
            None => {
                out.push(byte);
                leading = false;
                index += 1;
            }
        }
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

/// Render the exact bytes to write for one mask line. When the log is
/// mid-line the mask is prefixed with a line break so the runner's parser
/// (which requires `::` at a line start after `TrimStart`) recognizes the
/// mask as its own command instead of trailing garbage on the previous line.
/// The returned bytes always hold exactly one `\n`-terminated line and only
/// the escaped secret — never the raw value.
fn mask_line_bytes(line: &str, mid_line: bool) -> Vec<u8> {
    let mut buf = Vec::with_capacity(line.len() + 2);
    if mid_line {
        buf.push(b'\n');
    }
    buf.extend_from_slice(line.as_bytes());
    buf.push(b'\n');
    buf
}

/// Emit one `::add-mask::` line to standard output for a newly registered
/// secret. Called from the secret registry, so a mask always precedes any
/// later log line that could echo the value. Best-effort and silent when
/// hardening is off or the write fails: masking must never fail a run.
///
/// The mask line bypasses the stdout sanitizer (it must stay a literal
/// workflow command), but it is first synchronized onto its own log line:
/// when either terminal sanitizer is mid-line — for example a previous step
/// streamed `lead` with no trailing newline — a `\n` is written before the
/// mask so the runner recognizes it. Both sanitizers are then resynced to
/// line-start: otherwise a mask landing mid-line would desynchronize them
/// and the next line-leading `::` would pass through raw. Checking both
/// streams covers the merged job log the runner parses (stdout and stderr
/// share its lines); when the descriptors diverge the extra break is at
/// most one blank line in one sink. Resyncing is fail-safe — a concurrent
/// mid-line write classified as line-start is only over-neutralized, never
/// under — and held-back split prefixes are kept so they still join the
/// next chunk. The line goes out in a single `write_all` so concurrent
/// terminal bytes cannot split it.
pub fn emit_mask_for_bytes(secret: &[u8]) {
    if !enabled() || secret.is_empty() {
        return;
    }
    let line = mask_command(&String::from_utf8_lossy(secret));
    // Fixed order (stdout, then stderr): the only site holding both locks,
    // so no reverse-order nesting can deadlock against it.
    let mid_line = {
        let mut stdout_guard = stdout_sanitizer()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut stderr_guard = stderr_sanitizer()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mid_line = !stdout_guard.at_line_start() || !stderr_guard.at_line_start();
        stdout_guard.note_external_line();
        stderr_guard.note_external_line();
        mid_line
    };
    use std::io::Write as _;
    let bytes = mask_line_bytes(&line, mid_line);
    let mut stdout = std::io::stdout().lock();
    let _ = stdout.write_all(&bytes).and_then(|()| stdout.flush());
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serializes the tests that flip the process-global hardening switch, so
    /// they cannot interleave with the read-only default-off assertion.
    #[cfg(test)]
    static GLOBAL_HARDENING_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn hardening_defaults_to_off_without_touching_process_state() {
        // Read-only: this suite never enables hardening, so parallel tests
        // keep legacy behavior.
        let _guard = GLOBAL_HARDENING_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(!enabled());
    }

    #[test]
    fn line_leading_commands_are_neutralized_but_read_the_same() {
        // Only the opening prefix is broken; the `::` before the data stays
        // literal because the runner never reaches it once the prefix fails.
        let out = sanitize_bytes(b"::stop-commands::token\n");
        assert_eq!(out, ":\u{200b}:stop-commands::token\n".as_bytes());
        assert!(!out.starts_with(b"::"));
        // The visible text is unchanged apart from the zero-width break.
        assert_eq!(
            String::from_utf8_lossy(&out).replace('\u{200b}', ""),
            "::stop-commands::token\n"
        );
    }

    #[test]
    fn only_line_leading_pairs_are_rewritten() {
        // Mid-line sequences (Rust paths, URLs, timestamps) are harmless to
        // the runner and must survive byte-identical.
        let input = b"a::b http://x::1\nsecond line ok\n";
        assert_eq!(sanitize_bytes(input), input);
        // The runner trims leading whitespace before checking for `::`, so an
        // indented command is exploitable and must be neutralized.
        assert_eq!(
            sanitize_bytes(b"  ::error::indented\n"),
            "  :\u{200b}:error::indented\n".as_bytes()
        );
        assert_eq!(
            sanitize_bytes(b"\t::warning::tabbed\n"),
            "\t:\u{200b}:warning::tabbed\n".as_bytes()
        );
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

    #[test]
    fn leading_whitespace_commands_are_neutralized_for_trim_start_runners() {
        // The runner trims spaces and tabs before checking for `::`.
        assert_eq!(
            sanitize_bytes("  ::error::x\n".as_bytes()),
            "  :\u{200b}:error::x\n".as_bytes()
        );
        assert_eq!(
            sanitize_bytes("\t ::notice::hi\n".as_bytes()),
            "\t :\u{200b}:notice::hi\n".as_bytes()
        );
        // A non-breaking space is Unicode whitespace and is trimmed too.
        assert_eq!(
            sanitize_bytes("\u{a0}::error::nbsp\n".as_bytes()),
            "\u{a0}:\u{200b}:error::nbsp\n".as_bytes()
        );
        // Other content before the pair keeps it mid-line and raw.
        assert_eq!(sanitize_bytes(b"x ::error::mid\n"), b"x ::error::mid\n");
        assert_eq!(
            String::from_utf8_lossy(&sanitize_bytes("  ::error::x\n".as_bytes()))
                .replace('\u{200b}', ""),
            "  ::error::x\n"
        );
    }

    #[test]
    fn legacy_hash_commands_are_neutralized_anywhere() {
        // The legacy runner searches for `##[` anywhere in the line, so even
        // mid-line occurrences must break.
        assert_eq!(
            sanitize_bytes(b"##[error]legacy\n"),
            "#\u{200b}#[error]legacy\n".as_bytes()
        );
        assert_eq!(
            sanitize_bytes(b"mid ##[warning]x\n"),
            "mid #\u{200b}#[warning]x\n".as_bytes()
        );
        // Overlapping runs break every `##[` window.
        assert_eq!(
            sanitize_bytes(b"###[x\n"),
            "##\u{200b}#[x\n".as_bytes()
        );
        // Single hashes and `#[` pairs are not commands and stay raw.
        assert_eq!(sanitize_bytes(b"# not a command\n"), b"# not a command\n");
        assert_eq!(sanitize_bytes(b"a #[b\n"), b"a #[b\n");
        // Neutralizing twice is stable and the visible text is preserved.
        let once = sanitize_bytes(b"##[error]x\n");
        assert!(!once.windows(3).any(|w| w == b"##["));
        assert_eq!(sanitize_bytes(&once), once);
        assert_eq!(
            String::from_utf8_lossy(&once).replace('\u{200b}', ""),
            "##[error]x\n"
        );
    }

    #[test]
    fn stream_sanitizer_neutralizes_split_whitespace_and_legacy_prefixes() {
        // Leading spaces in one chunk followed by `::` in the next.
        let mut indented = StreamSanitizer::new();
        let mut out = indented.push(b"  ");
        out.extend(indented.push(b"::error::x\n"));
        out.extend(indented.flush());
        assert_eq!(out, "  :\u{200b}:error::x\n".as_bytes());

        // A lone `:` held at a line start still joins a `:` opening the next
        // chunk even after whitespace handling.
        let mut split_colon = StreamSanitizer::new();
        assert_eq!(split_colon.push(b"\n:"), b"\n");
        assert_eq!(
            split_colon.push(b":notice::hi\n"),
            ":\u{200b}:notice::hi\n".as_bytes()
        );

        // Legacy `##[` split as `#` | `#[`, `##` | `[`, and `#` | `#` | `[`.
        let mut split_hash = StreamSanitizer::new();
        let mut out = split_hash.push(b"mid #");
        out.extend(split_hash.push(b"#[warning]x\n"));
        out.extend(split_hash.flush());
        assert_eq!(out, "mid #\u{200b}#[warning]x\n".as_bytes());

        let mut split_bracket = StreamSanitizer::new();
        let mut out = split_bracket.push(b"##");
        out.extend(split_bracket.push(b"[error]y\n"));
        out.extend(split_bracket.flush());
        assert_eq!(out, "#\u{200b}#[error]y\n".as_bytes());

        // A trailing `#` with no command behind it flushes unchanged.
        let mut lone_hash = StreamSanitizer::new();
        assert_eq!(lone_hash.push(b"v#"), b"v");
        assert_eq!(lone_hash.push(b"x\n"), b"#x\n");
        assert!(lone_hash.flush().is_empty());
    }

    #[test]
    fn stream_sanitizer_holds_a_multibyte_whitespace_split() {
        // U+00A0 (C2 A0) split across chunks still counts as leading
        // whitespace, so the following `::` is neutralized.
        let nbsp = "\u{a0}";
        let bytes = nbsp.as_bytes();
        let mut stream = StreamSanitizer::new();
        let mut out = stream.push(&bytes[..1]);
        assert!(out.is_empty());
        out.extend(stream.push(&[bytes[1]]));
        out.extend(stream.push(b"::error::x\n"));
        out.extend(stream.flush());
        let mut expected = nbsp.as_bytes().to_vec();
        expected.extend_from_slice(":\u{200b}:error::x\n".as_bytes());
        assert_eq!(out, expected);
    }

    #[test]
    fn external_line_resyncs_a_mid_line_sanitizer_without_dropping_prefixes() {
        // A mask line lands out-of-band while the log is mid-line: the next
        // line starts over in the terminal, so the sanitizer must too.
        let mut stream = StreamSanitizer::new();
        let mut out = stream.push(b"partial ");
        assert_eq!(out, b"partial ");
        stream.note_external_line();
        out = stream.push(b"::error::after-mask\n");
        assert_eq!(out, ":\u{200b}:error::after-mask\n".as_bytes());
        assert!(stream.flush().is_empty());

        // A held-back split prefix still joins the next chunk after the
        // external line instead of being lost.
        let mut split = StreamSanitizer::new();
        assert_eq!(split.push(b"\n:"), b"\n");
        split.note_external_line();
        assert_eq!(
            split.push(b":notice::hi\n"),
            ":\u{200b}:notice::hi\n".as_bytes()
        );
        assert!(split.flush().is_empty());

        // Trailing `#`s are kept as well: the legacy command they may open
        // is neutralized anywhere in the line regardless.
        let mut hashes = StreamSanitizer::new();
        assert_eq!(hashes.push(b"mid ##"), b"mid ");
        hashes.note_external_line();
        let mut out = hashes.push(b"[warning]x\n");
        out.extend(hashes.flush());
        assert_eq!(out, "#\u{200b}#[warning]x\n".as_bytes());
    }

    #[test]
    fn mask_emission_resyncs_the_shared_stdout_sanitizer() {
        // End-to-end through the process-global path: streamed bytes leave the
        // sanitizer mid-line, a mask line is emitted out-of-band, and a forged
        // command opening the next terminal line is still neutralized.
        let _guard = GLOBAL_HARDENING_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let was = enabled();
        set_enabled(true);
        reset_terminal_state();
        let streamed = filter_stdout(b"partial ");
        assert_eq!(streamed, b"partial ");
        // Writes a real `::add-mask::` line to the test harness output; the
        // value is a canary, and the harness captures it per test.
        emit_mask_for_bytes(b"mask-resync-canary-499");
        let forged = filter_stdout(b"::error::after-mask\n");
        assert_eq!(forged, ":\u{200b}:error::after-mask\n".as_bytes());
        assert!(flush_stdout().is_empty());
        reset_terminal_state();
        set_enabled(was);
    }

    #[test]
    fn terminal_reset_clears_cross_run_line_carryover() {
        // A previous run ending mid-line (or holding a split prefix) must not
        // misclassify the next run's first chunk in a long-lived host.
        let _guard = GLOBAL_HARDENING_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let was = enabled();
        set_enabled(true);
        reset_terminal_state();
        let _ = filter_stdout(b"leftover ");
        let _ = filter_stderr(b"\n:");
        reset_terminal_state();
        assert_eq!(
            filter_stdout(b"::error::next-run\n"),
            ":\u{200b}:error::next-run\n".as_bytes()
        );
        assert_eq!(
            filter_stderr(b"::error::next-run\n"),
            ":\u{200b}:error::next-run\n".as_bytes()
        );
        assert!(flush_stdout().is_empty());
        assert!(flush_stderr().is_empty());
        reset_terminal_state();
        set_enabled(was);
    }

    #[test]
    fn registry_sweep_is_a_no_op_without_hardening_or_secrets() {
        // The sweep must never emit mask lines (or fail) when hardening is
        // off, so non-CI runs stay byte-identical.
        let _guard = GLOBAL_HARDENING_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let was = enabled();
        set_enabled(false);
        let registry = crate::manifest::Registry::default();
        emit_masks_for_registry(&registry);
        assert!(!enabled());
        set_enabled(was);
    }

    #[test]
    fn mask_line_bytes_start_their_own_line_only_when_mid_line() {
        // Pure layout: no global state, no stdout writes.
        let line = mask_command("s3cret%0A");
        assert_eq!(
            mask_line_bytes(&line, false),
            format!("{line}\n").into_bytes()
        );
        assert_eq!(
            mask_line_bytes(&line, true),
            format!("\n{line}\n").into_bytes()
        );
        // Exactly one terminated mask line either way (plus the line break
        // that opens it when mid-line): the runner parses per line, so a
        // mask must never span lines or smuggle a raw newline.
        for mid_line in [false, true] {
            let bytes = mask_line_bytes(&line, mid_line);
            let expected_newlines = if mid_line { 2 } else { 1 };
            assert_eq!(
                bytes.iter().filter(|byte| **byte == b'\n').count(),
                expected_newlines
            );
            assert!(bytes.ends_with(b"\n"));
            assert!(!bytes.contains(&b'\r'));
            let mask_part = bytes.strip_prefix(b"\n".as_slice()).unwrap_or(&bytes);
            assert_eq!(mask_part, format!("{line}\n").into_bytes());
        }
    }

    #[test]
    fn at_line_start_tracks_released_newlines() {
        let mut stream = StreamSanitizer::new();
        assert!(stream.at_line_start());
        let _ = stream.push(b"partial ");
        assert!(!stream.at_line_start());
        let _ = stream.push(b"done\n");
        assert!(stream.at_line_start());
        // Held-back bytes do not move the terminal: a line-leading `:`
        // still counts as a line start until it is released.
        let _ = stream.push(b":");
        assert!(stream.at_line_start());
        assert!(!stream.flush().is_empty());
    }

    #[test]
    fn mask_emission_resyncs_both_shared_sanitizers() {
        // The merged job log shares lines across stdout and stderr: a
        // partial stderr line must break the stdout mask onto its own line
        // too, and both streams must classify the next line as a line start
        // afterwards so forgeries stay neutralized.
        let _guard = GLOBAL_HARDENING_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let was = enabled();
        set_enabled(true);
        reset_terminal_state();
        let _ = filter_stdout(b"lead");
        assert!(!stdout_sanitizer()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .at_line_start());
        // Writes a real `::add-mask::` line to the test harness output; the
        // value is a canary, and the harness captures it per test.
        emit_mask_for_bytes(b"mask-resync-both-499");
        // Both streams resync even though only stdout was mid-line here;
        // either stream mid-line breaks the next mask onto its own line.
        for filtered in [
            filter_stdout(b"::error::after-mask\n"),
            filter_stderr(b"::warning::after-mask\n"),
        ] {
            assert!(!filtered.starts_with(b"::"), "{filtered:?}");
        }
        assert_eq!(
            filter_stdout(b"::error::after-mask\n"),
            ":\u{200b}:error::after-mask\n".as_bytes()
        );
        assert!(flush_stdout().is_empty());
        assert!(flush_stderr().is_empty());
        reset_terminal_state();
        set_enabled(was);
    }

    #[test]
    fn mask_emission_breaks_after_a_partial_stderr_line() {
        // A partial stderr line leaves the merged log mid-line while stdout
        // alone looks line-started: the next stdout mask must still open a
        // fresh line, and stderr must resync with it.
        let _guard = GLOBAL_HARDENING_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let was = enabled();
        set_enabled(true);
        reset_terminal_state();
        let _ = filter_stderr(b"err-partial ");
        // Writes a real `::add-mask::` line to the test harness output; the
        // value is a canary, and the harness captures it per test.
        emit_mask_for_bytes(b"mask-resync-stderr-499");
        assert_eq!(
            filter_stderr(b"::error::after-mask\n"),
            ":\u{200b}:error::after-mask\n".as_bytes()
        );
        assert!(flush_stdout().is_empty());
        assert!(flush_stderr().is_empty());
        reset_terminal_state();
        set_enabled(was);
    }
}
