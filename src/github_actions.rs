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
//! Explicit raw file-descriptor wirings stay raw: a manifest `inherit` stdio
//! disposition, an interactive terminal handoff, and program-level redirects
//! that attach the terminal descriptors directly all bypass neutralization by
//! construction. Only the bytes Execute itself copies to the log are filtered.
//! `execute check --format github` intentionally emits workflow commands and is
//! unaffected.

use std::sync::atomic::{AtomicBool, Ordering};

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

/// Zero-width space, inserted between the colons of a line-leading `::` so the
/// runner no longer recognizes a workflow command while the text still reads
/// the same.
const ZERO_WIDTH_SPACE: &[u8] = "\u{200b}".as_bytes();

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
}
